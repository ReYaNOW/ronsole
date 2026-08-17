use super::App;
use crate::input_types::{
    KeyCode, KeyInput, KeyState, Modifiers, PhysicalKey, PointerButton, PointerPosition,
    ScrollDelta,
};
use crate::renderer::{SettingsHit, SettingsTab, TerminalTabHit};
use crate::terminal::TerminalPresentationIntent;
use crate::terminal_process::TerminalCleanupWorker;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const PGO_AUTOMATION_SCENARIO_VERSION: u32 = 1;
const FIXTURE_NAME: &str = "terminal_fixture.sh";
const SETTINGS_SETTLED_EPSILON: f32 = 0.001;
const TAB_TRAINING_COUNT: usize = 10;
const TAB_CLOSE_COUNT: usize = 3;
const ALT_SCREEN_PRESENTED_FRAMES: u8 = 3;
const SCROLL_IMPULSES_AWAY: u8 = 6;
const SCROLL_IMPULSES_TOWARD: u8 = 3;
const SCROLL_IMPULSES_TO_TAIL: u8 = 6;
const NARROW_WIDTH_MAX: u32 = 720;
const WIDE_WIDTH_MAX: u32 = 1_600;
const PROCESS_TREE_SPAWN_GRACE: Duration = Duration::from_millis(300);
const STEP_TIMEOUT: Duration = Duration::from_secs(8);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const BULK_TIMEOUT: Duration = Duration::from_secs(45);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutomationOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) report: PathBuf,
    pub(crate) timeout: Duration,
}

pub(crate) fn automation_options_from_args(args: &[OsString]) -> Result<AutomationOptions, String> {
    parse_automation_args(args)?.ok_or_else(|| "--pgo-train is required".to_string())
}

pub(crate) fn write_automation_startup_failure(
    options: &AutomationOptions,
    failed_step: &str,
    error: &str,
) -> Result<(), String> {
    if let Some(parent) = options.report.parent() {
        fs::create_dir_all(parent).map_err(|io_error| {
            format!(
                "failed to create PGO report directory {}: {io_error}",
                parent.display()
            )
        })?;
    }
    let json = serialize_report("failed", &[], &[], 0, Some(failed_step), Some(error));
    atomic_write(&options.report, json.as_bytes()).map_err(|io_error| {
        format!(
            "failed to write PGO startup report {}: {io_error}",
            options.report.display()
        )
    })
}

fn parse_automation_args(args: &[OsString]) -> Result<Option<AutomationOptions>, String> {
    if !args
        .iter()
        .skip(1)
        .any(|arg| arg == OsStr::new("--pgo-train"))
    {
        return Ok(None);
    }

    let mut workspace = None;
    let mut report = None;
    let mut timeout = None;
    let mut train_seen = false;
    let mut index = 1usize;
    while index < args.len() {
        let arg = &args[index];
        if arg == OsStr::new("--pgo-train") {
            if train_seen {
                return Err("duplicate --pgo-train".to_string());
            }
            train_seen = true;
            index += 1;
            continue;
        }

        let target = if arg == OsStr::new("--pgo-workspace") {
            &mut workspace
        } else if arg == OsStr::new("--pgo-report") {
            &mut report
        } else if arg == OsStr::new("--pgo-timeout-seconds") {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--pgo-timeout-seconds requires a value".to_string())?;
            if timeout.is_some() {
                return Err("duplicate --pgo-timeout-seconds".to_string());
            }
            let seconds = value
                .to_str()
                .ok_or_else(|| "--pgo-timeout-seconds must be valid UTF-8".to_string())?
                .parse::<u64>()
                .map_err(|_| "--pgo-timeout-seconds must be a positive integer".to_string())?;
            if seconds == 0 {
                return Err("--pgo-timeout-seconds must be greater than zero".to_string());
            }
            timeout = Some(Duration::from_secs(seconds));
            index += 2;
            continue;
        } else {
            return Err(format!(
                "unknown PGO automation argument: {}",
                arg.to_string_lossy()
            ));
        };

        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", arg.to_string_lossy()))?;
        if target.is_some() {
            return Err(format!("duplicate {}", arg.to_string_lossy()));
        }
        *target = Some(PathBuf::from(value));
        index += 2;
    }

    let workspace = workspace.ok_or_else(|| "--pgo-workspace is required".to_string())?;
    let report = report.ok_or_else(|| "--pgo-report is required".to_string())?;
    let timeout = timeout.ok_or_else(|| "--pgo-timeout-seconds is required".to_string())?;
    if !workspace.is_absolute() {
        return Err("--pgo-workspace must be an absolute path".to_string());
    }
    if !report.is_absolute() {
        return Err("--pgo-report must be an absolute path".to_string());
    }

    Ok(Some(AutomationOptions {
        workspace,
        report,
        timeout,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SemanticBlock {
    Startup,
    ResizeReflow,
    BasicInput,
    Unicode,
    Ansi,
    Bulk,
    LongLines,
    AlternateScreen,
    Scroll,
    Selection,
    Search,
    Tabs,
    Settings,
    ProcessTree,
}

impl SemanticBlock {
    const ALL: [Self; 14] = [
        Self::Startup,
        Self::ResizeReflow,
        Self::BasicInput,
        Self::Unicode,
        Self::Ansi,
        Self::Bulk,
        Self::LongLines,
        Self::AlternateScreen,
        Self::Scroll,
        Self::Selection,
        Self::Search,
        Self::Tabs,
        Self::Settings,
        Self::ProcessTree,
    ];

    const fn report_name(self) -> &'static str {
        match self {
            Self::Startup => "startup-first-frame",
            Self::ResizeReflow => "resize-reflow",
            Self::BasicInput => "basic-input-echo",
            Self::Unicode => "unicode",
            Self::Ansi => "ansi-style-parser",
            Self::Bulk => "bulk-output",
            Self::LongLines => "long-lines-reflow",
            Self::AlternateScreen => "alternate-screen",
            Self::Scroll => "scroll",
            Self::Selection => "text-selection",
            Self::Search => "search",
            Self::Tabs => "tabs",
            Self::Settings => "settings",
            Self::ProcessTree => "process-tree-cleanup",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FixturePhase {
    Basic,
    Unicode,
    Ansi,
    Bulk,
    LongLines,
    AlternateScreen,
    ProcessTree,
}

impl FixturePhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Unicode => "unicode",
            Self::Ansi => "ansi",
            Self::Bulk => "bulk",
            Self::LongLines => "long-lines",
            Self::AlternateScreen => "alternate-screen",
            Self::ProcessTree => "process-tree",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeKind {
    Narrow,
    Wide,
    Original,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResizePlan {
    original: (u32, u32),
    narrow: (u32, u32),
    wide: (u32, u32),
}

fn resize_plan(width: u32, height: u32) -> ResizePlan {
    let original = (width.max(1), height.max(1));
    let narrow_width = if original.0 > 480 {
        (original.0.saturating_mul(2) / 3).clamp(360, NARROW_WIDTH_MAX)
    } else {
        (original.0.saturating_mul(3) / 4)
            .max(1)
            .min(original.0.saturating_sub(1).max(1))
    };
    let wide_width = original
        .0
        .clamp(960, WIDE_WIDTH_MAX)
        .max(narrow_width.saturating_add(1));
    ResizePlan {
        original,
        narrow: (narrow_width, original.1),
        wide: (wide_width, original.1),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum AutomationStep {
    WaitReady,
    Resize(ResizeKind),
    WaitResize(ResizeKind),
    BasicInputEdit,
    UnicodeInput,
    WaitText(&'static str),
    StartFixture(FixturePhase),
    WaitFixture(FixturePhase),
    WaitPresentedFrame,
    WaitAltScreenEnter,
    WaitAltPresentedFrames(u8),
    WaitAltScreenExit,
    PositionTerminalPointer,
    ScrollImpulses { y: f32, count: u8 },
    WaitScrollAway,
    WaitScrollToward,
    WaitScrollTail,
    SelectionDrag,
    SelectionCopyAndClear,
    SearchOpenQuery(&'static str),
    SearchNext,
    SearchPrevious,
    SearchToggleCase,
    SearchClose,
    CreateTab,
    WaitActiveTerminalReady,
    VerifyTabOverflow,
    ScrollTabStrip,
    WaitTabStripScrolled,
    SwitchVisibleOverflowTab,
    DragReorderVisibleTab,
    CloseActiveTab,
    WaitPendingCleanupQueue,
    CleanupBarrierRestart,
    SettingsOpen,
    WaitSettingsOpen,
    SettingsFontIncrease,
    SettingsFontDecrease,
    SettingsScrollIncrease,
    SettingsScrollDecrease,
    SettingsBackgroundAlternate,
    SettingsBackgroundRestore,
    SettingsHelp,
    SettingsClose,
    WaitSettingsClosed,
    WaitElapsed(Duration),
    VerifyProcessTreeStillRunning,
    CleanupBarrierFinal,
    Complete(SemanticBlock),
    Finish,
}

impl AutomationStep {
    fn name(&self) -> String {
        match self {
            Self::WaitReady => "wait-ready".to_string(),
            Self::Resize(kind) => format!("resize-{kind:?}"),
            Self::WaitResize(kind) => format!("wait-resize-{kind:?}"),
            Self::BasicInputEdit => "basic-input-edit".to_string(),
            Self::UnicodeInput => "unicode-input".to_string(),
            Self::WaitText(marker) => format!("wait-text-{marker}"),
            Self::StartFixture(phase) => format!("fixture-{}-start", phase.as_str()),
            Self::WaitFixture(phase) => format!("fixture-{}-wait", phase.as_str()),
            Self::WaitPresentedFrame => "wait-presented-frame".to_string(),
            Self::WaitAltScreenEnter => "wait-alt-screen-enter".to_string(),
            Self::WaitAltPresentedFrames(_) => "wait-alt-screen-frames".to_string(),
            Self::WaitAltScreenExit => "wait-alt-screen-exit".to_string(),
            Self::PositionTerminalPointer => "position-terminal-pointer".to_string(),
            Self::ScrollImpulses { y, .. } if *y > 0.0 => "scroll-away".to_string(),
            Self::ScrollImpulses { .. } => "scroll-toward-tail".to_string(),
            Self::WaitScrollAway => "wait-scroll-away".to_string(),
            Self::WaitScrollToward => "wait-scroll-toward".to_string(),
            Self::WaitScrollTail => "wait-scroll-tail".to_string(),
            Self::SelectionDrag => "selection-drag".to_string(),
            Self::SelectionCopyAndClear => "selection-copy-clear".to_string(),
            Self::SearchOpenQuery(query) => format!("search-open-{query}"),
            Self::SearchNext => "search-next".to_string(),
            Self::SearchPrevious => "search-previous".to_string(),
            Self::SearchToggleCase => "search-toggle-case".to_string(),
            Self::SearchClose => "search-close".to_string(),
            Self::CreateTab => "tab-create".to_string(),
            Self::WaitActiveTerminalReady => "tab-wait-ready".to_string(),
            Self::VerifyTabOverflow => "tab-overflow-verify".to_string(),
            Self::ScrollTabStrip => "tab-strip-scroll".to_string(),
            Self::WaitTabStripScrolled => "tab-strip-scroll-wait".to_string(),
            Self::SwitchVisibleOverflowTab => "tab-overflow-switch".to_string(),
            Self::DragReorderVisibleTab => "tab-drag-reorder".to_string(),
            Self::CloseActiveTab => "tab-close".to_string(),
            Self::WaitPendingCleanupQueue => "cleanup-queue-wait".to_string(),
            Self::CleanupBarrierRestart => "cleanup-barrier-restart".to_string(),
            Self::SettingsOpen => "settings-open".to_string(),
            Self::WaitSettingsOpen => "settings-open-wait".to_string(),
            Self::SettingsFontIncrease => "settings-font-increase".to_string(),
            Self::SettingsFontDecrease => "settings-font-decrease".to_string(),
            Self::SettingsScrollIncrease => "settings-scroll-increase".to_string(),
            Self::SettingsScrollDecrease => "settings-scroll-decrease".to_string(),
            Self::SettingsBackgroundAlternate => "settings-background-alternate".to_string(),
            Self::SettingsBackgroundRestore => "settings-background-restore".to_string(),
            Self::SettingsHelp => "settings-help".to_string(),
            Self::SettingsClose => "settings-close".to_string(),
            Self::WaitSettingsClosed => "settings-close-wait".to_string(),
            Self::WaitElapsed(duration) => format!("wait-elapsed-{}ms", duration.as_millis()),
            Self::VerifyProcessTreeStillRunning => "process-tree-live-verify".to_string(),
            Self::CleanupBarrierFinal => "cleanup-barrier-final".to_string(),
            Self::Complete(block) => format!("complete-{}", block.report_name()),
            Self::Finish => "finish".to_string(),
        }
    }

    const fn timeout(&self) -> Duration {
        match self {
            Self::WaitReady => READY_TIMEOUT,
            Self::WaitFixture(FixturePhase::Bulk) => BULK_TIMEOUT,
            Self::WaitFixture(FixturePhase::ProcessTree) => PROCESS_TIMEOUT,
            _ => STEP_TIMEOUT,
        }
    }
}

fn scenario() -> Vec<AutomationStep> {
    use AutomationStep as S;
    use FixturePhase as F;
    use ResizeKind as R;
    use SemanticBlock as B;

    let mut steps = vec![
        S::WaitReady,
        S::Complete(B::Startup),
        S::Resize(R::Narrow),
        S::WaitResize(R::Narrow),
        S::Resize(R::Wide),
        S::WaitResize(R::Wide),
        S::Resize(R::Original),
        S::WaitResize(R::Original),
        S::Complete(B::ResizeReflow),
        S::BasicInputEdit,
        S::WaitText("__RONSOLE_PGO_INPUT_edit"),
        S::WaitPresentedFrame,
        S::StartFixture(F::Basic),
        S::WaitFixture(F::Basic),
        S::WaitPresentedFrame,
        S::Complete(B::BasicInput),
        S::UnicodeInput,
        S::WaitText("__RONSOLE_PGO_UNICODE_ok"),
        S::WaitPresentedFrame,
        S::StartFixture(F::Unicode),
        S::WaitFixture(F::Unicode),
        S::WaitPresentedFrame,
        S::Complete(B::Unicode),
        S::StartFixture(F::Ansi),
        S::WaitFixture(F::Ansi),
        S::WaitPresentedFrame,
        S::Complete(B::Ansi),
        S::StartFixture(F::Bulk),
        S::WaitFixture(F::Bulk),
        S::WaitPresentedFrame,
        S::Complete(B::Bulk),
        S::StartFixture(F::LongLines),
        S::WaitFixture(F::LongLines),
        S::WaitPresentedFrame,
        S::Resize(R::Narrow),
        S::WaitResize(R::Narrow),
        S::Resize(R::Wide),
        S::WaitResize(R::Wide),
        S::Resize(R::Original),
        S::WaitResize(R::Original),
        S::WaitPresentedFrame,
        S::Complete(B::LongLines),
        S::StartFixture(F::AlternateScreen),
        S::WaitAltScreenEnter,
        S::WaitAltPresentedFrames(ALT_SCREEN_PRESENTED_FRAMES),
        S::WaitFixture(F::AlternateScreen),
        S::WaitAltScreenExit,
        S::WaitPresentedFrame,
        S::Complete(B::AlternateScreen),
        S::PositionTerminalPointer,
        S::ScrollImpulses {
            y: 6.0,
            count: SCROLL_IMPULSES_AWAY,
        },
        S::WaitScrollAway,
        S::ScrollImpulses {
            y: -3.0,
            count: SCROLL_IMPULSES_TOWARD,
        },
        S::WaitScrollToward,
        S::ScrollImpulses {
            y: -12.0,
            count: SCROLL_IMPULSES_TO_TAIL,
        },
        S::WaitScrollTail,
        S::Complete(B::Scroll),
        S::SelectionDrag,
        S::SelectionCopyAndClear,
        S::Complete(B::Selection),
        S::SearchOpenQuery("__RONSOLE_PGO_DONE_bulk"),
        S::SearchNext,
        S::SearchPrevious,
        S::WaitPresentedFrame,
        S::SearchToggleCase,
        S::WaitPresentedFrame,
        S::SearchClose,
        S::SearchOpenQuery("__RONSOLE_PGO_INPUT_edit"),
        S::SearchNext,
        S::SearchClose,
        S::Complete(B::Search),
        S::Resize(R::Narrow),
        S::WaitResize(R::Narrow),
    ];
    for _ in 0..TAB_TRAINING_COUNT {
        steps.extend([
            S::CreateTab,
            S::WaitActiveTerminalReady,
            S::StartFixture(F::Basic),
            S::WaitFixture(F::Basic),
            S::WaitPresentedFrame,
        ]);
    }
    steps.extend([
        S::VerifyTabOverflow,
        S::ScrollTabStrip,
        S::WaitTabStripScrolled,
        S::SwitchVisibleOverflowTab,
        S::WaitActiveTerminalReady,
        S::DragReorderVisibleTab,
    ]);
    for _ in 0..TAB_CLOSE_COUNT {
        steps.extend([S::CloseActiveTab, S::WaitActiveTerminalReady]);
    }
    steps.extend([
        S::WaitPendingCleanupQueue,
        S::CleanupBarrierRestart,
        S::Resize(R::Original),
        S::WaitResize(R::Original),
        S::Complete(B::Tabs),
        S::SettingsOpen,
        S::WaitSettingsOpen,
        S::SettingsFontIncrease,
        S::WaitPresentedFrame,
        S::SettingsFontDecrease,
        S::WaitPresentedFrame,
        S::SettingsScrollIncrease,
        S::WaitPresentedFrame,
        S::SettingsScrollDecrease,
        S::WaitPresentedFrame,
        S::SettingsBackgroundAlternate,
        S::WaitPresentedFrame,
        S::SettingsBackgroundRestore,
        S::WaitPresentedFrame,
        S::SettingsHelp,
        S::WaitPresentedFrame,
        S::SettingsClose,
        S::WaitSettingsClosed,
        S::Complete(B::Settings),
        S::CreateTab,
        S::WaitActiveTerminalReady,
        S::StartFixture(F::ProcessTree),
        S::WaitText("__RONSOLE_PGO_PROCESS_TREE_STARTED"),
        S::WaitElapsed(PROCESS_TREE_SPAWN_GRACE),
        S::VerifyProcessTreeStillRunning,
        S::WaitPresentedFrame,
        S::CloseActiveTab,
        S::WaitActiveTerminalReady,
        S::WaitPendingCleanupQueue,
        S::CleanupBarrierFinal,
        S::Complete(B::ProcessTree),
        S::Finish,
    ]);
    steps
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepResult {
    Pending,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutomationTick {
    Running { deadline: Instant },
    ExitOk,
    ExitFailed,
}

pub(super) struct AutomationController {
    options: AutomationOptions,
    fixture_path: PathBuf,
    steps: Vec<AutomationStep>,
    step_index: usize,
    step_started: Instant,
    started: Instant,
    deadline: Instant,
    completed_steps: Vec<&'static str>,
    skipped_steps: Vec<String>,
    resize_plan: Option<ResizePlan>,
    original_cols: usize,
    narrow_cols: usize,
    presented_frame_armed: bool,
    alt_frames_seen: u8,
    scroll_away_peak: f32,
    original_background: Option<String>,
    line_scratch: String,
    report_written: bool,
    terminal_cleanup_finalized: bool,
    exit_error: Option<String>,
}

impl AutomationController {
    pub(super) fn new(options: AutomationOptions) -> Result<Self, String> {
        let fixture_path = options.workspace.join(FIXTURE_NAME);
        if let Some(parent) = options.report.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create PGO report directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        if !fixture_path.is_file() {
            let error = format!(
                "PGO terminal fixture is missing: {}",
                fixture_path.display()
            );
            let _ = write_automation_startup_failure(&options, "startup", &error);
            return Err(error);
        }
        if fixture_path.to_str().is_none() {
            let error = "PGO terminal fixture path must be valid UTF-8".to_string();
            let _ = write_automation_startup_failure(&options, "startup", &error);
            return Err(error);
        }
        let started = Instant::now();
        let Some(deadline) = started.checked_add(options.timeout) else {
            let error = "PGO automation timeout is too large".to_string();
            let _ = write_automation_startup_failure(&options, "startup", &error);
            return Err(error);
        };
        Ok(Self {
            options,
            fixture_path,
            steps: scenario(),
            step_index: 0,
            step_started: started,
            started,
            deadline,
            completed_steps: Vec::with_capacity(SemanticBlock::ALL.len() + 1),
            skipped_steps: Vec::with_capacity(2),
            resize_plan: None,
            original_cols: 0,
            narrow_cols: 0,
            presented_frame_armed: false,
            alt_frames_seen: 0,
            scroll_away_peak: 0.0,
            original_background: None,
            line_scratch: String::with_capacity(512),
            report_written: false,
            terminal_cleanup_finalized: false,
            exit_error: None,
        })
    }

    fn current_step(&self) -> Option<&AutomationStep> {
        self.steps.get(self.step_index)
    }

    fn current_deadline(&self) -> Instant {
        let step_deadline = self
            .step_started
            .checked_add(
                self.current_step()
                    .map_or(STEP_TIMEOUT, AutomationStep::timeout),
            )
            .unwrap_or(self.deadline);
        self.deadline.min(step_deadline)
    }

    fn current_wake_deadline(&self) -> Instant {
        let deadline = self.current_deadline();
        match self.current_step() {
            Some(AutomationStep::WaitElapsed(duration)) => self
                .step_started
                .checked_add(*duration)
                .map_or(deadline, |wake| deadline.min(wake)),
            _ => deadline,
        }
    }

    pub(super) fn tick(&mut self, app: &mut App, now: Instant) -> AutomationTick {
        if deadline_expired(now, self.deadline) {
            return self.fail_current("PGO automation global timeout reached");
        }
        if deadline_expired(now, self.current_deadline()) {
            return self.fail_current("PGO automation step timeout reached");
        }

        let Some(step) = self.current_step().cloned() else {
            return self.fail_current("PGO automation scenario ended without Finish");
        };
        let result = match self.run_step(app, &step) {
            Ok(result) => result,
            Err(error) => return self.fail_step(&step, &error),
        };
        match result {
            StepResult::Pending => AutomationTick::Running {
                deadline: self.current_wake_deadline(),
            },
            StepResult::Failed => self.fail_step(&step, "PGO automation step failed"),
            StepResult::Done => {
                if step == AutomationStep::Finish {
                    return match self.finish_success() {
                        Ok(()) => AutomationTick::ExitOk,
                        Err(error) => self.fail_step(&step, &error),
                    };
                }
                self.step_index = next_step_index(self.step_index, true);
                self.step_started = now;
                self.presented_frame_armed = false;
                app.request_frame();
                AutomationTick::Running {
                    deadline: self.current_wake_deadline(),
                }
            }
        }
    }

    fn run_step(&mut self, app: &mut App, step: &AutomationStep) -> Result<StepResult, String> {
        match *step {
            AutomationStep::WaitReady => self.wait_ready(app),
            AutomationStep::Resize(kind) => {
                let (width, height) = self.resize_target(kind)?;
                app.on_resize_with_logical_size(width, height, None);
                Ok(StepResult::Done)
            }
            AutomationStep::WaitResize(kind) => self.wait_resize(app, kind),
            AutomationStep::BasicInputEdit => {
                app.on_text("printf '%s%s\\n' '__RONSOLE_PGO_INPUT_' 'edix'");
                press_key(app, Modifiers::empty(), KeyCode::ArrowLeft)?;
                press_key(app, Modifiers::empty(), KeyCode::Backspace)?;
                app.on_text("t");
                press_key(app, Modifiers::empty(), KeyCode::ArrowRight)?;
                press_key(app, Modifiers::empty(), KeyCode::Enter)?;
                Ok(StepResult::Done)
            }
            AutomationStep::UnicodeInput => {
                app.on_text("printf '%s%s\\n' '__RONSOLE_PGO_UNICODE_' 'ok-Привет-Latin-界-é-😀'");
                press_key(app, Modifiers::empty(), KeyCode::Enter)?;
                Ok(StepResult::Done)
            }
            AutomationStep::WaitText(marker) => Ok(if self.active_grid_contains(app, marker) {
                StepResult::Done
            } else {
                StepResult::Pending
            }),
            AutomationStep::StartFixture(phase) => {
                let command = fixture_command(&self.fixture_path, phase)?;
                app.on_text(&command);
                press_key(app, Modifiers::empty(), KeyCode::Enter)?;
                Ok(StepResult::Done)
            }
            AutomationStep::WaitFixture(phase) => self.wait_fixture(app, phase),
            AutomationStep::WaitPresentedFrame => self.wait_presented_frame(app),
            AutomationStep::WaitAltScreenEnter => Ok(if active_is_alt(app) {
                StepResult::Done
            } else {
                StepResult::Pending
            }),
            AutomationStep::WaitAltPresentedFrames(required) => {
                if !active_is_alt(app) {
                    return Err(
                        "alternate screen exited before frame sampling completed".to_string()
                    );
                }
                if !self.presented_frame_armed {
                    app.request_frame();
                    self.presented_frame_armed = true;
                    return Ok(StepResult::Pending);
                }
                if app.dirty {
                    return Ok(StepResult::Pending);
                }
                self.presented_frame_armed = false;
                self.alt_frames_seen = self.alt_frames_seen.saturating_add(1);
                Ok(if self.alt_frames_seen >= required {
                    self.alt_frames_seen = 0;
                    StepResult::Done
                } else {
                    app.request_frame();
                    self.presented_frame_armed = true;
                    StepResult::Pending
                })
            }
            AutomationStep::WaitAltScreenExit => Ok(if !active_is_alt(app) {
                StepResult::Done
            } else {
                StepResult::Pending
            }),
            AutomationStep::PositionTerminalPointer => {
                let layout = app.interaction.layout;
                if layout.body.w <= 0.0 || layout.body.h <= 0.0 {
                    return Ok(StepResult::Pending);
                }
                app.on_pointer_motion(PointerPosition {
                    x: layout.body.x + layout.body.w * 0.5,
                    y: layout.body.y + layout.body.h * 0.5,
                });
                Ok(StepResult::Done)
            }
            AutomationStep::ScrollImpulses { y, count } => {
                for _ in 0..count {
                    app.on_scroll(ScrollDelta::Line { x: 0.0, y });
                }
                Ok(StepResult::Done)
            }
            AutomationStep::WaitScrollAway => {
                let Some(terminal) = app.terminals.get(app.active_terminal) else {
                    return Ok(StepResult::Pending);
                };
                self.scroll_away_peak = self.scroll_away_peak.max(terminal.scroll_y.current);
                Ok(
                    if terminal.scroll_y.is_settled() && terminal.scroll_y.current > 0.5 {
                        self.scroll_away_peak = terminal.scroll_y.current;
                        StepResult::Done
                    } else {
                        StepResult::Pending
                    },
                )
            }
            AutomationStep::WaitScrollToward => {
                let Some(terminal) = app.terminals.get(app.active_terminal) else {
                    return Ok(StepResult::Pending);
                };
                Ok(
                    if terminal.scroll_y.is_settled()
                        && terminal.scroll_y.current >= 0.0
                        && terminal.scroll_y.current < self.scroll_away_peak
                    {
                        StepResult::Done
                    } else {
                        StepResult::Pending
                    },
                )
            }
            AutomationStep::WaitScrollTail => {
                let Some(terminal) = app.terminals.get(app.active_terminal) else {
                    return Ok(StepResult::Pending);
                };
                Ok(
                    if terminal.scroll_y.is_settled() && terminal.scroll_y.current <= 0.5 {
                        StepResult::Done
                    } else {
                        StepResult::Pending
                    },
                )
            }
            AutomationStep::SelectionDrag => self.selection_drag(app),
            AutomationStep::SelectionCopyAndClear => self.selection_copy_and_clear(app),
            AutomationStep::SearchOpenQuery(query) => {
                press_key(app, Modifiers::CONTROL, KeyCode::KeyF)?;
                app.on_text(query);
                Ok(StepResult::Done)
            }
            AutomationStep::SearchNext => {
                press_key(app, Modifiers::empty(), KeyCode::Enter)?;
                Ok(StepResult::Done)
            }
            AutomationStep::SearchPrevious => {
                press_key(app, Modifiers::SHIFT, KeyCode::Enter)?;
                Ok(StepResult::Done)
            }
            AutomationStep::SearchToggleCase => self.search_toggle_case(app),
            AutomationStep::SearchClose => {
                press_key(app, Modifiers::empty(), KeyCode::Escape)?;
                Ok(StepResult::Done)
            }
            AutomationStep::CreateTab => {
                let before = app.terminals.len();
                press_key(app, Modifiers::CONTROL | Modifiers::SHIFT, KeyCode::KeyT)?;
                if app.terminals.len() != before.saturating_add(1) {
                    return Err("production NewTab shortcut did not create a terminal".to_string());
                }
                Ok(StepResult::Done)
            }
            AutomationStep::WaitActiveTerminalReady => Ok(if active_terminal_ready(app) {
                StepResult::Done
            } else {
                StepResult::Pending
            }),
            AutomationStep::VerifyTabOverflow => {
                let Some(runtime) = app.runtime.as_ref() else {
                    return Ok(StepResult::Pending);
                };
                let strip = runtime.terminal_tab_strip_layout();
                if app.terminals.len() < TAB_TRAINING_COUNT + 1 {
                    return Err("tab training did not create enough terminals".to_string());
                }
                Ok(if strip.max_scroll > 0.0 {
                    StepResult::Done
                } else {
                    StepResult::Pending
                })
            }
            AutomationStep::ScrollTabStrip => self.scroll_tab_strip(app),
            AutomationStep::WaitTabStripScrolled => Ok(
                if app.terminal_tab_scroll.is_settled() && app.terminal_tab_scroll.current > 0.5 {
                    StepResult::Done
                } else {
                    StepResult::Pending
                },
            ),
            AutomationStep::SwitchVisibleOverflowTab => self.switch_visible_overflow_tab(app),
            AutomationStep::DragReorderVisibleTab => self.drag_reorder_visible_tab(app),
            AutomationStep::CloseActiveTab => {
                if app.terminals.len() <= 1 {
                    return Err(
                        "refusing to close the final terminal during PGO training".to_string()
                    );
                }
                let before = app.terminals.len();
                press_key(app, Modifiers::CONTROL, KeyCode::Digit4)?;
                if app.terminals.len() + 1 != before {
                    return Err("production CloseTab shortcut did not close a terminal".to_string());
                }
                Ok(StepResult::Done)
            }
            AutomationStep::WaitPendingCleanupQueue => {
                app.flush_pending_terminal_cleanup();
                Ok(if app.pending_terminal_cleanup.is_empty() {
                    StepResult::Done
                } else {
                    StepResult::Pending
                })
            }
            AutomationStep::CleanupBarrierRestart => {
                app.terminal_cleanup.shutdown_and_join();
                app.terminal_cleanup = TerminalCleanupWorker::new();
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsOpen => {
                press_key(app, Modifiers::empty(), KeyCode::F1)?;
                if !app.settings_open {
                    return Err("production F1 shortcut did not open settings".to_string());
                }
                Ok(StepResult::Done)
            }
            AutomationStep::WaitSettingsOpen => Ok(
                if app.settings_open
                    && (1.0 - app.settings_progress).abs() <= SETTINGS_SETTLED_EPSILON
                {
                    StepResult::Done
                } else {
                    StepResult::Pending
                },
            ),
            AutomationStep::SettingsFontIncrease => {
                app.apply_settings_hit(SettingsHit::Tab(SettingsTab::General));
                app.apply_settings_hit(SettingsHit::FontIncrease);
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsFontDecrease => {
                app.apply_settings_hit(SettingsHit::FontDecrease);
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsScrollIncrease => {
                app.apply_settings_hit(SettingsHit::ScrollIncrease);
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsScrollDecrease => {
                app.apply_settings_hit(SettingsHit::ScrollDecrease);
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsBackgroundAlternate => {
                let original = app.config.terminal_background.to_hex();
                self.original_background = Some(original.clone());
                let alternate = if original.eq_ignore_ascii_case("#223344") {
                    "#445566"
                } else {
                    "#223344"
                };
                edit_settings_background(app, alternate)?;
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsBackgroundRestore => {
                let original = self
                    .original_background
                    .as_deref()
                    .ok_or_else(|| {
                        "settings background original value was not captured".to_string()
                    })?
                    .to_string();
                edit_settings_background(app, &original)?;
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsHelp => {
                app.apply_settings_hit(SettingsHit::Tab(SettingsTab::Help));
                Ok(StepResult::Done)
            }
            AutomationStep::SettingsClose => {
                press_key(app, Modifiers::empty(), KeyCode::F1)?;
                if app.settings_open {
                    return Err("production F1 shortcut did not close settings".to_string());
                }
                Ok(StepResult::Done)
            }
            AutomationStep::WaitSettingsClosed => Ok(
                if !app.settings_open && app.settings_progress.abs() <= SETTINGS_SETTLED_EPSILON {
                    StepResult::Done
                } else {
                    StepResult::Pending
                },
            ),
            AutomationStep::WaitElapsed(duration) => {
                Ok(if self.step_started.elapsed() >= duration {
                    StepResult::Done
                } else {
                    StepResult::Pending
                })
            }
            AutomationStep::VerifyProcessTreeStillRunning => {
                if self.active_grid_contains(app, "__RONSOLE_PGO_FAIL_process-tree") {
                    return Err("process-tree fixture failed before cleanup".to_string());
                }
                if self.active_grid_contains(app, "__RONSOLE_PGO_DONE_process-tree") {
                    return Err("process-tree fixture completed before cleanup".to_string());
                }
                Ok(StepResult::Done)
            }
            AutomationStep::CleanupBarrierFinal => {
                app.terminal_cleanup.shutdown_and_join();
                self.terminal_cleanup_finalized = true;
                Ok(StepResult::Done)
            }
            AutomationStep::Complete(block) => {
                if self.completed_steps.contains(&block.report_name()) {
                    return Err(format!(
                        "semantic block completed twice: {}",
                        block.report_name()
                    ));
                }
                self.completed_steps.push(block.report_name());
                Ok(StepResult::Done)
            }
            AutomationStep::Finish => {
                if !mandatory_completion_valid(&self.completed_steps) {
                    return Err(
                        "mandatory semantic PGO blocks are incomplete or out of order".to_string(),
                    );
                }
                if !self.terminal_cleanup_finalized {
                    return Err("process cleanup barrier was not finalized".to_string());
                }
                self.completed_steps.push("finish");
                Ok(StepResult::Done)
            }
        }
    }

    fn wait_ready(&mut self, app: &mut App) -> Result<StepResult, String> {
        let Some(runtime) = app.runtime.as_ref() else {
            return Ok(StepResult::Pending);
        };
        let Some(terminal) = app.terminals.get(app.active_terminal) else {
            return Ok(StepResult::Pending);
        };
        let layout = app.interaction.layout;
        if !app.active_terminal_presented
            || !terminal.presentation_ready()
            || layout.cols < 10
            || layout.visible_rows < 2
            || layout.body.w <= 0.0
            || layout.body.h <= 0.0
            || app.dirty
        {
            return Ok(StepResult::Pending);
        }
        let metrics = runtime.wayland_metrics();
        self.resize_plan = Some(resize_plan(metrics.physical_width, metrics.physical_height));
        self.original_cols = layout.cols;
        Ok(StepResult::Done)
    }

    fn resize_target(&self, kind: ResizeKind) -> Result<(u32, u32), String> {
        let plan = self
            .resize_plan
            .ok_or_else(|| "resize plan is unavailable before startup readiness".to_string())?;
        Ok(match kind {
            ResizeKind::Narrow => plan.narrow,
            ResizeKind::Wide => plan.wide,
            ResizeKind::Original => plan.original,
        })
    }

    fn wait_resize(&mut self, app: &App, kind: ResizeKind) -> Result<StepResult, String> {
        let cols = app.interaction.layout.cols;
        if cols == 0 || app.dirty {
            return Ok(StepResult::Pending);
        }
        match kind {
            ResizeKind::Narrow => {
                if cols < self.original_cols {
                    self.narrow_cols = cols;
                    Ok(StepResult::Done)
                } else {
                    Ok(StepResult::Pending)
                }
            }
            ResizeKind::Wide => Ok(if cols > self.narrow_cols.max(1) {
                StepResult::Done
            } else {
                StepResult::Pending
            }),
            ResizeKind::Original => Ok(if cols == self.original_cols {
                StepResult::Done
            } else {
                StepResult::Pending
            }),
        }
    }

    fn wait_presented_frame(&mut self, app: &mut App) -> Result<StepResult, String> {
        if !app.renderable() {
            return Ok(StepResult::Pending);
        }
        if !self.presented_frame_armed {
            app.request_frame();
            self.presented_frame_armed = true;
            return Ok(StepResult::Pending);
        }
        Ok(if app.dirty {
            StepResult::Pending
        } else {
            self.presented_frame_armed = false;
            StepResult::Done
        })
    }

    fn wait_fixture(&mut self, app: &App, phase: FixturePhase) -> Result<StepResult, String> {
        let success = format!("__RONSOLE_PGO_DONE_{}", phase.as_str());
        let failure = format!("__RONSOLE_PGO_FAIL_{}", phase.as_str());
        if self.active_grid_contains(app, &failure) {
            return Err(format!("terminal fixture phase {} failed", phase.as_str()));
        }
        Ok(if self.active_grid_contains(app, &success) {
            StepResult::Done
        } else {
            StepResult::Pending
        })
    }

    fn active_grid_contains(&mut self, app: &App, needle: &str) -> bool {
        let Some(terminal) = app.terminals.get(app.active_terminal) else {
            return false;
        };
        let grid = crate::platform::lock_recover(&terminal.grid);
        let total = grid.scrollback.len() + grid.lines.len();
        let first = total.saturating_sub(96);
        for row_index in first..total {
            let row = if row_index < grid.scrollback.len() {
                &grid.scrollback[row_index]
            } else {
                &grid.lines[row_index - grid.scrollback.len()]
            };
            self.line_scratch.clear();
            for cell in row.iter().take(grid.cols) {
                cell.append_text_to(&mut self.line_scratch);
            }
            if self.line_scratch.contains(needle) {
                return true;
            }
        }
        false
    }

    fn selection_drag(&mut self, app: &mut App) -> Result<StepResult, String> {
        let layout = app.interaction.layout;
        if layout.char_w <= 0.0 || layout.char_h <= 0.0 || layout.visible_rows < 4 {
            return Ok(StepResult::Pending);
        }
        let start = PointerPosition {
            x: layout.text_x + layout.char_w * 2.5,
            y: layout.body.y + layout.char_h * 1.5,
        };
        let end = PointerPosition {
            x: (layout.text_x + layout.char_w * 18.5).min(layout.body.x + layout.body.w - 2.0),
            y: (layout.body.y + layout.char_h * 3.5).min(layout.body.y + layout.body.h - 2.0),
        };
        app.on_pointer_motion(start);
        if app.on_pointer_button(KeyState::Pressed, PointerButton::Left) {
            return Err("selection unexpectedly requested application exit".to_string());
        }
        app.on_pointer_motion(end);
        if app.on_pointer_button(KeyState::Released, PointerButton::Left) {
            return Err("selection release unexpectedly requested application exit".to_string());
        }
        let selected = app
            .terminals
            .get(app.active_terminal)
            .is_some_and(|terminal| {
                crate::platform::lock_recover(&terminal.grid)
                    .selection
                    .is_some()
            });
        Ok(if selected {
            StepResult::Done
        } else {
            StepResult::Failed
        })
    }

    fn selection_copy_and_clear(&mut self, app: &mut App) -> Result<StepResult, String> {
        if let Some(previous_clipboard) = app.interaction.clipboard_text() {
            press_key(app, Modifiers::CONTROL, KeyCode::KeyC)?;
            let _ = app.interaction.set_clipboard_text(previous_clipboard);
        } else {
            self.skipped_steps
                .push("selection-copy-clipboard-unavailable".to_string());
            let active = app.active_terminal;
            if let Some(terminal) = app.terminals.get_mut(active) {
                app.interaction.clear_text_selection(terminal);
                app.request_frame();
            }
        }
        let selection_active = app
            .terminals
            .get(app.active_terminal)
            .is_some_and(|terminal| {
                crate::platform::lock_recover(&terminal.grid)
                    .selection
                    .is_some()
            });
        Ok(if selection_active {
            StepResult::Failed
        } else {
            StepResult::Done
        })
    }

    fn search_toggle_case(&mut self, app: &mut App) -> Result<StepResult, String> {
        let Some(search) = app.interaction.layout.search else {
            return Ok(StepResult::Pending);
        };
        if search.case_toggle.w <= 0.0 || search.case_toggle.h <= 0.0 {
            return Err("search case-toggle control is not renderable".to_string());
        }
        app.on_pointer_motion(PointerPosition {
            x: search.case_toggle.x + search.case_toggle.w * 0.5,
            y: search.case_toggle.y + search.case_toggle.h * 0.5,
        });
        if app.on_pointer_button(KeyState::Pressed, PointerButton::Left)
            || app.on_pointer_button(KeyState::Released, PointerButton::Left)
        {
            return Err("search case toggle unexpectedly requested application exit".to_string());
        }
        Ok(StepResult::Done)
    }

    fn scroll_tab_strip(&mut self, app: &mut App) -> Result<StepResult, String> {
        let Some(runtime) = app.runtime.as_ref() else {
            return Ok(StepResult::Pending);
        };
        let strip = runtime.terminal_tab_strip_layout();
        if strip.max_scroll <= 0.0 {
            return Ok(StepResult::Pending);
        }
        app.on_pointer_motion(PointerPosition {
            x: strip.rect.x + strip.rect.w * 0.5,
            y: strip.rect.y + strip.rect.h * 0.5,
        });
        for _ in 0..4 {
            app.on_scroll(ScrollDelta::Line { x: 0.0, y: -4.0 });
        }
        Ok(StepResult::Done)
    }

    fn switch_visible_overflow_tab(&mut self, app: &mut App) -> Result<StepResult, String> {
        let Some((index, position)) = visible_tab_target(app, Some(app.active_terminal)) else {
            return Ok(StepResult::Pending);
        };
        app.on_pointer_motion(position);
        if app.on_pointer_button(KeyState::Pressed, PointerButton::Left)
            || app.on_pointer_button(KeyState::Released, PointerButton::Left)
        {
            return Err("tab switch unexpectedly requested application exit".to_string());
        }
        Ok(
            if app.active_terminal == index || !app.active_terminal_presented {
                StepResult::Done
            } else {
                StepResult::Pending
            },
        )
    }

    fn drag_reorder_visible_tab(&mut self, app: &mut App) -> Result<StepResult, String> {
        let Some((start_index, start)) = visible_tab_target(app, None) else {
            return Ok(StepResult::Pending);
        };
        let Some(runtime) = app.runtime.as_ref() else {
            return Ok(StepResult::Pending);
        };
        let strip = runtime.terminal_tab_strip_layout();
        let destination_x = if start.x < strip.rect.x + strip.rect.w * 0.5 {
            strip.rect.x + strip.rect.w - 12.0
        } else {
            strip.rect.x + 12.0
        };
        app.on_pointer_motion(start);
        if app.on_pointer_button(KeyState::Pressed, PointerButton::Left) {
            return Err("tab drag press unexpectedly requested application exit".to_string());
        }
        app.on_pointer_motion(PointerPosition {
            x: destination_x,
            y: start.y,
        });
        let threshold_passed = app
            .terminal_tab_drag
            .as_ref()
            .is_some_and(|drag| drag.threshold_passed);
        let destination = app.runtime.as_ref().and_then(|runtime| {
            app.terminal_tab_drag
                .as_ref()
                .and_then(|drag| runtime.terminal_tab_drag_destination(drag))
        });
        if app.on_pointer_button(KeyState::Released, PointerButton::Left) {
            return Err("tab drag release unexpectedly requested application exit".to_string());
        }
        if !threshold_passed
            || destination.is_none()
            || destination == Some(start_index)
            || app.terminal_tab_drag.is_some()
        {
            return Err("production tab drag did not perform a bounded reorder".to_string());
        }
        Ok(StepResult::Done)
    }

    fn finish_success(&mut self) -> Result<(), String> {
        self.write_report("ok", None, None)?;
        self.report_written = true;
        Ok(())
    }

    fn fail_current(&mut self, error: &str) -> AutomationTick {
        let step = self
            .current_step()
            .map_or_else(|| "unknown".to_string(), AutomationStep::name);
        self.fail_named(&step, error)
    }

    fn fail_step(&mut self, step: &AutomationStep, error: &str) -> AutomationTick {
        self.fail_named(&step.name(), error)
    }

    fn fail_named(&mut self, step: &str, error: &str) -> AutomationTick {
        let mut exit_error = format!("PGO automation failed during {step}: {error}");
        if !self.report_written {
            if let Err(report_error) = self.write_report("failed", Some(step), Some(error)) {
                exit_error.push_str(&format!("; failed to write report: {report_error}"));
                eprintln!("Ronsole PGO: failed to write failure report: {report_error}");
            } else {
                self.report_written = true;
            }
        }
        self.exit_error = Some(exit_error);
        AutomationTick::ExitFailed
    }

    pub(super) fn interrupt(&mut self, error: &str) {
        if self.report_written {
            return;
        }
        let step = self
            .current_step()
            .map_or_else(|| "unknown".to_string(), AutomationStep::name);
        let mut full = format!("PGO automation interrupted during {step}: {error}");
        if let Err(report_error) = self.write_report("failed", Some(&step), Some(&full)) {
            full.push_str(&format!("; failed to write report: {report_error}"));
        } else {
            self.report_written = true;
        }
        self.exit_error = Some(full);
    }

    pub(super) fn take_exit_result(&mut self) -> Result<(), String> {
        self.exit_error.take().map_or(Ok(()), Err)
    }

    fn write_report(
        &self,
        status: &str,
        failed_step: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let duration_ms = self.started.elapsed().as_millis();
        let json = serialize_report(
            status,
            &self.completed_steps,
            &self.skipped_steps,
            duration_ms,
            failed_step,
            error,
        );
        atomic_write(&self.options.report, json.as_bytes()).map_err(|io_error| {
            format!(
                "failed to write PGO report {}: {io_error}",
                self.options.report.display()
            )
        })
    }
}

fn press_key(app: &mut App, modifiers: Modifiers, code: KeyCode) -> Result<(), String> {
    app.on_modifiers(modifiers);
    let pressed_exit = app.on_key(KeyInput {
        state: KeyState::Pressed,
        physical_key: PhysicalKey::Code(code),
    });
    let released_exit = app.on_key(KeyInput {
        state: KeyState::Released,
        physical_key: PhysicalKey::Code(code),
    });
    app.on_modifiers(Modifiers::empty());
    if pressed_exit || released_exit {
        Err(format!(
            "key {code:?} unexpectedly requested application exit"
        ))
    } else {
        Ok(())
    }
}

fn edit_settings_background(app: &mut App, value: &str) -> Result<(), String> {
    app.apply_settings_hit(SettingsHit::BackgroundField);
    press_key(app, Modifiers::CONTROL, KeyCode::KeyA)?;
    app.on_text(value);
    press_key(app, Modifiers::empty(), KeyCode::Enter)?;
    Ok(())
}

fn active_terminal_ready(app: &App) -> bool {
    !app.terminals.iter().any(|terminal| {
        terminal.presentation_intent == TerminalPresentationIntent::ActivateWhenReady
    }) && app.active_terminal_presented
        && app
            .terminals
            .get(app.active_terminal)
            .is_some_and(|terminal| terminal.presentation_ready())
        && app.interaction.layout.cols > 0
        && app.interaction.layout.visible_rows > 0
        && !app.dirty
}

fn active_is_alt(app: &App) -> bool {
    app.terminals
        .get(app.active_terminal)
        .is_some_and(|terminal| crate::platform::lock_recover(&terminal.grid).is_alt)
}

fn visible_tab_target(app: &App, exclude: Option<usize>) -> Option<(usize, PointerPosition)> {
    let runtime = app.runtime.as_ref()?;
    let strip = runtime.terminal_tab_strip_layout();
    if strip.rect.w <= 0.0 || strip.rect.h <= 0.0 {
        return None;
    }
    let y = strip.rect.y + strip.rect.h * 0.5;
    for sample in 1..64 {
        let x = strip.rect.x + strip.rect.w * (sample as f32 / 64.0);
        if let TerminalTabHit::Body(index) = runtime.terminal_tab_hit_test(x, y)
            && exclude != Some(index)
        {
            return Some((index, PointerPosition { x, y }));
        }
    }
    None
}

fn fixture_command(path: &Path, phase: FixturePhase) -> Result<String, String> {
    let quoted_path = shell_quote_path(path)?;
    let phase = phase.as_str();
    if phase == FixturePhase::ProcessTree.as_str() {
        return Ok(format!(
            "printf '%s%s\\n' '__RONSOLE_PGO_PROCESS_' 'TREE_STARTED'; /bin/sh {quoted_path} {phase}; rc=$?; if [ \"$rc\" -eq 0 ]; then printf '%s%s\\n' '__RONSOLE_PGO_DONE_' '{phase}'; else printf '%s%s:%s\\n' '__RONSOLE_PGO_FAIL_' '{phase}' \"$rc\"; fi"
        ));
    }
    Ok(format!(
        "/bin/sh {quoted_path} {phase}; rc=$?; if [ \"$rc\" -eq 0 ]; then printf '%s%s\\n' '__RONSOLE_PGO_DONE_' '{phase}'; else printf '%s%s:%s\\n' '__RONSOLE_PGO_FAIL_' '{phase}' \"$rc\"; fi"
    ))
}

fn shell_quote_path(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "PGO fixture path must be valid UTF-8".to_string())?;
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn deadline_expired(now: Instant, deadline: Instant) -> bool {
    now >= deadline
}

fn mandatory_completion_valid(completed_steps: &[&str]) -> bool {
    let expected = SemanticBlock::ALL.map(SemanticBlock::report_name);
    completed_steps == expected.as_slice()
}

fn next_step_index(current: usize, condition: bool) -> usize {
    current.saturating_add(usize::from(condition))
}

fn serialize_report(
    status: &str,
    completed_steps: &[&str],
    skipped_steps: &[String],
    duration_ms: u128,
    failed_step: Option<&str>,
    error: Option<&str>,
) -> String {
    let completed = completed_steps
        .iter()
        .map(|value| format!("\"{}\"", json_escape(value)))
        .collect::<Vec<_>>()
        .join(", ");
    let skipped = skipped_steps
        .iter()
        .map(|value| format!("\"{}\"", json_escape(value)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut output = format!(
        "{{\n  \"status\": \"{}\",\n  \"scenario_version\": {},\n  \"completed_steps\": [{}],\n  \"skipped_steps\": [{}],\n  \"duration_ms\": {}",
        json_escape(status),
        PGO_AUTOMATION_SCENARIO_VERSION,
        completed,
        skipped,
        duration_ms
    );
    if let Some(failed_step) = failed_step {
        output.push_str(&format!(
            ",\n  \"failed_step\": \"{}\"",
            json_escape(failed_step)
        ));
    }
    if let Some(error) = error {
        output.push_str(&format!(",\n  \"error\": \"{}\"", json_escape(error)));
    }
    output.push_str("\n}\n");
    output
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch <= '\u{1f}' => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", ch as u32);
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("pgo-report.json");
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

impl App {
    pub(crate) fn load_with_automation(options: AutomationOptions) -> Result<Self, String> {
        let mut app = Self::from_config(crate::config::AppConfig::load());
        app.automation = Some(AutomationController::new(options)?);
        Ok(app)
    }

    #[cold]
    #[inline(never)]
    pub(super) fn advance_automation(&mut self, now: Instant) -> Option<AutomationTick> {
        let mut automation = self.automation.take()?;
        let tick = automation.tick(self, now);
        self.automation = Some(automation);
        Some(tick)
    }

    #[cold]
    #[inline(never)]
    pub(super) fn interrupt_automation(&mut self, reason: &str) {
        let Some(mut automation) = self.automation.take() else {
            return;
        };
        automation.interrupt(reason);
        self.automation = Some(automation);
    }

    pub(super) fn take_automation_exit_result(&mut self) -> Result<(), String> {
        self.automation
            .as_mut()
            .map_or(Ok(()), AutomationController::take_exit_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn cli_parser_keeps_normal_app_uninstrumented() {
        assert_eq!(parse_automation_args(&args(&["ronsole"])).unwrap(), None);
        let app = App::new();
        assert!(app.automation.is_none());
    }

    #[test]
    fn cli_parser_accepts_frozen_contract() {
        let parsed = automation_options_from_args(&args(&[
            "ronsole",
            "--pgo-train",
            "--pgo-workspace",
            "/tmp/workspace",
            "--pgo-report",
            "/tmp/report.json",
            "--pgo-timeout-seconds",
            "120",
        ]))
        .unwrap();
        assert_eq!(parsed.workspace, Path::new("/tmp/workspace"));
        assert_eq!(parsed.report, Path::new("/tmp/report.json"));
        assert_eq!(parsed.timeout, Duration::from_secs(120));
    }

    #[test]
    fn cli_parser_rejects_missing_or_invalid_contract_values() {
        for invalid in [
            args(&["ronsole", "--pgo-train"]),
            args(&[
                "ronsole",
                "--pgo-train",
                "--pgo-workspace",
                "/tmp/workspace",
                "--pgo-report",
                "/tmp/report.json",
            ]),
            args(&[
                "ronsole",
                "--pgo-train",
                "--pgo-workspace",
                "relative",
                "--pgo-report",
                "/tmp/report.json",
                "--pgo-timeout-seconds",
                "1",
            ]),
            args(&[
                "ronsole",
                "--pgo-train",
                "--pgo-workspace",
                "/tmp/workspace",
                "--pgo-report",
                "/tmp/report.json",
                "--pgo-timeout-seconds",
                "0",
            ]),
        ] {
            assert!(automation_options_from_args(&invalid).is_err());
        }
    }

    #[test]
    fn deferred_tab_activation_blocks_automation_readiness_until_settled() {
        let mut app = App::new();
        let old = crate::terminal::Terminal::new_for_test(8, 2, 1);
        old.grid.lock().unwrap().mark_presentation_ready();
        let pending = crate::terminal::Terminal::new_for_test(8, 2, 2);
        app.terminals.push(old);
        app.terminals.push(pending);
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        app.interaction.layout.cols = 8;
        app.interaction.layout.visible_rows = 2;
        app.dirty = false;

        assert!(active_terminal_ready(&app));
        app.request_terminal_activation(1, false);

        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        assert_eq!(
            app.terminals[1].presentation_intent,
            TerminalPresentationIntent::ActivateWhenReady
        );
        assert!(!active_terminal_ready(&app));

        app.terminals[1]
            .grid
            .lock()
            .unwrap()
            .mark_presentation_ready();
        assert!(app.process_terminal_presentation_intents());
        assert_eq!(app.active_terminal, 1);
        assert!(
            app.terminals
                .iter()
                .all(|terminal| terminal.presentation_intent == TerminalPresentationIntent::None)
        );

        app.interaction.layout.cols = 8;
        app.interaction.layout.visible_rows = 2;
        app.dirty = false;
        assert!(active_terminal_ready(&app));
    }

    #[test]
    fn scenario_is_version_one_complete_and_finishes_last() {
        assert_eq!(PGO_AUTOMATION_SCENARIO_VERSION, 1);
        let steps = scenario();
        assert_eq!(steps.last(), Some(&AutomationStep::Finish));
        for block in SemanticBlock::ALL {
            assert!(steps.contains(&AutomationStep::Complete(block)));
        }
        let source = include_str!("automation.rs");
        assert!(!source.contains(&["h", "top"].concat()));
        assert!(!source.contains(&["b", "top"].concat()));
        let settings_steps = steps
            .iter()
            .filter(|step| {
                matches!(
                    step,
                    AutomationStep::SettingsOpen
                        | AutomationStep::WaitSettingsOpen
                        | AutomationStep::SettingsFontIncrease
                        | AutomationStep::SettingsFontDecrease
                        | AutomationStep::SettingsScrollIncrease
                        | AutomationStep::SettingsScrollDecrease
                        | AutomationStep::SettingsBackgroundAlternate
                        | AutomationStep::SettingsBackgroundRestore
                        | AutomationStep::SettingsHelp
                        | AutomationStep::SettingsClose
                        | AutomationStep::WaitSettingsClosed
                )
            })
            .count();
        assert!(settings_steps * 3 < steps.len());
    }

    #[test]
    fn condition_wait_advances_exactly_once() {
        assert_eq!(next_step_index(5, false), 5);
        assert_eq!(next_step_index(5, true), 6);
    }

    #[test]
    fn deadline_timeout_and_completed_state_are_deterministic() {
        let now = Instant::now();
        assert!(!deadline_expired(now, now + Duration::from_millis(1)));
        assert!(deadline_expired(now, now));
        let completed = SemanticBlock::ALL.map(SemanticBlock::report_name);
        assert!(mandatory_completion_valid(&completed));
        assert!(!mandatory_completion_valid(
            &completed[..completed.len() - 1]
        ));
    }

    #[test]
    fn report_serialization_covers_success_and_failure() {
        let success = serialize_report(
            "ok",
            &["startup-first-frame", "finish"],
            &[],
            123,
            None,
            None,
        );
        assert!(success.contains("\"status\": \"ok\""));
        assert!(success.contains("\"scenario_version\": 1"));
        assert!(!success.contains("failed_step"));

        let failure = serialize_report(
            "failed",
            &["startup-first-frame"],
            &["selection-copy-clipboard-unavailable".to_string()],
            456,
            Some("bulk-output"),
            Some("timed out \"waiting\""),
        );
        assert!(failure.contains("\"failed_step\": \"bulk-output\""));
        assert!(failure.contains("timed out \\\"waiting\\\""));
    }

    #[test]
    fn fixture_command_quotes_paths_and_uses_frozen_phase_names() {
        let command = fixture_command(
            Path::new("/tmp/work space/it's/terminal_fixture.sh"),
            FixturePhase::LongLines,
        )
        .unwrap();
        assert!(command.contains("'/tmp/work space/it'\"'\"'s/terminal_fixture.sh'"));
        assert!(command.contains(" long-lines;"));
        assert!(!command.contains("__RONSOLE_PGO_DONE_long-lines"));
        assert!(command.contains("'__RONSOLE_PGO_DONE_' 'long-lines'"));

        let process_tree = fixture_command(
            Path::new("/tmp/workspace/terminal_fixture.sh"),
            FixturePhase::ProcessTree,
        )
        .unwrap();
        assert!(process_tree.contains("'TREE_STARTED'; /bin/sh"));
        assert!(process_tree.contains(" process-tree; rc=$?"));
        assert!(!process_tree.contains("fixture_pid"));
        assert!(!process_tree.contains("__RONSOLE_PGO_PROCESS_TREE_STARTED"));
        assert!(process_tree.contains("'__RONSOLE_PGO_PROCESS_' 'TREE_STARTED'"));
    }

    #[test]
    fn resize_plan_is_narrow_wide_and_restores_original() {
        let plan = resize_plan(1200, 800);
        assert_eq!(plan.original, (1200, 800));
        assert!(plan.narrow.0 < plan.original.0);
        assert!(plan.narrow.0 <= NARROW_WIDTH_MAX);
        assert!(plan.wide.0 > plan.narrow.0);
        assert!(plan.wide.0 <= WIDE_WIDTH_MAX);
        assert_eq!(plan.narrow.1, plan.original.1);
        assert_eq!(plan.wide.1, plan.original.1);
    }

    #[test]
    fn scroll_plan_is_bounded_and_bidirectional() {
        let counts = [
            SCROLL_IMPULSES_AWAY,
            SCROLL_IMPULSES_TOWARD,
            SCROLL_IMPULSES_TO_TAIL,
        ];
        assert!(counts.into_iter().all(|count| (1..=16).contains(&count)));
        let steps = scenario();
        assert!(
            steps
                .iter()
                .any(|step| matches!(step, AutomationStep::ScrollImpulses { y, .. } if *y > 0.0))
        );
        assert!(
            steps
                .iter()
                .any(|step| matches!(step, AutomationStep::ScrollImpulses { y, .. } if *y < 0.0))
        );
    }

    #[test]
    fn cleanup_and_finish_are_mandatory_before_success() {
        let steps = scenario();
        let process_start = steps
            .iter()
            .position(|step| *step == AutomationStep::StartFixture(FixturePhase::ProcessTree))
            .unwrap();
        let live_spawn_wait = steps
            .iter()
            .position(|step| *step == AutomationStep::WaitElapsed(PROCESS_TREE_SPAWN_GRACE))
            .unwrap();
        let live_verify = steps
            .iter()
            .position(|step| *step == AutomationStep::VerifyProcessTreeStillRunning)
            .unwrap();
        let process_close = steps[process_start..]
            .iter()
            .position(|step| *step == AutomationStep::CloseActiveTab)
            .map(|offset| process_start + offset)
            .unwrap();
        let cleanup = steps
            .iter()
            .position(|step| *step == AutomationStep::CleanupBarrierFinal)
            .unwrap();
        let process_complete = steps
            .iter()
            .position(|step| *step == AutomationStep::Complete(SemanticBlock::ProcessTree))
            .unwrap();
        let finish = steps.len() - 1;
        assert!(process_start < live_spawn_wait);
        assert!(live_spawn_wait < live_verify);
        assert!(live_verify < process_close);
        assert!(
            !steps[process_start..process_close]
                .contains(&AutomationStep::WaitFixture(FixturePhase::ProcessTree))
        );
        assert!(cleanup < process_complete);
        assert!(process_complete < finish);
        assert_eq!(steps[finish], AutomationStep::Finish);
    }
}
