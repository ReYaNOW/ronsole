//! Headless command session: owns the offscreen `App`, its profile and the input
//! state that protocol commands drive.

use super::Profile;
use super::backend::HeadlessCounters;
use super::bench;
use super::dump;
use super::keys::{key_input, modifier_key_input};
use super::protocol::{
    BenchAction, ClickPhase, Command, KeyCombo, MouseButtonArg, Response, WheelUnit, parse_line,
};
use crate::app::{App, HeadlessRunError, LoopFlow, LoopStep, StepReport};
use crate::input_types::{KeyInput, KeyState, Modifiers, PointerButton, PointerPosition, ScrollDelta};
use crate::launch::TerminalLaunchSpec;
use crate::runtime::WindowRuntime;
use crate::terminal_process::TERMINAL_OUTPUT_BATCH_IDLE;
use std::fs;
use std::io::{self, BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Settle quiet time before `settle`'s start shell output is considered finished.
const START_SETTLE_MS: u64 = 500;
/// Longest single poll wait used by the counting commands, so their clock checks stay fine-grained.
const POLL_SLICE_MS: u64 = 2;

/// Where the session's profile directory lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileChoice {
    /// `${XDG_RUNTIME_DIR:-/tmp}/ronsole-headless-<pid>[-<n>]`, removed on exit unless `keep`.
    Temp { keep: bool },
    /// Caller-owned directory (`--profile DIR`); never removed.
    Dir(PathBuf),
}

/// Where the frame budget value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HzSource {
    /// Given explicitly with `--hz` or `--budget-ms`.
    Arg,
    /// Fallback default frame rate.
    Default,
}

/// Frame pacing budget of the session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameBudget {
    pub(crate) budget_ms: f64,
    pub(crate) hz: f64,
    pub(crate) source: HzSource,
}

/// Options of one protocol session, parsed from the `--headless` flags.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionOptions {
    /// Physical size of the offscreen surface.
    pub(crate) size: (u32, u32),
    pub(crate) scale: f32,
    pub(crate) profile: ProfileChoice,
    pub(crate) shell: Option<PathBuf>,
    pub(crate) budget: FrameBudget,
}

/// Why a session could not start. `Egl` maps to exit code 3, `Other` to 1.
#[derive(Debug)]
pub(crate) enum StartError {
    Egl(String),
    Other(String),
}

/// Why the app's `wake` step returned, in the reporting order of spec §4.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    Redraw,
    Poll,
    UiWaker,
    Deadline,
    Timeout,
}

impl Cause {
    fn name(self) -> &'static str {
        match self {
            Cause::Redraw => "redraw",
            Cause::Poll => "poll",
            Cause::UiWaker => "ui_waker",
            Cause::Deadline => "deadline",
            Cause::Timeout => "timeout",
        }
    }
}

/// Outcome of one `wake` attempt: its cause and the last step that ran.
struct WakeOutcome {
    cause: Cause,
    frame: bool,
    last: StepReport,
}

/// A running headless app plus the state the protocol commands need.
pub(crate) struct HeadlessSession {
    app: App,
    /// Receives the links the app would open instead of launching them; `dump` drains it.
    receiver: Receiver<String>,
    /// Newest link drained from `receiver` and not yet reported as `external_request`.
    last_external: Option<String>,
    /// Declared after `app` so the app is torn down before its profile directory goes.
    profile: Profile,
    /// Modifiers the protocol holds (`mods`), as the app last saw them.
    mods: Modifiers,
    /// Last pointer position sent with `mouse_move`, in physical pixels.
    pointer: PointerPosition,
    size: (u32, u32),
    scale: f32,
    budget: FrameBudget,
    last_report: Option<StepReport>,
    exited: bool,
}

impl HeadlessSession {
    /// Opens the profile, starts one terminal tab and settles its first output.
    pub(crate) fn start(options: SessionOptions) -> Result<Self, StartError> {
        let profile = open_profile(&options.profile)
            .map_err(|error| StartError::Other(format!("profile: {error}")))?;
        crate::prefer_egl_vendor();
        crate::tune_glibc_allocator();
        let (mut app, receiver) = App::load_headless(None, &profile.dir, options.shell.clone())
            .map_err(StartError::Other)?;
        app.start_headless(options.size, options.scale, TerminalLaunchSpec::default())
            .map_err(|error| match error {
                HeadlessRunError::Egl(message) => StartError::Egl(message),
                HeadlessRunError::Run(message) => StartError::Other(message),
            })?;
        let mut session = Self {
            app,
            receiver,
            last_external: None,
            profile,
            mods: Modifiers::empty(),
            pointer: PointerPosition::default(),
            size: options.size,
            scale: options.scale,
            budget: options.budget,
            last_report: None,
            exited: false,
        };
        session
            .settle(START_SETTLE_MS)
            .map_err(StartError::Other)?;
        Ok(session)
    }

    /// Runs one parsed command and returns its response.
    pub(crate) fn execute(&mut self, command: Command) -> Response {
        let result = match command {
            Command::Type(text) => self.type_text(&text),
            Command::Key { combo, repeats } => self.key(combo, repeats).map(|()| None),
            Command::Mods(mods) => self.set_mods(mods).map(|()| None),
            Command::MouseMove { x, y } => self.mouse_move(x, y).map(|()| None),
            Command::Click { button, phase, alt } => {
                self.click(button, phase, alt).map(|()| None)
            }
            Command::Wheel { dx, dy, unit } => self.wheel(dx, dy, unit).map(|()| None),
            Command::Resize { w, h } => self.resize(w, h),
            Command::Scale(scale) => self.scale_to(scale).map(|()| None),
            Command::Screenshot(path) => self.screenshot(&path),
            Command::Settle { ms } => self.settle(ms),
            Command::Wait { ms } => self.wait(ms),
            Command::Wake { ms } => self.wake(ms),
            Command::Idle { ms } => self.idle(ms),
            Command::Info => self.info(),
            Command::Quit => Ok(None),
            Command::Dump(path) => self.dump(path.as_deref()),
            Command::DumpRow(row) => self.dump_row(row),
            Command::MouseCell { col, row } => self.mouse_cell(col, row),
            Command::Bench { frames, csv, action } => {
                return bench::bench(self, frames, csv, action);
            }
        };
        match result {
            Ok(payload) => Response::Ok(payload),
            Err(reason) => Response::Err(reason),
        }
    }

    /// True once the app has ended; the protocol reads no further commands.
    pub(crate) fn exited(&self) -> bool {
        self.exited
    }

    /// Removes the profile directory when the session is dropped, after the app's teardown
    /// has written to it. Tests use it for the caller-owned directories they create.
    #[cfg(test)]
    pub(super) fn remove_profile_on_drop(&mut self) {
        self.profile.remove_on_drop = true;
    }

    #[cfg(test)]
    pub(super) fn profile_dir(&self) -> &Path {
        &self.profile.dir
    }

    /// The app, for `dump` and other readers of its state.
    pub(crate) fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// The last loop step report of the session, from any step.
    pub(crate) fn last_report(&self) -> Option<StepReport> {
        self.last_report
    }

    /// The window size in physical pixels, as the session last set it.
    pub(super) fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The scale factor, as the session last set it.
    pub(super) fn scale(&self) -> f32 {
        self.scale
    }

    /// Drains the link-open channel and returns the newest link not yet reported, so each
    /// link is reported by one `dump` only.
    pub(super) fn take_external_request(&mut self) -> Option<String> {
        while let Ok(arg) = self.receiver.try_recv() {
            self.last_external = Some(arg);
        }
        self.last_external.take()
    }

    /// `dump [path]`: the state as one JSON line, or written to `path` with its absolute path
    /// as the payload.
    fn dump(&mut self, path: Option<&Path>) -> Result<Option<String>, String> {
        let text = dump::dump_json(self).to_string();
        let Some(path) = path else {
            return Ok(Some(text));
        };
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        fs::write(path, &text).map_err(|error| format!("write {}: {error}", path.display()))?;
        let absolute = fs::canonicalize(path)
            .map_err(|error| format!("resolve {}: {error}", path.display()))?;
        Ok(Some(absolute.display().to_string()))
    }

    /// `dump row <row>`: the non-blank cells of one visible row as one JSON line.
    fn dump_row(&mut self, row: usize) -> Result<Option<String>, String> {
        dump::dump_row_json(self, row).map(|value| Some(value.to_string()))
    }

    /// `mouse_cell <col> <row>`: moves the pointer to the centre of a visible cell, as `mouse_move` does.
    fn mouse_cell(&mut self, col: usize, row: usize) -> Result<Option<String>, String> {
        let (x, y) = dump::cell_center_at(self, col, row)
            .ok_or_else(|| "cell out of range".to_string())?;
        self.mouse_move(f64::from(x), f64::from(y))?;
        Ok(Some(format!("{x} {y}")))
    }

    fn type_text(&mut self, text: &str) -> Result<Option<String>, String> {
        self.app.on_ime_commit(text);
        self.frame()?;
        Ok(None)
    }

    /// Presses the combo's missing modifiers, then the key, then releases both.
    fn key(&mut self, combo: KeyCombo, repeats: u32) -> Result<(), String> {
        let base = self.mods;
        let target = union(base, combo.mods);
        if self.move_modifiers(target) {
            return Ok(());
        }
        if self.send_key(key_input(&combo.key, KeyState::Pressed, target)) {
            return Ok(());
        }
        for _ in 0..repeats {
            if self.send_key(key_input(&combo.key, KeyState::Repeat, target)) {
                return Ok(());
            }
        }
        self.frame()?;
        if self.send_key(key_input(&combo.key, KeyState::Released, target)) {
            return Ok(());
        }
        if self.move_modifiers(base) {
            return Ok(());
        }
        self.frame()
    }

    /// Sets the held modifiers to `mods` with one event per changed modifier, then draws.
    fn set_mods(&mut self, mods: Modifiers) -> Result<(), String> {
        if self.move_modifiers(mods) {
            return Ok(());
        }
        self.frame()
    }

    fn mouse_move(&mut self, x: f64, y: f64) -> Result<(), String> {
        self.pointer = PointerPosition {
            x: x as f32,
            y: y as f32,
        };
        self.app.on_pointer_motion(self.pointer);
        self.frame()
    }

    /// Sends press and/or release for `button`, with Alt held around the events when asked.
    fn click(&mut self, button: MouseButtonArg, phase: ClickPhase, alt: bool) -> Result<(), String> {
        let base = self.mods;
        let target = if alt {
            union(base, Modifiers::new(false, false, true, false))
        } else {
            base
        };
        if self.move_modifiers(target) {
            return Ok(());
        }
        let button = match button {
            MouseButtonArg::Left => PointerButton::Left,
            MouseButtonArg::Right => PointerButton::Right,
            MouseButtonArg::Middle => PointerButton::Middle,
        };
        let mut states = Vec::with_capacity(2);
        if matches!(phase, ClickPhase::Both | ClickPhase::Down) {
            states.push(KeyState::Pressed);
        }
        if matches!(phase, ClickPhase::Both | ClickPhase::Up) {
            states.push(KeyState::Released);
        }
        for state in states {
            if self.app.on_pointer_button(state, button) {
                self.exited = true;
                return Ok(());
            }
            self.frame()?;
        }
        if alt && self.move_modifiers(base) {
            return Ok(());
        }
        if alt { self.frame() } else { Ok(()) }
    }

    fn wheel(&mut self, dx: f64, dy: f64, unit: WheelUnit) -> Result<(), String> {
        let (x, y) = (dx as f32, dy as f32);
        let delta = match unit {
            WheelUnit::Lines => ScrollDelta::Line { x, y },
            WheelUnit::Pixels => ScrollDelta::Pixel { x, y },
        };
        self.app.on_scroll(delta);
        self.frame()
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<Option<String>, String> {
        let runtime = self
            .app
            .runtime_mut()
            .ok_or_else(|| "no headless runtime".to_string())?;
        runtime.resize_offscreen(width, height)?;
        let logical = (
            f64::from(width) / f64::from(self.scale),
            f64::from(height) / f64::from(self.scale),
        );
        self.app
            .on_resize_with_logical_size(width, height, Some(logical));
        self.size = (width, height);
        self.frame()?;
        Ok(Some(format!("{width}x{height}")))
    }

    fn scale_to(&mut self, scale: f32) -> Result<(), String> {
        self.scale = scale;
        let (width, height) = self.size;
        self.app.on_scale_changed(scale, width, height);
        self.frame()
    }

    fn screenshot(&mut self, path: &Path) -> Result<Option<String>, String> {
        if self.counters().is_some_and(|counters| counters.redraw_requested) {
            self.frame()?;
        }
        let runtime = self
            .app
            .runtime()
            .ok_or_else(|| "no headless runtime".to_string())?;
        let (width, height, pixels) = runtime.read_pixels()?;
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        write_png(path, width, height, &pixels)?;
        let absolute = fs::canonicalize(path)
            .map_err(|error| format!("resolve {}: {error}", path.display()))?;
        Ok(Some(format!("{} {width}x{height}", absolute.display())))
    }

    /// Steps until output has been quiet for two output batch idle periods on the clock,
    /// or until `ms` has passed.
    fn settle(&mut self, ms: u64) -> Result<Option<String>, String> {
        let started = Instant::now();
        let budget = Duration::from_millis(ms);
        let quiet_needed = TERMINAL_OUTPUT_BATCH_IDLE * 2;
        let frames_before = self.frame_count();
        let mut quiet_since: Option<Instant> = None;
        let mut settled = false;
        loop {
            let step_start = Instant::now();
            let Some(report) = self.step(Some(POLL_SLICE_MS as i32))? else {
                break;
            };
            let now = Instant::now();
            let redraw = self.counters().is_some_and(|counters| counters.redraw_requested);
            let idle = !report.frame
                && !redraw
                && report
                    .deadline
                    .is_none_or(|deadline| deadline > started + budget);
            if idle {
                let since = *quiet_since.get_or_insert(step_start);
                if now.duration_since(since) >= quiet_needed {
                    settled = true;
                    break;
                }
            } else {
                quiet_since = None;
            }
            if now.duration_since(started) >= budget {
                break;
            }
        }
        let frames = self.frame_count() - frames_before;
        Ok(Some(format!("frames={frames} settled={settled}")))
    }

    /// Steps in slices of at most `POLL_SLICE_MS` until `ms` has passed on the clock.
    fn wait(&mut self, ms: u64) -> Result<Option<String>, String> {
        let frames_before = self.frame_count();
        let end = Instant::now() + Duration::from_millis(ms);
        while !self.exited {
            let now = Instant::now();
            if now >= end {
                break;
            }
            let slice = (end - now).as_millis().clamp(1, u128::from(POLL_SLICE_MS)) as i32;
            self.step(Some(slice))?;
        }
        let frames = self.frame_count() - frames_before;
        Ok(Some(format!("frames={frames}")))
    }

    fn wake(&mut self, ms: u64) -> Result<Option<String>, String> {
        let Some(outcome) = self.wake_once(ms)? else {
            return Ok(None);
        };
        let flow = flow_name(outcome.last.flow);
        let deadline = self.deadline_ms(outcome.last);
        Ok(Some(format!(
            "cause={} frame={} flow={flow} deadline_ms={deadline}",
            outcome.cause.name(),
            outcome.frame
        )))
    }

    /// Repeats `wake` with the remaining time until `ms` has passed, counting causes.
    fn idle(&mut self, ms: u64) -> Result<Option<String>, String> {
        let frames_before = self.frame_count();
        let end = Instant::now() + Duration::from_millis(ms);
        let (mut redraws, mut polls, mut wakes, mut deadlines) = (0_u64, 0_u64, 0_u64, 0_u64);
        let mut last: Option<StepReport> = None;
        while !self.exited {
            let now = Instant::now();
            if now >= end {
                break;
            }
            let remaining = (end - now).as_millis().max(1) as u64;
            let Some(outcome) = self.wake_once(remaining)? else {
                break;
            };
            match outcome.cause {
                Cause::Redraw => redraws += 1,
                Cause::Poll => polls += 1,
                Cause::UiWaker => wakes += 1,
                Cause::Deadline => deadlines += 1,
                Cause::Timeout => {}
            }
            last = Some(outcome.last);
        }
        let frames = self.frame_count() - frames_before;
        let (flow, deadline) = match last {
            Some(report) => (flow_name(report.flow), self.deadline_ms(report)),
            None => ("wait", "none".to_string()),
        };
        Ok(Some(format!(
            "frames={frames} redraws={redraws} polls={polls} wakes={wakes} deadlines={deadlines} flow={flow} deadline_ms={deadline}"
        )))
    }

    /// One wake attempt. The cause is read from the first step, before any frame is drawn;
    /// a follow-up zero-wait step then draws a frame requested by the wake-up.
    fn wake_once(&mut self, ms: u64) -> Result<Option<WakeOutcome>, String> {
        let requested_before = self.counters().is_some_and(|counters| counters.redraw_requested);
        let Some(first) = self.step(Some(ms.min(i32::MAX as u64) as i32))? else {
            return Ok(None);
        };
        let mut frame = first.frame;
        let mut last = first;
        let mut follow_up_done = false;
        let cause = if first.poll.is_none() && first.frame {
            Cause::Redraw
        } else if requested_before && first.poll.is_some_and(|poll| poll.timed_out) {
            let Some(second) = self.step(Some(0))? else {
                return Ok(None);
            };
            follow_up_done = true;
            frame |= second.frame;
            last = second;
            if second.frame {
                Cause::Redraw
            } else {
                classify(first, Instant::now())
            }
        } else {
            classify(first, Instant::now())
        };
        if !follow_up_done && matches!(cause, Cause::Poll | Cause::UiWaker | Cause::Deadline) {
            let Some(second) = self.step(Some(0))? else {
                return Ok(None);
            };
            frame |= second.frame;
            last = second;
        }
        Ok(Some(WakeOutcome { cause, frame, last }))
    }

    fn info(&self) -> Result<Option<String>, String> {
        let runtime = self
            .app
            .runtime()
            .ok_or_else(|| "no headless runtime".to_string())?;
        let strings = runtime
            .gl_strings()
            .ok_or_else(|| "no offscreen GL context".to_string())?;
        let payload = serde_json::json!({
            "gl_renderer": strings.renderer,
            "gl_version": strings.version,
            "gl_vendor": strings.vendor,
            "profile": self.profile.dir.display().to_string(),
            "budget_ms": self.budget.budget_ms,
            "hz": self.budget.hz,
        });
        Ok(Some(payload.to_string()))
    }

    /// Sends the modifier events that turn the held set into `to`: presses in ctrl, shift,
    /// alt, super order, then releases in reverse. Returns `true` if the app asked to exit.
    fn move_modifiers(&mut self, to: Modifiers) -> bool {
        let before = flags(self.mods);
        let after = flags(to);
        let mut held = before;
        for index in 0..4 {
            if after[index] && !before[index] {
                if self.send_key(modifier_key_input(single(index), KeyState::Pressed)) {
                    return true;
                }
                held[index] = true;
                self.app.on_modifiers(from_flags(held));
            }
        }
        for index in (0..4).rev() {
            if before[index] && !after[index] {
                if self.send_key(modifier_key_input(single(index), KeyState::Released)) {
                    return true;
                }
                held[index] = false;
                self.app.on_modifiers(from_flags(held));
            }
        }
        self.mods = to;
        false
    }

    /// Delivers one key event; `true` if the app asked to exit.
    fn send_key(&mut self, input: KeyInput) -> bool {
        let exit = self.app.on_key(input);
        if exit {
            self.exited = true;
        }
        exit
    }

    pub(super) fn counters(&self) -> Option<HeadlessCounters> {
        self.app.runtime().and_then(WindowRuntime::headless_counters)
    }

    fn frame_count(&self) -> u64 {
        self.counters().map_or(0, |counters| counters.frames)
    }

    fn deadline_ms(&self, report: StepReport) -> String {
        report.deadline.map_or_else(
            || "none".to_string(),
            |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .to_string()
            },
        )
    }

    /// One loop step with the wait capped at `max_wait_ms`; `None` once the app has exited.
    fn step(&mut self, max_wait_ms: Option<i32>) -> Result<Option<StepReport>, String> {
        let step = self.app.headless_step(max_wait_ms)?;
        self.record(step)
    }

    /// One forced frame; `None` once the app has exited.
    fn forced(&mut self) -> Result<Option<StepReport>, String> {
        let step = self.app.forced_step()?;
        self.record(step)
    }

    fn frame(&mut self) -> Result<(), String> {
        self.forced().map(|_| ())
    }

    /// The frame budget the session was started with.
    pub(super) fn budget(&self) -> FrameBudget {
        self.budget
    }

    /// Applies one bench action as events only, then draws one forced frame, so each call
    /// draws exactly one frame. Returns the time of the action plus `about_to_wait`
    /// (`update_ms`) and the time of the frame (`draw_cpu_ms`).
    pub(super) fn bench_frame(&mut self, action: &BenchAction) -> Result<(Duration, Duration), String> {
        let start = Instant::now();
        let exit = match action {
            BenchAction::None => false,
            BenchAction::Wheel { dx, dy } => {
                self.app.on_scroll(ScrollDelta::Line {
                    x: *dx as f32,
                    y: *dy as f32,
                });
                false
            }
            BenchAction::Type(text) => {
                self.app.on_ime_commit(text);
                false
            }
            BenchAction::Key(combo) => self.press_release(*combo),
        };
        if exit {
            return Err("app exited during bench".to_string());
        }
        let (step, draw_start) = self.app.forced_step_timed()?;
        let draw_end = Instant::now();
        if self.record(step)?.is_none() {
            return Err("app exited during bench".to_string());
        }
        Ok((draw_start - start, draw_end - draw_start))
    }

    /// Presses and releases `combo`, with its modifiers that `mods` does not hold, and draws
    /// no frame between the events. Returns `true` if the app asked to exit.
    fn press_release(&mut self, combo: KeyCombo) -> bool {
        let base = self.mods;
        let target = union(base, combo.mods);
        self.move_modifiers(target)
            || self.send_key(key_input(&combo.key, KeyState::Pressed, target))
            || self.send_key(key_input(&combo.key, KeyState::Released, target))
            || self.move_modifiers(base)
    }

    fn record(&mut self, step: LoopStep) -> Result<Option<StepReport>, String> {
        match step {
            LoopStep::Continue(report) => {
                // forced_step does not check the session error itself, so check it here.
                if let Some(error) = self.app.runtime_mut().and_then(WindowRuntime::take_session_error) {
                    return Err(error);
                }
                self.last_report = Some(report);
                Ok(Some(report))
            }
            LoopStep::Exit(result) => {
                self.exited = true;
                result.map(|()| None)
            }
        }
    }
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        self.app.headless_exit();
    }
}

/// Runs the protocol over `input` and writes one response line per answered command.
/// Returns 0 when every answer was `ok`, 1 after an `err`, unreadable input or a failed
/// write, and 3 when the EGL session cannot start. Reading ends at EOF, `quit`, or app exit.
pub(crate) fn run_protocol(
    options: SessionOptions,
    input: impl BufRead,
    output: &mut impl Write,
) -> u8 {
    let mut session = match HeadlessSession::start(options) {
        Ok(session) => session,
        Err(StartError::Egl(error)) => {
            eprintln!("headless: {error}");
            return 3;
        }
        Err(StartError::Other(error)) => {
            eprintln!("headless: {error}");
            return 1;
        }
    };
    serve(&mut session, input, output)
}

/// Answers each protocol line of `input` on `output` with the session's response.
pub(crate) fn serve(
    session: &mut HeadlessSession,
    mut input: impl BufRead,
    output: &mut impl Write,
) -> u8 {
    let mut code = 0;
    let mut line = Vec::new();
    // The app may have ended before the first command, e.g. during the start `settle`.
    if session.exited() {
        return code;
    }
    loop {
        line.clear();
        match input.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("headless: input: {error}");
                return 1;
            }
        }
        let (response, stop) = match parse_line(&line) {
            Ok(None) => continue,
            Ok(Some(command)) => {
                let stop = matches!(command, Command::Quit);
                (session.execute(command), stop)
            }
            Err(reason) => (Response::Err(reason), false),
        };
        if matches!(response, Response::Err(_)) {
            code = 1;
        }
        let written = writeln!(output, "{}", response.line()).and_then(|()| output.flush());
        if written.is_err() {
            return 1;
        }
        if stop || session.exited() {
            break;
        }
    }
    code
}

fn open_profile(choice: &ProfileChoice) -> io::Result<Profile> {
    match choice {
        ProfileChoice::Temp { keep } => {
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            let profile = Profile::create(runtime_dir, std::process::id())?;
            Ok(if *keep { profile.keep() } else { profile })
        }
        ProfileChoice::Dir(dir) => Profile::existing(dir.clone()),
    }
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), String> {
    let file = fs::File::create(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("png header: {error}"))?;
    writer
        .write_image_data(pixels)
        .map_err(|error| format!("png data: {error}"))
}

/// Maps a wake step to its cause, reading the first step before any follow-up draws.
fn classify(report: StepReport, now: Instant) -> Cause {
    if report.flow == LoopFlow::Poll {
        Cause::Poll
    } else if report.poll.is_some_and(|poll| poll.woke) {
        Cause::UiWaker
    } else if report.deadline.is_some_and(|deadline| deadline <= now) {
        Cause::Deadline
    } else {
        Cause::Timeout
    }
}

pub(super) fn flow_name(flow: LoopFlow) -> &'static str {
    match flow {
        LoopFlow::Wait => "wait",
        LoopFlow::Poll => "poll",
        LoopFlow::WaitUntil => "wait_until",
    }
}

/// Modifier flags in press order: ctrl, shift, alt, super.
fn flags(mods: Modifiers) -> [bool; 4] {
    [mods.control_key(), mods.shift_key(), mods.alt_key(), mods.super_key()]
}

fn from_flags([ctrl, shift, alt, super_key]: [bool; 4]) -> Modifiers {
    Modifiers::new(shift, ctrl, alt, super_key)
}

fn single(index: usize) -> Modifiers {
    let mut value = [false; 4];
    value[index] = true;
    from_flags(value)
}

fn union(a: Modifiers, b: Modifiers) -> Modifiers {
    let (fa, fb) = (flags(a), flags(b));
    from_flags([fa[0] || fb[0], fa[1] || fb[1], fa[2] || fb[2], fa[3] || fb[3]])
}
