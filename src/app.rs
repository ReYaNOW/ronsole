use crate::config::{
    AppConfig, DEFAULT_SCROLL_SENSITIVITY, DEFAULT_TERMINAL_BACKGROUND,
    DEFAULT_TERMINAL_BOLD_FOREGROUND, DEFAULT_TERMINAL_FONT_SIZE, DEFAULT_TERMINAL_FOREGROUND,
    DEFAULT_TERMINAL_PALETTE, RgbColor, SCROLL_SENSITIVITY_STEP, TERMINAL_FONT_SIZE_STEP,
};
use crate::input::TerminalInteraction;
use crate::input_types::{
    CursorKind, KeyCode, KeyInput, KeyState, Modifiers, PhysicalKey, PointerButton,
    PointerPosition, ScrollDelta,
};
use crate::launch::TerminalLaunchSpec;
use crate::panels::{Panel, grid_fits, grid_tiles};
use crate::platform::{
    KdeActivationWorker, OpenSink, kde_session_active, open_uri_allowed, open_uri_with,
};
use crate::platform::single_instance::BridgeResult;
use crate::renderer::{
    CloseConfirmHit, CloseConfirmView, SettingsColorField, SettingsColorView, SettingsHit,
    SettingsTab, TerminalTabHit, TerminalUiLayout, settings_color_value, terminal_body_rect,
    terminal_palette,
};
use crate::runtime::{TerminalRenderParams, WindowRuntime};
use crate::scroll::ScrollState;
use crate::search::{Rect, TerminalSearchHit};
use crate::single_line_input::SingleLineInput;
use crate::tabs::{
    DRAG_AUTOSCROLL_EDGE_PX, TabDragState, active_index_after_move, active_index_after_remove,
    drag_autoscroll_delta, drag_autoscroll_speed, take_terminal_creation_number,
};
use crate::terminal::{
    SYNC_RESERVE_GRACE, Terminal, TerminalPresentationIntent, earliest_terminal_deadline,
    screen_hash, sync_reserve_wake,
};
use crate::terminal_process::{TerminalCleanupWorker, TerminalProcess, shutdown_terminal_processes};
use ronsole_term::TermPalette;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

mod automation;
mod direct_wayland;
mod headless_run;
mod tab_delivery;

pub(crate) use automation::{
    AutomationOptions, automation_options_from_args, write_automation_startup_failure,
};
pub(crate) use direct_wayland::{LoopFlow, LoopStep, StepReport};
pub(crate) use headless_run::HeadlessRunError;

const TERMINAL_CLEANUP_PENDING_CAPACITY: usize = 16;
const PENDING_EXTERNAL_LAUNCH_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopMode {
    Wait,
    Poll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppLoopControl {
    Exit,
    Wait,
    WaitUntil(Instant),
    Poll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FramePlan {
    request_frame: bool,
    loop_mode: LoopMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlobalShortcut {
    ToggleSettings,
    ToggleFps,
    NewTab,
    CloseTab,
    Search,
    ToggleClaudeGrid,
}

#[derive(Debug, PartialEq, Eq)]
enum ExternalLaunchActivation {
    WaylandXdgActivation { activation_token: String },
    KdeForceActivate,
    WaylandBestEffort,
}

#[derive(Debug, PartialEq, Eq)]
struct ExternalLaunchPlan {
    terminal_launch: Option<TerminalLaunchSpec>,
    activation: Option<ExternalLaunchActivation>,
}

#[inline(always)]
fn animation_dt(raw_dt: f32) -> f32 {
    raw_dt.min(0.016)
}

#[inline(always)]
fn frame_plan(renderable: bool, dirty: bool, animation_active: bool) -> FramePlan {
    if !renderable {
        return FramePlan {
            request_frame: false,
            loop_mode: LoopMode::Wait,
        };
    }

    FramePlan {
        request_frame: dirty || animation_active,
        loop_mode: if animation_active {
            LoopMode::Poll
        } else {
            LoopMode::Wait
        },
    }
}

#[inline(always)]
fn continuous_frame_reason(
    focused: bool,
    renderable: bool,
    animation_active: bool,
    show_fps: bool,
) -> bool {
    focused && renderable && (animation_active || show_fps)
}

#[inline]
fn exact_global_modifiers(modifiers: Modifiers, shift: bool) -> bool {
    modifiers.control_key()
        && modifiers.shift_key() == shift
        && !modifiers.alt_key()
        && !modifiers.super_key()
}

fn global_shortcut(key: PhysicalKey, modifiers: Modifiers) -> Option<GlobalShortcut> {
    match key {
        PhysicalKey::Code(KeyCode::F1) if modifiers.is_empty() => {
            Some(GlobalShortcut::ToggleSettings)
        }
        PhysicalKey::Code(KeyCode::F8) if exact_global_modifiers(modifiers, false) => {
            Some(GlobalShortcut::ToggleFps)
        }
        PhysicalKey::Code(KeyCode::KeyT) if exact_global_modifiers(modifiers, true) => {
            Some(GlobalShortcut::NewTab)
        }
        PhysicalKey::Code(KeyCode::Digit4) if exact_global_modifiers(modifiers, false) => {
            Some(GlobalShortcut::CloseTab)
        }
        PhysicalKey::Code(KeyCode::KeyF) if exact_global_modifiers(modifiers, false) => {
            Some(GlobalShortcut::Search)
        }
        PhysicalKey::Code(KeyCode::KeyS) if exact_global_modifiers(modifiers, true) => {
            Some(GlobalShortcut::ToggleClaudeGrid)
        }
        _ => None,
    }
}

#[inline]
fn global_shortcut_allowed_in_settings(shortcut: GlobalShortcut) -> bool {
    matches!(
        shortcut,
        GlobalShortcut::ToggleSettings | GlobalShortcut::ToggleFps
    )
}

const SETTINGS_BACKGROUND_MAX_CHARS: usize = 7;

#[derive(Debug, Default, PartialEq, Eq)]
struct SettingsBackgroundKeyOutcome {
    consumed: bool,
    text_changed: bool,
    visual_changed: bool,
    copy_text: Option<String>,
    finish: bool,
}

/// Tab rows listed by the close confirmation before the "and N more" line.
const CLOSE_CONFIRM_MAX_ROWS: usize = 8;

/// What a confirmed close does: the whole window or one tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseTarget {
    Window,
    Tab(usize),
}

/// Open close confirmation. Its text is built once when the dialog opens.
struct CloseConfirm {
    target: CloseTarget,
    title: &'static str,
    rows: Vec<String>,
    more: Option<String>,
}

impl CloseConfirm {
    fn line_count(&self) -> usize {
        self.rows.len() + usize::from(self.more.is_some())
    }

    /// Builds the dialog text from the busy tabs of a target, given as
    /// (tab title, job name) in tab order. Returns None when nothing is busy.
    /// Only the first `CLOSE_CONFIRM_MAX_ROWS` tabs get a row; the rest are counted.
    fn from_busy(target: CloseTarget, busy: &[(String, String)]) -> Option<Self> {
        if busy.is_empty() {
            return None;
        }
        let rows: Vec<String> = busy
            .iter()
            .take(CLOSE_CONFIRM_MAX_ROWS)
            .map(|(tab_title, process)| format!("{tab_title} — {process}"))
            .collect();
        let hidden = busy.len() - rows.len();
        let title = match target {
            CloseTarget::Window => "Закрыть окно?",
            CloseTarget::Tab(_) => "Закрыть вкладку?",
        };
        Some(Self {
            target,
            title,
            rows,
            more: (hidden > 0).then(|| format!("и ещё {hidden}")),
        })
    }
}

fn settings_hex_insert_text(input: &mut SingleLineInput, text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut clean = String::with_capacity(text.len().min(SETTINGS_BACKGROUND_MAX_CHARS));
    for ch in text.chars() {
        if ch == '\r' || ch == '\n' {
            continue;
        }
        if ch == '#' {
            clean.push(ch);
        } else if ch.is_ascii_hexdigit() {
            clean.push(ch.to_ascii_uppercase());
        } else {
            return false;
        }
    }
    if clean.is_empty() {
        return false;
    }
    let remaining = input
        .char_count()
        .saturating_sub(input.selected_char_count());
    if remaining + clean.chars().count() > SETTINGS_BACKGROUND_MAX_CHARS {
        return false;
    }
    input.insert_text(&clean)
}

/// Hex draft text of a Colors-tab field; an unset bold foreground has an empty draft.
fn settings_color_text(config: &AppConfig, field: SettingsColorField) -> String {
    settings_color_value(config, field).map_or_else(String::new, |color| color.to_hex())
}

fn default_settings_color_value(field: SettingsColorField) -> Option<RgbColor> {
    match field {
        SettingsColorField::Foreground => Some(DEFAULT_TERMINAL_FOREGROUND),
        SettingsColorField::BoldForeground => DEFAULT_TERMINAL_BOLD_FOREGROUND,
        SettingsColorField::Palette(index) => DEFAULT_TERMINAL_PALETTE.get(index).copied(),
    }
}

/// Stores one Colors-tab value. `value` is `None` only to unset bold foreground; for the
/// other fields it is ignored. Returns whether the config changed.
fn set_settings_color_field(
    config: &mut AppConfig,
    field: SettingsColorField,
    value: Option<RgbColor>,
) -> bool {
    if settings_color_value(config, field) == value {
        return false;
    }
    match (field, value) {
        (SettingsColorField::Foreground, Some(color)) => config.terminal_foreground = color,
        (SettingsColorField::BoldForeground, _) => config.terminal_bold_foreground = value,
        (SettingsColorField::Palette(index), Some(color)) => {
            let Some(slot) = config.terminal_palette.get_mut(index) else {
                return false;
            };
            *slot = color;
        }
        _ => return false,
    }
    true
}

fn settings_background_editor_key(
    input: &mut SingleLineInput,
    key: PhysicalKey,
    ctrl: bool,
    shift: bool,
    paste_text: Option<&str>,
) -> SettingsBackgroundKeyOutcome {
    let before_text = input.text.clone();
    let before_cursor = input.cursor;
    let before_anchor = input.selection_anchor;
    let mut outcome = SettingsBackgroundKeyOutcome::default();
    outcome.consumed = true;
    match key {
        PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter) => outcome.finish = true,
        PhysicalKey::Code(KeyCode::ArrowLeft) => {
            let _ = input.move_left(shift);
        }
        PhysicalKey::Code(KeyCode::ArrowRight) => {
            let _ = input.move_right(shift);
        }
        PhysicalKey::Code(KeyCode::Home) => {
            let _ = input.move_home(shift);
        }
        PhysicalKey::Code(KeyCode::End) => {
            let _ = input.move_end(shift);
        }
        PhysicalKey::Code(KeyCode::Backspace) => {
            let _ = input.backspace();
        }
        PhysicalKey::Code(KeyCode::Delete) => {
            let _ = input.delete_forward();
        }
        PhysicalKey::Code(KeyCode::KeyA) if ctrl => input.select_all(),
        PhysicalKey::Code(KeyCode::KeyC) if ctrl => {
            outcome.copy_text = input.selected_text();
        }
        PhysicalKey::Code(KeyCode::KeyX) if ctrl => {
            outcome.copy_text = input.selected_text();
            if outcome.copy_text.is_some() {
                let _ = input.delete_selection();
            }
        }
        PhysicalKey::Code(KeyCode::KeyV) if ctrl => {
            if let Some(text) = paste_text {
                let _ = settings_hex_insert_text(input, text);
            }
        }
        _ => outcome.consumed = false,
    }
    outcome.text_changed = input.text != before_text;
    outcome.visual_changed = outcome.text_changed
        || input.cursor != before_cursor
        || input.selection_anchor != before_anchor;
    outcome
}

fn begin_settings_background_pointer_selection(input: &mut SingleLineInput, cursor: usize) -> bool {
    let previous = (input.cursor, input.selection_anchor);
    let _ = input.move_cursor(cursor, false);
    input.selection_anchor = Some(input.cursor);
    previous != (input.cursor, input.selection_anchor)
}

fn update_settings_background_pointer_selection(
    input: &mut SingleLineInput,
    cursor: usize,
) -> bool {
    input.move_cursor(cursor, true)
}

#[inline]
fn ui_cursor_icon(
    settings_modal_active: bool,
    settings_hit: SettingsHit,
    settings_text_dragging: bool,
    search_hit: TerminalSearchHit,
    tab_hit: TerminalTabHit,
    tab_dragging: bool,
) -> CursorKind {
    if settings_modal_active {
        let text_field_hit = matches!(
            settings_hit,
            SettingsHit::BackgroundField | SettingsHit::ColorField(_)
        );
        return if settings_text_dragging || text_field_hit {
            CursorKind::Text
        } else {
            CursorKind::Default
        };
    }
    match search_hit {
        TerminalSearchHit::Input => return CursorKind::Text,
        TerminalSearchHit::Close
        | TerminalSearchHit::Previous
        | TerminalSearchHit::Next
        | TerminalSearchHit::CaseToggle => return CursorKind::Pointer,
        TerminalSearchHit::Chrome => return CursorKind::Default,
        TerminalSearchHit::None => {}
    }
    if tab_dragging {
        return CursorKind::Default;
    }
    match tab_hit {
        TerminalTabHit::Close(_) | TerminalTabHit::Add => CursorKind::Pointer,
        TerminalTabHit::Body(_) | TerminalTabHit::None => CursorKind::Default,
    }
}

#[inline]
fn cursor_icon_change(current: CursorKind, desired: CursorKind) -> Option<CursorKind> {
    (current != desired).then_some(desired)
}

#[inline]
fn external_launch_plan(
    focused: bool,
    kde_session: bool,
    request: crate::platform::single_instance::ExternalLaunchRequest,
) -> ExternalLaunchPlan {
    let explicit_terminal = request.launch.working_directory.is_some()
        || !request.launch.command.is_empty()
        || request.launch.hold;
    let terminal_launch = (focused || explicit_terminal).then_some(request.launch);
    let activation = if focused {
        None
    } else if let Some(activation_token) = request.activation_token {
        Some(ExternalLaunchActivation::WaylandXdgActivation { activation_token })
    } else if kde_session {
        Some(ExternalLaunchActivation::KdeForceActivate)
    } else {
        Some(ExternalLaunchActivation::WaylandBestEffort)
    };
    ExternalLaunchPlan {
        terminal_launch,
        activation,
    }
}

#[inline]
fn drag_threshold_passed(start_x: f32, current_x: f32, scale: f32) -> bool {
    (current_x - start_x).abs() > 5.0 * scale.max(0.1)
}

#[inline]
fn settings_animation_step(current: f32, open: bool, dt: f32) -> (f32, bool) {
    let target = if open { 1.0 } else { 0.0 };
    let diff = target - current.clamp(0.0, 1.0);
    if diff.abs() <= 0.001 {
        return (target, false);
    }
    let factor = (10.0 * dt.max(0.0)).clamp(0.0, 1.0);
    let next = (current + diff * factor).clamp(0.0, 1.0);
    if (target - next).abs() <= 0.001 {
        (target, false)
    } else {
        (next, true)
    }
}

#[inline]
fn tab_wheel_delta(delta: ScrollDelta, scale: f32) -> f32 {
    match delta {
        ScrollDelta::Line { y, .. } => -y * 40.0 * scale.max(0.1),
        ScrollDelta::Pixel { y, .. } => -y,
    }
}

fn ready_terminal_presentation<I>(states: I) -> Option<(usize, bool)>
where
    I: IntoIterator<Item = (usize, TerminalPresentationIntent, bool, bool)>,
{
    states
        .into_iter()
        .find_map(|(index, intent, ready, reveal_tail)| {
            (intent == TerminalPresentationIntent::ActivateWhenReady && ready)
                .then_some((index, reveal_tail))
        })
}

/// Window and terminal-cell metrics the panel layout is computed from.
/// `width`/`height` are in the units `terminal_body_rect` takes.
#[derive(Clone, Copy, PartialEq, Default)]
struct GridMetrics {
    width: f32,
    height: f32,
    scale: f32,
    char_w: f32,
    char_h: f32,
}

/// Grid mode state: the tabs shown as tiles. `members` holds tab indices in tab order.
#[derive(Default)]
struct ClaudeGrid {
    members: Vec<usize>,
}

/// Why grid mode did not open or had to close. Shown on the tab strip until
/// `GRID_HINT_DURATION` has passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GridHint {
    NoClaudeTabs,
    TooManyTabs,
}

impl GridHint {
    fn text(self) -> &'static str {
        match self {
            Self::NoClaudeTabs => "Нет вкладок с Claude",
            Self::TooManyTabs => "Слишком много вкладок с Claude для окна",
        }
    }
}

const GRID_HINT_DURATION: Duration = Duration::from_secs(2);

pub struct App {
    runtime: Option<WindowRuntime>,
    config: AppConfig,
    config_dirty: bool,
    config_save_attempted: bool,
    settings_font_value: String,
    settings_scroll_value: String,
    settings_background_input: SingleLineInput,
    /// Draft of the Colors-tab field named by `settings_color_target`.
    settings_color_input: SingleLineInput,
    /// Field whose draft `settings_color_input` holds; `None` while the hex editor
    /// edits the terminal background (or is closed).
    settings_color_target: Option<SettingsColorField>,
    settings_colors: SettingsColorView,
    settings_background_editing: bool,
    settings_background_dragging: bool,
    terminals: Vec<Terminal>,
    active_terminal: usize,
    active_terminal_presented: bool,
    next_terminal_creation_number: u64,
    terminal_tab_scroll: ScrollState,
    terminal_tab_drag: Option<TabDragState>,
    pending_tab_reveal: Option<bool>,
    interaction: TerminalInteraction,
    /// Where Ctrl+click links are sent: the system opener, or a recording sink when headless.
    open_sink: OpenSink,
    /// Shell program every new tab spawns; set only by headless runs, `None` keeps the usual resolution.
    shell_override: Option<PathBuf>,
    terminal_cleanup: TerminalCleanupWorker,
    kde_activation_worker: Option<KdeActivationWorker>,
    pending_terminal_cleanup: VecDeque<TerminalProcess>,
    modifiers: Modifiers,
    pointer_x: f32,
    pointer_y: f32,
    cursor_icon: CursorKind,
    settings_open: bool,
    settings_progress: f32,
    settings_active_tab: SettingsTab,
    close_confirm: Option<CloseConfirm>,
    show_fps: bool,
    focused: bool,
    unfocused_redraw_pending: bool,
    /// Set once a BEL asked the compositor for window attention during the
    /// current unfocused period; cleared when focus returns.
    window_attention_requested: bool,
    pending_external_launches: VecDeque<crate::platform::single_instance::ExternalLaunchRequest>,
    tab_input: tab_delivery::TabInputState,
    /// Reusable buffer for `Terminal::observe_dev_channel_warning`.
    dev_warning_scratch: String,
    /// A frame was skipped by `withhold_active_frame`; redraw once hiding ends.
    frame_withheld: bool,
    occluded: bool,
    zero_sized: bool,
    dirty: bool,
    animation_active: bool,
    last_frame: Instant,
    automation: Option<automation::AutomationController>,
    /// Written only by `set_grid_metrics`.
    grid_metrics: GridMetrics,
    /// Panels drawn in the current frame; rebuilt by `rebuild_panels` before each render.
    panels: Vec<Panel>,
    /// Layouts of `panels` from the last render, in `panels` order.
    panel_layouts: Vec<TerminalUiLayout>,
    /// `None` while the ordinary single-panel view is shown.
    claude_grid: Option<ClaudeGrid>,
    /// Grows whenever the panel composition, order, metrics or mode change.
    grid_generation: u64,
    /// `grid_generation` at the time `panel_layouts` were rendered.
    panel_layouts_generation: u64,
    /// Hint shown on the tab strip until its deadline.
    grid_hint: Option<(GridHint, Instant)>,
    /// Scratch for the tab indices of the current composition; swapped with `ClaudeGrid::members`.
    grid_members_scratch: Vec<usize>,
    /// Scratch for the tile rectangles `rebuild_panels` gets from `grid_tiles`.
    grid_tile_rects: Vec<(Rect, Rect)>,
    /// Pointer state reserved for grid tile handling.
    suppressed_release: Option<PointerButton>,
    last_header_press: Option<(Instant, usize)>,
    hovered_panel_terminal: Option<usize>,
    /// Pending deadline reserved for grid frame withholding.
    withheld_until: Option<Instant>,
}

impl App {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::from_config(AppConfig::default())
    }

    pub fn load() -> Self {
        Self::from_config(AppConfig::load())
    }

    fn from_config(config: AppConfig) -> Self {
        Self::from_parts(config, TerminalInteraction::default(), OpenSink::System)
    }

    /// Shared constructor for the system and headless paths, which differ only in
    /// the clipboard owner held by `interaction` and in `open_sink`.
    fn from_parts(
        config: AppConfig,
        mut interaction: TerminalInteraction,
        open_sink: OpenSink,
    ) -> Self {
        interaction.set_scroll_sensitivity(config.scroll_sensitivity);
        let settings_font_value = format!("{:.0}", config.terminal_font_size);
        let settings_scroll_value = format!("{:.2}", config.scroll_sensitivity);
        let settings_background_input =
            SingleLineInput::from_text(&config.terminal_background.to_hex());
        let settings_colors = SettingsColorView::new(&config);
        Self {
            runtime: None,
            config,
            config_dirty: false,
            config_save_attempted: false,
            settings_font_value,
            settings_scroll_value,
            settings_background_input,
            settings_color_input: SingleLineInput::default(),
            settings_color_target: None,
            settings_colors,
            settings_background_editing: false,
            settings_background_dragging: false,
            terminals: Vec::new(),
            active_terminal: 0,
            active_terminal_presented: false,
            next_terminal_creation_number: 1,
            terminal_tab_scroll: ScrollState::new(7.0),
            terminal_tab_drag: None,
            pending_tab_reveal: None,
            interaction,
            open_sink,
            shell_override: None,
            terminal_cleanup: TerminalCleanupWorker::new(),
            kde_activation_worker: None,
            pending_terminal_cleanup: VecDeque::with_capacity(TERMINAL_CLEANUP_PENDING_CAPACITY),
            modifiers: Modifiers::empty(),
            pointer_x: 0.0,
            pointer_y: 0.0,
            cursor_icon: CursorKind::Default,
            settings_open: false,
            settings_progress: 0.0,
            settings_active_tab: SettingsTab::General,
            close_confirm: None,
            show_fps: false,
            focused: true,
            unfocused_redraw_pending: false,
            window_attention_requested: false,
            pending_external_launches: VecDeque::with_capacity(PENDING_EXTERNAL_LAUNCH_CAPACITY),
            tab_input: tab_delivery::TabInputState::default(),
            dev_warning_scratch: String::new(),
            frame_withheld: false,
            occluded: false,
            zero_sized: false,
            dirty: true,
            animation_active: false,
            last_frame: Instant::now(),
            automation: None,
            grid_metrics: GridMetrics::default(),
            panels: Vec::new(),
            panel_layouts: Vec::new(),
            claude_grid: None,
            grid_generation: 0,
            panel_layouts_generation: 0,
            grid_hint: None,
            grid_members_scratch: Vec::new(),
            grid_tile_rects: Vec::new(),
            suppressed_release: None,
            last_header_press: None,
            hovered_panel_terminal: None,
            withheld_until: None,
        }
    }

    #[inline(always)]
    fn renderable(&self) -> bool {
        (self.focused || self.unfocused_redraw_pending)
            && !self.occluded
            && !self.zero_sized
            && self.runtime.is_some()
    }

    #[inline(always)]
    fn settings_modal_active(&self) -> bool {
        self.settings_open || self.settings_progress > 0.001
    }

    #[inline(always)]
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn mark_config_dirty(&mut self) {
        self.config_dirty = true;
        self.config_save_attempted = false;
    }

    fn persist_config_if_needed(&mut self) {
        if !self.config_dirty || self.config_save_attempted {
            return;
        }
        self.config_save_attempted = true;
        match self.config.save() {
            Ok(()) => self.config_dirty = false,
            Err(error) => eprintln!("Ronsole: failed to persist config: {error}"),
        }
    }

    fn prepare_exit(&mut self) {
        self.persist_config_if_needed();
    }

    fn suspend_frame_clock(&mut self) {
        self.last_frame = Instant::now();
    }

    fn request_frame(&mut self) {
        self.mark_dirty();
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.request_frame();
        }
    }

    fn cancel_terminal_presentation_intents(&mut self) {
        for terminal in &mut self.terminals {
            terminal.presentation_intent = TerminalPresentationIntent::None;
            terminal.reveal_right_tail_when_presented = false;
        }
    }

    /// Reports a focus change to one terminal. `Term::focus_changed` clears that
    /// terminal's sent-key set on every change, so the call always runs; the
    /// mode 1004 report is written only when `report` is set (terminal presented).
    fn send_terminal_focus_state(&self, index: usize, focused: bool, report: bool) {
        let Some(terminal) = self.terminals.get(index) else {
            return;
        };
        let sequence = crate::platform::lock_recover(&terminal.grid).focus_changed(focused);
        if let Some(sequence) = sequence.filter(|_| report) {
            let _ = terminal.write_input(sequence);
        }
    }

    fn send_terminal_focus_transition(&self, old: Option<usize>, new: Option<usize>) {
        if !self.focused || old == new {
            return;
        }
        if let Some(index) = old {
            self.send_terminal_focus_state(index, false, true);
        }
        if let Some(index) = new {
            self.send_terminal_focus_state(index, true, true);
        }
    }

    fn activate_ready_terminal(&mut self, index: usize, reveal_tail: bool) {
        if index >= self.terminals.len() {
            return;
        }
        if index == self.active_terminal && self.active_terminal_presented {
            self.pending_tab_reveal = Some(reveal_tail);
            self.mark_dirty();
            return;
        }
        let old_presented = self
            .active_terminal_presented
            .then_some(self.active_terminal);
        if index != self.active_terminal && self.active_terminal < self.terminals.len() {
            let old = self.active_terminal;
            self.interaction
                .reset_for_terminal_switch(&mut self.terminals[old]);
        }
        self.active_terminal = index;
        if let Some(terminal) = self.terminals.get_mut(index) {
            terminal.clear_bell_pending();
        }
        self.active_terminal_presented = true;
        self.send_terminal_focus_transition(old_presented, Some(index));
        self.pending_tab_reveal = Some(reveal_tail);
        self.mark_dirty();
    }

    fn process_terminal_presentation_intents(&mut self) -> bool {
        let ready = ready_terminal_presentation(self.terminals.iter().enumerate().map(
            |(index, terminal)| {
                (
                    index,
                    terminal.presentation_intent,
                    terminal.presentation_ready(),
                    terminal.reveal_right_tail_when_presented,
                )
            },
        ));
        let Some((index, reveal_tail)) = ready else {
            return false;
        };
        self.cancel_terminal_presentation_intents();
        self.activate_ready_terminal(index, reveal_tail);
        true
    }

    fn request_terminal_activation(&mut self, index: usize, reveal_tail: bool) {
        if index >= self.terminals.len() {
            return;
        }
        self.cancel_terminal_presentation_intents();
        self.terminals[index].presentation_intent = TerminalPresentationIntent::ActivateWhenReady;
        self.terminals[index].reveal_right_tail_when_presented = reveal_tail;
        let _ = self.process_terminal_presentation_intents();
    }

    fn select_terminal_tab_from_user(&mut self, index: usize) {
        self.exit_claude_grid(None);
        self.request_terminal_activation(index, false);
    }

    fn add_terminal(&mut self) -> Option<usize> {
        // A new tab starts where the active tab's shell reported it is (OSC 7).
        let inherited_directory = self
            .terminals
            .get(self.active_terminal)
            .and_then(|terminal| terminal.osc7_cwd.clone());
        self.add_terminal_with_launch(TerminalLaunchSpec {
            inherited_directory,
            ..TerminalLaunchSpec::default()
        })
    }

    fn add_terminal_with_launch(&mut self, mut launch: TerminalLaunchSpec) -> Option<usize> {
        // Leaves grid mode at the request, before the runtime check can return early.
        self.exit_claude_grid(None);
        let wake = self.runtime.as_ref()?.wake_handle();
        launch.shell.clone_from(&self.shell_override);
        self.cancel_terminal_presentation_intents();
        let display_number = take_terminal_creation_number(&mut self.next_terminal_creation_number);
        let terminal = Terminal::spawn(
            Some(wake),
            display_number,
            launch,
            self.new_terminal_palette(),
        );
        let index = self.terminals.len();
        self.terminals.push(terminal);
        if self.terminals.len() == 1 {
            self.active_terminal = 0;
        }
        self.request_terminal_activation(index, true);
        self.request_frame();
        Some(index)
    }

    /// Palette a tab created now starts with: the configured background, with
    /// the renderer's default foreground, ANSI colours and cursor.
    fn new_terminal_palette(&self) -> TermPalette {
        terminal_palette(&self.config)
    }

    pub(crate) fn find_terminal_by_bridge_launch_id(&self, id: [u8; 16]) -> Option<usize> {
        let now = Instant::now();
        self.terminals.iter().position(|terminal| {
            terminal
                .bridge_launch_state()
                .is_some_and(|state| state.bridge_launch_id == id && state.active_at(now))
        })
    }

    /// Development-channels warning check for every tab (see
    /// `Terminal::observe_dev_channel_warning`); runs before
    /// `advance_bridge_launches` so one warning gets one Enter. Requests the
    /// frame that `withhold_active_frame` skipped once hiding has ended.
    fn advance_dev_channel_warnings(&mut self, now: Instant) {
        for terminal in &mut self.terminals {
            terminal.observe_dev_channel_warning(now, &mut self.dev_warning_scratch, |pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
            terminal.observe_dev_channel_banner(|pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
        }
        if self.frame_withheld && !self.active_dev_warning_hidden(now) {
            self.frame_withheld = false;
            self.request_frame();
        }
    }

    fn active_dev_warning_hidden(&self, now: Instant) -> bool {
        self.terminals
            .get(self.active_terminal)
            .is_some_and(|terminal| terminal.dev_warning_frame_hidden(now))
    }

    /// Whether to skip this frame because the active tab shows the warning that
    /// Enter was already sent for: the compositor keeps the last presented
    /// frame. Rescans the active tab first, since output may have arrived after
    /// `on_about_to_wait`. Input is not affected. Also applies the banner
    /// hiding (`Terminal::observe_dev_channel_banner`) to the new content.
    fn withhold_active_frame(&mut self, now: Instant) -> bool {
        if let Some(terminal) = self.terminals.get_mut(self.active_terminal) {
            terminal.observe_dev_channel_warning(now, &mut self.dev_warning_scratch, |pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
            terminal.observe_dev_channel_banner(|pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
        }
        if !self.active_dev_warning_hidden(now) {
            return false;
        }
        self.frame_withheld = true;
        self.dirty = false;
        true
    }

    fn advance_bridge_launches(&mut self, now: Instant) {
        let mut sent = false;
        for terminal in &mut self.terminals {
            let pending = terminal
                .bridge_launch_state()
                .is_some_and(|state| state.active_at(now) && !state.enter_sent);
            sent |= pending && terminal.maybe_auto_confirm_bridge(now);
        }
        if sent {
            self.request_frame();
        }
    }

    fn handle_bridge_request(
        &mut self,
        envelope: crate::platform::single_instance::BridgeRequestEnvelope,
    ) {
        if envelope.peer.uid != crate::platform::single_instance::effective_uid() {
            let _ = envelope.reply.send(BridgeResult::code(4));
            return;
        }
        let request = envelope.request;
        let result = match request.op {
            0 => self.find_terminal_by_bridge_launch_id(request.bridge_launch_id)
                .map(|index| {
                    let screen = self.terminals[index].screen_tail_text(usize::MAX).join("\n");
                    let hash = screen_hash(&screen);
                    BridgeResult {
                        code: 0,
                        screen: Some((screen, hash)),
                    }
                })
                .unwrap_or_else(|| BridgeResult::code(3)),
            1 => match self.find_terminal_by_bridge_launch_id(request.bridge_launch_id) {
                None => BridgeResult::code(3),
                Some(index) => {
                    if self.terminals[index]
                        .bridge_launch_state()
                        .is_some_and(|state| state.enter_sent)
                    {
                        BridgeResult::code(2)
                    } else {
                        if let Some(expected_hash) = request.screen_hash {
                            if self.terminals[index].manually_confirm_bridge(
                                Instant::now(),
                                expected_hash,
                            ) {
                                self.request_frame();
                                BridgeResult::code(0)
                            } else {
                                BridgeResult::code(1)
                            }
                        } else {
                            BridgeResult::code(1)
                        }
                    }
                }
            },
            2 => self.find_terminal_by_bridge_launch_id(request.bridge_launch_id)
                .map(|index| {
                    self.terminals[index].clear_bridge_launch_state();
                    BridgeResult::code(0)
                })
                .unwrap_or_else(|| BridgeResult::code(3)),
            crate::platform::single_instance::BRIDGE_OP_ESCAPE => match request.claude_process {
                Some((pid, starttime)) => BridgeResult::code(self.send_bridge_escape(pid, starttime)),
                None => BridgeResult::code(3),
            },
            _ => BridgeResult::code(3),
        };
        let _ = envelope.reply.send(result);
    }

    fn handle_external_launch(
        &mut self,
        request: crate::platform::single_instance::ExternalLaunchRequest,
    ) {
        if self.runtime.is_none() {
            if self.pending_external_launches.len() < PENDING_EXTERNAL_LAUNCH_CAPACITY {
                self.pending_external_launches.push_back(request);
            }
            return;
        }
        let kde_session =
            !self.focused && request.activation_token.is_none() && kde_session_active();
        let plan = external_launch_plan(self.focused, kde_session, request);
        if let Some(launch) = plan.terminal_launch {
            let _ = self.add_terminal_with_launch(launch);
        }
        match plan.activation {
            None => {}
            Some(ExternalLaunchActivation::WaylandXdgActivation { activation_token }) => {
                if let Some(runtime) = self.runtime.as_mut() {
                    runtime.activate_existing_window(Some(activation_token));
                }
            }
            Some(ExternalLaunchActivation::KdeForceActivate) => {
                let worker = self
                    .kde_activation_worker
                    .get_or_insert_with(KdeActivationWorker::new);
                worker.try_activate(std::process::id());
            }
            Some(ExternalLaunchActivation::WaylandBestEffort) => {
                if let Some(runtime) = self.runtime.as_mut() {
                    runtime.activate_existing_window(None);
                }
            }
        }
    }

    fn flush_pending_external_launches(&mut self) {
        while let Some(request) = self.pending_external_launches.pop_front() {
            self.handle_external_launch(request);
        }
    }

    fn close_terminal_tab_at(&mut self, index: usize) -> bool {
        if index >= self.terminals.len() {
            return false;
        }
        let old_active = self.active_terminal;
        // Grid mode: the tile that takes focus when the focused tile closes,
        // numbered for the tab list after the removal.
        let grid_successor = if index == old_active {
            self.grid_successor_after_removal(index)
        } else {
            None
        };
        let is_final = self.terminals.len() == 1;
        let active_was_presented = index == old_active && self.active_terminal_presented;
        if index == old_active {
            if active_was_presented {
                self.send_terminal_focus_transition(Some(index), None);
            }
        }

        if !is_final {
            let process = self.terminals[index].take_process_for_cleanup();
            if let Some(process) = process
                && let Err(process) = self.try_schedule_terminal_cleanup(process)
            {
                self.terminals[index].restore_process_after_cleanup_backpressure(process);
                if active_was_presented {
                    self.send_terminal_focus_transition(None, Some(index));
                }
                return false;
            }
        }

        if index == old_active {
            self.interaction
                .reset_for_terminal_switch(&mut self.terminals[index]);
            self.active_terminal_presented = false;
        }
        let mut removed = self.terminals.remove(index);
        // Removing a tab shifts the indices a `CloseTarget::Tab` points at, so an open
        // dialog no longer names the tab it was opened for. Cancel it instead.
        self.cancel_close_dialog();
        if is_final {
            removed.shutdown();
        }
        self.terminal_tab_drag = None;
        self.pending_tab_reveal = None;

        if self.terminals.is_empty() {
            self.active_terminal = 0;
            self.active_terminal_presented = false;
            self.cancel_terminal_presentation_intents();
            return true;
        }

        self.active_terminal = active_index_after_remove(old_active, index, self.terminals.len());
        if index == old_active {
            if let Some(successor) = grid_successor {
                // Focus stays on a tile; the neighbour outside the grid gets no
                // deferred activation.
                self.cancel_terminal_presentation_intents();
                self.active_terminal = successor;
                self.activate_ready_terminal(successor, false);
            } else if self.terminals[self.active_terminal].presentation_ready() {
                self.activate_ready_terminal(self.active_terminal, false);
            } else {
                self.cancel_terminal_presentation_intents();
                self.terminals[self.active_terminal].presentation_intent =
                    TerminalPresentationIntent::ActivateWhenReady;
            }
        }
        self.pending_tab_reveal = Some(false);
        self.request_frame();
        self.sync_claude_grid(Instant::now());
        false
    }

    fn try_schedule_terminal_cleanup(
        &mut self,
        process: TerminalProcess,
    ) -> Result<(), TerminalProcess> {
        if !self.terminal_cleanup.is_available() {
            return Err(process);
        }
        let wake = self.runtime.as_ref().map(WindowRuntime::wake_handle);
        match self.terminal_cleanup.try_enqueue(process, wake) {
            Ok(()) => Ok(()),
            Err(process)
                if self.pending_terminal_cleanup.len() < TERMINAL_CLEANUP_PENDING_CAPACITY =>
            {
                self.pending_terminal_cleanup.push_back(process);
                Ok(())
            }
            Err(process) => Err(process),
        }
    }

    fn flush_pending_terminal_cleanup(&mut self) {
        if self.pending_terminal_cleanup.is_empty() {
            return;
        }
        let wake = self.runtime.as_ref().map(WindowRuntime::wake_handle);
        while let Some(process) = self.pending_terminal_cleanup.pop_front() {
            match self.terminal_cleanup.try_enqueue(process, wake.clone()) {
                Ok(()) => {}
                Err(process) => {
                    self.pending_terminal_cleanup.push_front(process);
                    break;
                }
            }
        }
    }

    fn close_active_terminal(&mut self) -> bool {
        self.request_terminal_close(CloseTarget::Tab(self.active_terminal))
    }

    /// Closes at once when nothing runs in the target or the prompt is disabled,
    /// otherwise opens the confirmation. A request made while a dialog is already
    /// open is ignored, so the open dialog keeps its target. Returns true when the
    /// event loop must exit.
    fn request_terminal_close(&mut self, target: CloseTarget) -> bool {
        if self.close_confirm.is_some() {
            return false;
        }
        if !self.config.confirm_close {
            return self.perform_terminal_close(target);
        }
        let Some(confirm) = CloseConfirm::from_busy(target, &self.busy_close_targets(target))
        else {
            return self.perform_terminal_close(target);
        };
        self.open_close_dialog(confirm);
        false
    }

    /// Shows the dialog. Pointer state begun before it opened is dropped, because
    /// the dialog swallows the release events that would otherwise end that drag.
    /// An unfocused window also asks for attention, so the prompt is noticed.
    fn open_close_dialog(&mut self, confirm: CloseConfirm) {
        self.cancel_pointer_interactions();
        self.settings_background_dragging = false;
        self.close_confirm = Some(confirm);
        if !self.focused
            && let Some(runtime) = self.runtime.as_mut()
        {
            // The result is not needed: the flag for bell attention stays untouched.
            let _ = runtime.request_window_attention();
        }
        self.refresh_cursor_icon();
        self.request_frame();
    }

    fn perform_terminal_close(&mut self, target: CloseTarget) -> bool {
        match target {
            CloseTarget::Window => {
                self.finish_terminal_session();
                true
            }
            CloseTarget::Tab(index) => self.close_terminal_tab_at(index),
        }
    }

    /// Reads foreground job names only at the close moment. Returns (tab title, job
    /// name) for each tab in the target that runs a job; empty when none does, so the
    /// close proceeds without a dialog.
    fn busy_close_targets(&self, target: CloseTarget) -> Vec<(String, String)> {
        let range = match target {
            CloseTarget::Window => 0..self.terminals.len(),
            CloseTarget::Tab(index) => index..index.saturating_add(1).min(self.terminals.len()),
        };
        let mut busy = Vec::new();
        for index in range {
            let Some(terminal) = self.terminals.get(index) else {
                continue;
            };
            let Some(process) = terminal.busy_foreground_name() else {
                continue;
            };
            let mut tab_title = String::new();
            terminal.write_display_title(&mut tab_title);
            busy.push((tab_title, process));
        }
        busy
    }

    /// Closes the dialog's target. Returns true when the event loop must exit.
    fn confirm_close_dialog(&mut self) -> bool {
        let Some(confirm) = self.close_confirm.take() else {
            return false;
        };
        self.request_frame();
        self.perform_terminal_close(confirm.target)
    }

    fn cancel_close_dialog(&mut self) {
        if self.close_confirm.take().is_some() {
            self.update_link_hover();
            self.request_frame();
        }
    }

    fn close_confirm_hit_test(&self, x: f32, y: f32) -> CloseConfirmHit {
        let (Some(confirm), Some(runtime)) = (self.close_confirm.as_ref(), self.runtime.as_ref())
        else {
            return CloseConfirmHit::None;
        };
        runtime.close_confirm_hit_test(confirm.line_count(), x, y)
    }

    fn reorder_terminal_tab(&mut self, from: usize, to: usize) {
        if from >= self.terminals.len() || to >= self.terminals.len() || from == to {
            return;
        }
        let terminal = self.terminals.remove(from);
        self.terminals.insert(to, terminal);
        self.active_terminal = active_index_after_move(self.active_terminal, from, to);
        self.pending_tab_reveal = Some(false);
        self.mark_dirty();
    }

    fn cancel_pointer_interactions(&mut self) {
        self.terminal_tab_drag = None;
        let active = self.active_terminal;
        if let Some(terminal) = self.terminals.get_mut(active) {
            self.interaction.cancel_pointer_interaction(terminal);
        }
    }

    fn clear_active_terminal_text_selection(&mut self) -> bool {
        let active = self.active_terminal;
        self.terminals
            .get_mut(active)
            .is_some_and(|terminal| self.interaction.clear_text_selection(terminal))
    }

    pub(crate) fn on_focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        self.unfocused_redraw_pending = !focused;
        if focused {
            self.window_attention_requested = false;
        }
        self.modifiers = Modifiers::empty();
        self.interaction.set_modifiers(Modifiers::empty());
        self.interaction.cancel_link_press();
        self.update_link_hover();
        self.send_terminal_focus_state(self.active_terminal, focused, self.active_terminal_presented);
        if !focused {
            self.cancel_pointer_interactions();
            self.settings_background_dragging = false;
        }
        if self.show_fps
            && let Some(runtime) = self.runtime.as_mut()
        {
            runtime.reset_fps_counter();
        }
        self.suspend_frame_clock();
        self.request_frame();
    }

    fn toggle_settings(&mut self) {
        let opening = !self.settings_open;
        if opening {
            self.cancel_pointer_interactions();
            self.settings_background_input
                .set_text(&self.config.terminal_background.to_hex());
            self.settings_background_editing = false;
            self.settings_background_dragging = false;
        } else {
            self.finish_settings_background_edit();
        }
        self.settings_open = !self.settings_open;
        self.refresh_cursor_icon();
        self.request_frame();
    }

    fn toggle_fps(&mut self) {
        self.show_fps = !self.show_fps;
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.reset_fps_counter();
        }
        self.request_frame();
    }

    fn close_settings(&mut self) {
        if self.settings_open {
            self.finish_settings_background_edit();
            self.settings_open = false;
            self.settings_background_dragging = false;
            self.refresh_cursor_icon();
            self.request_frame();
        }
    }

    fn set_terminal_background(&mut self, color: RgbColor) -> bool {
        if self.config.terminal_background == color {
            return false;
        }
        self.config.terminal_background = color;
        for terminal in &self.terminals {
            let mut grid = crate::platform::lock_recover(&terminal.grid);
            grid.set_palette(terminal_palette(&self.config));
        }
        self.request_frame();
        if let Some(runtime) = self.runtime.as_mut() {
            let _ = runtime.set_terminal_background(color);
        }
        true
    }

    /// Applies the current draft when it is a complete colour; partial or invalid text
    /// leaves the config unchanged.
    fn apply_valid_settings_background_draft(&mut self) {
        let target = self.settings_color_target;
        let changed = match target {
            None => RgbColor::parse_hex(&self.settings_background_input.text)
                .is_some_and(|color| self.set_terminal_background(color)),
            Some(field) => self.apply_settings_color_draft(field),
        };
        if changed {
            self.mark_config_dirty();
        }
    }

    fn apply_settings_color_draft(&mut self, field: SettingsColorField) -> bool {
        let text = self.settings_color_input.text.as_str();
        let value = if text.is_empty() && field == SettingsColorField::BoldForeground {
            Some(None)
        } else {
            RgbColor::parse_hex(text).map(Some)
        };
        let Some(value) = value else {
            return false;
        };
        let changed = set_settings_color_field(&mut self.config, field, value);
        if changed {
            self.apply_terminal_colors();
        }
        changed
    }

    /// Pushes the configured colours to every tab's grid and to the renderer. Call only
    /// after a real change; the renderer returns whether its palette actually moved.
    fn apply_terminal_colors(&mut self) {
        for terminal in &self.terminals {
            let mut grid = crate::platform::lock_recover(&terminal.grid);
            grid.set_palette(terminal_palette(&self.config));
        }
        self.settings_colors = SettingsColorView::new(&self.config);
        if let Some(runtime) = self.runtime.as_mut() {
            let _ = runtime.set_terminal_colors(&self.config);
        }
        self.request_frame();
    }

    fn begin_settings_color_edit(&mut self, field: SettingsColorField) {
        self.settings_color_input
            .set_text(&settings_color_text(&self.config, field));
        self.settings_color_target = Some(field);
        self.settings_background_editing = true;
        self.request_frame();
    }

    fn finish_settings_background_edit(&mut self) {
        if !self.settings_background_editing {
            return;
        }
        self.apply_valid_settings_background_draft();
        match self.settings_color_target.take() {
            None => self
                .settings_background_input
                .set_text(&self.config.terminal_background.to_hex()),
            Some(field) => self
                .settings_color_input
                .set_text(&settings_color_text(&self.config, field)),
        }
        self.settings_background_editing = false;
        self.settings_background_dragging = false;
        self.request_frame();
    }

    fn edit_settings_background_text(&mut self, text: &str) -> bool {
        if !self.settings_background_editing {
            return false;
        }
        let input = match self.settings_color_target {
            None => &mut self.settings_background_input,
            Some(_) => &mut self.settings_color_input,
        };
        if settings_hex_insert_text(input, text) {
            self.apply_valid_settings_background_draft();
            self.request_frame();
        }
        true
    }

    fn handle_settings_background_key(&mut self, key: PhysicalKey) -> bool {
        if !self.settings_background_editing {
            return false;
        }
        let ctrl = self.modifiers.control_key();
        let shift = self.modifiers.shift_key();
        let paste = if ctrl && key == PhysicalKey::Code(KeyCode::KeyV) {
            self.interaction.clipboard_text()
        } else {
            None
        };
        let input = match self.settings_color_target {
            None => &mut self.settings_background_input,
            Some(_) => &mut self.settings_color_input,
        };
        let outcome = settings_background_editor_key(
            input,
            key,
            ctrl,
            shift,
            paste.as_deref(),
        );
        if let Some(copy) = outcome.copy_text {
            let _ = self.interaction.set_clipboard_text(copy);
        }
        if outcome.finish {
            self.finish_settings_background_edit();
            return true;
        }
        if outcome.text_changed {
            self.apply_valid_settings_background_draft();
        }
        if outcome.visual_changed {
            self.request_frame();
        }
        outcome.consumed
    }

    fn settings_background_cursor_from_x(&mut self, x: f32) -> Option<usize> {
        let input = &self.settings_background_input;
        self.runtime.as_mut()?.settings_background_cursor_from_x(
            self.settings_progress,
            self.settings_active_tab,
            &input.text,
            x,
            input.scroll_x,
        )
    }

    fn begin_settings_background_pointer_edit(&mut self, cursor: usize) {
        if self.settings_color_target.is_some() {
            self.finish_settings_background_edit();
        }
        if !self.settings_background_editing {
            self.settings_background_input
                .set_text(&self.config.terminal_background.to_hex());
            self.settings_background_editing = true;
        }
        let changed = begin_settings_background_pointer_selection(
            &mut self.settings_background_input,
            cursor,
        );
        self.settings_background_dragging = true;
        if changed {
            self.request_frame();
        }
    }

    fn apply_settings_hit(&mut self, hit: SettingsHit) {
        if hit == SettingsHit::Outside {
            self.close_settings();
            return;
        }
        if let SettingsHit::ColorField(field) = hit {
            let already_editing =
                self.settings_background_editing && self.settings_color_target == Some(field);
            if !already_editing {
                self.finish_settings_background_edit();
                self.begin_settings_color_edit(field);
            }
            return;
        }
        if hit != SettingsHit::BackgroundField && self.settings_background_editing {
            self.finish_settings_background_edit();
        }
        if hit == SettingsHit::BackgroundField {
            if !self.settings_background_editing {
                self.settings_background_input
                    .set_text(&self.config.terminal_background.to_hex());
                self.settings_background_editing = true;
                self.request_frame();
            }
            return;
        }
        if let SettingsHit::Tab(tab) = hit {
            if self.settings_active_tab != tab {
                self.settings_active_tab = tab;
                self.request_frame();
            }
            return;
        }
        let changed = match hit {
            SettingsHit::FontDecrease | SettingsHit::FontIncrease => {
                let delta = if hit == SettingsHit::FontDecrease {
                    -TERMINAL_FONT_SIZE_STEP
                } else {
                    TERMINAL_FONT_SIZE_STEP
                };
                if !self.config.adjust_terminal_font_size(delta) {
                    false
                } else {
                    let logical_size = self.config.terminal_font_size;
                    if let Some(runtime) = self.runtime.as_mut() {
                        let _ = runtime.set_terminal_font_size(logical_size);
                    }
                    self.refresh_grid_metrics();
                    self.settings_font_value = format!("{logical_size:.0}");
                    true
                }
            }
            SettingsHit::FontReset => {
                if (self.config.terminal_font_size - DEFAULT_TERMINAL_FONT_SIZE).abs()
                    < f32::EPSILON
                {
                    false
                } else {
                    self.config.terminal_font_size = DEFAULT_TERMINAL_FONT_SIZE;
                    if let Some(runtime) = self.runtime.as_mut() {
                        let _ = runtime.set_terminal_font_size(DEFAULT_TERMINAL_FONT_SIZE);
                    }
                    self.refresh_grid_metrics();
                    self.settings_font_value = format!("{DEFAULT_TERMINAL_FONT_SIZE:.0}");
                    true
                }
            }
            SettingsHit::ScrollDecrease | SettingsHit::ScrollIncrease => {
                let delta = if hit == SettingsHit::ScrollDecrease {
                    -SCROLL_SENSITIVITY_STEP
                } else {
                    SCROLL_SENSITIVITY_STEP
                };
                if !self.config.adjust_scroll_sensitivity(delta) {
                    false
                } else {
                    let sensitivity = self.config.scroll_sensitivity;
                    self.interaction.set_scroll_sensitivity(sensitivity);
                    self.settings_scroll_value = format!("{sensitivity:.2}");
                    true
                }
            }
            SettingsHit::ScrollReset => {
                if (self.config.scroll_sensitivity - DEFAULT_SCROLL_SENSITIVITY).abs()
                    < f32::EPSILON
                {
                    false
                } else {
                    self.config.scroll_sensitivity = DEFAULT_SCROLL_SENSITIVITY;
                    self.interaction
                        .set_scroll_sensitivity(DEFAULT_SCROLL_SENSITIVITY);
                    self.settings_scroll_value = format!("{DEFAULT_SCROLL_SENSITIVITY:.2}");
                    true
                }
            }
            SettingsHit::ConfirmCloseToggle => {
                self.config.confirm_close = !self.config.confirm_close;
                true
            }
            SettingsHit::BackgroundReset => {
                let changed = self.set_terminal_background(DEFAULT_TERMINAL_BACKGROUND);
                self.settings_background_input
                    .set_text(&DEFAULT_TERMINAL_BACKGROUND.to_hex());
                changed
            }
            SettingsHit::ColorReset(field) => {
                let changed = set_settings_color_field(
                    &mut self.config,
                    field,
                    default_settings_color_value(field),
                );
                if changed {
                    self.apply_terminal_colors();
                }
                changed
            }
            SettingsHit::None
            | SettingsHit::Outside
            | SettingsHit::Tab(_)
            | SettingsHit::BackgroundField
            | SettingsHit::ColorField(_) => false,
        };
        if changed {
            self.mark_config_dirty();
            self.request_frame();
        }
    }

    fn settings_hit_test(&self, x: f32, y: f32) -> SettingsHit {
        self.runtime.as_ref().map_or(SettingsHit::None, |runtime| {
            runtime.settings_hit_test(self.settings_progress, self.settings_active_tab, x, y)
        })
    }

    fn search_hit_test(&self, x: f32, y: f32) -> TerminalSearchHit {
        self.interaction
            .layout
            .search
            .map_or(TerminalSearchHit::None, |search| search.hit_test(x, y))
    }

    fn desired_cursor_icon(&self) -> CursorKind {
        let modal_active = self.settings_modal_active() || self.close_confirm.is_some();
        let settings_hit = if modal_active {
            self.settings_hit_test(self.pointer_x, self.pointer_y)
        } else {
            SettingsHit::None
        };
        let search_hit = if modal_active {
            TerminalSearchHit::None
        } else {
            self.search_hit_test(self.pointer_x, self.pointer_y)
        };
        let tab_hit = if modal_active || search_hit != TerminalSearchHit::None {
            TerminalTabHit::None
        } else {
            self.runtime
                .as_ref()
                .map_or(TerminalTabHit::None, |runtime| {
                    runtime.terminal_tab_hit_test(self.pointer_x, self.pointer_y)
                })
        };
        let icon = ui_cursor_icon(
            modal_active,
            settings_hit,
            self.settings_background_dragging,
            search_hit,
            tab_hit,
            self.terminal_tab_drag.is_some(),
        );
        let link_hovered = self
            .terminals
            .get(self.active_terminal)
            .is_some_and(|terminal| terminal.hovered_link().is_some());
        if icon == CursorKind::Default && link_hovered {
            CursorKind::Pointer
        } else {
            icon
        }
    }

    /// Recomputes the Ctrl+hover link of the active tab and refreshes the cursor.
    /// Called on pointer motion and on modifier changes. Marks the app dirty
    /// when the hovered link changed, so the underline is drawn on an idle window.
    fn update_link_hover(&mut self) {
        let modal_active = self.settings_modal_active() || self.close_confirm.is_some();
        let (x, y) = (self.pointer_x, self.pointer_y);
        let changed = match self.terminals.get_mut(self.active_terminal) {
            Some(terminal) if modal_active => terminal.set_hovered_link(None),
            Some(terminal) => self.interaction.update_link_hover(terminal, x, y, self.interaction.layout),
            None => false,
        };
        if changed {
            self.mark_dirty();
        }
        self.refresh_cursor_icon();
    }

    fn refresh_cursor_icon(&mut self) {
        let desired = self.desired_cursor_icon();
        let Some(next) = cursor_icon_change(self.cursor_icon, desired) else {
            return;
        };
        self.cursor_icon = next;
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.set_cursor_kind(next);
        }
    }

    fn sync_animation_state(&mut self) {
        let terminal_animation = self
            .terminals
            .get(self.active_terminal)
            .is_some_and(|terminal| self.interaction.animation_active(terminal));
        let tab_animation = !self.terminal_tab_scroll.is_settled()
            || self
                .runtime
                .as_ref()
                .is_some_and(WindowRuntime::terminal_tab_animation_active)
            || self
                .terminal_tab_drag
                .is_some_and(|drag| drag.threshold_passed);
        let settings_animation =
            (self.settings_progress - if self.settings_open { 1.0 } else { 0.0 }).abs() > 0.001;
        self.animation_active = terminal_animation || tab_animation || settings_animation;
    }

    fn loop_control(&self) -> AppLoopControl {
        let renderable = self.renderable();
        let plan = frame_plan(
            renderable,
            self.dirty,
            continuous_frame_reason(
                self.focused,
                renderable,
                self.animation_active,
                self.show_fps,
            ),
        );
        let now = Instant::now();
        let search_deadline = (self.focused && renderable)
            .then(|| self.interaction.search_refresh_deadline())
            .flatten();
        let search_due = search_deadline.is_some_and(|deadline| deadline <= now);
        if plan.request_frame || search_due {
            if let Some(runtime) = self.runtime.as_ref() {
                runtime.request_frame();
            }
        }
        match plan.loop_mode {
            LoopMode::Wait => search_deadline
                .filter(|deadline| *deadline > now)
                .into_iter()
                .chain(self.tab_input.next_deadline())
                .chain(
                    earliest_terminal_deadline(&self.terminals)
                        .map(|deadline| sync_reserve_wake(deadline, now)),
                )
                .chain(
                    self.terminals
                        .get(self.active_terminal)
                        .and_then(Terminal::dev_warning_hidden_until)
                        .filter(|deadline| *deadline > now),
                )
                .chain(
                    self.grid_hint
                        .map(|(_, deadline)| deadline)
                        .filter(|deadline| *deadline > now),
                )
                .min()
                .map_or(AppLoopControl::Wait, AppLoopControl::WaitUntil),
            LoopMode::Poll => AppLoopControl::Poll,
        }
    }

    fn loop_control_with_automation_deadline(
        &self,
        automation_deadline: Instant,
    ) -> AppLoopControl {
        match self.loop_control() {
            AppLoopControl::Wait => AppLoopControl::WaitUntil(automation_deadline),
            AppLoopControl::WaitUntil(deadline) => {
                AppLoopControl::WaitUntil(deadline.min(automation_deadline))
            }
            control => control,
        }
    }

    fn update_tab_drag_autoscroll(&mut self, dt: f32) -> bool {
        let Some(drag) = self.terminal_tab_drag.as_mut() else {
            return false;
        };
        if !drag.threshold_passed {
            return false;
        }
        let Some(runtime) = self.runtime.as_ref() else {
            return false;
        };
        let strip = runtime.terminal_tab_strip_layout();
        if strip.max_scroll <= 0.0 || strip.rect.w <= 0.0 {
            return false;
        }
        let edge = (DRAG_AUTOSCROLL_EDGE_PX * runtime.scale_factor()).max(28.0);
        let delta = drag_autoscroll_delta(
            self.pointer_x,
            strip.rect.x,
            strip.rect.x + strip.rect.w,
            edge,
        );
        if delta == 0.0 {
            return false;
        }
        let old = self.terminal_tab_scroll.current;
        let new =
            (old + delta.signum() * drag_autoscroll_speed(delta) * dt).clamp(0.0, strip.max_scroll);
        let scroll_delta = new - old;
        if scroll_delta == 0.0 {
            return false;
        }
        self.terminal_tab_scroll.current = new;
        self.terminal_tab_scroll.target = new;
        drag.start_x -= scroll_delta;
        true
    }

    fn handle_global_key(&mut self, shortcut: GlobalShortcut) -> bool {
        match shortcut {
            GlobalShortcut::ToggleSettings => self.toggle_settings(),
            GlobalShortcut::ToggleFps => self.toggle_fps(),
            GlobalShortcut::NewTab => {
                let _ = self.add_terminal();
            }
            GlobalShortcut::CloseTab => {
                // The caller exits the event loop when this was the final tab.
                return self.close_active_terminal();
            }
            GlobalShortcut::Search => {
                let active = self.active_terminal;
                if let Some(terminal) = self.terminals.get_mut(active) {
                    self.interaction.open_search(terminal);
                    self.request_frame();
                }
            }
            GlobalShortcut::ToggleClaudeGrid => self.toggle_claude_grid(Instant::now()),
        }
        false
    }

    /// App-exit kill barrier: every tab session (open, pending, and already in
    /// the cleanup worker) gets the same bounded SIGTERM grace concurrently and
    /// is SIGKILLed and reaped before this returns, so no tab process outlives
    /// Ronsole.
    fn shutdown_all(&mut self) {
        let mut processes: Vec<TerminalProcess> = self
            .terminals
            .iter_mut()
            .filter_map(Terminal::take_process_for_cleanup)
            .collect();
        processes.extend(self.pending_terminal_cleanup.drain(..));
        shutdown_terminal_processes(&mut processes);
        drop(processes);
        self.terminals.clear();
        self.terminal_cleanup.shutdown_and_join();
        if let Some(worker) = self.kde_activation_worker.as_mut() {
            worker.shutdown_and_join();
        }
        self.kde_activation_worker = None;
    }

    fn remove_closed_terminals(&mut self) -> bool {
        let mut index = self.terminals.len();
        while index > 0 {
            index -= 1;
            if let Some(process) = self.terminals[index].take_finished_held_process_for_cleanup() {
                if let Err(process) = self.try_schedule_terminal_cleanup(process) {
                    self.terminals[index].restore_process_after_cleanup_backpressure(process);
                }
                continue;
            }
            if self.terminals[index].is_closed() && self.close_terminal_tab_at(index) {
                return true;
            }
        }
        false
    }
}

impl App {
    fn on_runtime_ready(
        &mut self,
        runtime: WindowRuntime,
        width: u32,
        height: u32,
        initial_launch: TerminalLaunchSpec,
    ) {
        self.zero_sized = width == 0 || height == 0;
        if std::env::var_os("RONSOLE_GL_DIAGNOSTICS")
            .is_some_and(|value| value != std::ffi::OsStr::new("0"))
        {
            eprintln!("Ronsole graphics:\n{}", runtime.diagnostics_report());
        }
        self.runtime = Some(runtime);
        self.refresh_grid_metrics();
        self.last_frame = Instant::now();
        let _ = self.add_terminal_with_launch(initial_launch);
        self.flush_pending_external_launches();
        self.mark_dirty();
    }

    /// Compositor close request. Returns true when the event loop must exit.
    fn on_close_requested(&mut self) -> bool {
        self.request_terminal_close(CloseTarget::Window)
    }

    /// Persists config and reaps every owned PTY session. Called directly by the
    /// window-close path and the signal shutdown path, so a future close prompt
    /// stays on the close request only and never blocks a signal.
    fn finish_terminal_session(&mut self) {
        self.prepare_exit();
        self.shutdown_all();
    }

    /// The only writer of `grid_metrics`. A change while grid mode is open
    /// starts a new generation at once, so input that arrives before the next
    /// frame is not matched against the old tile geometry.
    fn set_grid_metrics(&mut self, metrics: GridMetrics) {
        if self.claude_grid.is_some() && metrics != self.grid_metrics {
            self.bump_grid_generation();
        }
        self.grid_metrics = metrics;
    }

    /// Reads window size, scale and cell size from the runtime. Without a runtime
    /// the stored metrics are left unchanged.
    fn refresh_grid_metrics(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        let (width, height) = runtime.window_size();
        let (char_w, char_h) = runtime.terminal_cell_size();
        let metrics = GridMetrics {
            width,
            height,
            scale: runtime.scale_factor(),
            char_w,
            char_h,
        };
        self.set_grid_metrics(metrics);
    }

    /// Ordinary mode: one panel, the active terminal filling the window body.
    /// Grid mode: one tile per member, in tab order.
    fn rebuild_panels(&mut self) {
        let metrics = self.grid_metrics;
        let body = terminal_body_rect(metrics.width, metrics.height, metrics.scale);
        self.panels.clear();
        let Some(grid) = self.claude_grid.as_ref() else {
            self.panels.push(Panel {
                terminal: self.active_terminal,
                body,
                header: None,
                content_hidden: false,
            });
            return;
        };
        grid_tiles(body, grid.members.len(), metrics.scale, &mut self.grid_tile_rects);
        for (&terminal, &(header, tile_body)) in grid.members.iter().zip(&self.grid_tile_rects) {
            self.panels.push(Panel {
                terminal,
                body: tile_body,
                header: Some(header),
                content_hidden: false,
            });
        }
    }

    /// Whether grid mode is open.
    pub(crate) fn grid_open(&self) -> bool {
        self.claude_grid.is_some()
    }

    /// Number of tiles while grid mode is open; zero otherwise.
    pub(crate) fn grid_tile_count(&self) -> usize {
        self.claude_grid
            .as_ref()
            .map_or(0, |grid| grid.members.len())
    }

    /// Tab-strip hint that is still due at `now`.
    fn tab_strip_hint(&self, now: Instant) -> Option<&'static str> {
        self.grid_hint
            .filter(|(_, deadline)| *deadline > now)
            .map(|(hint, _)| hint.text())
    }

    /// Clears the hint once its deadline has passed and asks for one frame to remove it.
    fn advance_grid_hint(&mut self, now: Instant) {
        if let Some((_, deadline)) = self.grid_hint
            && deadline <= now
        {
            self.grid_hint = None;
            self.request_frame();
        }
    }

    fn set_grid_hint(&mut self, hint: GridHint, now: Instant) {
        self.grid_hint = Some((hint, now + GRID_HINT_DURATION));
        self.request_frame();
    }

    /// Starts a new panel generation and drops the pointer capture of the
    /// active terminal, so a press from before the change is not finished in a
    /// tile whose geometry has moved.
    fn bump_grid_generation(&mut self) {
        self.grid_generation = self.grid_generation.wrapping_add(1);
        if let Some(terminal) = self.terminals.get_mut(self.active_terminal) {
            self.interaction.cancel_pointer_interaction(terminal);
        }
    }

    /// Fills `grid_members_scratch` with the tabs whose foreground is Claude Code.
    fn collect_claude_members(&mut self) {
        self.grid_members_scratch.clear();
        for (index, terminal) in self.terminals.iter().enumerate() {
            if terminal.claude_foreground() {
                self.grid_members_scratch.push(index);
            }
        }
    }

    /// Makes the collected scratch list the grid's members. The previous list
    /// becomes the scratch buffer, so both allocations are reused.
    fn adopt_grid_members(&mut self) {
        let grid = self.claude_grid.get_or_insert_with(ClaudeGrid::default);
        std::mem::swap(&mut grid.members, &mut self.grid_members_scratch);
    }

    fn grid_contains(&self, index: usize) -> bool {
        self.claude_grid
            .as_ref()
            .is_some_and(|grid| grid.members.contains(&index))
    }

    /// Whether the tab at `index` is on screen: a tile while grid mode is open,
    /// otherwise the presented active tab.
    fn tab_visible(&self, index: usize) -> bool {
        if self.claude_grid.is_some() {
            self.grid_contains(index)
        } else {
            index == self.active_terminal && self.active_terminal_presented
        }
    }

    /// Tile that takes focus when the tab at `index` closes: the next member
    /// in tab order, else the previous one. Returns its index after `index`
    /// is removed. `None` outside grid mode or when no other tile exists.
    fn grid_successor_after_removal(&self, index: usize) -> Option<usize> {
        let grid = self.claude_grid.as_ref()?;
        let target = grid
            .members
            .iter()
            .copied()
            .find(|&member| member > index)
            .or_else(|| grid.members.iter().rev().copied().find(|&member| member < index))?;
        Some(if target > index { target - 1 } else { target })
    }

    /// Whether the current members fit the window body at the current metrics (Q9).
    fn grid_fits_current(&self, count: usize) -> bool {
        let metrics = self.grid_metrics;
        let body = terminal_body_rect(metrics.width, metrics.height, metrics.scale);
        grid_fits(body, count, metrics.scale, metrics.char_w, metrics.char_h)
    }

    /// Ctrl+Shift+S. Opens grid mode over the Claude tabs, or leaves it onto the
    /// focused tile.
    fn toggle_claude_grid(&mut self, now: Instant) {
        if self.claude_grid.is_some() {
            self.exit_claude_grid(Some(self.active_terminal));
            return;
        }
        self.collect_claude_members();
        let count = self.grid_members_scratch.len();
        if count == 0 {
            self.set_grid_hint(GridHint::NoClaudeTabs, now);
            return;
        }
        if !self.grid_fits_current(count) {
            self.set_grid_hint(GridHint::TooManyTabs, now);
            return;
        }
        // Focus: the active tab when it is a member, otherwise the first tile.
        let active = self.active_terminal;
        let focus = if self.grid_members_scratch.contains(&active) {
            active
        } else {
            self.grid_members_scratch.first().copied().unwrap_or(active)
        };
        self.cancel_terminal_presentation_intents();
        self.adopt_grid_members();
        self.bump_grid_generation();
        self.activate_ready_terminal(focus, false);
        self.request_frame();
    }

    /// Leaves grid mode. `target` becomes the active tab (plain activation,
    /// as a tab click); `None` keeps the active tab. Does nothing when grid mode
    /// is closed.
    fn exit_claude_grid(&mut self, target: Option<usize>) {
        if self.claude_grid.take().is_none() {
            return;
        }
        self.bump_grid_generation();
        if let Some(index) = target
            && index != self.active_terminal
        {
            self.cancel_terminal_presentation_intents();
            self.activate_ready_terminal(index, false);
        }
        self.request_frame();
    }

    /// Brings the grid in line with the Claude flags of the tabs. Does nothing
    /// while grid mode is closed. Membership changes start a new generation;
    /// when the focused tile loses its flag, focus moves to the next member
    /// (or the previous one); when the members stop fitting (Q9), grid mode
    /// closes onto the focused tile with a hint.
    fn sync_claude_grid(&mut self, now: Instant) {
        if self.claude_grid.is_none() {
            return;
        }
        self.collect_claude_members();
        let members_changed = self
            .claude_grid
            .as_ref()
            .is_some_and(|grid| grid.members != self.grid_members_scratch);
        if members_changed {
            self.bump_grid_generation();
            self.adopt_grid_members();
            self.request_frame();
        }
        let Some(grid) = self.claude_grid.as_ref() else {
            return;
        };
        if grid.members.is_empty() {
            self.exit_claude_grid(None);
            return;
        }
        let active = self.active_terminal;
        if !self.grid_contains(active) {
            // Members are tab indices in ascending order: the next one after the
            // focused tab, else the last one (all of them lie before it).
            let target = grid
                .members
                .iter()
                .copied()
                .find(|&index| index > active)
                .or_else(|| grid.members.last().copied());
            if let Some(target) = target {
                self.cancel_terminal_presentation_intents();
                self.activate_ready_terminal(target, false);
                self.request_frame();
            }
        }
        let count = self
            .claude_grid
            .as_ref()
            .map_or(0, |grid| grid.members.len());
        if !self.grid_fits_current(count) {
            self.exit_claude_grid(Some(self.active_terminal));
            self.set_grid_hint(GridHint::TooManyTabs, now);
        }
    }

    pub(crate) fn on_resize_with_logical_size(
        &mut self,
        width: u32,
        height: u32,
        logical_size: Option<(f64, f64)>,
    ) {
        self.zero_sized = width == 0 || height == 0;
        self.suspend_frame_clock();
        if self.zero_sized {
            return;
        }
        if let Some((logical_width, logical_height)) = logical_size
            && self.config.set_window_size(logical_width, logical_height)
        {
            self.mark_config_dirty();
        }
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.resize(width, height);
        }
        self.refresh_grid_metrics();
        self.mark_dirty();
    }

    pub(crate) fn on_scale_changed(&mut self, scale_factor: f32, width: u32, height: u32) {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.update_scale_factor(scale_factor);
            if width > 0 && height > 0 {
                runtime.resize(width, height);
            }
        }
        self.refresh_grid_metrics();
        self.terminal_tab_scroll.reset();
        self.mark_dirty();
    }

    pub(crate) fn on_modifiers(&mut self, modifiers: Modifiers) {
        self.modifiers = modifiers;
        self.interaction.set_modifiers(modifiers);
        self.update_link_hover();
    }

    pub(crate) fn on_ime_commit(&mut self, text: &str) {
        if self.close_confirm.is_some() {
            return;
        }
        if self.settings_modal_active() {
            if self.settings_background_editing {
                let _ = self.edit_settings_background_text(text);
            }
            return;
        }
        let active = self.active_terminal;
        if let Some(terminal) = self.terminals.get_mut(active)
            && self.interaction.handle_ime_commit(text, terminal)
        {
            self.sync_animation_state();
            self.request_frame();
        }
    }

    pub(crate) fn on_key(&mut self, key_input: KeyInput) -> bool {
        // Before the early returns below, so modifier presses and releases seen while the
        // settings modal or a global shortcut owns the key still update the held state.
        self.interaction.note_modifier_key(&key_input);
        if self.close_confirm.is_some() {
            if key_input.state.is_pressed() {
                match key_input.physical_key {
                    PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter) => {
                        return self.confirm_close_dialog();
                    }
                    PhysicalKey::Code(KeyCode::Escape) => self.cancel_close_dialog(),
                    _ => {}
                }
            }
            return false;
        }
        if key_input.state.is_pressed()
            && let Some(shortcut) = global_shortcut(key_input.physical_key, self.modifiers)
        {
            if self.settings_modal_active() && !global_shortcut_allowed_in_settings(shortcut) {
                return false;
            }
            if self.handle_global_key(shortcut) {
                return true;
            }
            self.sync_animation_state();
            return false;
        }
        if self.settings_modal_active() {
            if key_input.state.is_pressed()
                && key_input.physical_key == PhysicalKey::Code(KeyCode::Escape)
            {
                self.close_settings();
            } else if key_input.state.is_pressed() && self.settings_background_editing {
                let _ = self.handle_settings_background_key(key_input.physical_key);
            }
            // Text of the key follows the key, as the former separate text event did.
            if let Some(text) = key_input.text.as_deref() {
                self.on_text(text);
            }
            return false;
        }
        let active = self.active_terminal;
        if let Some(terminal) = self.terminals.get_mut(active)
            && self.interaction.handle_key_event(&key_input, terminal)
        {
            self.sync_animation_state();
            self.request_frame();
        }
        false
    }

    fn on_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.settings_modal_active() {
            if self.settings_background_editing
                && !self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && !self.modifiers.super_key()
            {
                let _ = self.edit_settings_background_text(text);
            }
            return;
        }
        let active = self.active_terminal;
        if let Some(terminal) = self.terminals.get_mut(active)
            && self.interaction.handle_text(text, terminal)
        {
            self.sync_animation_state();
            self.request_frame();
        }
    }

    pub(crate) fn on_pointer_motion(&mut self, position: PointerPosition) {
        let previous_x = self.pointer_x;
        let previous_y = self.pointer_y;
        self.pointer_x = position.x;
        self.pointer_y = position.y;
        self.update_link_hover();
        // The dialog owns the pointer: tab drag and reorder must not advance under it.
        if self.close_confirm.is_some() {
            return;
        }
        if self.settings_modal_active() {
            let previous_hit = self.settings_hit_test(previous_x, previous_y);
            let current_hit = self.settings_hit_test(self.pointer_x, self.pointer_y);
            let mut changed = previous_hit != current_hit;
            if self.settings_background_dragging
                && let Some(cursor) = self.settings_background_cursor_from_x(self.pointer_x)
            {
                changed |= update_settings_background_pointer_selection(
                    &mut self.settings_background_input,
                    cursor,
                );
            }
            self.refresh_cursor_icon();
            if changed {
                self.request_frame();
            }
            return;
        }
        let search_hover_changed = self.search_hit_test(previous_x, previous_y)
            != self.search_hit_test(self.pointer_x, self.pointer_y);
        self.refresh_cursor_icon();
        if search_hover_changed {
            self.request_frame();
        }
        if let Some(drag) = self.terminal_tab_drag.as_mut() {
            drag.current_x = self.pointer_x;
            if !drag.threshold_passed {
                let scale = self
                    .runtime
                    .as_ref()
                    .map_or(1.0, WindowRuntime::scale_factor);
                drag.threshold_passed = drag_threshold_passed(drag.start_x, drag.current_x, scale);
            }
            self.sync_animation_state();
            self.request_frame();
            return;
        }
        if !self.interaction.terminal_selection_active()
            && self.runtime.as_ref().is_some_and(|runtime| {
                runtime
                    .terminal_tab_strip_layout()
                    .rect
                    .contains(self.pointer_x, self.pointer_y)
            })
        {
            self.request_frame();
            return;
        }
        let active = self.active_terminal;
        if let (Some(runtime), Some(terminal)) =
            (self.runtime.as_mut(), self.terminals.get_mut(active))
        {
            let changed = self.interaction.cursor_moved(
                self.pointer_x,
                self.pointer_y,
                terminal,
                |text, x, scroll| runtime.terminal_search_cursor_from_x(text, x, scroll),
            );
            if changed {
                self.sync_animation_state();
                self.request_frame();
            }
        }
    }

    pub(crate) fn on_pointer_leave(&mut self, width: u32, height: u32) {
        if let Some(next) = cursor_icon_change(self.cursor_icon, CursorKind::Default) {
            self.cursor_icon = next;
            if let Some(runtime) = self.runtime.as_ref() {
                runtime.set_cursor_kind(next);
            }
        }
        if self.settings_modal_active() || self.terminal_tab_drag.is_some() {
            return;
        }
        let active = self.active_terminal;
        if let Some(terminal) = self.terminals.get_mut(active)
            && self
                .interaction
                .cursor_left(width as f32, height as f32, terminal)
        {
            self.sync_animation_state();
            self.request_frame();
        }
    }

    pub(crate) fn on_pointer_button(&mut self, state: KeyState, button: PointerButton) -> bool {
        if self.close_confirm.is_some() {
            if button == PointerButton::Left && state == KeyState::Pressed {
                match self.close_confirm_hit_test(self.pointer_x, self.pointer_y) {
                    CloseConfirmHit::Confirm => return self.confirm_close_dialog(),
                    CloseConfirmHit::Cancel | CloseConfirmHit::Outside => {
                        self.cancel_close_dialog();
                    }
                    CloseConfirmHit::None => {}
                }
            }
            return false;
        }
        if button == PointerButton::Left
            && state == KeyState::Pressed
            && self.clear_active_terminal_text_selection()
        {
            self.request_frame();
        }
        if self.settings_modal_active() {
            if button == PointerButton::Left {
                if state == KeyState::Pressed {
                    let hit = self.settings_hit_test(self.pointer_x, self.pointer_y);
                    if hit == SettingsHit::BackgroundField {
                        let cursor = self
                            .settings_background_cursor_from_x(self.pointer_x)
                            .unwrap_or(self.settings_background_input.cursor);
                        self.begin_settings_background_pointer_edit(cursor);
                    } else {
                        self.settings_background_dragging = false;
                        self.apply_settings_hit(hit);
                    }
                    self.refresh_cursor_icon();
                } else if self.settings_background_dragging {
                    self.settings_background_dragging = false;
                    self.refresh_cursor_icon();
                    self.request_frame();
                }
            }
            return false;
        }
        if button == PointerButton::Left && state == KeyState::Pressed {
            let hit = self
                .runtime
                .as_ref()
                .map_or(TerminalTabHit::None, |runtime| {
                    runtime.terminal_tab_hit_test(self.pointer_x, self.pointer_y)
                });
            match hit {
                TerminalTabHit::Close(index) => {
                    return self.request_terminal_close(CloseTarget::Tab(index));
                }
                TerminalTabHit::Add => {
                    let _ = self.add_terminal();
                    return false;
                }
                TerminalTabHit::Body(index) => {
                    self.terminal_tab_drag = Some(TabDragState {
                        start_idx: index,
                        start_x: self.pointer_x,
                        current_x: self.pointer_x,
                        threshold_passed: false,
                    });
                    self.select_terminal_tab_from_user(index);
                    self.request_frame();
                    return false;
                }
                TerminalTabHit::None => {}
            }
        }
        if button == PointerButton::Left
            && state == KeyState::Released
            && let Some(drag) = self.terminal_tab_drag.take()
        {
            if drag.threshold_passed
                && let Some(destination) = self
                    .runtime
                    .as_ref()
                    .and_then(|runtime| runtime.terminal_tab_drag_destination(&drag))
            {
                self.reorder_terminal_tab(drag.start_idx, destination);
            }
            self.sync_animation_state();
            self.request_frame();
            return false;
        }
        let active = self.active_terminal;
        if let (Some(runtime), Some(terminal)) =
            (self.runtime.as_mut(), self.terminals.get_mut(active))
        {
            let handled =
                self.interaction
                    .mouse_input(state, button, terminal, |text, x, scroll| {
                        runtime.terminal_search_cursor_from_x(text, x, scroll)
                    });
            if handled {
                self.sync_animation_state();
                self.request_frame();
            }
        }
        if let Some(uri) = self.interaction.take_link_uri()
            && open_uri_allowed(&uri, crate::terminal::local_host_name())
        {
            open_uri_with(&uri, self.open_sink.clone());
        }
        false
    }

    pub(crate) fn on_scroll(&mut self, delta: ScrollDelta) {
        if self.close_confirm.is_some() || self.settings_modal_active() {
            // The Colors tab may hold more rows than a small window shows.
            if self.close_confirm.is_none()
                && self.settings_open
                && self.settings_active_tab == SettingsTab::Colors
                && let Some(runtime) = self.runtime.as_ref()
                && runtime
                    .renderer()
                    .scroll_settings_colors(self.settings_progress, delta)
            {
                // A field scrolled out of view must not keep taking keystrokes.
                if self.settings_color_target.is_some() {
                    self.finish_settings_background_edit();
                }
                self.refresh_cursor_icon();
                self.request_frame();
            }
            return;
        }
        if let Some(runtime) = self.runtime.as_ref() {
            let strip = runtime.terminal_tab_strip_layout();
            if strip.rect.contains(self.pointer_x, self.pointer_y) {
                self.terminal_tab_scroll
                    .scroll_by(tab_wheel_delta(delta, runtime.scale_factor()));
                self.terminal_tab_scroll.clamp_target(0.0, strip.max_scroll);
                self.sync_animation_state();
                self.request_frame();
                return;
            }
        }
        let active = self.active_terminal;
        if let Some(terminal) = self.terminals.get_mut(active)
            && self.interaction.mouse_wheel(delta, terminal, self.interaction.layout)
        {
            self.sync_animation_state();
            self.request_frame();
        }
    }

    fn on_frame(&mut self) -> Result<bool, String> {
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.acknowledge_wake();
        }
        if !self.renderable() {
            return Ok(false);
        }

        let _ = self.process_terminal_presentation_intents();
        let now = Instant::now();
        if self.withhold_active_frame(now) {
            return Ok(false);
        }
        let dt = animation_dt((now - self.last_frame).as_secs_f32());
        self.last_frame = now;

        if self.update_tab_drag_autoscroll(dt) {
            self.mark_dirty();
        }
        self.terminal_tab_scroll.update(dt);
        if let Some(runtime) = self.runtime.as_ref() {
            let max = runtime.terminal_tab_strip_layout().max_scroll;
            self.terminal_tab_scroll.clamp_target(0.0, max);
            self.terminal_tab_scroll.clamp_current(0.0, max);
        }
        let active = self.active_terminal;
        let mut selection_autoscrolled = false;
        if let Some(terminal) = self.terminals.get_mut(active) {
            selection_autoscrolled = self.interaction.update_selection_autoscroll(dt, terminal);
            terminal.scroll_y.update(dt);
            terminal
                .scroll_y
                .clamp_target(0.0, self.interaction.layout.max_scroll);
            terminal
                .scroll_y
                .clamp_current(0.0, self.interaction.layout.max_scroll);
        }
        if selection_autoscrolled {
            self.mark_dirty();
        }
        let (settings_progress, _) =
            settings_animation_step(self.settings_progress, self.settings_open, dt);
        self.settings_progress = settings_progress;

        self.rebuild_panels();
        let tab_strip_hint = self.tab_strip_hint(now);
        let render_result = if let Some(runtime) = self.runtime.as_mut() {
            Some(runtime.render_terminal_and_present(TerminalRenderParams {
                terminals: &self.terminals,
                active_terminal: self.active_terminal,
                panels: &self.panels,
                panel_layouts: &mut self.panel_layouts,
                tab_strip_hint,
                search: &mut self.interaction.search,
                focused: self.focused,
                tab_scroll_x: self.terminal_tab_scroll.current,
                drag: self.terminal_tab_drag.as_ref(),
                pointer_x: self.pointer_x,
                pointer_y: self.pointer_y,
                settings_progress: self.settings_progress,
                settings_tab: self.settings_active_tab,
                show_fps: self.show_fps,
                settings_font_value: &self.settings_font_value,
                settings_scroll_value: &self.settings_scroll_value,
                settings_background_input: &mut self.settings_background_input,
                settings_background_editing: self.settings_background_editing
                    && self.settings_color_target.is_none(),
                settings_confirm_close: self.config.confirm_close,
                settings_colors: &self.settings_colors,
                settings_color_editing: self.settings_color_target,
                settings_color_input: &self.settings_color_input,
                close_confirm: self.close_confirm.as_ref().map(|confirm| CloseConfirmView {
                    title: confirm.title,
                    rows: &confirm.rows,
                    more: confirm.more.as_deref(),
                }),
            }))
        } else {
            None
        };
        match render_result {
            Some(Ok(layout)) => {
                self.panel_layouts_generation = self.grid_generation;
                self.interaction.update_layout(layout);
                let active = self.active_terminal;
                if let Some(terminal) = self.terminals.get_mut(active) {
                    terminal.scroll_y.clamp_target(0.0, layout.max_scroll);
                    terminal.scroll_y.clamp_current(0.0, layout.max_scroll);
                }
                if let Some(runtime) = self.runtime.as_ref() {
                    let max = runtime.terminal_tab_strip_layout().max_scroll;
                    self.terminal_tab_scroll.clamp_target(0.0, max);
                    self.terminal_tab_scroll.clamp_current(0.0, max);
                    if let Some(reveal_tail) = self.pending_tab_reveal.take() {
                        let target = runtime.terminal_tab_reveal_target(
                            self.active_terminal,
                            reveal_tail,
                            self.terminal_tab_scroll.target,
                        );
                        self.terminal_tab_scroll.animate_to(target);
                    }
                }
                self.refresh_cursor_icon();
                self.sync_animation_state();
                if !self.focused {
                    self.unfocused_redraw_pending = false;
                }
                self.dirty = false;
            }
            Some(Err(error)) => return Err(error),
            None => {}
        }
        Ok(true)
    }

    /// Applies host events parked by each tab's parser thread and, as a
    /// fallback for a stalled parser thread, replays synchronized-output
    /// buffers that are overdue by `SYNC_RESERVE_GRACE`, when the locks are
    /// free. Takes its own clock so the about-to-wait body keeps its clock
    /// after the early return.
    fn drain_terminal_host_events(&mut self) {
        let now = Instant::now();
        let active = self.active_terminal;
        // Read before the loop: `tab_visible` cannot be called while `terminals` is borrowed.
        let grid_members = self.claude_grid.as_ref().map(|grid| grid.members.as_slice());
        let mut bell_rang = false;
        for (index, terminal) in self.terminals.iter_mut().enumerate() {
            // Only the pending-events lock is held while clipboard text is set;
            // the published deadline is read from the same pending state.
            if terminal.apply_host_events(&mut self.interaction) {
                bell_rang = true;
                // Tiles of an open grid and the active tab are on screen; only
                // other tabs get a mark.
                let on_grid_tile = grid_members.is_some_and(|members| members.contains(&index));
                if !on_grid_tile && index != active {
                    terminal.mark_bell_pending();
                }
            }
            if terminal
                .sync_deadline()
                .is_some_and(|deadline| deadline + SYNC_RESERVE_GRACE <= now)
            {
                terminal.try_flush_expired(now);
            }
        }
        if bell_rang {
            self.request_bell_attention();
            self.mark_dirty();
        }
    }

    /// Asks for window attention after a BEL, at most once per unfocused
    /// period. A focused window shows the bell in the tab strip only.
    fn request_bell_attention(&mut self) {
        if self.focused || self.window_attention_requested {
            return;
        }
        // Set only once the compositor request is in flight, so a window that
        // cannot be flagged retries on the next bell.
        self.window_attention_requested = self
            .runtime
            .as_mut()
            .is_some_and(WindowRuntime::request_window_attention);
    }

    fn on_about_to_wait(&mut self) -> AppLoopControl {
        self.drain_terminal_host_events();
        self.advance_dev_channel_warnings(Instant::now());
        self.advance_bridge_launches(Instant::now());
        self.advance_due_tab_deliveries();
        self.flush_pending_terminal_cleanup();
        if self.remove_closed_terminals() {
            self.interrupt_automation("terminal process closed the final terminal");
            self.prepare_exit();
            return AppLoopControl::Exit;
        }
        if self.process_terminal_presentation_intents() {
            self.request_frame();
        }
        self.advance_grid_hint(Instant::now());
        self.sync_claude_grid(Instant::now());
        if !self.renderable() {
            self.suspend_frame_clock();
        }
        self.sync_animation_state();
        if self.automation.is_none() {
            return self.loop_control();
        }
        let now = Instant::now();
        match self.advance_automation(now) {
            Some(automation::AutomationTick::Running { deadline }) => {
                self.loop_control_with_automation_deadline(deadline)
            }
            Some(automation::AutomationTick::ExitOk | automation::AutomationTick::ExitFailed) => {
                self.prepare_exit();
                AppLoopControl::Exit
            }
            None => self.loop_control(),
        }
    }

    fn on_exiting(&mut self) {
        self.persist_config_if_needed();
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge_call(
        app: &mut App,
        op: u8,
        id: [u8; 16],
        hash: Option<[u8; 32]>,
    ) -> crate::platform::single_instance::BridgeResult {
        bridge_call_as(app, op, id, hash, crate::platform::single_instance::effective_uid())
    }

    fn bridge_call_as(
        app: &mut App,
        op: u8,
        id: [u8; 16],
        hash: Option<[u8; 32]>,
        uid: u32,
    ) -> crate::platform::single_instance::BridgeResult {
        let (reply, response) = std::sync::mpsc::channel();
        app.handle_bridge_request(crate::platform::single_instance::BridgeRequestEnvelope {
            request: crate::platform::single_instance::BridgeRequest {
                op,
                bridge_launch_id: id,
                screen_hash: hash,
                claude_process: None,
            },
            peer: crate::platform::single_instance::PeerCred { uid, pid: 7 },
            reply,
        });
        response.recv().unwrap()
    }

    #[test]
    fn bridge_snapshot_confirm_forget_and_missing_tab_behave_as_specified() {
        let id = [9; 16];
        let terminal = Terminal::new_bridge_for_test(120, 8, 1, id);
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        crate::platform::lock_recover(&terminal.grid).advance(
            b"ordinary screen without bridge warning",
            Instant::now(),
            &mut sink,
        );
        let mut app = App::new();
        app.terminals.push(terminal);

        assert_eq!(bridge_call_as(&mut app, 0, id, None, u32::MAX).code, 4);
        let snapshot = bridge_call(&mut app, 0, id, None);
        assert_eq!(snapshot.code, 0);
        let (screen, hash) = snapshot.screen.unwrap();
        assert_eq!(hash, crate::terminal::screen_hash(&screen));
        assert_eq!(bridge_call(&mut app, 1, id, Some([0; 32])).code, 1);
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
        assert_eq!(bridge_call(&mut app, 1, id, Some(hash)).code, 0);
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\r");
        assert_eq!(bridge_call(&mut app, 1, id, Some(hash)).code, 2);
        assert_eq!(bridge_call(&mut app, 2, id, None).code, 0);
        assert_eq!(bridge_call(&mut app, 0, id, None).code, 3);

        let mut ordinary = App::new();
        ordinary.terminals.push(Terminal::new_for_test(80, 24, 2));
        assert_eq!(bridge_call(&mut ordinary, 0, id, None).code, 3);
    }

    #[test]
    fn bridge_manual_confirm_after_auto_confirm_returns_already_confirmed() {
        let id = [10; 16];
        let terminal = Terminal::new_bridge_for_test(120, 8, 1, id);
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        crate::platform::lock_recover(&terminal.grid).advance(
            b"WARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n\xe2\x9d\xaf 1. I am using this for local development",
            Instant::now(),
            &mut sink,
        );
        let mut app = App::new();
        app.terminals.push(terminal);
        assert!(app.terminals[0].maybe_auto_confirm_bridge(std::time::Instant::now()));
        let screen = app.terminals[0].screen_tail_text(usize::MAX).join("\n");

        assert_eq!(bridge_call(&mut app, 1, id, Some(screen_hash(&screen))).code, 2);
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\r");
    }

    /// Renders `app.panels` through the runtime render path. `on_frame` rebuilds the panel
    /// list from the window body, so panel-list tests call the runtime with the same
    /// arguments `on_frame` passes, but with `app.panels` as set by the test.
    fn render_test_panels(app: &mut App) -> Vec<TerminalUiLayout> {
        let color_view = SettingsColorView::new(&crate::config::AppConfig::default());
        let mut background_input = SingleLineInput::default();
        let color_input = SingleLineInput::default();
        let mut panel_layouts = Vec::new();
        let runtime = app.runtime.as_mut().expect("headless runtime");
        runtime
            .render_terminal_and_present(TerminalRenderParams {
                terminals: &app.terminals,
                active_terminal: app.active_terminal,
                panels: &app.panels,
                panel_layouts: &mut panel_layouts,
                tab_strip_hint: None,
                search: &mut app.interaction.search,
                focused: true,
                show_fps: false,
                tab_scroll_x: 0.0,
                drag: None,
                pointer_x: 0.0,
                pointer_y: 0.0,
                settings_progress: 0.0,
                settings_tab: Default::default(),
                settings_font_value: "",
                settings_scroll_value: "",
                settings_background_input: &mut background_input,
                settings_background_editing: false,
                settings_confirm_close: true,
                settings_colors: &color_view,
                settings_color_editing: None,
                settings_color_input: &color_input,
                close_confirm: None,
            })
            .expect("panel render");
        panel_layouts
    }

    #[test]
    fn panels_render_two_bodies_with_search_only_on_active() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "panels-two",
                2,
            );
        let metrics = app.grid_metrics;
        let body = terminal_body_rect(metrics.width, metrics.height, metrics.scale);
        let half = body.w * 0.5;
        let left = crate::search::Rect { x: body.x, y: body.y, w: half, h: body.h };
        let right = crate::search::Rect {
            x: body.x + half,
            y: body.y,
            w: body.w - half,
            h: body.h,
        };
        app.panels = vec![
            Panel { terminal: 0, body: left, header: None, content_hidden: false },
            Panel { terminal: 1, body: right, header: None, content_hidden: false },
        ];
        app.interaction.search.open();
        // Changing the inactive grid after the search opened must not trigger its refresh.
        feed_batch(&app.terminals[1], "inactive output\r\n");
        let active_gen =
            crate::platform::lock_recover(&app.terminals[0].grid).content_generation();
        let inactive_gen =
            crate::platform::lock_recover(&app.terminals[1].grid).content_generation();

        let layouts = render_test_panels(&mut app);

        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].body, left);
        assert_eq!(layouts[1].body, right);
        let outer = layouts[0].search.expect("active panel draws its search").outer;
        assert!(outer.x >= left.x && outer.y >= left.y);
        assert!(outer.x + outer.w <= left.x + left.w && outer.y + outer.h <= left.y + left.h);
        assert!(layouts[1].search.is_none());
        // Only the active grid was scanned: the scan generation matches it, not the inactive one.
        let active_after =
            crate::platform::lock_recover(&app.terminals[0].grid).content_generation();
        let inactive_after =
            crate::platform::lock_recover(&app.terminals[1].grid).content_generation();
        let scanned = app.interaction.search.scanned_grid_generation_for_test();
        assert_eq!(
            scanned, active_after,
            "active gen before {active_gen} after {active_after}; inactive before {inactive_gen} after {inactive_after}"
        );
        assert_ne!(scanned, inactive_after);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn panels_normal_mode_layout_matches_window_body() {
        // Window literals of the headless fixture: `started_headless` in
        // src/app/headless_run.rs starts the runtime at (640, 400) with scale 1.0.
        const WIDTH: f32 = 640.0;
        const HEIGHT: f32 = 400.0;
        const SCALE: f32 = 1.0;
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "panels-normal",
                1,
            );
        // The test does not set `panels`: on_frame must build the single ordinary-mode panel.
        assert!(app.panels.is_empty());
        assert_eq!(app.on_frame(), Ok(true));

        let body = terminal_body_rect(WIDTH, HEIGHT, SCALE);
        assert_eq!(app.panels.len(), 1);
        assert_eq!(app.panels[0].body, body);
        assert_eq!(app.panel_layouts.len(), 1);
        assert_eq!(app.panel_layouts[0].body, app.panels[0].body);
        assert_eq!(app.interaction.layout.body, app.panels[0].body);

        // Expected grid size comes from terminal_grid_size, the window-body formula
        // pinned by terminal_grid_size_matches_current_body_formula in renderer/terminal_ui.rs.
        let metrics = app.grid_metrics;
        let expected =
            crate::renderer::terminal_grid_size(body, metrics.char_w, metrics.char_h, SCALE);
        assert_eq!(app.panel_layouts[0].cols, expected.cols);
        assert_eq!(app.panel_layouts[0].visible_rows, expected.rows);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn feed_batch(terminal: &Terminal, bytes: &str) {
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        grid.advance(bytes.as_bytes(), Instant::now(), &mut sink);
        grid.mark_changed();
    }

    #[test]
    fn dev_warning_withholds_only_the_active_tab_until_it_clears_or_times_out() {
        let warning = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
        let claude = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 7,
        };
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(120, 8, 1));
        app.terminals.push(Terminal::new_for_test(120, 8, 2));
        app.active_terminal = 0;
        feed_batch(&app.terminals[0], warning);
        let sent_at = Instant::now();
        let mut scratch = String::new();
        assert!(app.terminals[0].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));

        // Hidden tab: the frame is skipped without a pending redraw loop, and the
        // loop wakes at the safety limit.
        app.dirty = true;
        assert!(app.withhold_active_frame(sent_at));
        assert!(app.frame_withheld);
        assert!(!app.dirty);
        assert_eq!(
            app.loop_control(),
            AppLoopControl::WaitUntil(sent_at + crate::terminal::DEV_WARNING_HIDE_LIMIT)
        );
        // User input still reaches the tab.
        app.terminals[0].write_user_input(b"x");
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\rx");

        // Another active tab renders normally.
        app.active_terminal = 1;
        assert!(!app.withhold_active_frame(sent_at));
        app.active_terminal = 0;

        // Safety limit passed: the screen is shown as is.
        let late = sent_at + crate::terminal::DEV_WARNING_HIDE_LIMIT;
        assert!(!app.withhold_active_frame(late));
        app.frame_withheld = true;
        app.dirty = false;
        app.advance_dev_channel_warnings(late);
        assert!(!app.frame_withheld);
        assert!(app.dirty);

        // The first frame without the warning ends hiding and redraws.
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(120, 8, 1));
        feed_batch(&app.terminals[0], warning);
        assert!(app.terminals[0].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));
        assert!(app.withhold_active_frame(sent_at));
        feed_batch(&app.terminals[0], "\x1b[2J\x1b[H> ready");
        app.dirty = false;
        app.advance_dev_channel_warnings(sent_at);
        assert!(!app.frame_withheld);
        assert!(app.dirty);
        assert!(!app.withhold_active_frame(sent_at));
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\r");
    }

    #[test]
    fn bridge_forget_rejects_expired_state() {
        let id = [11; 16];
        let mut terminal = Terminal::new_bridge_for_test(80, 8, 1, id);
        terminal.bridge_launch_state_mut().unwrap().created_at =
            std::time::Instant::now() - crate::terminal::BRIDGE_LAUNCH_TTL;
        let mut app = App::new();
        app.terminals.push(terminal);

        assert_eq!(bridge_call(&mut app, 2, id, None).code, 3);
    }

    #[test]
    fn animation_dt_clamps_idle_gap_without_quantizing_high_refresh() {
        assert_eq!(animation_dt(0.5), 0.016);
        assert_eq!(animation_dt(1.0 / 60.0), 0.016);
        assert!((animation_dt(1.0 / 240.0) - 1.0 / 240.0).abs() < f32::EPSILON);
        assert_eq!(animation_dt(0.0), 0.0);
    }

    #[test]
    fn redraw_plan_sleeps_when_idle_or_not_renderable() {
        assert_eq!(
            frame_plan(true, false, false),
            FramePlan {
                request_frame: false,
                loop_mode: LoopMode::Wait,
            }
        );
        assert_eq!(
            frame_plan(false, true, true),
            FramePlan {
                request_frame: false,
                loop_mode: LoopMode::Wait,
            }
        );

        let mut app = App::new();
        app.dirty = false;
        assert!(app.automation.is_none());
        assert_eq!(app.loop_control(), AppLoopControl::Wait);
        assert_eq!(app.on_about_to_wait(), AppLoopControl::Wait);
        assert!(app.automation.is_none());

        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        let about_to_wait = production
            .split("    fn on_about_to_wait")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_exiting").next())
            .expect("about-to-wait routing must remain present");
        let guard = about_to_wait
            .find("if self.automation.is_none()")
            .expect("normal path must check automation first");
        let idle_return = about_to_wait
            .find("return self.loop_control();")
            .expect("disabled automation must return directly to normal loop control");
        let clock = about_to_wait
            .find("let now = Instant::now();")
            .expect("enabled automation must take its clock only after the guard");
        let advance = about_to_wait
            .find("self.advance_automation(now)")
            .expect("enabled automation must still advance");
        assert!(guard < idle_return);
        assert!(idle_return < clock);
        assert!(clock < advance);
    }

    #[test]
    fn redraw_plan_only_polls_for_live_animation() {
        assert_eq!(
            frame_plan(true, true, false),
            FramePlan {
                request_frame: true,
                loop_mode: LoopMode::Wait,
            }
        );
        assert_eq!(
            frame_plan(true, false, true),
            FramePlan {
                request_frame: true,
                loop_mode: LoopMode::Poll,
            }
        );
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "ronsole-pgo-about-to-wait-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("terminal_fixture.sh"), "#!/bin/sh\nexit 0\n").unwrap();

        let options = AutomationOptions {
            workspace: workspace.clone(),
            report: workspace.join("report.json"),
            timeout: Duration::from_secs(30),
        };
        let mut app = App::new();
        app.dirty = false;
        app.automation = Some(automation::AutomationController::new(options).unwrap());

        assert_eq!(app.loop_control(), AppLoopControl::Wait);
        assert!(matches!(
            app.on_about_to_wait(),
            AppLoopControl::WaitUntil(_)
        ));
        assert!(app.automation.is_some());

        drop(app);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn pending_activation_waits_for_parser_readiness_and_keeps_tail_intent() {
        use TerminalPresentationIntent as Intent;

        assert_eq!(
            ready_terminal_presentation([
                (0, Intent::None, true, false),
                (1, Intent::ActivateWhenReady, false, true),
            ]),
            None
        );
        assert_eq!(
            ready_terminal_presentation([
                (0, Intent::None, true, false),
                (1, Intent::ActivateWhenReady, true, true),
            ]),
            Some((1, true))
        );
        assert_eq!(
            ready_terminal_presentation([
                (0, Intent::None, true, false),
                (1, Intent::None, true, false),
                (2, Intent::ActivateWhenReady, true, false),
            ]),
            Some((2, false))
        );
    }

    #[test]
    fn fps_continuous_frame_reason_requires_focus_and_renderability() {
        assert!(continuous_frame_reason(true, true, false, true));
        assert!(continuous_frame_reason(true, true, true, false));
        assert!(!continuous_frame_reason(false, true, false, true));
        assert!(!continuous_frame_reason(true, false, false, true));
        assert!(!continuous_frame_reason(true, true, false, false));

        assert_eq!(
            frame_plan(
                true,
                false,
                continuous_frame_reason(true, true, false, true),
            ),
            FramePlan {
                request_frame: true,
                loop_mode: LoopMode::Poll,
            }
        );
        assert_eq!(
            frame_plan(
                false,
                false,
                continuous_frame_reason(true, false, false, true),
            ),
            FramePlan {
                request_frame: false,
                loop_mode: LoopMode::Wait,
            }
        );
    }

    #[test]
    fn pending_tab_switch_keeps_old_terminal_committed_until_new_terminal_is_ready() {
        let mut app = App::new();
        let old = Terminal::new_for_test(8, 2, 1);
        old.grid.lock().unwrap().mark_presentation_ready();
        let pending = Terminal::new_for_test(8, 2, 2);
        app.terminals.push(old);
        app.terminals.push(pending);
        app.active_terminal = 0;
        app.active_terminal_presented = true;

        app.request_terminal_activation(1, false);
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        assert_eq!(
            app.terminals[1].presentation_intent,
            TerminalPresentationIntent::ActivateWhenReady
        );

        app.terminals[1]
            .grid
            .lock()
            .unwrap()
            .mark_presentation_ready();
        assert!(app.process_terminal_presentation_intents());
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
    }

    /// Holds `bytes` in a synchronized-output block seen at `now`, as the
    /// parser thread does, without a PTY.
    fn hold_sync_block_at(terminal: &Terminal, bytes: &[u8], now: Instant) {
        terminal.hold_sync_block_for_test(bytes, now);
    }

    #[test]
    fn background_change_updates_live_and_new_tabs() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(8, 2, 1));
        assert!(app.set_terminal_background(RgbColor::new(0x11, 0x22, 0x33)));

        // Live tab: an OSC 11 colour query is answered with the new background.
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(8);
        {
            let mut grid = crate::platform::lock_recover(&app.terminals[0].grid);
            let mut sink = crate::terminal_process::SessionSink {
                reply_tx: Some(&reply_tx),
                title_cache: None,
                pending: None,
                wake: None,
            };
            grid.advance(b"\x1b]11;?\x1b\\", Instant::now(), &mut sink);
        }
        let reply = reply_rx.try_recv().expect("OSC 11 query is answered");
        assert!(String::from_utf8_lossy(&reply).contains("rgb:1111/2222/3333"));

        // New tabs start from the palette of the current configured background.
        let palette = app.new_terminal_palette();
        assert_eq!(
            (palette.background.r, palette.background.g, palette.background.b),
            (0x11, 0x22, 0x33)
        );
    }

    #[test]
    fn drain_flushes_only_expired_sync_blocks() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(40, 3, 1));
        app.terminals.push(Terminal::new_for_test(40, 3, 2));
        // Negative: the deadline is an hour ahead, so the held bytes stay.
        let future = Instant::now() + std::time::Duration::from_secs(3600);
        hold_sync_block_at(&app.terminals[0], b"\x1b[?2026hX", future);
        let before = crate::platform::lock_recover(&app.terminals[0].grid).content_generation();
        app.drain_terminal_host_events();
        assert!(app.terminals[0].sync_deadline().is_some());
        let after = crate::platform::lock_recover(&app.terminals[0].grid).content_generation();
        assert_eq!(after, before);

        // Positive: the deadline passed a second ago, so the drain replays the block.
        let past = Instant::now() - std::time::Duration::from_secs(1);
        hold_sync_block_at(&app.terminals[1], b"\x1b[?2026hX", past);
        let before = crate::platform::lock_recover(&app.terminals[1].grid).content_generation();
        app.drain_terminal_host_events();
        assert!(app.terminals[1].sync_deadline().is_none());
        let after = crate::platform::lock_recover(&app.terminals[1].grid).content_generation();
        assert_ne!(after, before);
    }

    #[test]
    fn background_tab_sync_deadline_sets_loop_timeout() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(8, 2, 1));
        app.terminals.push(Terminal::new_for_test(8, 2, 2));
        let now = Instant::now();
        hold_sync_block_at(&app.terminals[1], b"\x1b[?2026hX", now);
        let deadline = app.terminals[1]
            .sync_deadline()
            .expect("held bytes set a deadline");
        app.dirty = false;
        // The main loop wakes 50 ms after the deadline, the grace the parser
        // thread gets to replay the block itself (value from the task brief).
        assert_eq!(
            app.loop_control(),
            AppLoopControl::WaitUntil(deadline + std::time::Duration::from_millis(50))
        );
    }

    #[test]
    fn reorder_preserves_presented_active_terminal_identity_without_recommit() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(8, 2, 1));
        app.terminals.push(Terminal::new_for_test(8, 2, 2));
        app.active_terminal = 0;
        app.active_terminal_presented = true;

        app.reorder_terminal_tab(0, 1);

        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        assert!(
            app.terminals
                .iter()
                .all(|terminal| terminal.presentation_intent == TerminalPresentationIntent::None)
        );
    }

    #[test]
    fn terminal_tab_routes_share_factory_close_and_non_blocking_presentation_lifecycle() {
        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        let renderer = include_str!("renderer/terminal_ui.rs");

        assert_eq!(production.matches("Terminal::spawn(").count(), 1);
        assert_eq!(production.matches("fn add_terminal_with_launch").count(), 1);
        assert!(production.contains("self.add_terminal_with_launch(TerminalLaunchSpec {"));
        assert_eq!(production.matches("self.add_terminal()").count(), 2);
        let startup = production
            .split("    fn on_runtime_ready")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_close_requested").next())
            .expect("runtime startup path must remain present");
        assert_eq!(startup.matches("add_terminal_with_launch(").count(), 1);
        assert!(startup.contains("self.add_terminal_with_launch(initial_launch)"));
        assert!(!startup.contains("self.add_terminal()"));
        assert!(production.contains("GlobalShortcut::NewTab"));
        assert!(production.contains("TerminalTabHit::Add"));

        let external = production
            .split("    fn handle_external_launch")
            .nth(1)
            .and_then(|tail| tail.split("    fn flush_pending_external_launches").next())
            .expect("external terminal routing must remain present");
        assert_eq!(external.matches("add_terminal_with_launch(").count(), 1);
        let add_terminal = external
            .find("self.add_terminal_with_launch(launch)")
            .expect("external terminal requests must use the shared factory");
        let activation = external
            .find("match plan.activation")
            .expect("external activation routing must remain separate");
        assert!(add_terminal < activation);

        let close = production
            .split("    fn close_terminal_tab_at")
            .nth(1)
            .and_then(|tail| tail.split("    fn close_active_terminal").next())
            .expect("shared terminal close lifecycle must remain present");
        assert!(close.contains("self.terminals.remove(index)"));
        assert!(close.contains("if self.terminals.is_empty()"));
        assert!(close.contains("return true;"));
        assert!(!close.contains("add_terminal"));
        assert_eq!(production.matches("self.close_terminal_tab_at(").count(), 2);

        let request = production
            .split("    fn request_terminal_activation")
            .nth(1)
            .and_then(|tail| tail.split("    fn select_terminal_tab_from_user").next())
            .expect("shared presentation request lifecycle must remain present");
        let cancel = request
            .find("self.cancel_terminal_presentation_intents();")
            .expect("new request must cancel stale activation");
        let set = request
            .find("TerminalPresentationIntent::ActivateWhenReady")
            .expect("new request must install deferred activation");
        assert!(cancel < set);

        let switch = production
            .split("    fn activate_ready_terminal")
            .nth(1)
            .and_then(|tail| {
                tail.split("    fn process_terminal_presentation_intents")
                    .next()
            })
            .expect("activation method must remain present");
        assert!(switch.contains("reset_for_terminal_switch"));

        let layout_ready = renderer
            .find("grid.mark_presentation_layout_ready();")
            .expect("renderer must mark presentation layout ready");
        let visible = renderer
            .find("let presentation_visible = grid.presentation_visible();")
            .expect("renderer must gate terminal presentation");
        assert!(layout_ready < visible);
    }

    #[test]
    fn non_final_app_tab_close_does_not_wait_for_blocking_process_shutdown() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = std::env::temp_dir().join(format!(
            "ronsole-nonfinal-close-{}-{unique}.ready",
            std::process::id()
        ));
        let mut app = App::new();
        let terminal = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec::hermetic_shell_for_test(),
            terminal_palette(&crate::config::AppConfig::default()),
        );
        terminal
            .write_input(format!("trap '' TERM; printf ready > {}\r", marker.display()).as_bytes())
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(
                Instant::now() < ready_deadline,
                "shell did not install SIGTERM trap"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        app.terminals.push(terminal);
        app.terminals.push(Terminal::new_for_test(8, 2, 2));
        app.active_terminal = 0;
        app.active_terminal_presented = false;

        let started = Instant::now();
        assert!(!app.close_terminal_tab_at(0));
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(150),
            "non-final close waited for process cleanup: {elapsed:?}"
        );
        assert_eq!(app.terminals.len(), 1);
        let _ = std::fs::remove_file(&marker);
        println!("App non-final close enqueue duration={elapsed:?}");
    }

    #[test]
    fn direct_command_hold_keeps_tab_and_releases_finished_process_to_cleanup_worker() {
        use std::ffi::OsString;
        use std::time::Duration;

        let mut app = App::new();
        let held = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec {
                shell: None,
                working_directory: None,
                inherited_directory: None,
                command: vec![OsString::from("/bin/true")],
                hold: true,
                bridge_launch_id: None,
            },
            terminal_palette(&crate::config::AppConfig::default()),
        );
        app.terminals.push(held);
        app.active_terminal = 0;

        let deadline = Instant::now() + Duration::from_secs(3);
        while app.terminals[0].has_process() {
            assert!(!app.remove_closed_terminals());
            assert!(
                Instant::now() < deadline,
                "held direct command did not release process ownership"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(app.terminals.len(), 1);
        assert!(!app.terminals[0].is_closed());
        assert_eq!(app.loop_control(), AppLoopControl::Wait);
        app.shutdown_all();
    }

    #[test]
    fn direct_command_without_hold_remains_eligible_for_auto_close() {
        use std::ffi::OsString;
        use std::time::Duration;

        let mut app = App::new();
        app.terminals.push(Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec {
                shell: None,
                working_directory: None,
                inherited_directory: None,
                command: vec![OsString::from("/bin/true")],
                hold: false,
                bridge_launch_id: None,
            },
            terminal_palette(&crate::config::AppConfig::default()),
        ));
        app.active_terminal = 0;

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if app.remove_closed_terminals() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "non-held direct command did not become eligible for auto close"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(app.terminals.is_empty());
        app.shutdown_all();
    }

    #[test]
    fn direct_command_spawn_failure_stays_as_ready_error_terminal() {
        let terminal = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec {
                shell: None,
                working_directory: Some("/tmp".into()),
                inherited_directory: None,
                command: vec!["/definitely/missing/ronsole-test-command".into()],
                hold: false,
                bridge_launch_id: None,
            },
            terminal_palette(&crate::config::AppConfig::default()),
        );

        assert!(!terminal.has_process());
        assert!(terminal.presentation_ready());
        let mut title = String::new();
        terminal.write_display_title(&mut title);
        assert_eq!(title, "terminal error (1)");

        let source = include_str!("terminal.rs");
        let spawn = source
            .split("    pub(crate) fn spawn(")
            .nth(1)
            .and_then(|tail| tail.split("    pub fn write_display_title").next())
            .expect("terminal spawn path must remain present");
        assert!(spawn.contains("write_terminal_spawn_error(&mut grid, &error)"));
    }

    #[test]
    fn global_shortcuts_keep_plain_f8_for_terminal_and_use_exact_ctrl_f8_for_fps() {
        let none = Modifiers::empty();
        let ctrl = Modifiers::CONTROL;
        let ctrl_shift = Modifiers::CONTROL | Modifiers::SHIFT;
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::F1), none),
            Some(GlobalShortcut::ToggleSettings)
        );
        assert_eq!(global_shortcut(PhysicalKey::Code(KeyCode::F8), none), None);
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::F8), ctrl),
            Some(GlobalShortcut::ToggleFps)
        );
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::KeyT), ctrl_shift),
            Some(GlobalShortcut::NewTab)
        );
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::Digit4), ctrl),
            Some(GlobalShortcut::CloseTab)
        );
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::KeyF), ctrl),
            Some(GlobalShortcut::Search)
        );

        for modifiers in [
            ctrl,
            Modifiers::ALT,
            Modifiers::SHIFT,
            Modifiers::SUPER,
            ctrl | Modifiers::ALT,
            ctrl | Modifiers::SHIFT,
            ctrl | Modifiers::SUPER,
        ] {
            assert_eq!(
                global_shortcut(PhysicalKey::Code(KeyCode::F1), modifiers),
                None
            );
        }
        for modifiers in [
            Modifiers::ALT,
            Modifiers::SHIFT,
            Modifiers::SUPER,
            ctrl | Modifiers::ALT,
            ctrl | Modifiers::SHIFT,
            ctrl | Modifiers::SUPER,
        ] {
            assert_eq!(
                global_shortcut(PhysicalKey::Code(KeyCode::F8), modifiers),
                None
            );
        }
        for modifiers in [ctrl_shift | Modifiers::ALT, ctrl_shift | Modifiers::SUPER] {
            assert_eq!(
                global_shortcut(PhysicalKey::Code(KeyCode::KeyT), modifiers),
                None
            );
        }
        for key in [KeyCode::Digit4, KeyCode::KeyF] {
            for modifiers in [ctrl | Modifiers::ALT, ctrl | Modifiers::SUPER] {
                assert_eq!(global_shortcut(PhysicalKey::Code(key), modifiers), None);
            }
        }
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::KeyT), ctrl),
            None
        );
    }

    #[test]
    fn close_without_busy_target_skips_the_dialog() {
        let mut app = App::new();
        // No terminals means no busy target: the close runs at once.
        assert!(!app.request_terminal_close(CloseTarget::Tab(0)));
        assert!(app.close_confirm.is_none());
        assert!(app.busy_close_targets(CloseTarget::Window).is_empty());
    }

    /// Polls `condition` every 10 ms and fails the test after five seconds.
    fn wait_for_test_condition(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Starts a hermetic shell tab and waits until it runs a command, so input
    /// written afterwards reaches the shell.
    fn spawn_shell_tab(label: &str) -> Terminal {
        let marker = std::env::temp_dir().join(format!(
            "ronsole-close-{label}-{}.ready",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let terminal = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec::hermetic_shell_for_test(),
            terminal_palette(&crate::config::AppConfig::default()),
        );
        terminal
            .write_input(format!("printf ready > {}\r", marker.display()).as_bytes())
            .unwrap();
        wait_for_test_condition("shell readiness", || marker.exists());
        let _ = std::fs::remove_file(&marker);
        terminal
    }

    /// Returns an app whose only tab runs `sleep 30` in the foreground, once the
    /// tab reports that job as busy.
    pub(super) fn busy_shell_app(label: &str) -> App {
        let mut app = App::new();
        app.terminals.push(spawn_shell_tab(label));
        app.terminals[0].write_input(b"sleep 30\r").unwrap();
        wait_for_test_condition("foreground sleep job", || {
            !app.busy_close_targets(CloseTarget::Window).is_empty()
        });
        app
    }

    #[test]
    fn window_close_with_running_job_opens_the_dialog() {
        let mut app = busy_shell_app("window-job");
        let busy = app.busy_close_targets(CloseTarget::Window);
        assert_eq!(busy.len(), 1);
        assert_eq!(busy[0].1, "sleep");
        assert!(!busy[0].0.is_empty());
        assert!(!app.request_terminal_close(CloseTarget::Window));
        assert!(app.close_confirm.is_some());
        app.shutdown_all();
    }

    #[test]
    fn window_close_with_running_job_closes_at_once_when_confirm_is_off() {
        let mut app = busy_shell_app("confirm-off");
        // Set directly, not through the settings hit, so no config save is queued.
        app.config.confirm_close = false;
        // The close reaps the shell and its sleep job.
        assert!(app.request_terminal_close(CloseTarget::Window));
        assert!(app.close_confirm.is_none());
    }

    #[test]
    fn background_job_does_not_make_the_tab_busy() {
        let mut app = App::new();
        app.terminals.push(spawn_shell_tab("background-job"));
        let marker = std::env::temp_dir().join(format!(
            "ronsole-close-background-{}.done",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        // The marker is written after the shell has started the job and returned
        // to its prompt, so the foreground group is the shell again.
        app.terminals[0]
            .write_input(format!("sleep 30 &\rprintf done > {}\r", marker.display()).as_bytes())
            .unwrap();
        wait_for_test_condition("background job start", || marker.exists());
        let _ = std::fs::remove_file(&marker);
        assert!(app.busy_close_targets(CloseTarget::Window).is_empty());
        app.shutdown_all();
    }

    #[test]
    fn compositor_close_request_with_running_job_opens_the_dialog() {
        let mut app = busy_shell_app("compositor-job");
        assert!(!app.on_close_requested());
        assert!(app.close_confirm.is_some());
        app.shutdown_all();
    }

    #[test]
    fn close_confirm_escape_cancels_and_other_keys_are_swallowed() {
        let key = |physical_key| KeyInput {
            state: KeyState::Pressed,
            physical_key,
            keycode: 0,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        };
        let open = || CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
        };
        let mut app = App::new();
        app.close_confirm = Some(open());
        // F1 would toggle settings, but the open dialog owns every key except Enter and Esc.
        assert!(!app.on_key(key(PhysicalKey::Code(KeyCode::F1))));
        assert!(app.close_confirm.is_some());
        assert!(!app.settings_modal_active());
        assert!(!app.on_key(key(PhysicalKey::Code(KeyCode::Escape))));
        assert!(app.close_confirm.is_none());
    }

    #[test]
    fn close_confirm_enter_runs_the_close_and_clears_the_dialog() {
        let key = KeyInput {
            state: KeyState::Pressed,
            physical_key: PhysicalKey::Code(KeyCode::Enter),
            keycode: 0,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        };
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(80, 24, 1));
        app.close_confirm = Some(CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: Some("и ещё 2".to_string()),
        });
        // The only tab is the target, so the confirmed close removes it and reports the
        // exit. Without the close running, the tab would still be there.
        assert!(app.on_key(key));
        assert!(app.terminals.is_empty());
        assert!(app.close_confirm.is_none());
    }

    #[test]
    fn close_request_while_dialog_is_open_is_ignored() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(80, 24, 1));
        app.close_confirm = Some(CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
        });
        // A window close must neither replace the tab dialog nor exit the event loop.
        assert!(!app.request_terminal_close(CloseTarget::Window));
        let confirm = app.close_confirm.as_ref().expect("dialog stays open");
        assert_eq!(confirm.target, CloseTarget::Tab(0));
        assert_eq!(confirm.title, "Закрыть вкладку?");
        assert_eq!(app.terminals.len(), 1);
    }

    #[test]
    fn removing_another_tab_cancels_the_open_tab_dialog() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(80, 24, 1));
        app.terminals.push(Terminal::new_for_test(80, 24, 2));
        app.open_close_dialog(CloseConfirm {
            target: CloseTarget::Tab(1),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
        });
        let mut kept_before = String::new();
        app.terminals[1].write_display_title(&mut kept_before);
        // Tab 0 closes and tab 1 shifts to index 0; the dialog must not follow it there.
        assert!(!app.close_terminal_tab_at(0));
        assert!(app.close_confirm.is_none());
        assert_eq!(app.terminals.len(), 1);
        let mut kept_after = String::new();
        app.terminals[0].write_display_title(&mut kept_after);
        assert_eq!(kept_before, kept_after);
    }

    #[test]
    fn opening_the_dialog_drops_drags_and_motion_leaves_the_tab_drag_alone() {
        let drag = || TabDragState {
            start_idx: 0,
            start_x: 10.0,
            current_x: 10.0,
            threshold_passed: true,
        };
        let mut app = App::new();
        app.settings_background_dragging = true;
        app.terminal_tab_drag = Some(drag());
        app.open_close_dialog(CloseConfirm {
            target: CloseTarget::Window,
            title: "Закрыть окно?",
            rows: vec!["tab — vim".to_string()],
            more: None,
        });
        assert!(app.terminal_tab_drag.is_none());
        assert!(!app.settings_background_dragging);
        // A drag that is somehow still set must not advance while the dialog is open.
        app.terminal_tab_drag = Some(drag());
        app.on_pointer_motion(PointerPosition { x: 200.0, y: 5.0 });
        let moved = app.terminal_tab_drag.as_ref().expect("drag untouched");
        assert_eq!(moved.current_x, 10.0);
    }

    #[test]
    fn wheel_is_swallowed_while_the_close_dialog_is_open() {
        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        // Source-structure check: the wheel path has no runtime or PTY in unit tests.
        let scroll = production
            .split("    pub(crate) fn on_scroll")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_frame").next())
            .expect("wheel routing must remain present");
        let guard = scroll
            .find("self.close_confirm.is_some()")
            .expect("the close dialog must gate the wheel");
        let strip = scroll
            .find("terminal_tab_strip_layout")
            .expect("tab strip wheel routing must remain present");
        let pty = scroll
            .find("self.interaction.mouse_wheel")
            .expect("terminal wheel routing must remain present");
        assert!(guard < strip);
        assert!(guard < pty);
    }

    #[test]
    fn close_confirm_rows_stop_at_the_limit_and_count_the_rest() {
        let busy = |count: usize| -> Vec<(String, String)> {
            (0..count)
                .map(|index| (format!("tab {index}"), "vim".to_string()))
                .collect()
        };
        let full = CloseConfirm::from_busy(CloseTarget::Window, &busy(8)).expect("busy tabs");
        assert_eq!(full.rows.len(), 8);
        assert_eq!(full.more, None);
        assert_eq!(full.title, "Закрыть окно?");
        assert_eq!(full.rows[0], "tab 0 — vim");

        let nine = CloseConfirm::from_busy(CloseTarget::Tab(0), &busy(9)).expect("busy tabs");
        assert_eq!(nine.rows.len(), 8);
        assert_eq!(nine.more.as_deref(), Some("и ещё 1"));
        assert_eq!(nine.line_count(), 9);
        assert_eq!(nine.title, "Закрыть вкладку?");

        let twelve = CloseConfirm::from_busy(CloseTarget::Window, &busy(12)).expect("busy tabs");
        assert_eq!(twelve.rows.len(), 8);
        assert_eq!(twelve.more.as_deref(), Some("и ещё 4"));

        assert!(CloseConfirm::from_busy(CloseTarget::Window, &[]).is_none());
    }

    #[test]
    fn settings_modal_allows_only_settings_and_fps_global_shortcuts() {
        assert!(global_shortcut_allowed_in_settings(
            GlobalShortcut::ToggleSettings
        ));
        assert!(global_shortcut_allowed_in_settings(
            GlobalShortcut::ToggleFps
        ));
        for shortcut in [
            GlobalShortcut::NewTab,
            GlobalShortcut::CloseTab,
            GlobalShortcut::Search,
        ] {
            assert!(!global_shortcut_allowed_in_settings(shortcut));
        }
    }

    #[test]
    fn fps_toggle_is_opt_in_and_requests_the_cleanup_frame() {
        let mut app = App::new();
        app.dirty = false;
        assert!(!app.show_fps);

        app.toggle_fps();
        assert!(app.show_fps);
        assert!(app.dirty);

        app.dirty = false;
        app.toggle_fps();
        assert!(!app.show_fps);
        assert!(app.dirty);
    }

    fn external_request(
        launch: TerminalLaunchSpec,
        activation_token: Option<&str>,
    ) -> crate::platform::single_instance::ExternalLaunchRequest {
        crate::platform::single_instance::ExternalLaunchRequest {
            activation_token: activation_token.map(str::to_owned),
            launch,
        }
    }

    #[test]
    fn focused_bare_launch_with_token_opens_default_tab_without_activation() {
        assert_eq!(
            external_launch_plan(
                true,
                true,
                external_request(TerminalLaunchSpec::default(), Some("one-shot-token")),
            ),
            ExternalLaunchPlan {
                terminal_launch: Some(TerminalLaunchSpec::default()),
                activation: None,
            }
        );
    }

    #[test]
    fn focused_bare_launch_without_token_opens_default_tab_without_activation() {
        assert_eq!(
            external_launch_plan(
                true,
                true,
                crate::platform::single_instance::ExternalLaunchRequest::default(),
            ),
            ExternalLaunchPlan {
                terminal_launch: Some(TerminalLaunchSpec::default()),
                activation: None,
            }
        );
    }

    #[test]
    fn unfocused_bare_launch_with_supplied_token_uses_only_xdg_activation() {
        assert_eq!(
            external_launch_plan(
                false,
                true,
                external_request(TerminalLaunchSpec::default(), Some("one-shot-token")),
            ),
            ExternalLaunchPlan {
                terminal_launch: None,
                activation: Some(ExternalLaunchActivation::WaylandXdgActivation {
                    activation_token: "one-shot-token".to_owned(),
                }),
            }
        );
    }

    #[test]
    fn unfocused_bare_kde_launch_without_token_uses_force_activation_worker() {
        assert_eq!(
            external_launch_plan(
                false,
                true,
                crate::platform::single_instance::ExternalLaunchRequest::default(),
            ),
            ExternalLaunchPlan {
                terminal_launch: None,
                activation: Some(ExternalLaunchActivation::KdeForceActivate),
            }
        );
    }

    #[test]
    fn unfocused_bare_non_kde_launch_without_token_keeps_generic_wayland_best_effort() {
        assert_eq!(
            external_launch_plan(
                false,
                false,
                crate::platform::single_instance::ExternalLaunchRequest::default(),
            ),
            ExternalLaunchPlan {
                terminal_launch: None,
                activation: Some(ExternalLaunchActivation::WaylandBestEffort),
            }
        );
    }

    #[test]
    fn focused_custom_workdir_launch_opens_requested_tab_without_activation() {
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: Some("/tmp/custom-cwd".into()),
            inherited_directory: None,
            command: Vec::new(),
            hold: false,
            bridge_launch_id: None,
        };
        assert_eq!(
            external_launch_plan(true, true, external_request(launch.clone(), None)),
            ExternalLaunchPlan {
                terminal_launch: Some(launch),
                activation: None,
            }
        );
    }

    #[test]
    fn focused_custom_command_launch_opens_requested_tab_without_activation() {
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: None,
            inherited_directory: None,
            command: vec!["htop".into(), "-d".into(), "10".into()],
            hold: false,
            bridge_launch_id: None,
        };
        assert_eq!(
            external_launch_plan(true, false, external_request(launch.clone(), None)),
            ExternalLaunchPlan {
                terminal_launch: Some(launch),
                activation: None,
            }
        );
    }

    #[test]
    fn focused_hold_only_launch_opens_requested_tab_without_activation() {
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: None,
            inherited_directory: None,
            command: Vec::new(),
            hold: true,
            bridge_launch_id: None,
        };
        assert_eq!(
            external_launch_plan(true, false, external_request(launch.clone(), None)),
            ExternalLaunchPlan {
                terminal_launch: Some(launch),
                activation: None,
            }
        );
    }

    #[test]
    fn unfocused_custom_command_with_token_opens_requested_tab_and_uses_xdg_activation() {
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: None,
            inherited_directory: None,
            command: vec!["htop".into()],
            hold: false,
            bridge_launch_id: None,
        };
        assert_eq!(
            external_launch_plan(
                false,
                true,
                external_request(launch.clone(), Some("one-shot-token")),
            ),
            ExternalLaunchPlan {
                terminal_launch: Some(launch),
                activation: Some(ExternalLaunchActivation::WaylandXdgActivation {
                    activation_token: "one-shot-token".to_owned(),
                }),
            }
        );
    }

    #[test]
    fn unfocused_custom_workdir_on_kde_opens_requested_tab_and_uses_kde_activation() {
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: Some("/tmp/custom-cwd".into()),
            inherited_directory: None,
            command: Vec::new(),
            hold: false,
            bridge_launch_id: None,
        };
        assert_eq!(
            external_launch_plan(false, true, external_request(launch.clone(), None)),
            ExternalLaunchPlan {
                terminal_launch: Some(launch),
                activation: Some(ExternalLaunchActivation::KdeForceActivate),
            }
        );
    }

    #[test]
    fn unfocused_custom_hold_off_kde_opens_requested_tab_and_uses_generic_activation() {
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: None,
            inherited_directory: None,
            command: Vec::new(),
            hold: true,
            bridge_launch_id: None,
        };
        assert_eq!(
            external_launch_plan(false, false, external_request(launch.clone(), None)),
            ExternalLaunchPlan {
                terminal_launch: Some(launch),
                activation: Some(ExternalLaunchActivation::WaylandBestEffort),
            }
        );
    }

    #[test]
    fn pending_external_launch_before_runtime_ready_preserves_full_request() {
        let request = external_request(
            TerminalLaunchSpec {
                shell: None,
                working_directory: Some("/tmp/pending-cwd".into()),
                inherited_directory: None,
                command: vec!["program".into(), "arg one".into(), "--child-option".into()],
                hold: true,
                bridge_launch_id: None,
            },
            Some("pending-token"),
        );
        let mut app = App::new();
        app.handle_external_launch(request.clone());

        assert_eq!(app.pending_external_launches.len(), 1);
        assert_eq!(app.pending_external_launches.front(), Some(&request));
    }

    /// Pure app with one terminal per flag; the flag is the Claude foreground state.
    fn grid_mode_app(claude: &[bool]) -> App {
        let mut app = App::new();
        for (index, &is_claude) in claude.iter().enumerate() {
            let terminal = Terminal::new_for_test(80, 24, index as u64 + 1);
            crate::platform::lock_recover(&terminal.grid).mark_presentation_ready();
            terminal.set_claude_foreground_for_test(is_claude);
            app.terminals.push(terminal);
        }
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        app.grid_metrics = GridMetrics {
            width: 1000.0,
            height: 700.0,
            scale: 1.0,
            char_w: 8.0,
            char_h: 16.0,
        };
        app
    }

    fn grid_mode_members(app: &App) -> Option<Vec<usize>> {
        app.claude_grid.as_ref().map(|grid| grid.members.clone())
    }

    #[test]
    fn grid_mode_toggle_enters_with_claude_tabs_and_exits_to_focused_tile() {
        let mut app = grid_mode_app(&[true, false, true]);
        app.active_terminal = 1;
        let now = Instant::now();

        app.toggle_claude_grid(now);
        assert_eq!(grid_mode_members(&app), Some(vec![0, 2]));
        assert_eq!(app.active_terminal, 0);
        assert_eq!(app.grid_tile_count(), 2);

        app.toggle_claude_grid(now);
        assert_eq!(grid_mode_members(&app), None);
        assert_eq!(app.active_terminal, 0);
    }

    #[test]
    fn grid_mode_toggle_without_claude_tabs_shows_hint_with_deadline() {
        let mut app = grid_mode_app(&[false, false]);
        let now = Instant::now();

        app.toggle_claude_grid(now);
        assert!(!app.grid_open());
        assert_eq!(
            app.grid_hint,
            Some((GridHint::NoClaudeTabs, now + Duration::from_secs(2)))
        );

        app.dirty = false;
        app.advance_grid_hint(now + Duration::from_secs(1));
        assert!(app.grid_hint.is_some());
        assert!(!app.dirty);

        app.advance_grid_hint(now + Duration::from_secs(2));
        assert_eq!(app.grid_hint, None);
        assert!(app.dirty, "clearing the hint requests one frame");

        app.dirty = false;
        app.advance_grid_hint(now + Duration::from_secs(3));
        assert!(!app.dirty);
    }

    #[test]
    fn grid_mode_exit_paths() {
        let mut app = grid_mode_app(&[true, false, true]);
        app.toggle_claude_grid(Instant::now());
        assert!(app.grid_open());
        app.select_terminal_tab_from_user(1);
        assert!(!app.grid_open());
        assert_eq!(app.active_terminal, 1);

        app.toggle_claude_grid(Instant::now());
        assert!(app.grid_open());
        assert_eq!(app.add_terminal_with_launch(TerminalLaunchSpec::default()), None);
        assert!(!app.grid_open(), "leaving happens before the runtime check");
    }

    #[test]
    fn grid_mode_q9_blocks_entry_when_tiles_do_not_fit() {
        let mut app = grid_mode_app(&[true, true]);
        app.grid_metrics.width = 40.0;
        let now = Instant::now();
        app.toggle_claude_grid(now);
        assert!(!app.grid_open());
        assert_eq!(
            app.grid_hint,
            Some((GridHint::TooManyTabs, now + Duration::from_secs(2)))
        );
    }

    #[test]
    fn grid_mode_q9_exits_open_grid_when_metrics_shrink() {
        let now = Instant::now();

        // A smaller window: the open grid no longer fits and closes onto the focused tile.
        let mut app = grid_mode_app(&[true, true]);
        app.toggle_claude_grid(now);
        assert!(app.grid_open());
        let current = app.grid_metrics;
        app.set_grid_metrics(GridMetrics {
            width: 40.0,
            ..current
        });
        app.sync_claude_grid(now);
        assert!(!app.grid_open());
        // The focused tile (tab 0) stays active after the grid closes.
        assert_eq!(app.active_terminal, 0);
        assert_eq!(
            app.grid_hint,
            Some((GridHint::TooManyTabs, now + Duration::from_secs(2)))
        );

        // A larger cell size (font change) makes the tiles too small as well.
        let mut app = grid_mode_app(&[true, true]);
        app.toggle_claude_grid(now);
        assert!(app.grid_open());
        let current = app.grid_metrics;
        app.set_grid_metrics(GridMetrics {
            char_w: 100.0,
            ..current
        });
        app.sync_claude_grid(now);
        assert!(!app.grid_open());
        // The focused tile (tab 0) stays active after the grid closes.
        assert_eq!(app.active_terminal, 0);
        assert_eq!(
            app.grid_hint,
            Some((GridHint::TooManyTabs, now + Duration::from_secs(2)))
        );
    }

    #[test]
    fn grid_mode_sync_does_nothing_when_closed() {
        let mut app = grid_mode_app(&[true, true]);
        app.grid_generation = 5;
        app.dirty = false;
        app.sync_claude_grid(Instant::now());
        assert_eq!(app.grid_generation, 5);
        assert!(!app.dirty);
    }

    #[test]
    fn grid_mode_generation_grows_on_composition_and_metrics_change() {
        let mut app = grid_mode_app(&[true, true, false]);
        app.toggle_claude_grid(Instant::now());
        let before = app.grid_generation;

        app.terminals[1].set_claude_foreground_for_test(false);
        app.sync_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0]));
        assert!(app.grid_generation > before);

        let before = app.grid_generation;
        let same = app.grid_metrics;
        app.set_grid_metrics(same);
        assert_eq!(app.grid_generation, before, "same metrics keep the generation");
        app.set_grid_metrics(GridMetrics {
            char_w: 9.0,
            ..same
        });
        assert!(app.grid_generation > before);

        // Window size and scale start a new generation before any sync.
        let before = app.grid_generation;
        let current = app.grid_metrics;
        app.set_grid_metrics(GridMetrics {
            width: current.width - 100.0,
            ..current
        });
        assert!(app.grid_generation > before);
        let before = app.grid_generation;
        let current = app.grid_metrics;
        app.set_grid_metrics(GridMetrics {
            scale: 2.0,
            ..current
        });
        assert!(app.grid_generation > before);

        // A pointer press from before the change is dropped with the generation.
        app.interaction.set_pressed_mouse_buttons_for_test(1);
        let before = app.grid_generation;
        let current = app.grid_metrics;
        app.set_grid_metrics(GridMetrics {
            char_h: 20.0,
            ..current
        });
        assert!(app.grid_generation > before);
        assert_eq!(app.interaction.pressed_mouse_buttons_for_test(), 0);

        // Closed grid mode: metric changes do not start a generation.
        app.exit_claude_grid(None);
        let before = app.grid_generation;
        let current = app.grid_metrics;
        app.set_grid_metrics(GridMetrics {
            width: current.width + 1.0,
            ..current
        });
        assert_eq!(app.grid_generation, before);
    }

    /// Ctrl+Shift+S key press, as the window delivers it.
    fn grid_mode_toggle_key() -> KeyInput {
        KeyInput {
            state: KeyState::Pressed,
            physical_key: PhysicalKey::Code(KeyCode::KeyS),
            keycode: 0,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        }
    }

    #[test]
    fn grid_mode_toggle_ignored_with_settings_or_close_dialog_open() {
        // Settings: the shortcut is not in the settings-allowed set, so on_key drops it.
        let mut app = grid_mode_app(&[true, true]);
        app.on_modifiers(Modifiers::CONTROL | Modifiers::SHIFT);
        app.settings_open = true;
        let generation = app.grid_generation;
        assert!(!app.on_key(grid_mode_toggle_key()));
        assert!(!app.grid_open());
        assert_eq!(app.grid_hint, None);
        assert_eq!(app.grid_generation, generation);

        // Close dialog: every key except Enter and Escape is swallowed before shortcuts.
        let mut app = grid_mode_app(&[true, true]);
        app.on_modifiers(Modifiers::CONTROL | Modifiers::SHIFT);
        app.close_confirm = Some(CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
        });
        let generation = app.grid_generation;
        assert!(!app.on_key(grid_mode_toggle_key()));
        assert!(!app.grid_open());
        assert_eq!(app.grid_hint, None);
        assert_eq!(app.grid_generation, generation);
    }

    #[test]
    fn grid_mode_focus_moves_without_exit_when_focused_tile_loses_claude() {
        // Tab 3 is outside the grid and has no Claude flag.
        let mut app = grid_mode_app(&[true, true, true, false]);
        for terminal in &app.terminals {
            enable_focus_reporting_for_test(terminal);
        }
        app.focused = true;
        app.active_terminal = 1;
        let now = Instant::now();
        app.toggle_claude_grid(now);
        assert_eq!(grid_mode_members(&app), Some(vec![0, 1, 2]));
        assert_eq!(app.active_terminal, 1);

        // A dangling activation of the tab outside the grid must not take focus.
        app.terminals[3].presentation_intent = TerminalPresentationIntent::ActivateWhenReady;

        // The focused tile loses its flag: focus moves to the next member, grid stays open.
        app.terminals[1].set_claude_foreground_for_test(false);
        app.sync_claude_grid(now);
        assert!(app.grid_open());
        assert_eq!(app.active_terminal, 2);
        assert!(!app.process_terminal_presentation_intents());
        assert_eq!(app.active_terminal, 2);
        assert_eq!(app.terminals[1].input_bytes_for_test(), b"\x1b[O");
        assert_eq!(app.terminals[2].input_bytes_for_test(), b"\x1b[I");

        // The last member loses its flag: focus moves to the previous member.
        app.terminals[2].set_claude_foreground_for_test(false);
        app.sync_claude_grid(now);
        assert_eq!(grid_mode_members(&app), Some(vec![0]));
        assert_eq!(app.active_terminal, 0);

        // No Claude tabs left: grid mode exits and keeps the active tab.
        app.terminals[0].set_claude_foreground_for_test(false);
        app.sync_claude_grid(now);
        assert!(!app.grid_open());
        assert_eq!(app.active_terminal, 0);
    }

    #[test]
    fn grid_mode_pending_activation_is_cancelled_on_entry() {
        let mut app = grid_mode_app(&[true, true]);
        // A dangling request to switch to tab 1; it is presentation-ready, so it would fire.
        app.terminals[1].presentation_intent = TerminalPresentationIntent::ActivateWhenReady;
        app.toggle_claude_grid(Instant::now());
        assert!(app.grid_open());
        assert_eq!(app.active_terminal, 0);
        assert!(matches!(
            app.terminals[1].presentation_intent,
            TerminalPresentationIntent::None
        ));
        assert!(!app.process_terminal_presentation_intents());
        assert_eq!(app.active_terminal, 0);
    }

    #[test]
    fn grid_mode_on_frame_builds_panel_layouts() {
        // Window literals of the headless fixture: `started_headless` in
        // src/app/headless_run.rs starts the runtime at (640, 400) with scale 1.0.
        const WIDTH: f32 = 640.0;
        const HEIGHT: f32 = 400.0;
        const SCALE: f32 = 1.0;
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-on-frame",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0, 1]));

        // The test does not set `panels`: on_frame must build one panel per tile.
        assert_eq!(app.on_frame(), Ok(true));

        let body = terminal_body_rect(WIDTH, HEIGHT, SCALE);
        let mut tiles = Vec::new();
        crate::panels::grid_tiles(body, 2, SCALE, &mut tiles);
        assert_eq!(app.panels.len(), 2);
        assert_eq!(app.panel_layouts.len(), 2);
        for (index, &(header, tile_body)) in tiles.iter().enumerate() {
            assert_eq!(app.panels[index].header, Some(header));
            assert_eq!(app.panels[index].body, tile_body);
            assert_eq!(app.panel_layouts[index].body, tile_body);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Turns on focus reporting (DECSET 1004) through the same grid path the PTY uses.
    fn enable_focus_reporting_for_test(terminal: &Terminal) {
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        crate::platform::lock_recover(&terminal.grid).advance(
            b"\x1b[?1004h",
            Instant::now(),
            &mut sink,
        );
    }

    #[test]
    fn focus_loss_reports_1004() {
        let mut app = App::new();
        let terminal = Terminal::new_for_test(8, 2, 1);
        enable_focus_reporting_for_test(&terminal);
        app.terminals.push(terminal);
        app.active_terminal_presented = true;
        app.on_focus_changed(false);
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\x1b[O");
    }

    #[test]
    fn focus_loss_without_1004_sends_nothing() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(8, 2, 1));
        app.active_terminal_presented = true;
        app.on_focus_changed(false);
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
    }

    /// A focus loss clears the sent-key set even when the terminal is not presented;
    /// only the mode 1004 report is gated on presentation.
    #[test]
    fn focus_loss_clears_sent_keys_when_terminal_not_presented() {
        let mut app = App::new();
        let terminal = Terminal::new_for_test(8, 2, 1);
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        let ctrl_press = ronsole_term::KeyEvent {
            physical: 1,
            key: ronsole_term::TermKey::Modifier(ronsole_term::ModKey::LeftCtrl),
            base: None,
            mods: ronsole_term::KeyMods { shift: false, ctrl: true, alt: false, super_key: false },
            kind: ronsole_term::KeyKind::Press,
            text: None,
        };
        let ctrl_release = ronsole_term::KeyEvent {
            mods: ronsole_term::KeyMods::default(),
            kind: ronsole_term::KeyKind::Release,
            ..ctrl_press
        };
        {
            let mut grid = crate::platform::lock_recover(&terminal.grid);
            // Flags 8+2: modifier events are reported and the press is remembered.
            grid.advance(b"\x1b[=10u", Instant::now(), &mut sink);
            let mut out = Vec::new();
            assert_eq!(grid.encode_key(&ctrl_press, &mut out), ronsole_term::KeyOutcome::Sent);
        }
        app.terminals.push(terminal);
        app.active_terminal_presented = false;
        app.on_focus_changed(false);
        let mut grid = crate::platform::lock_recover(&app.terminals[0].grid);
        let mut out = Vec::new();
        assert_eq!(grid.encode_key(&ctrl_release, &mut out), ronsole_term::KeyOutcome::Ignore);
        assert!(out.is_empty());
    }

    #[test]
    fn grid_mode_close_focused_tab_moves_to_next_tile_not_shell() {
        let mut app = grid_mode_app(&[true, false, true]);
        for terminal in &app.terminals {
            enable_focus_reporting_for_test(terminal);
        }
        app.focused = true;
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0, 2]));

        // A dangling activation of the shell tab (outside the grid) must not take focus.
        app.terminals[1].presentation_intent = TerminalPresentationIntent::ActivateWhenReady;
        assert!(!app.close_terminal_tab_at(0));
        // The former tab 2 is now index 1 and takes focus; the shell is index 0.
        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        assert_eq!(grid_mode_members(&app), Some(vec![1]));
        assert!(app.terminals.iter().all(|terminal| {
            terminal.presentation_intent != TerminalPresentationIntent::ActivateWhenReady
        }));
        assert!(!app.process_terminal_presentation_intents());
        assert_eq!(app.active_terminal, 1);
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
        assert_eq!(app.terminals[1].input_bytes_for_test(), b"\x1b[I");
    }

    #[test]
    fn grid_mode_close_last_tile_exits_to_neighbour_tab() {
        let mut app = grid_mode_app(&[true, false]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0]));

        assert!(!app.close_terminal_tab_at(0));
        assert!(!app.grid_open());
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        assert_eq!(app.terminals[0].presentation_intent, TerminalPresentationIntent::None);
    }

    #[test]
    fn grid_mode_close_other_tile_shifts_members_and_keeps_focus() {
        let mut app = grid_mode_app(&[true, false, true]);
        app.active_terminal = 2;
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0, 2]));
        assert_eq!(app.active_terminal, 2);
        let generation = app.grid_generation;

        assert!(!app.close_terminal_tab_at(0));
        assert_eq!(app.active_terminal, 1);
        assert_eq!(grid_mode_members(&app), Some(vec![1]));
        assert_ne!(app.grid_generation, generation);
    }

    #[test]
    fn grid_mode_out_of_range_tab_is_not_visible_and_close_does_not_panic() {
        let mut app = grid_mode_app(&[true, true]);
        let now = Instant::now();
        app.toggle_claude_grid(now);
        assert!(!app.tab_visible(7));
        assert!(!app.close_terminal_tab_at(7));

        app.toggle_claude_grid(now);
        assert!(!app.grid_open());
        assert!(!app.tab_visible(7));
    }

    #[test]
    fn grid_mode_bell_not_marked_on_visible_tiles() {
        let mut app = grid_mode_app(&[true, false, true]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0, 2]));
        for terminal in &app.terminals {
            terminal.ring_bell_for_test();
        }

        app.drain_terminal_host_events();
        assert!(!app.terminals[0].bell_pending());
        assert!(app.terminals[1].bell_pending());
        assert!(!app.terminals[2].bell_pending());
    }

    #[test]
    fn background_tab_bell_marks_tab_until_it_is_activated() {
        let mut app = App::new();
        for display_number in [1, 2] {
            app.terminals.push(Terminal::new_for_test(8, 2, display_number));
        }
        app.terminals[1].ring_bell_for_test();

        app.drain_terminal_host_events();
        assert!(app.terminals[1].bell_pending());
        assert!(!app.terminals[0].bell_pending());

        app.activate_ready_terminal(1, false);
        assert_eq!(app.active_terminal, 1);
        assert!(!app.terminals[1].bell_pending());
    }

    #[test]
    fn active_tab_bell_in_focused_window_sets_nothing() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(8, 2, 1));
        app.terminals.push(Terminal::new_for_test(8, 2, 2));
        assert!(app.focused);
        app.terminals[0].ring_bell_for_test();

        app.drain_terminal_host_events();

        assert!(!app.terminals[0].bell_pending());
        assert!(!app.window_attention_requested);
    }

    #[test]
    fn unfocused_window_bell_without_runtime_sends_no_attention() {
        // Without a window runtime no compositor request can be sent, so the
        // once-per-period flag must stay clear for the next bell to retry.
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(8, 2, 1));
        app.focused = false;

        app.terminals[0].ring_bell_for_test();
        app.drain_terminal_host_events();
        assert!(!app.window_attention_requested);
        // The active tab is on screen, so it gets no tab mark.
        assert!(!app.terminals[0].bell_pending());
    }

    #[test]
    fn tab_focus_transition_reports_out_then_in_only_when_focused() {
        let mut app = App::new();
        for display_number in [1, 2] {
            let terminal = Terminal::new_for_test(8, 2, display_number);
            enable_focus_reporting_for_test(&terminal);
            app.terminals.push(terminal);
        }
        app.focused = false;
        app.send_terminal_focus_transition(Some(0), Some(1));
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
        assert!(app.terminals[1].input_bytes_for_test().is_empty());

        app.focused = true;
        app.send_terminal_focus_transition(Some(0), Some(1));
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\x1b[O");
        assert_eq!(app.terminals[1].input_bytes_for_test(), b"\x1b[I");
    }

    #[test]
    fn settings_toggle_and_escape_close_contract_is_stable() {
        let mut app = App::new();
        assert!(!app.settings_open);
        app.toggle_settings();
        assert!(app.settings_open);
        app.toggle_settings();
        assert!(!app.settings_open);
        app.toggle_settings();
        app.close_settings();
        assert!(!app.settings_open);
    }

    #[test]
    fn settings_tab_switch_is_ui_only_and_does_not_dirty_config() {
        let mut app = App::new();
        app.config_dirty = false;
        app.dirty = false;
        assert_eq!(app.settings_active_tab, SettingsTab::General);

        app.apply_settings_hit(SettingsHit::Tab(SettingsTab::Help));

        assert_eq!(app.settings_active_tab, SettingsTab::Help);
        assert!(!app.config_dirty);
        assert!(app.dirty);

        app.dirty = false;
        app.apply_settings_hit(SettingsHit::Tab(SettingsTab::Help));
        assert!(!app.config_dirty);
        assert!(!app.dirty);
    }

    #[test]
    fn headless_app_uses_memory_clipboard_and_recording_opener() {
        let profile = std::env::temp_dir().join(format!("ronsole-headless-{}", std::process::id()));
        let (app, _receiver) = App::load_headless(None, &profile, None).expect("headless app");
        assert!(app.interaction.clipboard_is_memory());
        assert!(matches!(app.open_sink, OpenSink::Record(_)));
    }

    #[test]
    fn settings_resets_change_only_their_own_config_value() {
        let custom = AppConfig {
            terminal_font_size: 22.0,
            scroll_sensitivity: 2.25,
            terminal_background: RgbColor::new(0x11, 0x22, 0x33),
            ..AppConfig::default()
        };

        let mut font_app = App::from_config(custom.clone());
        font_app.apply_settings_hit(SettingsHit::FontReset);
        assert_eq!(
            font_app.config.terminal_font_size,
            DEFAULT_TERMINAL_FONT_SIZE
        );
        assert_eq!(
            font_app.config.scroll_sensitivity,
            custom.scroll_sensitivity
        );
        assert_eq!(
            font_app.config.terminal_background,
            custom.terminal_background
        );

        let mut scroll_app = App::from_config(custom.clone());
        scroll_app.apply_settings_hit(SettingsHit::ScrollReset);
        assert_eq!(
            scroll_app.config.terminal_font_size,
            custom.terminal_font_size
        );
        assert_eq!(
            scroll_app.config.scroll_sensitivity,
            DEFAULT_SCROLL_SENSITIVITY
        );
        assert_eq!(
            scroll_app.config.terminal_background,
            custom.terminal_background
        );

        let mut background_app = App::from_config(custom.clone());
        background_app.apply_settings_hit(SettingsHit::BackgroundReset);
        assert_eq!(
            background_app.config.terminal_font_size,
            custom.terminal_font_size
        );
        assert_eq!(
            background_app.config.scroll_sensitivity,
            custom.scroll_sensitivity
        );
        assert_eq!(
            background_app.config.terminal_background,
            DEFAULT_TERMINAL_BACKGROUND
        );
        assert_eq!(
            background_app.settings_background_input.text,
            DEFAULT_TERMINAL_BACKGROUND.to_hex()
        );
    }

    #[test]
    fn settings_outside_click_closes_but_empty_modal_space_and_controls_do_not() {
        let mut app = App::new();
        app.toggle_settings();
        assert!(app.settings_open);
        app.apply_settings_hit(SettingsHit::None);
        assert!(app.settings_open);
        app.apply_settings_hit(SettingsHit::FontReset);
        assert!(app.settings_open);
        app.apply_settings_hit(SettingsHit::Outside);
        assert!(!app.settings_open);
    }

    #[test]
    fn settings_outside_click_discards_incomplete_hex_but_keeps_last_valid_commit() {
        let mut app = App::new();
        app.toggle_settings();
        app.apply_settings_hit(SettingsHit::BackgroundField);
        app.settings_background_input.select_all();
        assert!(app.edit_settings_background_text("#112233"));
        assert_eq!(
            app.config.terminal_background,
            RgbColor::new(0x11, 0x22, 0x33)
        );

        app.settings_background_input.select_all();
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace)));
        assert_eq!(app.settings_background_input.text, "");
        assert_eq!(
            app.config.terminal_background,
            RgbColor::new(0x11, 0x22, 0x33)
        );

        app.apply_settings_hit(SettingsHit::Outside);
        assert!(!app.settings_open);
        assert!(!app.settings_background_editing);
        assert_eq!(app.settings_background_input.text, "#112233");
        assert_eq!(
            app.config.terminal_background,
            RgbColor::new(0x11, 0x22, 0x33)
        );
    }

    #[test]
    fn settings_hex_editor_keeps_invalid_drafts_uncommitted_and_canonicalizes_valid_input() {
        let mut app = App::new();
        let original = app.config.terminal_background;
        app.apply_settings_hit(SettingsHit::BackgroundField);
        app.settings_background_input.select_all();
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace)));
        assert!(app.edit_settings_background_text("#12"));
        assert_eq!(app.settings_background_input.text, "#12");
        assert_eq!(app.config.terminal_background, original);

        assert!(app.edit_settings_background_text("aBcF"));
        assert_eq!(app.settings_background_input.text, "#12ABCF");
        assert_eq!(
            app.config.terminal_background,
            RgbColor::new(0x12, 0xAB, 0xCF)
        );
        assert!(app.config_dirty);
    }

    #[test]
    fn settings_hex_editor_keyboard_selection_copy_cut_paste_and_navigation_are_shared() {
        let mut input = SingleLineInput::from_text("#12ABCF");
        let left = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::ArrowLeft),
            false,
            false,
            None,
        );
        assert!(left.visual_changed);
        let select = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::ArrowLeft),
            false,
            true,
            None,
        );
        assert!(select.visual_changed);
        assert_eq!(input.selected_text().as_deref(), Some("C"));

        let copy = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyC),
            true,
            false,
            None,
        );
        assert_eq!(copy.copy_text.as_deref(), Some("C"));
        assert!(!copy.text_changed);

        let cut = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyX),
            true,
            false,
            None,
        );
        assert_eq!(cut.copy_text.as_deref(), Some("C"));
        assert!(cut.text_changed);
        assert_eq!(input.text, "#12ABF");

        let paste = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyV),
            true,
            false,
            Some("c"),
        );
        assert!(paste.text_changed);
        assert_eq!(input.text, "#12ABCF");

        let home = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::Home),
            false,
            false,
            None,
        );
        assert!(home.visual_changed);
        assert_eq!(input.cursor, 0);
        let end_select = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::End),
            false,
            true,
            None,
        );
        assert!(end_select.visual_changed);
        assert_eq!(input.selection(), Some((0, 7)));
        let select_all = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyA),
            true,
            false,
            None,
        );
        assert!(select_all.consumed);
        assert_eq!(input.selection(), Some((0, 7)));
    }

    #[test]
    fn settings_hex_editor_valid_paste_commits_canonical_background() {
        let mut app = App::new();
        app.apply_settings_hit(SettingsHit::BackgroundField);
        app.settings_background_input.select_all();
        let outcome = settings_background_editor_key(
            &mut app.settings_background_input,
            PhysicalKey::Code(KeyCode::KeyV),
            true,
            false,
            Some("#a1b2c3"),
        );
        assert!(outcome.text_changed);
        app.apply_valid_settings_background_draft();
        assert_eq!(app.settings_background_input.text, "#A1B2C3");
        assert_eq!(
            app.config.terminal_background,
            RgbColor::new(0xA1, 0xB2, 0xC3)
        );
    }

    #[test]
    fn settings_hex_editor_rejects_invalid_or_oversized_paste_and_accepts_incomplete_draft() {
        let mut input = SingleLineInput::from_text("#282A36");
        input.select_all();
        let invalid = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyV),
            true,
            false,
            Some("#12GG34"),
        );
        assert!(!invalid.text_changed);
        assert_eq!(input.text, "#282A36");
        let oversized = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyV),
            true,
            false,
            Some("#1234567"),
        );
        assert!(!oversized.text_changed);
        assert_eq!(input.text, "#282A36");

        let incomplete = settings_background_editor_key(
            &mut input,
            PhysicalKey::Code(KeyCode::KeyV),
            true,
            false,
            Some("#12"),
        );
        assert!(incomplete.text_changed);
        assert_eq!(input.text, "#12");
        assert_eq!(RgbColor::parse_hex(&input.text), None);
    }

    #[test]
    fn settings_hex_mouse_drag_places_caret_and_selects_in_both_directions() {
        let mut input = SingleLineInput::from_text("#123456");
        assert!(begin_settings_background_pointer_selection(&mut input, 2));
        assert_eq!(input.cursor, 2);
        assert_eq!(input.selection_anchor, Some(2));
        assert!(update_settings_background_pointer_selection(&mut input, 6));
        assert_eq!(input.selection(), Some((2, 6)));
        assert!(update_settings_background_pointer_selection(&mut input, 1));
        assert_eq!(input.selection(), Some((1, 2)));
    }

    #[test]
    fn color_field_draft_applies_complete_hex_and_rejects_partial_text() {
        let mut app = App::from_config(AppConfig::default());
        app.settings_open = true;
        app.apply_settings_hit(SettingsHit::ColorField(SettingsColorField::Foreground));
        assert!(app.settings_background_editing);
        assert_eq!(
            app.settings_color_target,
            Some(SettingsColorField::Foreground)
        );
        assert_eq!(app.settings_color_input.text, "#E6E6E6");

        app.settings_color_input.select_all();
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace)));
        assert_eq!(app.config.terminal_foreground, DEFAULT_TERMINAL_FOREGROUND);

        assert!(app.edit_settings_background_text("#12ABCF"));
        assert_eq!(
            app.config.terminal_foreground,
            RgbColor::new(0x12, 0xAB, 0xCF)
        );
        assert_eq!(app.settings_background_input.text, "#282A36");

        app.settings_color_input.select_all();
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace)));
        assert!(app.edit_settings_background_text("#12"));
        assert_eq!(
            app.config.terminal_foreground,
            RgbColor::new(0x12, 0xAB, 0xCF),
            "a partial draft must not reach the config"
        );

        app.apply_settings_hit(SettingsHit::Outside);
        assert!(!app.settings_background_editing);
        assert_eq!(app.settings_color_target, None);
        assert_eq!(
            app.config.terminal_foreground,
            RgbColor::new(0x12, 0xAB, 0xCF)
        );
        assert_eq!(app.settings_color_input.text, "#12ABCF");
    }

    #[test]
    fn color_field_reset_restores_defaults_and_unsets_bold() {
        let mut app = App::from_config(AppConfig {
            terminal_foreground: RgbColor::new(0x12, 0xAB, 0xCF),
            terminal_bold_foreground: Some(RgbColor::new(0x01, 0x02, 0x03)),
            ..AppConfig::default()
        });
        app.apply_settings_hit(SettingsHit::ColorReset(SettingsColorField::Foreground));
        assert_eq!(app.config.terminal_foreground, DEFAULT_TERMINAL_FOREGROUND);
        app.apply_settings_hit(SettingsHit::ColorReset(SettingsColorField::BoldForeground));
        assert_eq!(app.config.terminal_bold_foreground, None);
        app.config.terminal_palette[3] = RgbColor::new(0x0A, 0x0B, 0x0C);
        app.apply_settings_hit(SettingsHit::ColorReset(SettingsColorField::Palette(3)));
        assert_eq!(app.config.terminal_palette[3], DEFAULT_TERMINAL_PALETTE[3]);
        assert_eq!(app.config.terminal_palette[2], DEFAULT_TERMINAL_PALETTE[2]);

        let fields = [SettingsColorField::Foreground, SettingsColorField::BoldForeground]
            .into_iter()
            .chain((0..16).map(SettingsColorField::Palette));
        for field in fields {
            app.apply_settings_hit(SettingsHit::ColorReset(field));
        }
        assert_eq!(app.config.terminal_foreground, DEFAULT_TERMINAL_FOREGROUND);
        assert_eq!(app.config.terminal_bold_foreground, DEFAULT_TERMINAL_BOLD_FOREGROUND);
        assert_eq!(app.config.terminal_palette, DEFAULT_TERMINAL_PALETTE);
        assert_eq!(app.settings_colors.hex[0], "#E6E6E6");
    }

    #[test]
    fn empty_bold_draft_turns_bold_foreground_off_on_commit() {
        let mut app = App::from_config(AppConfig {
            terminal_bold_foreground: Some(RgbColor::new(0x01, 0x02, 0x03)),
            ..AppConfig::default()
        });
        app.apply_settings_hit(SettingsHit::ColorField(SettingsColorField::BoldForeground));
        assert_eq!(app.settings_color_input.text, "#010203");
        app.settings_color_input.select_all();
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace)));
        assert_eq!(app.config.terminal_bold_foreground, None);
        app.apply_settings_hit(SettingsHit::Tab(SettingsTab::Help));
        assert_eq!(app.config.terminal_bold_foreground, None);
        assert_eq!(app.settings_colors.hex[1], "");
        assert!(app.settings_colors.swatch[1].is_none());
    }

    #[test]
    fn applied_colors_reach_existing_terminal_grid_palette() {
        fn assert_applied_palette(app: &App) {
            let rgb = |r, g, b| ronsole_term::Rgb { r, g, b };
            let grid = crate::platform::lock_recover(&app.terminals[0].grid);
            let palette = grid.palette();
            assert_eq!(palette.foreground, rgb(0x12, 0xAB, 0xCF));
            assert_eq!(palette.ansi[1], rgb(0x0A, 0x0B, 0x0C));
            assert_eq!(palette.bold_foreground, Some(rgb(0xFE, 0xDC, 0xBA)));
            assert_eq!(palette.background, rgb(0x28, 0x2A, 0x36));
        }

        let mut app = App::from_config(AppConfig::default());
        app.terminals.push(Terminal::new_for_test(40, 3, 1));

        app.settings_color_input.set_text("#12ABCF");
        assert!(app.apply_settings_color_draft(SettingsColorField::Foreground));
        app.settings_color_input.set_text("#0A0B0C");
        assert!(app.apply_settings_color_draft(SettingsColorField::Palette(1)));
        app.settings_color_input.set_text("#FEDCBA");
        assert!(app.apply_settings_color_draft(SettingsColorField::BoldForeground));
        assert_applied_palette(&app);

        // Re-applying the same colours changes nothing, in the draft or in the grid.
        app.settings_color_input.set_text("#12ABCF");
        assert!(!app.apply_settings_color_draft(SettingsColorField::Foreground));
        app.apply_terminal_colors();
        assert_applied_palette(&app);
    }

    #[test]
    fn set_settings_color_field_reports_only_real_changes() {
        let mut config = AppConfig::default();
        let palette = SettingsColorField::Palette(5);
        assert!(!set_settings_color_field(
            &mut config,
            palette,
            Some(DEFAULT_TERMINAL_PALETTE[5])
        ));
        assert!(set_settings_color_field(
            &mut config,
            palette,
            Some(RgbColor::new(0x01, 0x02, 0x03))
        ));
        assert!(!set_settings_color_field(
            &mut config,
            palette,
            Some(RgbColor::new(0x01, 0x02, 0x03))
        ));
        assert!(!set_settings_color_field(&mut config, palette, None));
        assert_eq!(config.terminal_palette[5], RgbColor::new(0x01, 0x02, 0x03));
    }

    #[test]
    fn settings_hex_backspace_delete_enter_and_reset_obey_commit_contract() {
        let mut app = App::from_config(AppConfig {
            terminal_background: RgbColor::new(0x11, 0x22, 0x33),
            ..AppConfig::default()
        });
        app.apply_settings_hit(SettingsHit::BackgroundField);
        app.settings_background_input.move_cursor(1, false);
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Delete)));
        assert_eq!(app.settings_background_input.text, "#12233");
        assert_eq!(
            app.config.terminal_background,
            RgbColor::new(0x11, 0x22, 0x33)
        );
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace)));
        assert_eq!(app.settings_background_input.text, "12233");
        assert!(app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Enter)));
        assert!(!app.settings_background_editing);
        assert_eq!(app.settings_background_input.text, "#112233");

        app.apply_settings_hit(SettingsHit::BackgroundField);
        app.settings_background_input.select_all();
        let _ = app.handle_settings_background_key(PhysicalKey::Code(KeyCode::Backspace));
        app.apply_settings_hit(SettingsHit::BackgroundReset);
        assert_eq!(app.config.terminal_background, DEFAULT_TERMINAL_BACKGROUND);
        assert_eq!(
            app.settings_background_input.text,
            DEFAULT_TERMINAL_BACKGROUND.to_hex()
        );
        assert_eq!(app.settings_background_input.selection(), None);
    }

    #[test]
    fn settings_scroll_buttons_use_tenth_steps_and_stable_display() {
        let mut app = App::new();
        app.apply_settings_hit(SettingsHit::ScrollIncrease);
        assert!((app.config.scroll_sensitivity - 1.1).abs() < 0.0001);
        assert_eq!(app.settings_scroll_value, "1.10");
        app.apply_settings_hit(SettingsHit::ScrollDecrease);
        assert!((app.config.scroll_sensitivity - 1.0).abs() < 0.0001);
        assert_eq!(app.settings_scroll_value, "1.00");
    }

    #[test]
    fn settings_and_tab_cursor_policy_prioritizes_modal_and_caches_identical_icon() {
        assert_eq!(
            ui_cursor_icon(
                true,
                SettingsHit::BackgroundField,
                false,
                TerminalSearchHit::Close,
                TerminalTabHit::Add,
                false
            ),
            CursorKind::Text
        );
        assert_eq!(
            ui_cursor_icon(
                true,
                SettingsHit::None,
                false,
                TerminalSearchHit::Input,
                TerminalTabHit::Add,
                false
            ),
            CursorKind::Default
        );
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalSearchHit::None,
                TerminalTabHit::Close(0),
                false
            ),
            CursorKind::Pointer
        );
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalSearchHit::None,
                TerminalTabHit::Add,
                false
            ),
            CursorKind::Pointer
        );
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalSearchHit::None,
                TerminalTabHit::Body(0),
                false
            ),
            CursorKind::Default
        );
        assert_eq!(
            ui_cursor_icon(
                true,
                SettingsHit::Outside,
                true,
                TerminalSearchHit::None,
                TerminalTabHit::None,
                false
            ),
            CursorKind::Text
        );
        assert_eq!(
            cursor_icon_change(CursorKind::Pointer, CursorKind::Pointer),
            None
        );
        assert_eq!(
            cursor_icon_change(CursorKind::Pointer, CursorKind::Default),
            Some(CursorKind::Default)
        );
    }

    #[test]
    fn search_cursor_policy_uses_text_pointer_and_chrome_priority_before_tabs() {
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalSearchHit::Input,
                TerminalTabHit::Add,
                false
            ),
            CursorKind::Text
        );
        for hit in [
            TerminalSearchHit::Close,
            TerminalSearchHit::Previous,
            TerminalSearchHit::Next,
            TerminalSearchHit::CaseToggle,
        ] {
            assert_eq!(
                ui_cursor_icon(
                    false,
                    SettingsHit::None,
                    false,
                    hit,
                    TerminalTabHit::Body(0),
                    false
                ),
                CursorKind::Pointer
            );
        }
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalSearchHit::Chrome,
                TerminalTabHit::Add,
                false
            ),
            CursorKind::Default
        );
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalSearchHit::None,
                TerminalTabHit::None,
                false
            ),
            CursorKind::Default
        );
    }

    #[test]
    fn global_shortcut_fires_on_repeat() {
        let mut app = App::new();
        let f1 = |state| KeyInput {
            state,
            physical_key: PhysicalKey::Code(KeyCode::F1),
            keycode: 59,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        };
        app.on_key(f1(KeyState::Released));
        assert!(!app.settings_open);
        app.on_key(f1(KeyState::Repeat));
        assert!(app.settings_open);
    }

    #[test]
    fn focus_loss_marks_one_unfocused_frame_dirty_for_separator_redraw() {
        let mut app = App::new();
        app.dirty = false;
        app.on_focus_changed(false);
        assert!(!app.focused);
        assert!(app.unfocused_redraw_pending);
        assert!(app.dirty);

        app.dirty = false;
        app.on_focus_changed(true);
        assert!(app.focused);
        assert!(!app.unfocused_redraw_pending);
        assert!(app.dirty);
    }

    #[test]
    fn settings_open_and_focus_loss_clear_tracked_physical_mouse_buttons() {
        let mut settings_app = App::new();
        settings_app.terminals.push(Terminal::new_for_test(8, 2, 1));
        settings_app.active_terminal_presented = true;
        settings_app
            .interaction
            .set_pressed_mouse_buttons_for_test(1);
        settings_app.toggle_settings();
        assert_eq!(settings_app.interaction.pressed_mouse_buttons_for_test(), 0);

        let mut focus_app = App::new();
        focus_app.terminals.push(Terminal::new_for_test(8, 2, 1));
        focus_app.active_terminal_presented = true;
        focus_app.interaction.set_pressed_mouse_buttons_for_test(1);
        focus_app.on_focus_changed(false);
        assert_eq!(focus_app.interaction.pressed_mouse_buttons_for_test(), 0);
    }

    #[test]
    fn ctrl_hover_link_change_marks_app_dirty_for_idle_redraw() {
        use crate::renderer::TerminalUiLayout;

        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(80, 30, 1));
        feed_batch(&app.terminals[0], "\x1b[30;1Hsee https://example.com/x");
        app.interaction.update_layout(TerminalUiLayout {
            body: crate::search::Rect {
                x: 0.0,
                y: 100.0,
                w: 800.0,
                h: 600.0,
            },
            text_x: 8.0,
            char_w: 10.0,
            char_h: 20.0,
            visible_rows: 29,
            cols: 80,
            scroll_offset: 0.0,
            bottom_pad: 8.0,
            scale: 1.0,
            ..TerminalUiLayout::default()
        });
        app.pointer_x = 73.0;
        app.pointer_y = 682.0;

        app.dirty = false;
        app.on_modifiers(Modifiers::CONTROL);
        assert!(app.terminals[0].hovered_link().is_some());
        assert!(app.dirty);

        app.dirty = false;
        app.on_modifiers(Modifiers::CONTROL);
        assert!(!app.dirty);

        app.on_modifiers(Modifiers::empty());
        assert!(app.terminals[0].hovered_link().is_none());
        assert!(app.dirty);
    }

    #[test]
    fn focus_transition_clears_app_and_terminal_modifiers_and_stale_shortcuts() {
        let mut app = App::new();
        app.modifiers = Modifiers::CONTROL;
        app.interaction.set_modifiers(Modifiers::CONTROL);
        app.on_focus_changed(false);
        assert_eq!(app.modifiers, Modifiers::empty());
        assert_eq!(app.interaction.modifiers_for_test(), Modifiers::empty());
        app.on_focus_changed(true);
        assert_eq!(app.modifiers, Modifiers::empty());
        for key in [KeyCode::KeyT, KeyCode::KeyF, KeyCode::Digit4] {
            assert_eq!(global_shortcut(PhysicalKey::Code(key), app.modifiers), None,);
        }
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::F1), app.modifiers),
            Some(GlobalShortcut::ToggleSettings),
        );
    }

    #[test]
    fn settings_open_and_focus_loss_both_cancel_terminal_tab_drag() {
        let drag = || TabDragState {
            start_idx: 0,
            start_x: 10.0,
            current_x: 40.0,
            threshold_passed: true,
        };

        let mut settings_app = App::new();
        settings_app.terminal_tab_drag = Some(drag());
        settings_app.toggle_settings();
        assert!(settings_app.settings_open);
        assert!(settings_app.terminal_tab_drag.is_none());

        let mut focus_app = App::new();
        focus_app.terminal_tab_drag = Some(drag());
        focus_app.on_focus_changed(false);
        assert!(!focus_app.focused);
        assert!(focus_app.terminal_tab_drag.is_none());
    }

    #[test]
    fn settings_and_focus_loss_share_pointer_cancellation_path() {
        let source = include_str!("app.rs");
        let production = source
            .split(
                "
#[cfg(test)]",
            )
            .next()
            .unwrap_or(source);
        let shared = production
            .split("    fn cancel_pointer_interactions")
            .nth(1)
            .and_then(|tail| tail.split("    pub(crate) fn on_focus_changed").next())
            .expect("shared pointer cancellation helper must remain present");
        assert!(shared.contains("self.terminal_tab_drag = None;"));
        assert!(shared.contains("self.interaction.cancel_pointer_interaction(terminal);"));

        let focus = production
            .split("    pub(crate) fn on_focus_changed")
            .nth(1)
            .and_then(|tail| tail.split("    fn toggle_settings").next())
            .expect("focus lifecycle helper must remain present");
        assert!(focus.contains("self.cancel_pointer_interactions();"));

        let toggle = production
            .split("    fn toggle_settings")
            .nth(1)
            .and_then(|tail| tail.split("    fn close_settings").next())
            .expect("settings toggle must remain present");
        let cancel = toggle
            .find("self.cancel_pointer_interactions();")
            .expect("settings opening must use shared pointer cancellation");
        let activate_modal = toggle
            .find("self.settings_open = !self.settings_open;")
            .expect("settings toggle must activate the modal");
        assert!(cancel < activate_modal);
    }

    #[test]
    fn active_terminal_selection_clear_helper_removes_manual_selection() {
        let mut app = App::new();
        app.terminals.push(Terminal::new_for_test(80, 30, 1));
        crate::platform::lock_recover(&app.terminals[0].grid).set_selection(Some((1, 2, 5, 2)));

        assert!(app.clear_active_terminal_text_selection());
        assert!(
            crate::platform::lock_recover(&app.terminals[0].grid)
                .selection()
                .is_none()
        );
        assert!(!app.clear_active_terminal_text_selection());
    }

    #[test]
    fn left_press_clear_and_cursor_leave_are_routed_before_ui_short_circuits() {
        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        let mouse = production
            .split("    pub(crate) fn on_pointer_button")
            .nth(1)
            .and_then(|tail| tail.split("    pub(crate) fn on_scroll").next())
            .expect("mouse input routing must remain present");
        let clear = mouse
            .find("self.clear_active_terminal_text_selection()")
            .expect("left press must clear stale terminal selection");
        let settings = mouse
            .find("if self.settings_modal_active()")
            .expect("settings routing must remain present");
        let tabs = mouse
            .find("terminal_tab_hit_test")
            .expect("tab routing must remain present");
        assert!(clear < settings);
        assert!(clear < tabs);

        let moved = production
            .split("    pub(crate) fn on_pointer_motion")
            .nth(1)
            .and_then(|tail| tail.split("    pub(crate) fn on_pointer_leave").next())
            .expect("cursor moved routing must remain present");
        assert!(moved.contains("!self.interaction.terminal_selection_active()"));
        let left = production
            .split("    pub(crate) fn on_pointer_leave")
            .nth(1)
            .and_then(|tail| tail.split("    pub(crate) fn on_pointer_button").next())
            .expect("cursor leave routing must remain present");
        assert!(left.contains(".cursor_left("));
        assert!(left.contains("width as f32"));
        assert!(left.contains("height as f32"));
    }

    #[test]
    fn settings_animation_uses_real_dt_and_settles_without_permanent_redraw() {
        let (high_refresh, active) = settings_animation_step(0.0, true, 1.0 / 240.0);
        assert!(active);
        assert!(high_refresh > 0.0 && high_refresh < 0.1);
        let (idle_clamped, _) = settings_animation_step(0.0, true, animation_dt(1.0));
        assert!(idle_clamped < 0.2);
        let (settled_open, active) = settings_animation_step(0.9995, true, 0.016);
        assert_eq!(settled_open, 1.0);
        assert!(!active);
        let (settled_closed, active) = settings_animation_step(0.0005, false, 0.016);
        assert_eq!(settled_closed, 0.0);
        assert!(!active);
    }

    #[test]
    fn tab_drag_threshold_scales_without_quantizing_pointer_motion() {
        assert!(!drag_threshold_passed(100.0, 104.9, 1.0));
        assert!(drag_threshold_passed(100.0, 105.1, 1.0));
        assert!(!drag_threshold_passed(100.0, 106.0, 1.3333334));
        assert!(drag_threshold_passed(100.0, 107.0, 1.3333334));
    }

    #[test]
    fn tab_wheel_scroll_uses_vertical_wheel_for_horizontal_strip() {
        assert_eq!(
            tab_wheel_delta(ScrollDelta::Line { x: 0.0, y: 1.0 }, 1.0),
            -40.0
        );
        assert_eq!(
            tab_wheel_delta(ScrollDelta::Pixel { x: 0.0, y: -12.5 }, 1.0,),
            12.5
        );
    }
}
