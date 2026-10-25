//! Headless run: bootstraps the offscreen runtime and drives the shared event loop
//! without a window, a Wayland connection, or a single-instance listener.

use super::direct_wayland::{LoopStep, StepReport, loop_flow};
use super::{App, AppLoopControl};
use crate::config::AppConfig;
use crate::input::TerminalInteraction;
use crate::launch::TerminalLaunchSpec;
use crate::runtime::WindowRuntime;
use crate::terminal::Terminal;
use std::time::Instant;

/// Why a headless run stopped with an error. `Egl` means the runtime never started,
/// so the caller reports it as a startup failure.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeadlessRunError {
    Egl(String),
    Run(String),
}

impl App {
    pub(crate) fn run_headless(
        &mut self,
        physical: (u32, u32),
        scale: f32,
        initial_launch: TerminalLaunchSpec,
    ) -> Result<(), HeadlessRunError> {
        let result = self.run_headless_inner(physical, scale, initial_launch);
        if let Err(HeadlessRunError::Run(error)) = result.as_ref() {
            self.interrupt_automation(&format!("headless runtime failure: {error}"));
        }
        self.on_exiting();
        result
    }

    fn run_headless_inner(
        &mut self,
        physical: (u32, u32),
        scale: f32,
        initial_launch: TerminalLaunchSpec,
    ) -> Result<(), HeadlessRunError> {
        self.start_headless(physical, scale, initial_launch)?;
        loop {
            match self.headless_step(None).map_err(HeadlessRunError::Run)? {
                LoopStep::Continue(_) => {}
                LoopStep::Exit(result) => return result.map_err(HeadlessRunError::Run),
            }
        }
    }

    /// Bootstraps the offscreen runtime and installs it with its first tab, without
    /// running the loop. A protocol session drives the loop with the step methods.
    pub(crate) fn start_headless(
        &mut self,
        physical: (u32, u32),
        scale: f32,
        launch: TerminalLaunchSpec,
    ) -> Result<(), HeadlessRunError> {
        let runtime = WindowRuntime::bootstrap_headless(
            physical.0,
            physical.1,
            scale,
            self.config.terminal_font_size,
            self.config.terminal_background,
        )
        .map_err(HeadlessRunError::Egl)?;
        self.prepare_bootstrapped_runtime(runtime, launch);
        Ok(())
    }

    /// One loop iteration with the wait capped at `max_wait_ms` (`None` or `-1` for no cap).
    pub(crate) fn headless_step(&mut self, max_wait_ms: Option<i32>) -> Result<LoopStep, String> {
        self.loop_step(None, max_wait_ms)
    }

    /// Draws a frame now, without waiting for the frame pace. Used after an input
    /// command so the session sees the state that command produced.
    pub(crate) fn forced_step(&mut self) -> Result<LoopStep, String> {
        self.forced_step_timed().map(|(step, _)| step)
    }

    /// `forced_step` that also returns the instant `about_to_wait` ended and the frame began,
    /// so `bench` can time the two parts apart.
    pub(crate) fn forced_step_timed(&mut self) -> Result<(LoopStep, Instant), String> {
        let control = self.on_about_to_wait();
        let draw_start = Instant::now();
        if control == AppLoopControl::Exit {
            return Ok((LoopStep::Exit(self.take_automation_exit_result()), draw_start));
        }
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.clear_frame_request();
        }
        let frame = self.on_frame()?;
        let deadline = self.wait_deadline(control);
        let step = LoopStep::Continue(StepReport {
            frame,
            flow: loop_flow(control),
            deadline,
            poll: None,
        });
        Ok((step, draw_start))
    }

    /// Ends a headless session: the teardown that `run_headless` runs after its loop.
    pub(crate) fn headless_exit(&mut self) {
        self.on_exiting();
    }

    /// Every open tab, in tab order.
    pub(crate) fn terminals(&self) -> &[Terminal] {
        &self.terminals
    }

    /// Index into `terminals` of the active tab.
    pub(crate) fn active_terminal_index(&self) -> usize {
        self.active_terminal
    }

    /// The active tab, or `None` when no tab exists.
    pub(crate) fn active_terminal(&self) -> Option<&Terminal> {
        self.terminals.get(self.active_terminal)
    }

    /// Selection, pointer, and layout state from the last drawn frame.
    pub(crate) fn interaction(&self) -> &TerminalInteraction {
        &self.interaction
    }

    /// Text of the in-memory clipboard, which is the only clipboard a headless run uses.
    pub(crate) fn clipboard_text(&mut self) -> Option<String> {
        self.interaction.clipboard_text()
    }

    /// The window runtime; `None` before `start_headless`.
    pub(crate) fn runtime(&self) -> Option<&WindowRuntime> {
        self.runtime.as_ref()
    }

    /// Mutable window runtime, used by the session to drive the offscreen backend.
    pub(crate) fn runtime_mut(&mut self) -> Option<&mut WindowRuntime> {
        self.runtime.as_mut()
    }

    /// Loaded configuration; the session reads the terminal background from it.
    pub(crate) fn config(&self) -> &AppConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AutomationOptions;
    use crate::app::automation::AutomationController;
    use crate::app::direct_wayland::LoopFlow;
    use crate::headless::Profile;
    use crate::headless::backend::HeadlessCounters;
    use crate::runtime::WaylandPollOutcome;
    use std::path::PathBuf;
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    /// Creates `<tmp>/ronsole-t4-<tag>-<pid>/workspace` with an empty fixture file
    /// (the name matches `FIXTURE_NAME` in the automation module).
    fn workspace_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ronsole-t4-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("test workspace");
        std::fs::write(workspace.join("terminal_fixture.sh"), "").expect("test fixture");
        root
    }

    fn options_for(root: &std::path::Path) -> AutomationOptions {
        AutomationOptions {
            workspace: root.join("workspace"),
            report: root.join("report.json"),
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn headless_session_error_fails_automation() {
        crate::prefer_egl_vendor();
        let root = workspace_root("session");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let (mut app, _receiver) = App::load_headless(Some(options_for(&root)), &profile.dir, None)
            .expect("headless app");
        let mut runtime = WindowRuntime::bootstrap_headless(
            320,
            200,
            1.0,
            14.0,
            crate::config::DEFAULT_TERMINAL_BACKGROUND,
        )
        .expect("headless runtime");
        runtime.inject_session_error("resize pbuffer: test".to_string());
        app.prepare_bootstrapped_runtime(
            runtime,
            TerminalLaunchSpec::hermetic_shell_for_test(),
        );

        let step = app.loop_step(None, None).expect("loop step");
        assert!(matches!(step, LoopStep::Exit(Err(_))));
        let report = std::fs::read_to_string(root.join("report.json"))
            .expect("automation must write its report");
        assert!(report.contains("\"status\": \"failed\""), "{report}");
        assert!(report.contains("resize pbuffer: test"), "{report}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn headless_short_automation_reaches_ok() {
        crate::prefer_egl_vendor();
        let root = workspace_root("short");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let controller = AutomationController::with_short_scenario(options_for(&root))
            .expect("short automation scenario");
        let (mut app, _receiver) = App::load_headless_with(Some(controller), &profile.dir, None)
            .expect("headless app");

        app.run_headless(
            (320, 200),
            1.0,
            TerminalLaunchSpec::hermetic_shell_for_test(),
        )
        .expect("short automation must finish");
        let report = std::fs::read_to_string(root.join("report.json"))
            .expect("automation must write its report");
        assert!(report.contains("\"status\": \"ok\""), "{report}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn headless_zero_size_maps_to_egl_error() {
        crate::prefer_egl_vendor();
        let root = workspace_root("zero");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let (mut app, _receiver) =
            App::load_headless(None, &profile.dir, None).expect("headless app");
        let error = app
            .run_headless((0, 0), 1.0, TerminalLaunchSpec::default())
            .expect_err("zero-size surface must not start");
        assert_eq!(
            error,
            HeadlessRunError::Egl("EGL setup failed at CreatePbuffer: zero size".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Headless app with no automation and its 640x400 runtime started on a hermetic bash.
    /// The caller removes the returned `root` at the end of the test.
    fn started_headless(tag: &str) -> (App, Receiver<String>, Profile, PathBuf) {
        crate::prefer_egl_vendor();
        let root = workspace_root(tag);
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let (mut app, receiver) =
            App::load_headless(None, &profile.dir, None).expect("headless app");
        app.start_headless((640, 400), 1.0, TerminalLaunchSpec::hermetic_shell_for_test())
            .expect("headless runtime starts");
        (app, receiver, profile, root)
    }

    fn counters(app: &App) -> HeadlessCounters {
        app.runtime
            .as_ref()
            .and_then(WindowRuntime::headless_counters)
            .expect("headless counters")
    }

    /// Steps until a step times out with nothing drawn, no pending frame request, and
    /// the app asking to wait, so the next step starts from an idle session. A step
    /// that woke from the PTY or left a frame request behind is not idle.
    fn settle(app: &mut App) {
        for _ in 0..200 {
            match app.headless_step(Some(20)).expect("headless step") {
                LoopStep::Continue(report)
                    if report.poll.is_some_and(|poll| poll.timed_out)
                        && !report.frame
                        && report.flow == LoopFlow::Wait
                        && !counters(app).redraw_requested =>
                {
                    return;
                }
                LoopStep::Continue(_) => {}
                LoopStep::Exit(result) => panic!("session ended while settling: {result:?}"),
            }
        }
        panic!("session did not settle within 200 steps");
    }

    #[test]
    fn step_reports_frame_after_request() {
        let (mut app, _receiver, _profile, root) = started_headless("step-frame");
        let before = counters(&app).frames;
        app.request_frame();
        let started = Instant::now();
        let mut drew = false;
        while started.elapsed() < Duration::from_millis(200) {
            match app.headless_step(Some(50)).expect("headless step") {
                LoopStep::Continue(report) if report.frame => {
                    drew = true;
                    break;
                }
                LoopStep::Continue(_) => {}
                LoopStep::Exit(result) => panic!("session ended early: {result:?}"),
            }
        }
        assert!(drew, "no frame within 200 ms of a request");
        let after = counters(&app);
        assert_eq!(after.frames, before + 1);
        assert!(after.frame_requests >= 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn idle_step_times_out_with_wait_flow() {
        let (mut app, _receiver, _profile, root) = started_headless("step-idle");
        settle(&mut app);
        let started = Instant::now();
        let step = app.headless_step(Some(20)).expect("headless step");
        let elapsed = started.elapsed();
        let LoopStep::Continue(report) = step else {
            panic!("idle step must continue: {step:?}");
        };
        assert_eq!(
            report.poll,
            Some(WaylandPollOutcome {
                woke: false,
                timed_out: true,
            })
        );
        assert_eq!(report.flow, LoopFlow::Wait);
        assert!(elapsed >= Duration::from_millis(15), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(200), "{elapsed:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wake_handle_wakes_step() {
        let (mut app, _receiver, _profile, root) = started_headless("step-wake");
        settle(&mut app);
        let wake = app.runtime.as_ref().expect("runtime").wake_handle();
        wake.wake();
        assert!(wake.pending(), "wake must be queued before the step");
        let started = Instant::now();
        let step = app.headless_step(Some(500)).expect("headless step");
        let elapsed = started.elapsed();
        let LoopStep::Continue(report) = step else {
            panic!("wake step must continue: {step:?}");
        };
        assert!(report.poll.is_some_and(|poll| poll.woke), "{report:?}");
        assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn forced_step_draws_without_pace() {
        let (mut app, _receiver, _profile, root) = started_headless("step-forced");
        settle(&mut app);
        app.request_frame();
        let before = counters(&app).frames;
        for _ in 0..2 {
            match app.forced_step().expect("forced step") {
                LoopStep::Continue(report) => {
                    assert!(report.frame, "{report:?}");
                    assert_eq!(report.poll, None);
                }
                LoopStep::Exit(result) => panic!("session ended early: {result:?}"),
            }
        }
        let after = counters(&app);
        assert_eq!(after.frames, before + 2);
        assert!(!after.redraw_requested);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_request_reaches_receiver() {
        let root = workspace_root("open");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let (app, receiver) = App::load_headless(None, &profile.dir, None).expect("headless app");
        // Expected value: `xdg_open_argument` passes plain https URIs through unchanged
        // (platform/process.rs test `xdg_open_argument_drops_file_host`).
        crate::platform::open_uri_with("https://example.com", app.open_sink.clone());
        let opened = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("opener must report the URI");
        assert_eq!(opened, "https://example.com");
        let _ = std::fs::remove_dir_all(&root);
    }
}
