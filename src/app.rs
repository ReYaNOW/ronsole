use crate::config::{
    AppConfig, DEFAULT_SCROLL_SENSITIVITY, DEFAULT_TERMINAL_BACKGROUND,
    DEFAULT_TERMINAL_BOLD_FOREGROUND, DEFAULT_TERMINAL_FONT_SIZE, DEFAULT_TERMINAL_FOREGROUND,
    DEFAULT_TERMINAL_PALETTE, RgbColor, SCROLL_SENSITIVITY_STEP, TERMINAL_FONT_SIZE_STEP,
};
use crate::input::{MULTI_CLICK_WINDOW, TerminalInteraction, terminal_scroll_animating};
use crate::input_types::{
    CursorKind, KeyCode, KeyInput, KeyState, Modifiers, PhysicalKey, PointerButton,
    PointerPosition, ScrollDelta,
};
use crate::launch::TerminalLaunchSpec;
use crate::panels::{Panel, PanelHit, grid_fits, grid_tiles, panel_hit};
use crate::platform::{
    KdeActivationWorker, OpenSink, kde_session_active, open_uri_allowed, open_uri_with,
};
use crate::platform::single_instance::BridgeResult;
use crate::renderer::{
    CloseConfirmHit, CloseConfirmView, SettingsColorField, SettingsColorView, SettingsHit,
    SettingsTab, TerminalTabHit, TerminalUiLayout, pty_dimension, settings_color_value,
    terminal_body_rect, terminal_palette,
};
use crate::runtime::{TabLabelIndex, TerminalRenderParams, WindowRuntime};
use crate::scroll::ScrollState;
use crate::search::{Rect, TerminalSearchHit};
use crate::single_line_input::SingleLineInput;
use crate::split::{
    PaneId, SplitAxis, SplitNode, layout_panes, leaf_count, pane_min, remove_leaf, split_leaf_at,
    successor,
};
use crate::tabs::{
    DRAG_AUTOSCROLL_EDGE_PX, TabDragState, active_index_after_move, active_index_after_remove,
    drag_autoscroll_delta, drag_autoscroll_speed, take_terminal_creation_number,
};
use crate::terminal::{
    SYNC_RESERVE_GRACE, Terminal, TerminalPresentationIntent, earliest_terminal_deadline,
    screen_hash, sync_reserve_wake,
};
use crate::terminal_process::{TerminalCleanupWorker, TerminalProcess, shutdown_terminal_processes};
use crate::wake::WakeHandle;
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
    SplitRight,
    SplitDown,
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
        PhysicalKey::Code(KeyCode::KeyD) if exact_global_modifiers(modifiers, true) => {
            Some(GlobalShortcut::SplitRight)
        }
        PhysicalKey::Code(KeyCode::KeyE) if exact_global_modifiers(modifiers, true) => {
            Some(GlobalShortcut::SplitDown)
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
    /// Button under the pointer: `Confirm`, `Cancel` or `None`.
    hovered: CloseConfirmHit,
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
            hovered: CloseConfirmHit::None,
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
    SplitTooSmall,
    SpawnFailed,
}

impl GridHint {
    fn text(self) -> &'static str {
        match self {
            Self::NoClaudeTabs => "Нет вкладок с Claude",
            Self::TooManyTabs => "Слишком много вкладок с Claude для окна",
            Self::SplitTooSmall => "Панель слишком мала для сплита",
            Self::SpawnFailed => "Не удалось запустить панель",
        }
    }
}

const GRID_HINT_DURATION: Duration = Duration::from_secs(2);

/// Stable identity of one tab in `App.tabs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TabId(u64);

impl TabId {
    fn new(raw: u64) -> Self {
        Self(raw)
    }
}

/// One terminal tab: a split tree over `App.terminals`, the focused pane and
/// the number the tab strip shows. The layout always contains `active_pane`.
struct Tab {
    id: TabId,
    layout: SplitNode,
    active_pane: PaneId,
    /// Display number of the founding terminal (`take_terminal_creation_number`).
    title_number: usize,
}

/// Whether `pane` is one of the leaves of this split subtree.
fn split_contains_pane(node: &SplitNode, pane: PaneId) -> bool {
    match node {
        SplitNode::Leaf(id) => *id == pane,
        SplitNode::Split { first, second, .. } => {
            split_contains_pane(first, pane) || split_contains_pane(second, pane)
        }
    }
}

/// Number of leaves in the subtree, optionally only those for `pane`.
#[cfg(any(debug_assertions, test))]
fn split_leaf_count(node: &SplitNode, pane: Option<PaneId>) -> usize {
    match node {
        SplitNode::Leaf(id) => usize::from(pane.is_none_or(|pane| pane == *id)),
        SplitNode::Split { first, second, .. } => {
            split_leaf_count(first, pane) + split_leaf_count(second, pane)
        }
    }
}

/// Collects the leaf panes of `node` in tree order. Allocates the caller's
/// vector; used only by the close path, never in a frame.
fn collect_split_panes(node: &SplitNode, panes: &mut Vec<PaneId>) {
    match node {
        SplitNode::Leaf(pane) => panes.push(*pane),
        SplitNode::Split { first, second, .. } => {
            collect_split_panes(first, panes);
            collect_split_panes(second, panes);
        }
    }
}

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
    /// Tabs in strip order; `tabs[active_tab]` owns `active_terminal`.
    tabs: Vec<Tab>,
    active_tab: usize,
    /// One strip label per tab, rebuilt from `tabs` before every render.
    tab_labels: Vec<TabLabelIndex>,
    next_tab_id: u64,
    /// Source of every `PaneId`; ids are never reused after a pane closes.
    next_pane_id: u64,
    /// Grows on structural tab/pane changes; `layout_generation` additionally
    /// grows when any saved split ratio changes.
    structure_generation: u64,
    layout_generation: u64,
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
    tab_waiters: VecDeque<tab_delivery::TabWaiter>,
    /// Reusable buffer for `Terminal::observe_dev_channel_warning`.
    dev_warning_scratch: String,
    /// Outside grid mode, a frame was skipped by `withhold_active_frame`; redraw
    /// once the active tab's hiding ends. Grid mode never sets it.
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
    /// Outside grid mode, the body of every pane of the active tab, in tree
    /// order, as laid out for the current frame. Reused by every rebuild.
    pane_rects: Vec<(PaneId, Rect)>,
    /// Layouts of `panels` from the last render, in `panels` order.
    panel_layouts: Vec<TerminalUiLayout>,
    /// `None` while the ordinary single-panel view is shown.
    claude_grid: Option<ClaudeGrid>,
    /// Grows whenever the grid composition changes: its members, their order,
    /// or grid mode opening and closing.
    grid_generation: u64,
    /// `layout_generation` at the time `panel_layouts` were rendered.
    panel_layouts_generation: u64,
    /// `grid_generation` at the time `panel_layouts` were rendered. Grid input
    /// matches against tile geometry only, so a focus change never stales it.
    panel_layouts_grid_generation: u64,
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
    /// Earliest end of the banner hiding among grid tiles that the last grid
    /// frame drew with `content_hidden`; grid mode only.
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
            tabs: Vec::new(),
            active_tab: 0,
            tab_labels: Vec::new(),
            next_tab_id: 1,
            next_pane_id: 1,
            structure_generation: 0,
            layout_generation: 0,
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
            tab_waiters: VecDeque::new(),
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
            pane_rects: Vec::new(),
            panel_layouts: Vec::new(),
            claude_grid: None,
            grid_generation: 0,
            panel_layouts_generation: 0,
            panel_layouts_grid_generation: 0,
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

    /// Allocates the next stable pane id; the only source of `PaneId`.
    fn issue_pane_id(&mut self) -> PaneId {
        let id = PaneId::new(self.next_pane_id);
        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        id
    }

    /// Allocates the next stable tab id; the only source of `TabId`.
    fn issue_tab_id(&mut self) -> TabId {
        let id = TabId::new(self.next_tab_id);
        self.next_tab_id = self.next_tab_id.wrapping_add(1);
        id
    }

    fn bump_layout_generation(&mut self) {
        self.layout_generation = self.layout_generation.wrapping_add(1);
    }

    /// A structural change (tree shape, tab composition/order or the active
    /// tab) invalidates both the panels and the saved layout.
    fn bump_structure_generation(&mut self) {
        self.structure_generation = self.structure_generation.wrapping_add(1);
        self.bump_layout_generation();
    }

    fn tab_index_of_pane(&self, pane: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| split_contains_pane(&tab.layout, pane))
    }

    fn terminal_index_of_pane(&self, pane: PaneId) -> Option<usize> {
        self.terminals
            .iter()
            .position(|terminal| terminal.pane_id() == pane)
    }

    /// The single focus operation: `pane` becomes the active pane of its tab,
    /// its tab becomes active, and `pane` is presented and reported as focused.
    /// Everything `activate_ready_terminal` did before lives here now.
    fn activate_pane(&mut self, pane: PaneId, reveal_tail: bool) {
        let Some(tab_index) = self.tab_index_of_pane(pane) else {
            return;
        };
        let Some(index) = self.terminal_index_of_pane(pane) else {
            return;
        };
        if tab_index != self.active_tab {
            self.active_tab = tab_index;
            self.bump_structure_generation();
        }
        self.tabs[tab_index].active_pane = pane;
        if index == self.active_terminal && self.active_terminal_presented {
            self.pending_tab_reveal = Some(reveal_tail);
            self.mark_dirty();
            #[cfg(debug_assertions)]
            self.debug_assert_invariants();
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
        #[cfg(debug_assertions)]
        self.debug_assert_invariants();
    }

    /// Background tab focus change: only `Tab.active_pane` moves, without
    /// touching `active_tab`, `active_terminal`, `interaction` or focus state.
    fn select_tab_pane_in_background(&mut self, tab: TabId, pane: PaneId) {
        let Some(index) = self.tabs.iter().position(|entry| entry.id == tab) else {
            return;
        };
        if split_contains_pane(&self.tabs[index].layout, pane) {
            self.tabs[index].active_pane = pane;
        }
    }

    /// Thin adapter over the pane identity: existing callers keep their index.
    fn activate_ready_terminal(&mut self, index: usize, reveal_tail: bool) {
        let Some(pane) = self.terminals.get(index).map(Terminal::pane_id) else {
            return;
        };
        self.activate_pane(pane, reveal_tail);
    }

    /// Checks the tab model: every terminal owns exactly one split leaf, every
    /// leaf names a live terminal, and the active tab's pane is the active
    /// terminal. Production calls it only under `debug_assertions`; tests run
    /// in a release profile, so its body uses `assert!` and tests call it.
    #[cfg(any(debug_assertions, test))]
    fn debug_assert_invariants(&self) {
        if self.terminals.is_empty() {
            assert!(self.tabs.is_empty(), "terminals are gone but tabs remain");
            return;
        }
        assert!(!self.tabs.is_empty(), "terminals exist without a tab");
        assert!(
            self.active_tab < self.tabs.len(),
            "active tab index {} is out of range",
            self.active_tab
        );
        assert!(
            self.active_terminal < self.terminals.len(),
            "active terminal index {} is out of range",
            self.active_terminal
        );
        assert_eq!(
            self.tabs[self.active_tab].active_pane,
            self.terminals[self.active_terminal].pane_id(),
            "active tab pane and active terminal disagree"
        );
        let mut leaves = 0;
        for tab in &self.tabs {
            leaves += split_leaf_count(&tab.layout, None);
            assert!(tab.title_number > 0, "a tab must carry its title number");
            assert!(
                split_contains_pane(&tab.layout, tab.active_pane),
                "a tab's active pane is not one of its leaves"
            );
        }
        assert_eq!(
            leaves,
            self.terminals.len(),
            "split leaves and terminals disagree"
        );
        for (position, terminal) in self.terminals.iter().enumerate() {
            let pane = terminal.pane_id();
            // Two terminals with one pane id would each claim the same leaf, so
            // the per-terminal count below cannot see a leaf that no terminal
            // owns. Reject duplicates before comparing the two multisets.
            assert!(
                self.terminals[position + 1..]
                    .iter()
                    .all(|other| other.pane_id() != pane),
                "two terminals share pane id {}",
                pane.raw()
            );
            let owned: usize = self
                .tabs
                .iter()
                .map(|tab| split_leaf_count(&tab.layout, Some(pane)))
                .sum();
            assert_eq!(owned, 1, "a terminal must own exactly one split leaf");
        }
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
        let Some(pane) = self.tabs.get(index).map(|tab| tab.active_pane) else {
            return;
        };
        self.cancel_terminal_presentation_intents();
        self.activate_pane(pane, false);
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

    fn add_terminal_with_launch(&mut self, launch: TerminalLaunchSpec) -> Option<usize> {
        // Leaves grid mode at the request, before the runtime check can return early.
        self.exit_claude_grid(None);
        let wake = self.runtime.as_ref()?.wake_handle();
        Some(self.spawn_terminal_tab(wake, launch, crate::terminal::DEFAULT_SPAWN_GRID))
    }

    /// Spawns the first tab before EGL starts, with its grid and PTY at the size
    /// the first frame lays out for a `physical` target at `scale`: the shell
    /// draws its first prompt at the final width while the GL driver initializes,
    /// and no resize follows. Output wakes `pre_gl.wake`, the runtime's eventfd.
    pub(crate) fn spawn_initial_terminal(
        &mut self,
        pre_gl: &crate::runtime::PreGlResources,
        physical: (u32, u32),
        scale: f32,
        launch: TerminalLaunchSpec,
    ) {
        let size = pre_gl.fonts.first_frame_grid_size(
            physical.0,
            physical.1,
            scale,
            self.config.terminal_font_size,
        );
        let grid = (pty_dimension(size.cols), pty_dimension(size.rows));
        let _ = self.spawn_terminal_tab(pre_gl.wake.clone(), launch, grid);
    }

    /// The one tab factory: spawns the shell at `grid` and queues its activation.
    fn spawn_terminal_tab(
        &mut self,
        wake: WakeHandle,
        launch: TerminalLaunchSpec,
        grid: (u16, u16),
    ) -> usize {
        self.cancel_terminal_presentation_intents();
        let display_number = take_terminal_creation_number(&mut self.next_terminal_creation_number);
        let pane_id = self.issue_pane_id();
        let index = self.spawn_terminal(wake, display_number, launch, grid, pane_id);
        let id = self.issue_tab_id();
        self.tabs.push(Tab {
            id,
            layout: SplitNode::Leaf(pane_id),
            active_pane: pane_id,
            title_number: display_number as usize,
        });
        self.bump_structure_generation();
        self.request_terminal_activation(index, true);
        self.request_frame();
        index
    }

    /// Spawns one shell at `grid` under the caller's pane id and display
    /// number and returns its index in `terminals`. The caller owns the tab and
    /// the split tree.
    fn spawn_terminal(
        &mut self,
        wake: WakeHandle,
        display_number: u64,
        mut launch: TerminalLaunchSpec,
        grid: (u16, u16),
        pane_id: PaneId,
    ) -> usize {
        launch.shell.clone_from(&self.shell_override);
        let terminal = Terminal::spawn_sized(
            Some(wake),
            display_number,
            launch,
            self.new_terminal_palette(),
            grid,
            pane_id,
        );
        let index = self.terminals.len();
        self.terminals.push(terminal);
        index
    }

    /// Splits the active pane of the active tab along `axis`: the new pane gets
    /// its own shell in the active pane's directory. Refuses, showing a hint,
    /// when a pane of the split would fall below `pane_min`; nothing is spawned
    /// and the tree does not change then.
    fn split_active_pane(&mut self, axis: SplitAxis) -> bool {
        let metrics = self.grid_metrics;
        let tab_index = self.active_tab;
        let Some(tab) = self.tabs.get(tab_index) else {
            return false;
        };
        let active_pane = tab.active_pane;
        // The split is laid out on a copy first: a pane below the minimum must
        // not spawn a shell and must not reach the real tree.
        let mut preview = tab.layout.clone();
        let pane_id = self.issue_pane_id();
        if !split_leaf_at(&mut preview, active_pane, axis, pane_id) {
            return false;
        }
        let body = terminal_body_rect(metrics.width, metrics.height, metrics.scale);
        let (min_w, min_h) = pane_min(metrics.char_w, metrics.char_h, metrics.scale);
        let mut scratch: Vec<(PaneId, Rect)> = Vec::with_capacity(leaf_count(&preview));
        layout_panes(
            &preview,
            body,
            metrics.scale,
            metrics.char_w,
            metrics.char_h,
            &mut scratch,
        );
        if scratch
            .iter()
            .any(|&(_, rect)| rect.w < min_w || rect.h < min_h)
        {
            self.set_grid_hint(GridHint::SplitTooSmall, Instant::now());
            return false;
        }
        let Some(wake) = self.runtime.as_ref().map(|runtime| runtime.wake_handle()) else {
            return false;
        };
        let inherited_directory = self
            .terminals
            .get(self.active_terminal)
            .and_then(|terminal| terminal.osc7_cwd.clone());
        let display_number = take_terminal_creation_number(&mut self.next_terminal_creation_number);
        let index = self.spawn_terminal(
            wake,
            display_number,
            TerminalLaunchSpec {
                inherited_directory,
                ..TerminalLaunchSpec::default()
            },
            crate::terminal::DEFAULT_SPAWN_GRID,
            pane_id,
        );
        // A failed spawn leaves a process-less error terminal: drop it and keep
        // the tree, the focus and the pending activation untouched.
        if !self.terminals[index].has_process() {
            self.terminals.pop();
            self.set_grid_hint(GridHint::SpawnFailed, Instant::now());
            return false;
        }
        self.cancel_terminal_presentation_intents();
        let applied = split_leaf_at(&mut self.tabs[tab_index].layout, active_pane, axis, pane_id);
        debug_assert!(applied, "the preview split already succeeded");
        if !applied {
            return false;
        }
        self.activate_pane(pane_id, false);
        self.bump_structure_generation();
        self.request_frame();
        true
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
    /// redraw once hiding has ended: outside grid mode that is the frame that
    /// `withhold_active_frame` skipped, in grid mode the `withheld_until`
    /// deadline (the next grid frame recomputes it).
    fn advance_dev_channel_warnings(&mut self, now: Instant) {
        for terminal in &mut self.terminals {
            terminal.observe_dev_channel_warning(now, &mut self.dev_warning_scratch, |pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
            terminal.observe_dev_channel_banner(|pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
        }
        if self.grid_open() || self.active_tab_split() {
            if self.withheld_until.is_some_and(|until| now >= until) {
                self.withheld_until = None;
                self.request_frame();
            }
        } else if self.frame_withheld && !self.active_dev_warning_hidden(now) {
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

    /// Grid counterpart of `withhold_active_frame`: applies the warning and
    /// banner hiding to every tile terminal and sets `content_hidden` on its
    /// panel, so the frame is drawn with only the hidden tile body blank. Must
    /// run after `rebuild_panels`, which resets `content_hidden`. Records the
    /// earliest hiding end in `withheld_until` for the loop to wake at.
    fn observe_grid_dev_warnings(&mut self, now: Instant) {
        let mut withheld_until: Option<Instant> = None;
        for panel in &mut self.panels {
            let Some(terminal) = self.terminals.get_mut(panel.terminal) else {
                panel.content_hidden = false;
                continue;
            };
            terminal.observe_dev_channel_warning(now, &mut self.dev_warning_scratch, |pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
            terminal.observe_dev_channel_banner(|pgrp| {
                pgrp.and_then(crate::tab_input::dev_channel_claude_in_group)
            });
            panel.content_hidden = terminal.dev_warning_frame_hidden(now);
            if panel.content_hidden {
                if let Some(until) = terminal.dev_warning_hidden_until() {
                    withheld_until = Some(withheld_until.map_or(until, |earliest| earliest.min(until)));
                }
            }
        }
        self.withheld_until = withheld_until;
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

    /// Thin adapter over the tab identity: existing callers keep their terminal
    /// index, the close itself belongs to `close_tab`. Returns true exactly when
    /// the window must close.
    fn close_terminal_tab_at(&mut self, index: usize) -> bool {
        let Some(pane) = self.terminals.get(index).map(Terminal::pane_id) else {
            return false;
        };
        let Some(tab_index) = self.tab_index_of_pane(pane) else {
            return false;
        };
        let tab = self.tabs[tab_index].id;
        self.close_tab(tab)
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
        self.refresh_close_confirm_hover();
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

    /// Reads foreground job names only at the close moment. Returns (pane title,
    /// job name) for every pane in the target that runs a job; empty when none
    /// does, so the close proceeds without a dialog.
    fn busy_close_targets(&self, target: CloseTarget) -> Vec<(String, String)> {
        // A tab target lists every pane of that tab. Its index names one of the
        // tab's terminals; the tab itself comes from the split tree.
        let tab_filter = match target {
            CloseTarget::Window => None,
            CloseTarget::Tab(index) => {
                let Some(terminal) = self.terminals.get(index) else {
                    return Vec::new();
                };
                let Some(tab) = self.tab_index_of_pane(terminal.pane_id()) else {
                    return Vec::new();
                };
                Some(tab)
            }
        };
        let mut busy = Vec::new();
        for terminal in &self.terminals {
            if let Some(tab) = tab_filter
                && self.tab_index_of_pane(terminal.pane_id()) != Some(tab)
            {
                continue;
            }
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

    /// Closes one pane: the process goes to the cleanup worker first, and only
    /// an accepted cleanup drops the leaf. A refused cleanup leaves the tree,
    /// the terminal storage and the focus untouched. Returns true when the
    /// window must close.
    fn close_pane(&mut self, pane: PaneId) -> bool {
        let Some(index) = self.terminal_index_of_pane(pane) else {
            return false;
        };
        let Some(tab_index) = self.tab_index_of_pane(pane) else {
            return false;
        };
        let tab = self.tabs[tab_index].id;
        let was_active_tab = tab_index == self.active_tab;
        let last_pane_of_tab = leaf_count(&self.tabs[tab_index].layout) == 1;
        // The last pane of the last tab is shut down synchronously instead of
        // waiting for the cleanup worker; that branch cannot refuse.
        let last_pane_of_window = last_pane_of_tab && self.tabs.len() == 1;
        let grid_successor = (self.claude_grid.is_some() && index == self.active_terminal)
            .then(|| self.grid_successor_after_removal(&[index]))
            .flatten();
        if !last_pane_of_window {
            let process = self.terminals[index].take_process_for_cleanup();
            if let Some(process) = process
                && let Err(process) = self.try_schedule_terminal_cleanup(process)
            {
                self.terminals[index].restore_process_after_cleanup_backpressure(process);
                return false;
            }
        }
        let tree_successor = if last_pane_of_tab {
            self.remove_last_pane_of_tab(tab);
            None
        } else {
            self.remove_terminal_at(index)
        };
        if let Some(next) = tree_successor
            && let Some(tab_index) = self.tabs.iter().position(|entry| entry.id == tab)
        {
            self.tabs[tab_index].active_pane = next;
        }
        self.terminal_tab_drag = None;
        self.cancel_close_dialog();
        self.pending_tab_reveal = Some(false);
        self.focus_after_removal(tab, was_active_tab, tree_successor, grid_successor);
        self.request_frame();
        self.sync_claude_grid(Instant::now());
        self.terminals.is_empty()
    }

    /// Closes a whole tab: every pane's process is handed to the cleanup worker
    /// first, then the panes go through the internal removal path without a
    /// second dialog. A refused cleanup stops the close; the panes removed so far
    /// stay removed. Returns true when the window must close.
    fn close_tab(&mut self, tab: TabId) -> bool {
        let Some(tab_index) = self.tabs.iter().position(|entry| entry.id == tab) else {
            return false;
        };
        let was_active_tab = tab_index == self.active_tab;
        let mut panes = Vec::new();
        collect_split_panes(&self.tabs[tab_index].layout, &mut panes);
        // The tile that holds the focus now: §6.4 keeps the focus on it when a
        // refused cleanup stops the close before its own pane leaves.
        let focused_pane = self
            .terminals
            .get(self.active_terminal)
            .map(Terminal::pane_id);
        let mut tree_successor = None;
        for pane in panes {
            let Some(index) = self.terminal_index_of_pane(pane) else {
                continue;
            };
            let Some(tab_index) = self.tabs.iter().position(|entry| entry.id == tab) else {
                break;
            };
            let last_pane_of_tab = leaf_count(&self.tabs[tab_index].layout) == 1;
            if !(last_pane_of_tab && self.tabs.len() == 1) {
                let process = self.terminals[index].take_process_for_cleanup();
                if let Some(process) = process
                    && let Err(process) = self.try_schedule_terminal_cleanup(process)
                {
                    self.terminals[index].restore_process_after_cleanup_backpressure(process);
                    break;
                }
            }
            if last_pane_of_tab {
                self.remove_last_pane_of_tab(tab);
                break;
            }
            if let Some(next) = self.remove_terminal_at(index) {
                tree_successor = Some(next);
                if let Some(tab_index) = self.tabs.iter().position(|entry| entry.id == tab) {
                    self.tabs[tab_index].active_pane = next;
                }
            }
        }
        // The successor is picked over the tiles that actually survived: a
        // refusal leaves the focused tile alive and the focus alone, and only a
        // vanished tile hands the focus to its grid neighbour.
        let grid_successor = if self.claude_grid.is_some()
            && was_active_tab
            && focused_pane.is_none_or(|pane| self.terminal_index_of_pane(pane).is_none())
        {
            self.grid_successor_after_focused_removal()
        } else {
            None
        };
        self.terminal_tab_drag = None;
        self.cancel_close_dialog();
        self.pending_tab_reveal = Some(false);
        self.focus_after_removal(tab, was_active_tab, tree_successor, grid_successor);
        self.request_frame();
        self.sync_claude_grid(Instant::now());
        self.terminals.is_empty()
    }

    /// Drops the only pane of `tab` together with the tab. The caller has
    /// already scheduled the process cleanup; the last terminal of the window is
    /// shut down here, synchronously within the bounded grace.
    fn remove_last_pane_of_tab(&mut self, tab: TabId) {
        let Some(tab_index) = self.tabs.iter().position(|entry| entry.id == tab) else {
            return;
        };
        let pane = self.tabs[tab_index].active_pane;
        let Some(index) = self.terminal_index_of_pane(pane) else {
            return;
        };
        if self.tabs.len() == 1 {
            self.terminals[index].shutdown();
        }
        self.remove_terminal_at(index);
        self.tabs.remove(tab_index);
        self.active_tab = active_index_after_remove(self.active_tab, tab_index, self.tabs.len());
        self.bump_structure_generation();
    }

    /// Low-level removal of one pane: drops the terminal, collapses the split
    /// leaf and grows the structural generations. Returns the tree successor of
    /// the tab when the removed pane was that tab's active pane. `Tab.active_pane`
    /// is left to the caller, which sets it in the same close call; no frame or
    /// event runs in between.
    fn remove_terminal_at(&mut self, index: usize) -> Option<PaneId> {
        if index >= self.terminals.len() {
            return None;
        }
        let pane = self.terminals[index].pane_id();
        let Some(tab_index) = self.tab_index_of_pane(pane) else {
            return None;
        };
        // The successor is read before the leaf leaves the tree.
        let tree_successor = (self.tabs[tab_index].active_pane == pane)
            .then(|| successor(&self.tabs[tab_index].layout, pane))
            .flatten();
        if index == self.active_terminal {
            // The focused terminal reports the focus loss; the successor reports
            // the focus gain when the caller activates it.
            if self.active_terminal_presented {
                self.send_terminal_focus_transition(Some(index), None);
            }
            self.interaction
                .reset_for_terminal_switch(&mut self.terminals[index]);
            self.active_terminal_presented = false;
        }
        self.terminals.remove(index);
        if index < self.active_terminal {
            self.active_terminal -= 1;
        } else if index == self.active_terminal {
            self.active_terminal = self
                .active_terminal
                .min(self.terminals.len().saturating_sub(1));
        }
        // A root leaf cannot collapse; `remove_last_pane_of_tab` drops its tab.
        remove_leaf(&mut self.tabs[tab_index].layout, pane);
        self.bump_structure_generation();
        tree_successor
    }

    /// §6.4 focus procedure after a close. `removed` names the tab the panes
    /// left, `tree_successor` is the successor `remove_terminal_at` reported and
    /// `grid_successor` the tile picked before the first removal. Both
    /// successors are `None` when the focus does not move.
    fn focus_after_removal(
        &mut self,
        removed: TabId,
        was_active_tab: bool,
        tree_successor: Option<PaneId>,
        grid_successor: Option<usize>,
    ) {
        if !self.tabs.iter().any(|tab| tab.id == removed) {
            // 1. The tab is gone.
            if was_active_tab {
                self.focus_after_tab_closed(grid_successor);
            }
            // 1b: a background tab leaves the global focus alone; its tiles
            // leave the grid with the member refresh.
        } else if self.claude_grid.is_some() {
            // 2a. The tab survives and the grid is open.
            if let Some(index) = grid_successor {
                if let Some(next) = tree_successor {
                    // Focus moves to another tab's tile: the closed tab only
                    // keeps its own pane choice, without focus reporting.
                    self.select_tab_pane_in_background(removed, next);
                }
                self.focus_tile_after_removal(index);
            } else {
                self.collect_claude_members();
                if self.grid_members_scratch.is_empty() {
                    // No tile left: the grid closes and 2b decides the focus.
                    self.exit_claude_grid(None);
                    self.focus_after_pane_closed(removed, was_active_tab, tree_successor);
                } else if let Some(next) = tree_successor {
                    // The pane was not the focused tile: focus stays.
                    self.select_tab_pane_in_background(removed, next);
                }
            }
        } else {
            // 2b. The tab survives and the grid is closed.
            self.focus_after_pane_closed(removed, was_active_tab, tree_successor);
        }
        #[cfg(debug_assertions)]
        self.debug_assert_invariants();
    }

    /// 1a. The active tab disappeared: the next tile keeps the grid focus, else
    /// the grid closes onto the neighbour tab, and a window without tabs closes.
    fn focus_after_tab_closed(&mut self, grid_successor: Option<usize>) {
        if self.claude_grid.is_some() {
            if let Some(index) = grid_successor {
                self.focus_tile_after_removal(index);
                return;
            }
            self.exit_claude_grid(None);
        }
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let pane = tab.active_pane;
        self.focus_pane_after_removal(pane);
    }

    /// 2b. The pane left a surviving tab: the active tab activates the tree
    /// successor, a background tab only retargets its active pane.
    fn focus_after_pane_closed(
        &mut self,
        removed: TabId,
        was_active_tab: bool,
        tree_successor: Option<PaneId>,
    ) {
        let Some(next) = tree_successor else {
            return;
        };
        if was_active_tab {
            self.focus_pane_after_removal(next);
        } else {
            self.select_tab_pane_in_background(removed, next);
        }
    }

    /// Focus on a grid tile: the neighbour takes the focus and reports it.
    fn focus_tile_after_removal(&mut self, index: usize) {
        if index >= self.terminals.len() {
            return;
        }
        self.cancel_terminal_presentation_intents();
        self.active_terminal = index;
        self.activate_ready_terminal(index, false);
    }

    /// Focus on a pane of the tree: through `activate_pane` when the pane can
    /// present now, otherwise as a deferred activation. The deferred path moves
    /// the logical focus at once — the tab, its active pane and the active
    /// terminal name the choice — while the presentation waits.
    fn focus_pane_after_removal(&mut self, pane: PaneId) {
        let Some(index) = self.terminal_index_of_pane(pane) else {
            return;
        };
        if self.terminals[index].presentation_ready() {
            self.cancel_terminal_presentation_intents();
            self.activate_pane(pane, false);
        } else {
            self.select_pane_for_deferred_activation(pane, index);
            self.request_terminal_activation(index, false);
        }
    }

    /// Points the focus at `pane` without presenting it. `active_terminal_presented`
    /// stays false and no focus report is written, so the `activate_pane` that
    /// runs when the pane is ready still presents it and reports the focus gain.
    fn select_pane_for_deferred_activation(&mut self, pane: PaneId, index: usize) {
        if let Some(tab_index) = self.tab_index_of_pane(pane) {
            if tab_index != self.active_tab {
                self.active_tab = tab_index;
                self.bump_structure_generation();
            }
            self.tabs[tab_index].active_pane = pane;
        }
        self.active_terminal = index;
        self.active_terminal_presented = false;
    }

    fn close_confirm_hit_test(&self, x: f32, y: f32) -> CloseConfirmHit {
        let (Some(confirm), Some(runtime)) = (self.close_confirm.as_ref(), self.runtime.as_ref())
        else {
            return CloseConfirmHit::None;
        };
        runtime.close_confirm_hit_test(confirm.line_count(), x, y)
    }

    /// Re-hit-tests the dialog at the stored pointer, e.g. after motion or a resize.
    fn refresh_close_confirm_hover(&mut self) {
        if self.close_confirm.is_some() {
            self.set_close_confirm_hover(
                self.close_confirm_hit_test(self.pointer_x, self.pointer_y),
            );
        }
    }

    /// Stores the dialog button under the pointer. Only a change of the hovered
    /// button asks for a frame; motion within one button or over the panel does not.
    fn set_close_confirm_hover(&mut self, hit: CloseConfirmHit) {
        let hovered = match hit {
            CloseConfirmHit::Confirm | CloseConfirmHit::Cancel => hit,
            CloseConfirmHit::None | CloseConfirmHit::Outside => CloseConfirmHit::None,
        };
        let Some(confirm) = self.close_confirm.as_mut() else {
            return;
        };
        if confirm.hovered != hovered {
            confirm.hovered = hovered;
            self.request_frame();
        }
    }

    /// Moves the tab at `from` to `to`. Panes travel with their tab because a
    /// tab owns the whole split tree; `terminals` keeps its storage order, so
    /// `active_terminal` and every pane identity stay valid.
    fn reorder_terminal_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active_tab = active_index_after_move(self.active_tab, from, to);
        self.bump_structure_generation();
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
        let changed = if self.grid_open() {
            self.update_grid_link_hover(modal_active, x, y)
        } else {
            // Grid mode is closed: a link still hovered in a tile is dropped first.
            let tile_cleared = self
                .hovered_panel_terminal
                .take()
                .and_then(|index| self.terminals.get_mut(index))
                .is_some_and(|terminal| terminal.set_hovered_link(None));
            let active_changed = match self.terminals.get_mut(self.active_terminal) {
                Some(terminal) if modal_active => terminal.set_hovered_link(None),
                Some(terminal) => self.interaction.update_link_hover(terminal, x, y, self.interaction.layout),
                None => false,
            };
            tile_cleared | active_changed
        };
        if changed {
            self.mark_dirty();
        }
        self.refresh_cursor_icon();
    }

    /// Grid-mode hover: only the tile under the pointer, with its own layout,
    /// may hold a link. Outdated layouts skip the hover entirely.
    fn update_grid_link_hover(&mut self, modal_active: bool, x: f32, y: f32) -> bool {
        if self.panel_layouts_grid_generation != self.grid_generation {
            return false;
        }
        let target = match panel_hit(&self.panels, x, y) {
            Some(PanelHit::Body(index)) if !modal_active => {
                self.panels.get(index).map(|panel| (index, panel.terminal))
            }
            _ => None,
        };
        let next = target.map(|(_, terminal)| terminal);
        let previous = std::mem::replace(&mut self.hovered_panel_terminal, next);
        let mut changed = false;
        if previous != next
            && let Some(index) = previous
            && let Some(terminal) = self.terminals.get_mut(index)
        {
            changed |= terminal.set_hovered_link(None);
        }
        match target {
            Some((index, terminal_index)) => {
                let Some(layout) = self.panel_layouts.get(index).copied() else {
                    return changed;
                };
                if let Some(terminal) = self.terminals.get_mut(terminal_index) {
                    changed |= self.interaction.update_link_hover(terminal, x, y, layout);
                }
            }
            None => {
                // No tile body under the pointer: the active tab must not keep a link.
                if let Some(terminal) = self.terminals.get_mut(self.active_terminal) {
                    changed |= terminal.set_hovered_link(None);
                }
            }
        }
        changed
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
        // Other visible grid tiles keep the frame loop running while their smooth scroll moves.
        let grid_tile_animation = self.panels.iter().any(|panel| {
            panel.terminal != self.active_terminal
                && self
                    .terminals
                    .get(panel.terminal)
                    .is_some_and(terminal_scroll_animating)
        });
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
        self.animation_active =
            terminal_animation || grid_tile_animation || tab_animation || settings_animation;
    }

    /// Future instant at which a hidden dev-channels banner must be redrawn:
    /// the earliest panel's hiding end in grid mode and in a split tab,
    /// otherwise the active tab's.
    fn dev_warning_wake_deadline(&self, now: Instant) -> Option<Instant> {
        let deadline = if self.grid_open() || self.active_tab_split() {
            self.withheld_until
        } else {
            self.terminals
                .get(self.active_terminal)
                .and_then(Terminal::dev_warning_hidden_until)
        };
        deadline.filter(|deadline| *deadline > now)
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
                .chain(self.dev_warning_wake_deadline(now))
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

    /// Whether the window handles `shortcut` now. Pane commands are not
    /// intercepted while grid mode shows tiles: there every pane key belongs to
    /// the focused tile's terminal.
    #[inline]
    fn shortcut_intercepted(&self, shortcut: GlobalShortcut) -> bool {
        match shortcut {
            GlobalShortcut::SplitRight | GlobalShortcut::SplitDown => self.claude_grid.is_none(),
            _ => true,
        }
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
            GlobalShortcut::SplitRight => {
                self.split_active_pane(SplitAxis::SideBySide);
            }
            GlobalShortcut::SplitDown => {
                self.split_active_pane(SplitAxis::Stacked);
            }
        }
        false
    }

    /// App-exit kill barrier: every tab session (open, pending, and already in
    /// the cleanup worker) gets the same bounded SIGTERM grace concurrently and
    /// is SIGKILLed and reaped before this returns, so no tab process outlives
    /// Ronsole.
    fn shutdown_all(&mut self) {
        self.tab_waiters.clear();
        let mut processes: Vec<TerminalProcess> = self
            .terminals
            .iter_mut()
            .filter_map(Terminal::take_process_for_cleanup)
            .collect();
        processes.extend(self.pending_terminal_cleanup.drain(..));
        shutdown_terminal_processes(&mut processes);
        drop(processes);
        self.terminals.clear();
        self.tabs.clear();
        self.active_tab = 0;
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
            if self.terminals[index].is_closed() {
                let pane = self.terminals[index].pane_id();
                if self.close_pane(pane) {
                    return true;
                }
            }
        }
        false
    }
}

impl App {
    /// Installs the runtime. The first tab already runs (`spawn_initial_terminal`);
    /// the dirty flag set here draws the output it produced while EGL started.
    fn on_runtime_ready(&mut self, runtime: WindowRuntime, width: u32, height: u32) {
        self.zero_sized = width == 0 || height == 0;
        if std::env::var_os("RONSOLE_GL_DIAGNOSTICS")
            .is_some_and(|value| value != std::ffi::OsStr::new("0"))
        {
            eprintln!("Ronsole graphics:\n{}", runtime.diagnostics_report());
        }
        self.runtime = Some(runtime);
        self.refresh_grid_metrics();
        self.last_frame = Instant::now();
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

    /// The only writer of `grid_metrics`. Any change starts new generations at
    /// once: the panels and their layouts of the last frame were computed from
    /// the old metrics, so input that arrives before the next frame must not be
    /// matched against that geometry. Grid mode also starts a new composition
    /// generation.
    fn set_grid_metrics(&mut self, metrics: GridMetrics) {
        if metrics != self.grid_metrics {
            if self.claude_grid.is_some() {
                self.bump_grid_generation();
            } else {
                self.bump_structure_generation();
            }
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

    /// Ordinary mode: one panel per pane of the active tab, laid out from its
    /// split tree. Grid mode: one tile per member, in tab order.
    fn rebuild_panels(&mut self) {
        let metrics = self.grid_metrics;
        let body = terminal_body_rect(metrics.width, metrics.height, metrics.scale);
        self.panels.clear();
        if let Some(grid) = self.claude_grid.as_ref() {
            grid_tiles(body, grid.members.len(), metrics.scale, &mut self.grid_tile_rects);
            for (&terminal, &(header, tile_body)) in grid.members.iter().zip(&self.grid_tile_rects)
            {
                self.panels.push(Panel {
                    terminal,
                    body: tile_body,
                    header: Some(header),
                    content_hidden: false,
                });
            }
            self.pane_rects.clear();
            return;
        }
        if self.tabs.get(self.active_tab).is_none() {
            self.pane_rects.clear();
            self.panels.push(Panel {
                terminal: self.active_terminal,
                body,
                header: None,
                content_hidden: false,
            });
            return;
        }
        let active_tab = self.active_tab;
        layout_panes(
            &self.tabs[active_tab].layout,
            body,
            metrics.scale,
            metrics.char_w,
            metrics.char_h,
            &mut self.pane_rects,
        );
        for &(pane, pane_body) in &self.pane_rects {
            let Some(terminal) = self.terminal_index_of_pane(pane) else {
                continue;
            };
            self.panels.push(Panel {
                terminal,
                body: pane_body,
                header: None,
                content_hidden: false,
            });
        }
    }

    /// Whether the active tab owns more than one pane.
    fn active_tab_split(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .is_some_and(|tab| leaf_count(&tab.layout) > 1)
    }

    /// Rebuilds the tab-strip labels from `tabs`. The buffer is reused every
    /// frame; the title itself stays in the owning `Terminal`.
    fn build_tab_labels(&mut self) {
        self.tab_labels.clear();
        for tab in &self.tabs {
            let title_terminal = self
                .terminals
                .iter()
                .position(|terminal| terminal.pane_id() == tab.active_pane)
                .unwrap_or(usize::MAX);
            self.tab_labels.push(TabLabelIndex {
                id: tab.id,
                title_terminal,
                number: tab.title_number,
                leaf_count: leaf_count(&tab.layout),
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

    /// Starts a new grid composition generation and drops the pointer capture of
    /// the active terminal, so a press from before the change is not finished in
    /// a tile whose geometry has moved. A composition change is structural: the
    /// panels, their layouts and the saved layout all move with it.
    fn bump_grid_generation(&mut self) {
        self.grid_generation = self.grid_generation.wrapping_add(1);
        self.bump_structure_generation();
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

    /// Tile that takes focus after `removed` terminals disappear: the next grid
    /// member after the focused tile in grid order, skipping every removed
    /// index, else the previous one. Returns the terminal index after the
    /// removals, and `None` outside grid mode or when no tile survives.
    fn grid_successor_after_removal(&self, removed: &[usize]) -> Option<usize> {
        let grid = self.claude_grid.as_ref()?;
        if removed.is_empty() {
            return None;
        }
        let live = |member: &usize| !removed.contains(member);
        let pivot = self.active_terminal;
        let target = grid
            .members
            .iter()
            .copied()
            .filter(live)
            .find(|&member| member > pivot)
            .or_else(|| {
                grid.members
                    .iter()
                    .rev()
                    .copied()
                    .filter(live)
                    .find(|&member| member < pivot)
            })?;
        Some(target - removed.iter().filter(|&&index| index < target).count())
    }

    /// Tile that takes the focus once the focused tile has left while the grid
    /// is open: the first surviving member at or after the slot the tile
    /// occupied, else the last member before it (§6.4). `remove_terminal_at`
    /// leaves `active_terminal` on that slot, so it is the pivot of the search.
    fn grid_successor_after_focused_removal(&mut self) -> Option<usize> {
        self.collect_claude_members();
        let slot = self.active_terminal;
        self.grid_members_scratch
            .iter()
            .copied()
            .find(|&member| member >= slot)
            .or_else(|| {
                self.grid_members_scratch
                    .iter()
                    .rev()
                    .copied()
                    .find(|&member| member < slot)
            })
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
        // A hint from a refused entry must not outlive the successful one.
        self.grid_hint = None;
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
        // The banner deadline belongs to the open grid; nothing reads it outside.
        self.withheld_until = None;
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
        self.refresh_close_confirm_hover();
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
        self.refresh_close_confirm_hover();
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
            && self.shortcut_intercepted(shortcut)
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
        if self.grid_open() {
            // Hit-tests of this event use the tile list as it is now.
            self.rebuild_panels();
        }
        self.update_link_hover();
        // The dialog owns the pointer: tab drag and reorder must not advance under it.
        if self.close_confirm.is_some() {
            self.refresh_close_confirm_hover();
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
        self.set_close_confirm_hover(CloseConfirmHit::None);
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
        // A new press of the same button means the release owed by an earlier
        // suppressed press was lost; that stale mark must not swallow this press's release.
        if state == KeyState::Pressed && self.suppressed_release == Some(button) {
            self.suppressed_release = None;
        }
        // A press that never reached the terminal (grid header, gap, stale
        // generation) owns its release, even when grid mode has closed since.
        if state == KeyState::Released && self.suppressed_release == Some(button) {
            self.suppressed_release = None;
            return false;
        }
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
                    // The strip addresses tabs; the close target still names a
                    // terminal, so map the tab's active pane to its storage index.
                    let target = self
                        .tabs
                        .get(index)
                        .and_then(|tab| self.terminal_index_of_pane(tab.active_pane));
                    let Some(target) = target else {
                        return false;
                    };
                    return self.request_terminal_close(CloseTarget::Tab(target));
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
        if self.grid_open() && !self.route_grid_pointer_button(state, button) {
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

    /// Grid-mode routing of one pointer button event. Returns `false` when the
    /// event is handled here and must not reach the active terminal, `true` when
    /// it goes on to the terminal's mouse handling.
    fn route_grid_pointer_button(&mut self, state: KeyState, button: PointerButton) -> bool {
        self.rebuild_panels();
        if state == KeyState::Released {
            return true;
        }
        if self.interaction.pointer_capture != crate::input::PointerCapture::None {
            // A captured pointer belongs to the active tile until its release.
            return true;
        }
        let (index, header) = match panel_hit(&self.panels, self.pointer_x, self.pointer_y) {
            Some(PanelHit::Header(index)) => (index, true),
            Some(PanelHit::Body(index)) => (index, false),
            None => {
                // A gap between tiles routes nothing; its release is swallowed.
                self.suppressed_release = Some(button);
                return false;
            }
        };
        if self.panel_layouts_grid_generation != self.grid_generation {
            // The frame's geometry is outdated: a press only chooses the tile under it.
            self.suppressed_release = Some(button);
            self.select_grid_tile(index);
            self.request_frame();
            return false;
        }
        if !header {
            self.select_grid_tile(index);
            return true;
        }
        self.suppressed_release = Some(button);
        if button == PointerButton::Left {
            let terminal = self.panels[index].terminal;
            let now = Instant::now();
            let repeated = self.last_header_press.is_some_and(|(at, previous)| {
                previous == terminal && now.saturating_duration_since(at) <= MULTI_CLICK_WINDOW
            });
            if repeated {
                self.last_header_press = None;
                self.exit_claude_grid(Some(terminal));
            } else {
                self.last_header_press = Some((now, terminal));
                self.select_grid_tile(index);
            }
        }
        false
    }

    /// Makes the terminal of tile `index` the active tab and points the terminal
    /// interaction at that tile's layout. Does nothing for the active tab.
    fn select_grid_tile(&mut self, index: usize) {
        let Some(terminal) = self.panels.get(index).map(|panel| panel.terminal) else {
            return;
        };
        if terminal == self.active_terminal {
            return;
        }
        self.cancel_terminal_presentation_intents();
        self.activate_ready_terminal(terminal, false);
        if let Some(layout) = self.panel_layouts.get(index) {
            self.interaction.layout = *layout;
        }
        self.request_frame();
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
        if self.grid_open() {
            // The wheel scrolls the tile under the pointer and never moves focus.
            self.rebuild_panels();
            if self.panel_layouts_grid_generation != self.grid_generation {
                return;
            }
            let Some(PanelHit::Body(index)) = panel_hit(&self.panels, self.pointer_x, self.pointer_y)
            else {
                return;
            };
            let (Some(panel), Some(layout)) = (self.panels.get(index), self.panel_layouts.get(index).copied())
            else {
                return;
            };
            let Some(terminal) = self.terminals.get_mut(panel.terminal) else {
                return;
            };
            if self.interaction.mouse_wheel(delta, terminal, layout) {
                self.sync_animation_state();
                self.request_frame();
            }
            return;
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
        // A split tab draws every visible pane, so its frame is never withheld
        // as a whole; each pane carries its own `content_hidden` flag instead.
        if !self.grid_open() && !self.active_tab_split() && self.withhold_active_frame(now) {
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
        // The last render decides whether the active layout is a hidden one; the
        // current panel list may already have been rebuilt with content visible.
        let active_content_hidden = self.interaction.layout.content_hidden;
        let mut selection_autoscrolled = false;
        if let Some(terminal) = self.terminals.get_mut(active) {
            selection_autoscrolled = self.interaction.update_selection_autoscroll(dt, terminal);
            let layout = &self.interaction.layout;
            // The last render's limit is stale once output grew the scrollback;
            // a held anchor returns the current one. No limit means no clamp.
            let limit = if active_content_hidden {
                None
            } else {
                terminal.anchor_scroll(layout.char_h, layout.body.h, layout.scale)
            };
            terminal.scroll_y.update(dt);
            if let Some(max) = limit {
                terminal.scroll_y.clamp_target(0.0, max);
                terminal.scroll_y.clamp_current(0.0, max);
            }
        }
        // Panels other than the active terminal. Their layouts come from the last
        // render, and a stale one is trusted only in the current grid generation:
        // after a membership change the same index names another tile. Split
        // panes need the same animation step outside grid mode.
        let layouts_current = self.panel_layouts_grid_generation == self.grid_generation;
        for (index, panel) in self.panels.iter().enumerate() {
            if panel.terminal == active {
                continue;
            }
            let Some(terminal) = self.terminals.get_mut(panel.terminal) else {
                continue;
            };
            // A layout written for a hidden tile carries no content limit.
            let Some(layout) = self
                .panel_layouts
                .get(index)
                .filter(|layout| layouts_current && !layout.content_hidden)
            else {
                terminal.scroll_y.update(dt);
                continue;
            };
            let limit = terminal.anchor_scroll(layout.char_h, layout.body.h, layout.scale);
            terminal.scroll_y.update(dt);
            if let Some(max) = limit {
                terminal.scroll_y.clamp_target(0.0, max);
                terminal.scroll_y.clamp_current(0.0, max);
            }
        }
        if selection_autoscrolled {
            self.mark_dirty();
        }
        let (settings_progress, _) =
            settings_animation_step(self.settings_progress, self.settings_open, dt);
        self.settings_progress = settings_progress;

        self.rebuild_panels();
        if self.grid_open() || self.active_tab_split() {
            self.observe_grid_dev_warnings(now);
        }
        self.build_tab_labels();
        let tab_strip_hint = self.tab_strip_hint(now);
        let render_result = if let Some(runtime) = self.runtime.as_mut() {
            Some(runtime.render_terminal_and_present(TerminalRenderParams {
                tabs: &self.tab_labels,
                active_tab: self.active_tab,
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
                    hovered: confirm.hovered,
                }),
            }))
        } else {
            None
        };
        match render_result {
            Some(Ok(layout)) => {
                self.panel_layouts_generation = self.layout_generation;
                self.panel_layouts_grid_generation = self.grid_generation;
                self.interaction.update_layout(layout);
                let active = self.active_terminal;
                let active_content_hidden = self
                    .panels
                    .iter()
                    .any(|panel| panel.terminal == active && panel.content_hidden);
                if !active_content_hidden && let Some(terminal) = self.terminals.get_mut(active) {
                    terminal.scroll_y.clamp_target(0.0, layout.max_scroll);
                    terminal.scroll_y.clamp_current(0.0, layout.max_scroll);
                }
                // The render above wrote this frame's layouts for every panel.
                for (index, panel) in self.panels.iter().enumerate() {
                    if panel.terminal == active || panel.content_hidden {
                        continue;
                    }
                    let Some(max) = self.panel_layouts.get(index).map(|layout| layout.max_scroll)
                    else {
                        continue;
                    };
                    if let Some(terminal) = self.terminals.get_mut(panel.terminal) {
                        terminal.scroll_y.clamp_target(0.0, max);
                        terminal.scroll_y.clamp_current(0.0, max);
                    }
                }
                if let Some(runtime) = self.runtime.as_ref() {
                    let max = runtime.terminal_tab_strip_layout().max_scroll;
                    self.terminal_tab_scroll.clamp_target(0.0, max);
                    self.terminal_tab_scroll.clamp_current(0.0, max);
                    if let Some(reveal_tail) = self.pending_tab_reveal.take() {
                        let target = runtime.terminal_tab_reveal_target(
                            self.active_tab,
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
        self.recheck_tab_waiters();
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
    use crate::app::headless_run::test_support::{attach_test_pane, push_test_terminal};
    use crate::split::SplitAxis;

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
        let mut app = App::new();
        let pane = app.issue_pane_id();
        let terminal = Terminal::new_bridge_for_test(120, 8, 1, id, pane);
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
        push_test_terminal(&mut app, terminal);

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
        let ordinary_pane = ordinary.issue_pane_id();
        push_test_terminal(
            &mut ordinary,
            Terminal::new_for_test(80, 24, 2, ordinary_pane),
        );
        assert_eq!(bridge_call(&mut ordinary, 0, id, None).code, 3);
    }

    #[test]
    fn bridge_manual_confirm_after_auto_confirm_returns_already_confirmed() {
        let id = [10; 16];
        let mut app = App::new();
        let pane = app.issue_pane_id();
        let terminal = Terminal::new_bridge_for_test(120, 8, 1, id, pane);
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
        push_test_terminal(&mut app, terminal);
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
        app.build_tab_labels();
        let runtime = app.runtime.as_mut().expect("headless runtime");
        runtime
            .render_terminal_and_present(TerminalRenderParams {
                tabs: &app.tab_labels,
                active_tab: app.active_tab,
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(120, 8, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(120, 8, 2, pane));
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(120, 8, 1, pane));
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
        let mut app = App::new();
        let pane = app.issue_pane_id();
        let mut terminal = Terminal::new_bridge_for_test(80, 8, 1, id, pane);
        terminal.bridge_launch_state_mut().unwrap().created_at =
            std::time::Instant::now() - crate::terminal::BRIDGE_LAUNCH_TTL;
        push_test_terminal(&mut app, terminal);

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
        let old_pane = app.issue_pane_id();
        let old = Terminal::new_for_test(8, 2, 1, old_pane);
        old.grid.lock().unwrap().mark_presentation_ready();
        let pending_pane = app.issue_pane_id();
        let pending = Terminal::new_for_test(8, 2, 2, pending_pane);
        push_test_terminal(&mut app, old);
        push_test_terminal(&mut app, pending);
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(40, 3, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(40, 3, 2, pane));
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pane));
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
        let first_pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, first_pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pane));
        app.active_terminal = 0;
        app.active_terminal_presented = true;

        app.reorder_terminal_tab(0, 1);

        // A tab owns its panes: the strip order changes, terminal storage does not,
        // so the presented terminal keeps its index and needs no recommit.
        assert_eq!(app.active_terminal, 0);
        assert_eq!(app.active_tab, 1);
        assert!(split_contains_pane(&app.tabs[1].layout, first_pane));
        assert!(app.active_terminal_presented);
        assert!(
            app.terminals
                .iter()
                .all(|terminal| terminal.presentation_intent == TerminalPresentationIntent::None)
        );
    }

    #[test]
    fn grid_pointer_capture_blocks_tile_switch_until_release() {
        let mut app = App::new();
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pane));
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        app.grid_metrics = GridMetrics {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
            char_w: 8.0,
            char_h: 16.0,
        };
        app.claude_grid = Some(ClaudeGrid { members: vec![0, 1] });
        app.rebuild_panels();
        let tile_b = app.panels[1].body;
        app.pointer_x = tile_b.x + tile_b.w * 0.5;
        app.pointer_y = tile_b.y + tile_b.h * 0.5;
        app.interaction.pointer_capture = crate::input::PointerCapture::TerminalScrollbar;

        app.route_grid_pointer_button(KeyState::Pressed, PointerButton::Right);

        assert_eq!(app.active_terminal, 0);
        assert_eq!(app.interaction.pointer_capture, crate::input::PointerCapture::TerminalScrollbar);
    }

    #[test]
    fn grid_tile_switch_ends_left_tile_scrollbar_drag_animation() {
        let mut app = App::new();
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pane));
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        app.panels = vec![
            Panel {
                terminal: 0,
                body: crate::search::Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 },
                header: None,
                content_hidden: false,
            },
            Panel {
                terminal: 1,
                body: crate::search::Rect { x: 400.0, y: 0.0, w: 400.0, h: 300.0 },
                header: None,
                content_hidden: false,
            },
        ];
        app.interaction.pointer_capture = crate::input::PointerCapture::TerminalScrollbar;
        app.terminals[0].scroll_y.is_dragging = true;

        app.select_grid_tile(1);
        app.sync_animation_state();

        assert_eq!(app.active_terminal, 1);
        assert!(!app.animation_active);
    }

    /// Both entry points spawn the first tab after the pre-GL resources exist and
    /// before the EGL bootstrap, and hand the same eventfd to the runtime.
    #[test]
    fn first_tab_spawns_before_egl_bootstrap() {
        let cases = [
            (
                include_str!("app/direct_wayland.rs"),
                "    fn run_direct_wayland_inner",
                "WindowRuntime::bootstrap_wayland(",
            ),
            (
                include_str!("app/headless_run.rs"),
                "    pub(crate) fn start_headless",
                "WindowRuntime::bootstrap_headless(",
            ),
        ];
        for (source, entry, bootstrap) in cases {
            let body = source
                .split(entry)
                .nth(1)
                .and_then(|tail| tail.split("\n    }\n").next())
                .expect("startup entry point must remain present");
            let pre_gl = body
                .find("PreGlResources::new()")
                .expect("pre-GL resources");
            let spawn = body
                .find("self.spawn_initial_terminal(")
                .expect("early spawn");
            let egl = body.find(bootstrap).expect("EGL bootstrap");
            let install = body
                .find("self.prepare_bootstrapped_runtime(runtime)")
                .expect("install");
            assert!(pre_gl < spawn && spawn < egl && egl < install, "{entry}");
            assert!(
                body[egl..].contains("pre_gl,"),
                "{entry}: runtime takes the early eventfd"
            );
        }
        let runtime = include_str!("runtime.rs");
        let runtime = runtime.split("\n#[cfg(test)]").next().unwrap_or(runtime);
        assert_eq!(runtime.matches("WakeHandle::new()").count(), 1);
    }

    #[test]
    fn terminal_tab_routes_share_factory_close_and_non_blocking_presentation_lifecycle() {
        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        let renderer = include_str!("renderer/terminal_ui.rs");

        assert_eq!(production.matches("Terminal::spawn_sized(").count(), 1);
        assert_eq!(production.matches("fn add_terminal_with_launch").count(), 1);
        // One factory: the runtime-backed tab route and the pre-EGL first tab.
        assert_eq!(production.matches("self.spawn_terminal_tab(").count(), 2);
        assert!(production.contains("self.add_terminal_with_launch(TerminalLaunchSpec {"));
        assert_eq!(production.matches("self.add_terminal()").count(), 2);
        // The first tab is spawned before EGL, not when the runtime is installed.
        let startup = production
            .split("    fn on_runtime_ready")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_close_requested").next())
            .expect("runtime startup path must remain present");
        assert!(!startup.contains("add_terminal_with_launch("));
        assert!(!startup.contains("spawn_terminal_tab("));
        assert!(!startup.contains("self.add_terminal()"));
        assert!(startup.contains("self.mark_dirty()"));
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
        // The index adapter routes into the tab close and owns no removal.
        assert!(close.contains("self.close_tab("));
        assert!(!close.contains("self.terminals.remove("));
        assert!(!close.contains("add_terminal"));
        assert_eq!(production.matches("self.close_terminal_tab_at(").count(), 1);

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
            .split("    fn activate_pane")
            .nth(1)
            .and_then(|tail| tail.split("    fn activate_ready_terminal").next())
            .expect("pane activation method must remain present");
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
        let terminal_pane = app.issue_pane_id();
        let terminal = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec::hermetic_shell_for_test(),
            terminal_palette(&crate::config::AppConfig::default()),
            terminal_pane,
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
        push_test_terminal(&mut app, terminal);
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pane));
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
        let held_pane = app.issue_pane_id();
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
            held_pane,
        );
        push_test_terminal(&mut app, held);
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
        let orphan_pane = app.issue_pane_id();
        let orphan = Terminal::spawn(
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
            orphan_pane,
        );
        push_test_terminal(&mut app, orphan);
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
            PaneId::new(1),
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
    fn spawn_shell_tab(label: &str, pane_id: PaneId) -> Terminal {
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
            pane_id,
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, spawn_shell_tab(label, pane));
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, spawn_shell_tab("background-job", pane));
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
            hovered: CloseConfirmHit::None,
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(80, 24, 1, pane));
        app.close_confirm = Some(CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: Some("и ещё 2".to_string()),
            hovered: CloseConfirmHit::None,
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(80, 24, 1, pane));
        app.close_confirm = Some(CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
            hovered: CloseConfirmHit::None,
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(80, 24, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(80, 24, 2, pane));
        app.open_close_dialog(CloseConfirm {
            target: CloseTarget::Tab(1),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
            hovered: CloseConfirmHit::None,
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
            hovered: CloseConfirmHit::None,
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
    fn close_confirm_hover_redraws_only_when_the_hovered_button_changes() {
        for (target, title) in [
            (CloseTarget::Window, "Закрыть окно?"),
            (CloseTarget::Tab(0), "Закрыть вкладку?"),
        ] {
            let mut app = App::new();
            app.close_confirm = Some(CloseConfirm {
                target,
                title,
                rows: vec!["tab — vim".to_string()],
                more: None,
                hovered: CloseConfirmHit::None,
            });
            let hovered = |app: &App| app.close_confirm.as_ref().map(|confirm| confirm.hovered);
            app.dirty = false;
            app.set_close_confirm_hover(CloseConfirmHit::Confirm);
            assert_eq!(hovered(&app), Some(CloseConfirmHit::Confirm));
            assert!(app.dirty);
            // Motion within the same button does not ask for a frame.
            app.dirty = false;
            app.set_close_confirm_hover(CloseConfirmHit::Confirm);
            assert!(!app.dirty);
            app.set_close_confirm_hover(CloseConfirmHit::Cancel);
            assert_eq!(hovered(&app), Some(CloseConfirmHit::Cancel));
            assert!(app.dirty);
            // Leaving the buttons for the panel or the scrim clears the hover once.
            app.dirty = false;
            app.set_close_confirm_hover(CloseConfirmHit::Outside);
            assert_eq!(hovered(&app), Some(CloseConfirmHit::None));
            assert!(app.dirty);
            app.dirty = false;
            app.set_close_confirm_hover(CloseConfirmHit::None);
            assert!(!app.dirty);
            // The pointer leaving the window drops the hover.
            app.set_close_confirm_hover(CloseConfirmHit::Confirm);
            app.dirty = false;
            app.on_pointer_leave(800, 600);
            assert_eq!(hovered(&app), Some(CloseConfirmHit::None));
            assert!(app.dirty);
        }

        // Source-structure check: hit-testing needs the runtime, absent in unit tests.
        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        let motion = production
            .split("    pub(crate) fn on_pointer_motion")
            .nth(1)
            .and_then(|tail| tail.split("    pub(crate) fn on_pointer_leave").next())
            .expect("pointer motion routing must remain present");
        let guard = motion
            .find("if self.close_confirm.is_some() {")
            .expect("the close dialog must gate pointer motion");
        let refresh = motion
            .find("self.refresh_close_confirm_hover();")
            .expect("pointer motion must refresh the dialog hover");
        assert!(guard < refresh);
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

    /// A modifier release swallowed by the settings modal must still clear the held state,
    /// so the next release reports without the stale flag. Goes through `App::on_key`, not
    /// `TerminalInteraction::note_modifier_key` directly.
    #[test]
    fn modifier_release_swallowed_by_settings_modal_updates_held_state() {
        let modifier_key = |state, modifier, keycode| KeyInput {
            state,
            physical_key: PhysicalKey::Unidentified,
            keycode,
            text: None,
            unshifted: None,
            named: Some(ronsole_term::TermKey::Modifier(modifier)),
            ime_preedit: false,
        };
        let mut app = App::new();
        let modal_pane = app.issue_pane_id();
        let terminal = Terminal::new_for_test(80, 30, 1, modal_pane);
        {
            let mut sink = crate::terminal_process::SessionSink {
                reply_tx: None,
                title_cache: None,
                pending: None,
                wake: None,
            };
            // Kitty flags 10: report all keys and report event types.
            crate::platform::lock_recover(&terminal.grid).advance(
                b"\x1b[>10u",
                Instant::now(),
                &mut sink,
            );
        }
        push_test_terminal(&mut app, terminal);

        app.on_modifiers(Modifiers::CONTROL | Modifiers::SHIFT);
        app.on_key(modifier_key(
            KeyState::Pressed,
            ronsole_term::ModKey::LeftCtrl,
            29,
        ));
        // The host drops Ctrl from its state, and the modal owns the keyboard for the release.
        app.on_modifiers(Modifiers::SHIFT);
        app.settings_open = true;
        app.on_key(modifier_key(
            KeyState::Released,
            ronsole_term::ModKey::LeftCtrl,
            29,
        ));
        app.settings_open = false;

        app.on_modifiers(Modifiers::CONTROL | Modifiers::SHIFT);
        app.on_key(modifier_key(
            KeyState::Pressed,
            ronsole_term::ModKey::RightCtrl,
            97,
        ));
        app.on_modifiers(Modifiers::SHIFT);
        app.on_key(modifier_key(
            KeyState::Released,
            ronsole_term::ModKey::RightCtrl,
            97,
        ));
        // If the swallowed release were not noted, the last release would keep the Ctrl flag (6 instead of 2).
        assert_eq!(
            app.terminals[0].input_bytes_for_test(),
            b"\x1b[57442;6u\x1b[57448;6u\x1b[57448;2:3u"
        );
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
            let pane = app.issue_pane_id();
            let terminal = Terminal::new_for_test(80, 24, index as u64 + 1, pane);
            crate::platform::lock_recover(&terminal.grid).mark_presentation_ready();
            terminal.set_claude_foreground_for_test(is_claude);
            push_test_terminal(&mut app, terminal);
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
    fn grid_fix_successful_entry_clears_refused_hint() {
        let mut app = grid_mode_app(&[false, false]);
        let now = Instant::now();
        app.toggle_claude_grid(now);
        assert!(!app.grid_open());
        assert!(app.tab_strip_hint(now).is_some());

        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        app.toggle_claude_grid(now);
        assert!(app.grid_open());
        assert_eq!(app.tab_strip_hint(now), None);
        assert_eq!(app.grid_hint, None);
    }

    #[test]
    fn grid_fix_exit_clears_withheld_deadline() {
        let mut app = grid_mode_app(&[true, true]);
        let now = Instant::now();
        app.toggle_claude_grid(now);
        assert!(app.grid_open());
        app.withheld_until = Some(now + Duration::from_secs(1));

        app.toggle_claude_grid(now);
        assert!(!app.grid_open());
        assert_eq!(app.withheld_until, None);
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
            hovered: CloseConfirmHit::None,
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
        let pane = app.issue_pane_id();
        let terminal = Terminal::new_for_test(8, 2, 1, pane);
        enable_focus_reporting_for_test(&terminal);
        push_test_terminal(&mut app, terminal);
        app.active_terminal_presented = true;
        app.on_focus_changed(false);
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\x1b[O");
    }

    #[test]
    fn focus_loss_without_1004_sends_nothing() {
        let mut app = App::new();
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
        app.active_terminal_presented = true;
        app.on_focus_changed(false);
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
    }

    /// A focus loss clears the sent-key set even when the terminal is not presented;
    /// only the mode 1004 report is gated on presentation.
    #[test]
    fn focus_loss_clears_sent_keys_when_terminal_not_presented() {
        let mut app = App::new();
        let clear_pane = app.issue_pane_id();
        let terminal = Terminal::new_for_test(8, 2, 1, clear_pane);
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
        push_test_terminal(&mut app, terminal);
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

    /// Centre of tile `index`'s header or body in the panels of the last rebuild.
    fn grid_pointer_point(app: &App, index: usize, header: bool) -> (f32, f32) {
        let rect = if header {
            app.panels[index].header.expect("grid tiles have headers")
        } else {
            app.panels[index].body
        };
        (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5)
    }

    fn grid_pointer_move(app: &mut App, (x, y): (f32, f32)) {
        app.on_pointer_motion(PointerPosition { x, y });
    }

    /// Enables X10 mouse tracking (DECSET 1000) on a test terminal.
    fn grid_pointer_enable_tracking(terminal: &Terminal) {
        feed_batch(terminal, "\x1b[?1000h");
    }

    #[test]
    fn grid_pointer_click_in_inactive_tile_focuses_it_and_routes_press() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-focus",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        for terminal in &app.terminals {
            enable_focus_reporting_for_test(terminal);
        }
        grid_pointer_enable_tracking(&app.terminals[1]);
        app.focused = true;
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        let point = grid_pointer_point(&app, 1, false);
        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);

        assert!(app.grid_open());
        assert_eq!(app.active_terminal, 1);
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\x1b[O");
        assert_eq!(app.interaction.layout.body, app.panel_layouts[1].body);
        // Focus-in comes first, then exactly one legacy X10 press (button 0) at the
        // cell under the click. The expectation is derived from tile 1's layout.
        let layout = app.panel_layouts[1];
        let (x, y) = point;
        let col = ((x - layout.text_x) / layout.char_w).floor() as usize + 1;
        // Rows are counted up from the bottom edge of the body, as the grid lays them out.
        let rows_from_bottom = ((layout.body.y + layout.body.h - layout.bottom_pad - y
            + layout.scroll_offset)
            / layout.char_h)
            .floor() as usize;
        let row = layout.visible_rows - rows_from_bottom;
        let mut expected = b"\x1b[I\x1b[M".to_vec();
        expected.extend_from_slice(&[32, col as u8 + 32, row as u8 + 32]);
        assert_eq!(app.terminals[1].input_bytes_for_test(), expected);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_click_cancels_pending_activation() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-cancel",
                3,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        app.terminals[2].set_claude_foreground_for_test(false);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));
        // Set after the frame: on_frame would run a dangling request before the click.
        app.terminals[2].presentation_intent = TerminalPresentationIntent::ActivateWhenReady;

        let point = grid_pointer_point(&app, 1, false);
        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);

        assert_eq!(app.active_terminal, 1);
        assert!(matches!(
            app.terminals[2].presentation_intent,
            TerminalPresentationIntent::None
        ));
        assert!(!app.process_terminal_presentation_intents());
        assert_eq!(app.active_terminal, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_header_double_click_exits_to_tile() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-header",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        grid_pointer_enable_tracking(&app.terminals[1]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        let point = grid_pointer_point(&app, 1, true);
        grid_pointer_move(&mut app, point);
        // A single press selects the tile and does not leave grid mode.
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        app.on_pointer_button(KeyState::Released, PointerButton::Left);
        assert!(app.grid_open());
        assert_eq!(app.active_terminal, 1);

        // The second press within the window exits onto the tile.
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert!(!app.grid_open());
        assert_eq!(app.active_terminal, 1);
        // The release arrives after the exit and must not reach the PTY.
        app.on_pointer_button(KeyState::Released, PointerButton::Left);
        assert!(app.terminals[1].input_bytes_for_test().is_empty());
        assert_eq!(app.suppressed_release, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_resize_then_press_before_frame_not_routed() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-resize",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        grid_pointer_enable_tracking(&app.terminals[1]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        // Old geometry: the centre of tile 1 is still inside tile 1 after the widening.
        let point = grid_pointer_point(&app, 1, false);
        let generation = app.grid_generation;
        app.on_resize_with_logical_size(800, 400, None);
        assert_ne!(app.grid_generation, generation);
        assert_ne!(app.panel_layouts_grid_generation, app.grid_generation);

        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert_eq!(app.active_terminal, 1);
        app.on_pointer_button(KeyState::Released, PointerButton::Left);
        assert!(app.terminals[1].input_bytes_for_test().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Grid with tracking on tile 0 and a suppressed-release mark whose release never arrived.
    fn grid_pointer_with_stale_mark(name: &str) -> (App, PathBuf) {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(name, 2);
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        grid_pointer_enable_tracking(&app.terminals[0]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));
        app.suppressed_release = Some(PointerButton::Left);
        (app, root)
    }

    #[test]
    fn grid_pointer_same_button_press_drops_stale_suppressed_release() {
        let (mut app, root) = grid_pointer_with_stale_mark("grid-pointer-stale-mark");
        let point = grid_pointer_point(&app, 0, false);
        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert_eq!(app.suppressed_release, None);
        // The press is routed as usual, so the body of the active tile gets bytes.
        assert!(!app.terminals[0].input_bytes_for_test().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_other_button_press_keeps_suppressed_release() {
        let (mut app, root) = grid_pointer_with_stale_mark("grid-pointer-other-press");
        let point = grid_pointer_point(&app, 0, false);
        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Right);
        assert_eq!(app.suppressed_release, Some(PointerButton::Left));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_generation_ignored_outside_grid() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-outside",
                2,
            );
        grid_pointer_enable_tracking(&app.terminals[0]);
        assert_eq!(app.on_frame(), Ok(true));
        app.panel_layouts_grid_generation = app.grid_generation.wrapping_add(1);
        assert!(!app.grid_open());

        let point = grid_pointer_point(&app, 0, false);
        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert!(!app.terminals[0].input_bytes_for_test().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_generation_bump_drops_capture() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-capture",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        // Tile 0 is active and has no tracking, so a body press starts a text selection.
        let start = grid_pointer_point(&app, 0, false);
        grid_pointer_move(&mut app, start);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        grid_pointer_move(&mut app, (start.0 + 4.0, start.1));
        assert!(app.interaction.terminal_selection_active());

        app.bump_grid_generation();
        assert!(!app.interaction.terminal_selection_active());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_stale_generation_press_only_selects_and_swallows_release() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-stale",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        grid_pointer_enable_tracking(&app.terminals[1]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        app.bump_grid_generation();
        let point = grid_pointer_point(&app, 1, false);
        grid_pointer_move(&mut app, point);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert_eq!(app.active_terminal, 1);
        assert_eq!(app.suppressed_release, Some(PointerButton::Left));
        assert!(app.terminals[1].input_bytes_for_test().is_empty());

        app.on_pointer_button(KeyState::Released, PointerButton::Left);
        assert_eq!(app.suppressed_release, None);
        assert!(app.terminals[1].input_bytes_for_test().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_header_press_release_in_body_not_sent() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-header-body",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        grid_pointer_enable_tracking(&app.terminals[0]);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        let header = grid_pointer_point(&app, 0, true);
        grid_pointer_move(&mut app, header);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert_eq!(app.suppressed_release, Some(PointerButton::Left));

        let body = grid_pointer_point(&app, 0, false);
        grid_pointer_move(&mut app, body);
        app.on_pointer_button(KeyState::Released, PointerButton::Left);
        assert_eq!(app.suppressed_release, None);
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_release_of_other_button_keeps_suppression() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-other-button",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        let header = grid_pointer_point(&app, 0, true);
        grid_pointer_move(&mut app, header);
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        app.on_pointer_button(KeyState::Released, PointerButton::Right);
        assert_eq!(app.suppressed_release, Some(PointerButton::Left));
        app.on_pointer_button(KeyState::Released, PointerButton::Left);
        assert_eq!(app.suppressed_release, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_wheel_over_inactive_tile_scrolls_it_without_focus() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-wheel",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        // Enough rows to push history into the scrollback of tile 1.
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        let point = grid_pointer_point(&app, 1, false);
        grid_pointer_move(&mut app, point);
        let before = app.terminals[1].scroll_y.target;
        app.on_scroll(ScrollDelta::Line { x: 0.0, y: 1.0 });

        assert_eq!(app.active_terminal, 0);
        assert_ne!(app.terminals[1].scroll_y.target, before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_stale_generation_wheel_has_no_effect() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-wheel-stale",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        let point = grid_pointer_point(&app, 1, false);
        grid_pointer_move(&mut app, point);
        let before = app.terminals[1].scroll_y.target;
        app.on_scroll(ScrollDelta::Line { x: 0.0, y: 1.0 });
        // Sanity: with a current frame the same wheel does move the tile.
        let scrolled = app.terminals[1].scroll_y.target;
        assert_ne!(scrolled, before);

        app.bump_grid_generation();
        app.on_scroll(ScrollDelta::Line { x: 0.0, y: 1.0 });
        assert_eq!(app.terminals[1].scroll_y.target, scrolled);
        assert_eq!(app.active_terminal, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_hover_moves_between_tiles_and_clears_previous() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-hover",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        app.on_modifiers(Modifiers::CONTROL);
        let first = grid_pointer_point(&app, 0, false);
        grid_pointer_move(&mut app, first);
        assert_eq!(app.hovered_panel_terminal, Some(0));
        // No link text is drawn in the test terminals, so seed one on tile 0
        // to observe that moving away clears it.
        let link = ronsole_term::LinkTarget {
            uri: "https://example.com/".into(),
            cells: Vec::new(),
        };
        assert!(app.terminals[0].set_hovered_link(Some((link, 0))));

        let second = grid_pointer_point(&app, 1, false);
        grid_pointer_move(&mut app, second);
        assert_eq!(app.hovered_panel_terminal, Some(1));
        assert!(app.terminals[0].hovered_link().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_pointer_wheel_outside_tiles_has_no_effect() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-pointer-wheel-outside",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        // Left of and above every tile, so no panel body is under the pointer.
        grid_pointer_move(&mut app, (-50.0, -50.0));
        let before = app.terminals[1].scroll_y.target;
        app.on_scroll(ScrollDelta::Line { x: 0.0, y: 1.0 });

        assert_eq!(app.active_terminal, 0);
        assert_eq!(app.terminals[1].scroll_y.target, before);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Runs one frame as if 16 ms had passed since the previous one, so the
    /// clamped `animation_dt` advances the smooth scroll by a full step.
    fn grid_frame_step(app: &mut App) -> Result<bool, String> {
        app.last_frame = Instant::now() - Duration::from_millis(16);
        app.on_frame()
    }

    #[test]
    fn grid_frame_scroll_animation_of_inactive_tile_keeps_frames() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-animation",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        // Tile 1 is inactive: its scroll animation alone must keep frames running.
        app.terminals[1].scroll_y.target = 5.0;
        app.sync_animation_state();
        assert!(app.animation_active);

        let mut frames = 0;
        while app.animation_active && frames < 200 {
            assert_eq!(grid_frame_step(&mut app), Ok(true));
            frames += 1;
        }
        assert!(!app.animation_active, "tile scroll did not settle in {frames} frames");
        assert!(app.terminals[1].scroll_y.is_settled());
        assert_eq!(app.terminals[1].scroll_y.current, 5.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_dev_warning_hides_only_its_tile_and_redraws_after_deadline() {
        let warning = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
        let claude = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 7,
        };
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-dev-warning",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], warning);
        let sent_at = Instant::now();
        let mut scratch = String::new();
        assert!(app.terminals[1].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));
        app.toggle_claude_grid(sent_at);
        // Enter was sent at `sent_at`, so the banner hides until this deadline.
        let deadline = sent_at + crate::terminal::DEV_WARNING_HIDE_LIMIT;
        let content_hidden = |app: &App, terminal: usize| {
            app.panels
                .iter()
                .find(|panel| panel.terminal == terminal)
                .is_some_and(|panel| panel.content_hidden)
        };

        // The frame is drawn, not withheld: only tile 1 shows its body background.
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert!(!content_hidden(&app, 0));
        assert!(content_hidden(&app, 1));
        assert!(!app.frame_withheld);
        assert_eq!(app.withheld_until, Some(deadline));

        // Let the grid entry animation settle so only the banner deadline can wake the loop.
        let mut frames = 0;
        while app.animation_active && frames < 200 {
            assert_eq!(grid_frame_step(&mut app), Ok(true));
            frames += 1;
        }
        assert!(!app.animation_active, "grid entry did not settle in {frames} frames");

        // Before the deadline nothing is requested.
        app.dirty = false;
        assert_eq!(app.loop_control(), AppLoopControl::WaitUntil(deadline));
        app.advance_dev_channel_warnings(sent_at + Duration::from_millis(1499));
        assert!(!app.dirty);
        assert_eq!(app.withheld_until, Some(deadline));

        // At the deadline the frame is requested and the wake deadline is cleared.
        app.advance_dev_channel_warnings(deadline);
        assert!(app.dirty);
        assert_eq!(app.withheld_until, None);

        // Without new output the next advance requests nothing and the loop settles.
        app.dirty = false;
        app.advance_dev_channel_warnings(deadline);
        assert!(!app.dirty);
        assert_eq!(app.loop_control(), AppLoopControl::Wait);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_two_hidden_tiles_redraw_first_at_its_deadline() {
        let warning = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
        let claude = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 7,
        };
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-two-dev-warnings",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        // Tile 0 stays hidden for about 300 ms more, tile 1 for about 1 s more.
        // The test sleeps past the first deadline so the real-clock frame sees it.
        let now = Instant::now();
        let enter_times = [
            now - Duration::from_millis(1200),
            now - Duration::from_millis(500),
        ];
        for (index, sent_at) in enter_times.into_iter().enumerate() {
            feed_batch(&app.terminals[index], warning);
            let mut scratch = String::new();
            assert!(app.terminals[index].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));
        }
        app.toggle_claude_grid(now);
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        let mut frames = 0;
        while app.animation_active && frames < 200 {
            assert_eq!(grid_frame_step(&mut app), Ok(true));
            frames += 1;
        }
        assert!(!app.animation_active, "grid entry did not settle in {frames} frames");
        let first = app.terminals[0]
            .dev_warning_hidden_until()
            .expect("tile 0 banner hides");
        let second = app.terminals[1]
            .dev_warning_hidden_until()
            .expect("tile 1 banner hides");
        assert!(first < second);
        assert_eq!(app.withheld_until, Some(first));
        app.dirty = false;
        assert_eq!(app.loop_control(), AppLoopControl::WaitUntil(first));

        std::thread::sleep(first.saturating_duration_since(Instant::now()) + Duration::from_millis(5));
        app.dirty = false;
        app.advance_dev_channel_warnings(first);
        assert!(app.dirty, "first deadline must request a redraw");
        assert_eq!(app.withheld_until, None);

        // The redraw recomputes the wake deadline: tile 0 is shown, tile 1 still hidden.
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert!(app.panels.iter().all(|panel| panel.terminal != 0 || !panel.content_hidden));
        assert!(app.panels.iter().any(|panel| panel.terminal == 1 && panel.content_hidden));
        assert_eq!(app.withheld_until, Some(second));
        app.dirty = false;
        assert_eq!(app.loop_control(), AppLoopControl::WaitUntil(second));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_hidden_tile_keeps_scroll_position() {
        let warning = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
        let claude = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 7,
        };
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-hidden-scroll",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        feed_batch(&app.terminals[1], warning);
        let sent_at = Instant::now();
        let mut scratch = String::new();
        assert!(app.terminals[1].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));
        app.toggle_claude_grid(sent_at);
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert!(app.panels.iter().any(|panel| panel.terminal == 1 && panel.content_hidden));

        app.terminals[1].scroll_y.target = 20.0;
        app.terminals[1].scroll_y.current = 20.0;
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.terminals[1].scroll_y.target, 20.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_hidden_tile_keeps_scroll_after_pointer_motion() {
        let warning = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
        let claude = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 7,
        };
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-hidden-pointer-scroll",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        feed_batch(&app.terminals[1], warning);
        let sent_at = Instant::now();
        let mut scratch = String::new();
        assert!(app.terminals[1].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));
        app.toggle_claude_grid(sent_at);
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert!(app.panels.iter().any(|panel| panel.terminal == 1 && panel.content_hidden));

        // Pointer motion over the grid rebuilds the tile list with content_hidden reset.
        app.terminals[1].scroll_y.target = 20.0;
        app.terminals[1].scroll_y.current = 20.0;
        app.on_pointer_motion(PointerPosition { x: 200.0, y: 5.0 });
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.terminals[1].scroll_y.target, 20.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_hidden_active_tile_keeps_scroll_after_pointer_motion() {
        let warning = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
        let claude = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 7,
        };
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-hidden-active-pointer-scroll",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[0], &"history line\r\n".repeat(200));
        feed_batch(&app.terminals[0], warning);
        let sent_at = Instant::now();
        let mut scratch = String::new();
        assert!(app.terminals[0].observe_dev_channel_warning(sent_at, &mut scratch, |_| Some(claude)));
        app.toggle_claude_grid(sent_at);
        assert_eq!(app.active_terminal, 0);
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert!(app.panels.iter().any(|panel| panel.terminal == 0 && panel.content_hidden));

        app.terminals[0].scroll_y.target = 20.0;
        app.terminals[0].scroll_y.current = 20.0;
        app.on_pointer_motion(PointerPosition { x: 200.0, y: 5.0 });
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.terminals[0].scroll_y.target, 20.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_clamp_skipped_for_stale_layouts() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-stale-clamp",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));
        // The fresh layout of tile 1 (index 1) allows the target below.
        assert!(app.panel_layouts[1].max_scroll >= 10.0);

        // Previous-frame layout under index 1 is too small for the target,
        // and the membership change makes it stale.
        app.panel_layouts[1].max_scroll = 0.0;
        app.terminals[1].scroll_y.target = 10.0;
        app.terminals[1].scroll_y.current = 10.0;
        app.bump_grid_generation();

        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.terminals[1].scroll_y.target, 10.0);

        // A permissive previous layout makes the pre-render clamp a no-op, so only
        // the post-render clamp by the fresh layout can cut the target.
        app.panel_layouts[1].max_scroll = 1_000_000.0;
        app.terminals[1].scroll_y.target = 10_000.0;
        app.terminals[1].scroll_y.current = 10_000.0;
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert!(app.terminals[1].scroll_y.target <= app.panel_layouts[1].max_scroll);
        assert!(app.terminals[1].scroll_y.target < 10_000.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_fix_outside_grid_close_does_not_clamp_other_tab_by_stale_layout() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-fix-outside-clamp",
                3,
            );
        // Tab 1 is short and active; tab 2 has history and a scroll position.
        feed_batch(&app.terminals[2], &"history line\r\n".repeat(200));
        app.active_terminal = 1;
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.panel_layouts[0].max_scroll, 0.0);

        app.terminals[2].scroll_y.target = 20.0;
        app.terminals[2].scroll_y.current = 20.0;
        // Removal as the close path does it outside grid mode: the tabs shift and
        // the active index follows, while `panels` keeps last frame's tile list.
        // `close_terminal_tab_at` is not used because the headless tab's process
        // cleanup can refuse the close.
        app.terminals.remove(0);
        app.active_terminal = 0;

        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.terminals[1].scroll_y.target, 20.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_reorder_keeps_tile_composition_and_inactive_scroll() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-fix-reorder-generation",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        // Tile 0 is short; tile 1 has history.
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.panel_layouts[0].max_scroll, 0.0);
        assert!(app.panel_layouts[1].max_scroll >= 10.0);
        let generation = app.grid_generation;
        let layout_generation = app.layout_generation;

        // Tiles are panes, not tabs: moving a tab in the strip leaves the grid
        // composition alone, so its generation and its tile order stay put.
        app.reorder_terminal_tab(0, 1);
        assert_eq!(app.grid_generation, generation);
        assert!(app.layout_generation > layout_generation);
        // The inactive history tile keeps its scroll target: index 1 still names it.
        app.terminals[1].scroll_y.target = 20.0;
        app.terminals[1].scroll_y.current = 20.0;

        assert_eq!(grid_frame_step(&mut app), Ok(true));
        assert_eq!(app.terminals[1].scroll_y.target, 20.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grid_frame_idle_settles_without_frames() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "grid-frame-idle",
                2,
            );
        app.terminals[0].set_claude_foreground_for_test(true);
        app.terminals[1].set_claude_foreground_for_test(true);
        feed_batch(&app.terminals[1], &"history line\r\n".repeat(200));
        app.toggle_claude_grid(Instant::now());
        assert_eq!(app.on_frame(), Ok(true));

        app.terminals[1].scroll_y.target = 5.0;
        app.sync_animation_state();
        let mut frames = 0;
        while app.animation_active && frames < 200 {
            assert_eq!(grid_frame_step(&mut app), Ok(true));
            frames += 1;
        }
        assert!(!app.animation_active, "tile scroll did not settle in {frames} frames");

        // Settled: no continuous frames. Either wait for an event or sleep
        // until a real deadline in the future.
        match app.loop_control() {
            AppLoopControl::Wait => {}
            AppLoopControl::WaitUntil(deadline) => assert!(deadline > Instant::now()),
            other => panic!("settled grid must not poll or redraw continuously: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn background_tab_bell_marks_tab_until_it_is_activated() {
        let mut app = App::new();
        for display_number in [1, 2] {
            let pane = app.issue_pane_id();
            push_test_terminal(&mut app, Terminal::new_for_test(8, 2, display_number, pane));
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
    fn spawn_creates_one_tab_per_terminal_with_unique_pane_ids() {
        let (app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "tabs-one-per-terminal",
                3,
            );
        assert_eq!(app.tabs.len(), 3);
        let mut pane_ids = Vec::new();
        for (index, tab) in app.tabs.iter().enumerate() {
            match &tab.layout {
                SplitNode::Leaf(pane) => {
                    assert_eq!(*pane, app.terminals[index].pane_id());
                    pane_ids.push(*pane);
                }
                SplitNode::Split { .. } => panic!("tab {index} must start as one leaf"),
            }
            assert_eq!(tab.active_pane, app.terminals[index].pane_id());
        }
        pane_ids.sort();
        pane_ids.dedup();
        assert_eq!(pane_ids.len(), 3, "pane ids must be unique");
        app.debug_assert_invariants();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn activate_pane_sets_focus_and_reports_1004() {
        let mut app = App::new();
        let first = app.issue_pane_id();
        let first_terminal = Terminal::new_for_test(8, 2, 1, first);
        enable_focus_reporting_for_test(&first_terminal);
        push_test_terminal(&mut app, first_terminal);
        let second = app.issue_pane_id();
        let second_terminal = Terminal::new_for_test(8, 2, 2, second);
        enable_focus_reporting_for_test(&second_terminal);
        push_test_terminal(&mut app, second_terminal);
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        app.focused = true;

        app.activate_pane(second, true);

        assert_eq!(app.active_tab, 1);
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        assert_eq!(app.pending_tab_reveal, Some(true));
        assert_eq!(app.tabs[1].active_pane, second);
        assert_eq!(app.terminals[0].input_bytes_for_test(), b"\x1b[O");
        assert_eq!(app.terminals[1].input_bytes_for_test(), b"\x1b[I");
        app.debug_assert_invariants();
    }

    #[test]
    fn select_tab_pane_in_background_changes_only_tab_pane() {
        let mut app = App::new();
        let front = app.issue_pane_id();
        let front_terminal = Terminal::new_for_test(8, 2, 1, front);
        enable_focus_reporting_for_test(&front_terminal);
        push_test_terminal(&mut app, front_terminal);
        let left = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, left));
        let right = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 3, right));
        // A background tab with two panes; the front tab keeps one pane, and
        // the third terminal moves into that split instead of its own tab.
        let background = app.tabs[1].id;
        app.tabs[1].layout = SplitNode::Split {
            axis: SplitAxis::SideBySide,
            ratio: 0.5,
            first: Box::new(SplitNode::Leaf(left)),
            second: Box::new(SplitNode::Leaf(right)),
        };
        app.tabs[1].active_pane = left;
        app.tabs.remove(2);
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        app.focused = true;
        app.interaction.layout.cols = 8;

        app.select_tab_pane_in_background(background, right);

        assert_eq!(app.tabs[1].active_pane, right);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        assert_eq!(app.interaction.layout.cols, 8);
        assert!(app.terminals[0].input_bytes_for_test().is_empty());
        assert!(app.terminals[1].input_bytes_for_test().is_empty());
        assert!(app.terminals[2].input_bytes_for_test().is_empty());
        app.debug_assert_invariants();
    }

    /// Headless app whose window body is 1000x700 at scale 1.0 with one 8x16
    /// cell, so a split of the active pane fits the pane minimum.
    fn headless_split_app(
        tag: &str,
        count: usize,
    ) -> (
        App,
        std::sync::mpsc::Receiver<String>,
        crate::headless::Profile,
        PathBuf,
    ) {
        let (mut app, receiver, profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(tag, count);
        app.set_grid_metrics(GridMetrics {
            width: 1000.0,
            height: 700.0,
            scale: 1.0,
            char_w: 8.0,
            char_h: 16.0,
        });
        app.rebuild_panels();
        (app, receiver, profile, root)
    }

    /// `code` as the window delivers a plain press of it.
    fn shortcut_key(code: KeyCode) -> KeyInput {
        KeyInput {
            state: KeyState::Pressed,
            physical_key: PhysicalKey::Code(code),
            keycode: 0,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        }
    }

    /// Ctrl+Shift+`code` through `App::on_key`; returns the key result.
    fn press_ctrl_shift(app: &mut App, code: KeyCode) -> bool {
        app.on_modifiers(Modifiers::CONTROL | Modifiers::SHIFT);
        app.on_key(shortcut_key(code))
    }

    #[test]
    fn ctrl_shift_d_splits_active_pane_into_two() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-right", 1);
        let first_pane = app.tabs[0].active_pane;
        let terminals_before = app.terminals.len();

        assert!(!press_ctrl_shift(&mut app, KeyCode::KeyD));
        app.rebuild_panels();

        assert_eq!(app.terminals.len(), terminals_before + 1);
        assert_eq!(app.panels.len(), 2);
        let SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } = &app.tabs[0].layout
        else {
            panic!("the active tab must hold one split");
        };
        assert_eq!(*axis, SplitAxis::SideBySide);
        assert_eq!(*ratio, 0.5);
        assert!(matches!(**first, SplitNode::Leaf(pane) if pane == first_pane));
        let new_pane = app.tabs[0].active_pane;
        assert_ne!(new_pane, first_pane);
        assert!(matches!(**second, SplitNode::Leaf(pane) if pane == new_pane));
        assert_eq!(app.active_terminal, terminals_before);
        assert_eq!(app.terminals[terminals_before].pane_id(), new_pane);
        assert!(app.active_terminal_presented);
        app.debug_assert_invariants();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ctrl_shift_e_splits_stacked() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-down", 1);
        let first_pane = app.tabs[0].active_pane;

        press_ctrl_shift(&mut app, KeyCode::KeyE);
        app.rebuild_panels();

        assert_eq!(app.panels.len(), 2);
        let SplitNode::Split {
            axis, ratio, first, ..
        } = &app.tabs[0].layout
        else {
            panic!("the active tab must hold one split");
        };
        assert_eq!(*axis, SplitAxis::Stacked);
        assert_eq!(*ratio, 0.5);
        assert!(matches!(**first, SplitNode::Leaf(pane) if pane == first_pane));
        let left = app.panels[0].body;
        let top = app.panels[1].body;
        assert_eq!(left.x, top.x);
        assert_eq!(left.w, top.w);
        assert!(left.y < top.y);
        app.debug_assert_invariants();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_in_narrow_pane_shows_hint_and_spawns_nothing() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-too-small", 1);
        // 994px of two side-by-side panes leaves 497px each; 200px leaves 97px,
        // below the 100px pane minimum of one 8px cell row of ten columns.
        app.set_grid_metrics(GridMetrics {
            width: 200.0,
            height: 700.0,
            scale: 1.0,
            char_w: 8.0,
            char_h: 16.0,
        });
        app.rebuild_panels();
        let terminals_before = app.terminals.len();

        press_ctrl_shift(&mut app, KeyCode::KeyD);

        assert_eq!(app.terminals.len(), terminals_before);
        assert_eq!(leaf_count(&app.tabs[0].layout), 1);
        assert_eq!(app.tabs[0].active_pane, app.terminals[0].pane_id());
        let hint = app.tab_strip_hint(Instant::now()).expect("a hint is shown");
        assert!(
            hint.contains("Панель слишком мала для сплита"),
            "unexpected hint {hint:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_spawn_failure_keeps_tree_and_shows_hint() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-spawn-failure", 1);
        app.shell_override = Some(PathBuf::from("/definitely/missing/ronsole-test-shell"));
        let terminals_before = app.terminals.len();
        let active_terminal_before = app.active_terminal;
        let active_pane_before = app.tabs[0].active_pane;
        let pending_reveal_before = app.pending_tab_reveal;
        // A refused split must not cancel an activation that was already
        // pending: seed one so a misplaced cancel in the refusal path shows.
        app.terminals[0].presentation_intent = TerminalPresentationIntent::ActivateWhenReady;
        let intents_before: Vec<_> = app
            .terminals
            .iter()
            .map(|terminal| {
                (
                    terminal.presentation_intent,
                    terminal.reveal_right_tail_when_presented,
                )
            })
            .collect();

        press_ctrl_shift(&mut app, KeyCode::KeyD);

        assert_eq!(app.terminals.len(), terminals_before);
        assert_eq!(leaf_count(&app.tabs[0].layout), 1);
        assert_eq!(app.active_terminal, active_terminal_before);
        assert_eq!(app.tabs[0].active_pane, active_pane_before);
        assert_eq!(app.pending_tab_reveal, pending_reveal_before);
        let intents_after: Vec<_> = app
            .terminals
            .iter()
            .map(|terminal| {
                (
                    terminal.presentation_intent,
                    terminal.reveal_right_tail_when_presented,
                )
            })
            .collect();
        assert_eq!(intents_after, intents_before);
        let hint = app.tab_strip_hint(Instant::now()).expect("a hint is shown");
        assert!(
            hint.contains("Не удалось запустить панель"),
            "unexpected hint {hint:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_uses_osc7_directory_of_active_pane() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-osc7", 1);
        app.shell_override = Some(PathBuf::from("/bin/bash"));
        let workspace = root.join("workspace");
        app.terminals[0].osc7_cwd = Some(workspace.clone());

        press_ctrl_shift(&mut app, KeyCode::KeyD);

        assert_eq!(app.terminals.len(), 2);
        // The split shell runs in the directory the active pane's shell reported.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut cwd = None;
        while cwd.as_ref() != Some(&workspace) && Instant::now() < deadline {
            if let Some(pid) = app.terminals[1].process_group_leader()
                && let Ok(link) = std::fs::read_link(format!("/proc/{pid}/cwd"))
            {
                cwd = Some(link);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cwd, Some(workspace));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebuild_panels_lays_out_all_panes_without_overlap() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-layout", 1);

        press_ctrl_shift(&mut app, KeyCode::KeyD);
        app.rebuild_panels();

        let body = terminal_body_rect(1000.0, 700.0, 1.0);
        assert_eq!(app.panels.len(), 2);
        assert_eq!(app.panels[0].terminal, 0);
        assert_eq!(app.panels[1].terminal, 1);
        let left = app.panels[0].body;
        let right = app.panels[1].body;
        for rect in [left, right] {
            assert_eq!(rect.x.fract(), 0.0, "x must be whole pixels");
            assert_eq!(rect.y.fract(), 0.0, "y must be whole pixels");
            assert_eq!(rect.w.fract(), 0.0, "w must be whole pixels");
            assert_eq!(rect.h.fract(), 0.0, "h must be whole pixels");
            assert!(rect.x >= body.x && rect.y >= body.y);
            assert!(rect.x + rect.w <= body.x + body.w);
            assert!(rect.y + rect.h <= body.y + body.h);
            assert!(rect.w >= 100.0 && rect.h >= 48.0, "pane minimum");
        }
        assert!(left.x + left.w <= right.x, "panes must not overlap");
        assert_eq!(left.y, right.y);
        assert_eq!(left.h, right.h);
        assert_eq!(right.x, left.x + left.w + 6.0, "one gap between panes");
        assert_eq!(left.w + right.w + 6.0, body.w);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn layout_generation_grows_on_split_and_metrics_change() {
        let (mut app, _receiver, _profile, root) = headless_split_app("split-generations", 1);
        let before = app.layout_generation;
        let same = app.grid_metrics;
        app.set_grid_metrics(same);
        assert_eq!(
            app.layout_generation, before,
            "same metrics keep the generation"
        );

        press_ctrl_shift(&mut app, KeyCode::KeyD);
        let after_split = app.layout_generation;
        assert!(after_split > before, "a split changes the layout");

        app.set_grid_metrics(GridMetrics {
            width: same.width - 100.0,
            ..same
        });
        assert!(
            app.layout_generation > after_split,
            "a metrics change invalidates the layout outside grid mode"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Closing the active terminal drops its tab too, so the strip stays in
    /// step with `terminals` and the successor is still activatable.
    #[test]
    fn close_active_terminal_keeps_tabs_in_sync() {
        let mut app = App::new();
        for number in 1..=3 {
            let pane = app.issue_pane_id();
            push_test_terminal(&mut app, Terminal::new_for_test(8, 2, number, pane));
        }
        app.active_terminal = 1;
        app.active_tab = 1;
        app.active_terminal_presented = true;

        assert!(!app.close_terminal_tab_at(1));

        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.tabs.len(), app.terminals.len());
        app.debug_assert_invariants();
        app.activate_ready_terminal(1, false);
        assert_eq!(app.active_terminal, 1);
        app.debug_assert_invariants();
    }

    /// Closing a background terminal removes only its own tab; the active
    /// terminal and tab follow the index shift.
    #[test]
    fn close_background_terminal_keeps_tabs_in_sync() {
        let mut app = App::new();
        for number in 1..=3 {
            let pane = app.issue_pane_id();
            push_test_terminal(&mut app, Terminal::new_for_test(8, 2, number, pane));
        }
        app.active_terminal = 2;
        app.active_tab = 2;
        app.active_terminal_presented = true;

        assert!(!app.close_terminal_tab_at(0));

        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.tabs.len(), app.terminals.len());
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        app.debug_assert_invariants();
    }

    /// The last terminal takes the last tab with it: nothing is left to draw.
    #[test]
    fn closing_only_terminal_clears_tabs() {
        let mut app = App::new();
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
        app.active_terminal = 0;
        app.active_tab = 0;
        app.active_terminal_presented = true;

        assert!(app.close_terminal_tab_at(0));

        assert!(app.terminals.is_empty());
        assert!(app.tabs.is_empty());
        app.debug_assert_invariants();
    }

    /// One tab with two side-by-side panes built from two terminals; `tabs[0]`
    /// owns both leaves and the first pane is the tab's active pane.
    fn two_pane_app(shell: App, first: Terminal, second: Terminal) -> (App, PaneId, PaneId) {
        let mut app = shell;
        let first_pane = push_test_terminal(&mut app, first);
        let second_pane = push_test_terminal(&mut app, second);
        app.tabs[0].layout = SplitNode::Split {
            axis: SplitAxis::SideBySide,
            ratio: 0.5,
            first: Box::new(SplitNode::Leaf(first_pane)),
            second: Box::new(SplitNode::Leaf(second_pane)),
        };
        app.tabs[0].active_pane = first_pane;
        app.tabs.remove(1);
        app.active_tab = 0;
        app.active_terminal = 0;
        app.active_terminal_presented = true;
        (app, first_pane, second_pane)
    }

    fn presentation_ready_terminal(number: u64, pane: PaneId) -> Terminal {
        let terminal = Terminal::new_for_test(8, 2, number, pane);
        crate::platform::lock_recover(&terminal.grid).mark_presentation_ready();
        terminal
    }

    fn test_shell(number: u64, pane: PaneId) -> Terminal {
        let terminal = Terminal::spawn(
            None,
            number,
            TerminalLaunchSpec::hermetic_shell_for_test(),
            terminal_palette(&crate::config::AppConfig::default()),
            pane,
        );
        crate::platform::lock_recover(&terminal.grid).mark_presentation_ready();
        terminal
    }

    #[test]
    fn close_pane_gives_place_to_neighbour_and_keeps_processes_alive() {
        let mut app = App::new();
        let closed = app.issue_pane_id();
        let kept = app.issue_pane_id();
        let (mut app, closed, kept) = two_pane_app(
            app,
            Terminal::new_for_test(8, 2, 1, closed),
            test_shell(2, kept),
        );

        assert!(!app.close_pane(closed));

        assert_eq!(app.terminals.len(), 1);
        assert_eq!(app.terminals[0].pane_id(), kept);
        assert!(
            app.terminals[0].has_process(),
            "the neighbour's shell must stay alive"
        );
        assert!(!app.terminals[0].is_closed());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].active_pane, kept);
        assert!(matches!(app.tabs[0].layout, SplitNode::Leaf(pane) if pane == kept));
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        app.debug_assert_invariants();
        app.shutdown_all();
    }

    #[test]
    fn close_pane_refusal_keeps_tree_and_process() {
        let mut app = App::new();
        let other = app.issue_pane_id();
        let kept = app.issue_pane_id();
        let (mut app, other, kept) = two_pane_app(
            app,
            Terminal::new_for_test(8, 2, 1, other),
            test_shell(2, kept),
        );
        // The live shell is the active pane, so the refusal must also keep focus.
        app.tabs[0].active_pane = kept;
        app.active_terminal = 1;
        app.active_terminal_presented = true;
        // A stopped worker drops the sender: every cleanup request is refused.
        app.terminal_cleanup.shutdown_and_join();

        assert!(!app.close_pane(kept));

        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.terminals[1].pane_id(), kept);
        assert!(
            app.terminals[1].has_process(),
            "the refused pane keeps its process"
        );
        assert_eq!(leaf_count(&app.tabs[0].layout), 2);
        assert_eq!(app.tabs[0].active_pane, kept);
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        let _ = other;
        app.debug_assert_invariants();
    }

    #[test]
    fn close_last_pane_closes_tab_and_focuses_neighbour() {
        let mut app = App::new();
        let first = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, first));
        let second = app.issue_pane_id();
        push_test_terminal(&mut app, presentation_ready_terminal(2, second));
        app.active_tab = 0;
        app.active_terminal = 0;
        app.active_terminal_presented = true;

        assert!(!app.close_pane(first));

        assert_eq!(app.terminals.len(), 1);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].active_pane, second);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        app.debug_assert_invariants();
    }

    #[test]
    fn close_tab_removes_all_panes_and_focuses_next_tab() {
        let mut app = App::new();
        let first = app.issue_pane_id();
        let second = app.issue_pane_id();
        let (mut app, first, second) = two_pane_app(
            app,
            Terminal::new_for_test(8, 2, 1, first),
            Terminal::new_for_test(8, 2, 2, second),
        );
        let third = app.issue_pane_id();
        push_test_terminal(&mut app, presentation_ready_terminal(3, third));
        let tab = app.tabs[0].id;

        assert!(!app.close_tab(tab));

        assert_eq!(app.terminals.len(), 1);
        assert_eq!(app.terminals[0].pane_id(), third);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].active_pane, third);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        let _ = (first, second);
        app.debug_assert_invariants();
    }

    #[test]
    fn close_tab_refusal_stops_and_keeps_remaining_panes() {
        let mut app = App::new();
        let plain = app.issue_pane_id();
        let kept = app.issue_pane_id();
        // The processless pane owns no cleanup request, so it is removed before
        // the live pane is refused and the tab survives with one pane.
        let (mut app, plain, kept) = two_pane_app(
            app,
            Terminal::new_for_test(8, 2, 1, plain),
            test_shell(2, kept),
        );
        app.tabs[0].active_pane = kept;
        app.active_terminal = 1;
        app.active_terminal_presented = true;
        app.terminal_cleanup.shutdown_and_join();
        // A surviving tab keeps the window alive, so the last pane of the target
        // tab still asks the worker and is refused.
        let survivor = app.issue_pane_id();
        push_test_terminal(&mut app, presentation_ready_terminal(3, survivor));
        let tab = app.tabs[0].id;

        assert!(!app.close_tab(tab));

        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.terminals[0].pane_id(), kept);
        assert!(
            app.terminals[0].has_process(),
            "the remaining pane keeps its process"
        );
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(leaf_count(&app.tabs[0].layout), 1);
        assert!(!split_contains_pane(&app.tabs[0].layout, plain));
        assert_eq!(app.tabs[0].active_pane, kept);
        assert_eq!(app.active_terminal, 0);
        assert!(app.active_terminal_presented);
        app.debug_assert_invariants();
    }

    #[test]
    fn close_tab_refusal_in_grid_keeps_focus_on_live_tile() {
        let mut app = App::new();
        let shell = app.issue_pane_id();
        let other = app.issue_pane_id();
        let focused = app.issue_pane_id();
        push_test_terminal(&mut app, test_shell(1, shell));
        push_test_terminal(&mut app, presentation_ready_terminal(2, other));
        push_test_terminal(&mut app, presentation_ready_terminal(3, focused));
        app.terminals[1].set_claude_foreground_for_test(true);
        app.terminals[2].set_claude_foreground_for_test(true);
        // The focused tile moves into the shell's tab, in tree order after the
        // shell, so the refused cleanup arrives on the first pane of the close.
        app.tabs[0].layout = SplitNode::Split {
            axis: SplitAxis::SideBySide,
            ratio: 0.5,
            first: Box::new(SplitNode::Leaf(shell)),
            second: Box::new(SplitNode::Leaf(focused)),
        };
        app.tabs[0].active_pane = focused;
        app.tabs.remove(2);
        app.active_tab = 0;
        app.active_terminal = 2;
        app.active_terminal_presented = true;
        app.grid_metrics = GridMetrics {
            width: 1000.0,
            height: 700.0,
            scale: 1.0,
            char_w: 8.0,
            char_h: 16.0,
        };
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![1, 2]));
        assert_eq!(app.active_terminal, 2);
        app.terminal_cleanup.shutdown_and_join();
        let tab = app.tabs[0].id;

        assert!(!app.close_tab(tab));

        // Nothing left, so the live focused tile keeps the focus (§6.4).
        assert_eq!(app.terminals.len(), 3);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.active_terminal, 2);
        assert_eq!(app.terminals[2].pane_id(), focused);
        assert!(app.active_terminal_presented);
        assert_eq!(app.tabs[0].active_pane, focused);
        assert!(
            app.terminals[0].has_process(),
            "the refused shell keeps its process"
        );
        assert_eq!(grid_mode_members(&app), Some(vec![1, 2]));
        app.debug_assert_invariants();
        app.shutdown_all();
    }

    #[test]
    fn close_tab_partial_refusal_focuses_actual_surviving_tile() {
        let mut app = App::new();
        let outside = app.issue_pane_id();
        let focused = app.issue_pane_id();
        let shell = app.issue_pane_id();
        let plain = app.issue_pane_id();
        let next_tile = app.issue_pane_id();
        let last_tile = app.issue_pane_id();
        push_test_terminal(&mut app, presentation_ready_terminal(1, outside));
        push_test_terminal(&mut app, presentation_ready_terminal(2, focused));
        push_test_terminal(&mut app, test_shell(3, shell));
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 4, plain));
        push_test_terminal(&mut app, presentation_ready_terminal(5, next_tile));
        push_test_terminal(&mut app, presentation_ready_terminal(6, last_tile));
        for index in [0, 1, 4, 5] {
            app.terminals[index].set_claude_foreground_for_test(true);
        }
        // The closed tab owns panes 1..4 in tree order [focused, shell, plain,
        // next]: the focused tile leaves first, then the live shell refuses.
        app.tabs[1].layout = SplitNode::Split {
            axis: SplitAxis::SideBySide,
            ratio: 0.5,
            first: Box::new(SplitNode::Leaf(focused)),
            second: Box::new(SplitNode::Split {
                axis: SplitAxis::Stacked,
                ratio: 0.5,
                first: Box::new(SplitNode::Leaf(shell)),
                second: Box::new(SplitNode::Split {
                    axis: SplitAxis::Stacked,
                    ratio: 0.5,
                    first: Box::new(SplitNode::Leaf(plain)),
                    second: Box::new(SplitNode::Leaf(next_tile)),
                }),
            }),
        };
        app.tabs[1].active_pane = focused;
        app.tabs.remove(4);
        app.tabs.remove(3);
        app.tabs.remove(2);
        app.active_tab = 1;
        app.active_terminal = 1;
        app.active_terminal_presented = true;
        app.grid_metrics = GridMetrics {
            width: 1000.0,
            height: 700.0,
            scale: 1.0,
            char_w: 8.0,
            char_h: 16.0,
        };
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0, 1, 4, 5]));
        app.terminal_cleanup.shutdown_and_join();
        let tab = app.tabs[1].id;

        assert!(!app.close_tab(tab));

        // The focused tile left before the refusal: the next live tile in grid
        // order is `next_tile` alone, not the shell's neighbour `plain`.
        assert_eq!(app.terminals.len(), 5);
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.active_terminal, 3);
        assert_eq!(app.terminals[3].pane_id(), next_tile);
        assert!(app.active_terminal_presented);
        assert_eq!(app.tabs[1].active_pane, next_tile);
        assert!(
            app.terminals[1].has_process(),
            "the refused shell keeps its process"
        );
        assert!(app.grid_open());
        app.debug_assert_invariants();
        app.shutdown_all();
    }

    #[test]
    fn close_active_pane_with_unready_successor_keeps_invariant() {
        let mut app = App::new();
        let outside = app.issue_pane_id();
        let pending = app.issue_pane_id();
        let other = app.issue_pane_id();
        let closed = app.issue_pane_id();
        push_test_terminal(&mut app, presentation_ready_terminal(1, outside));
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pending));
        push_test_terminal(&mut app, presentation_ready_terminal(3, other));
        push_test_terminal(&mut app, presentation_ready_terminal(4, closed));
        // The closed tab owns the ready focused pane and the unready successor;
        // the closed pane sits last in `terminals`, so its removal leaves
        // `active_terminal` on the pane of the neighbour tab.
        app.tabs[1].layout = SplitNode::Split {
            axis: SplitAxis::SideBySide,
            ratio: 0.5,
            first: Box::new(SplitNode::Leaf(pending)),
            second: Box::new(SplitNode::Leaf(closed)),
        };
        app.tabs[1].active_pane = closed;
        app.tabs.remove(3);
        app.active_tab = 1;
        app.active_terminal = 3;
        app.active_terminal_presented = true;

        assert!(!app.close_pane(closed));

        // The logical focus already names the successor; the presentation waits.
        assert_eq!(app.terminals.len(), 3);
        assert_eq!(app.active_tab, 1);
        assert_eq!(app.tabs[1].active_pane, pending);
        assert_eq!(app.active_terminal, 1);
        assert!(!app.active_terminal_presented);
        assert_eq!(
            app.terminals[1].presentation_intent,
            TerminalPresentationIntent::ActivateWhenReady
        );
        app.debug_assert_invariants();

        crate::platform::lock_recover(&app.terminals[1].grid).mark_presentation_ready();
        assert!(app.process_terminal_presentation_intents());

        assert_eq!(app.active_tab, 1);
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        assert_eq!(
            app.terminals[1].presentation_intent,
            TerminalPresentationIntent::None
        );
        app.debug_assert_invariants();
    }

    #[test]
    fn natural_exit_removes_only_its_pane() {
        let mut app = App::new();
        let exiting = app.issue_pane_id();
        let orphan = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec {
                shell: None,
                working_directory: None,
                inherited_directory: None,
                command: vec![std::ffi::OsString::from("/bin/true")],
                hold: false,
                bridge_launch_id: None,
            },
            terminal_palette(&crate::config::AppConfig::default()),
            exiting,
        );
        let kept = app.issue_pane_id();
        let (mut app, exiting, kept) = two_pane_app(app, orphan, test_shell(2, kept));
        app.tabs[0].active_pane = kept;
        app.active_terminal = 1;
        app.active_terminal_presented = true;

        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while app.terminals.len() > 1 {
            assert!(!app.remove_closed_terminals());
            assert!(
                Instant::now() < deadline,
                "the exited pane did not close on its own"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(app.terminals.len(), 1);
        assert_eq!(app.terminals[0].pane_id(), kept);
        assert!(
            app.terminals[0].has_process(),
            "the neighbour's shell must stay alive"
        );
        assert_eq!(app.tabs.len(), 1);
        assert!(matches!(app.tabs[0].layout, SplitNode::Leaf(pane) if pane == kept));
        assert_eq!(app.tabs[0].active_pane, kept);
        assert_eq!(app.active_terminal, 0);
        let _ = exiting;
        app.debug_assert_invariants();
        app.shutdown_all();
    }

    #[test]
    fn closing_active_tile_focuses_next_tile() {
        let mut app = grid_mode_app(&[true, true, true]);
        app.active_terminal = 1;
        app.active_tab = 1;
        app.toggle_claude_grid(Instant::now());
        assert_eq!(grid_mode_members(&app), Some(vec![0, 1, 2]));
        let closed = app.terminals[1].pane_id();

        assert!(!app.close_pane(closed));

        assert!(app.grid_open());
        assert_eq!(app.terminals.len(), 2);
        assert_eq!(grid_mode_members(&app), Some(vec![0, 1]));
        assert_eq!(app.active_terminal, 1);
        assert!(app.active_terminal_presented);
        app.debug_assert_invariants();
    }

    /// Two terminals with one pane id pass the leaf count; the strip keeps a
    /// second, distinct leaf, so the per-terminal counts still say one leaf
    /// each. Only the duplicate-id check can see the split.
    #[test]
    #[should_panic(expected = "two terminals share pane id")]
    fn debug_assert_invariants_rejects_duplicate_pane_ids() {
        let mut app = App::new();
        let shared = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, shared));
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, shared));
        let unowned = app.issue_pane_id();
        app.tabs[1].layout = SplitNode::Leaf(unowned);
        app.tabs[1].active_pane = unowned;
        app.active_terminal = 0;
        app.active_tab = 0;

        app.debug_assert_invariants();
    }

    #[test]
    fn active_tab_bell_in_focused_window_sets_nothing() {
        let mut app = App::new();
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 2, pane));
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(8, 2, 1, pane));
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
            let pane = app.issue_pane_id();
            let terminal = Terminal::new_for_test(8, 2, display_number, pane);
            enable_focus_reporting_for_test(&terminal);
            push_test_terminal(&mut app, terminal);
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(40, 3, 1, pane));

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

    fn cursor_test_app_with_search_overlay() -> App {
        let mut app = App::new();
        let body = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        app.interaction.layout.search = Some(crate::search::terminal_search_geometry(body, 1.0));
        app
    }

    fn cursor_test_point(app: &mut App, (x, y): (f32, f32)) {
        app.pointer_x = x;
        app.pointer_y = y;
    }

    fn cursor_test_center(rect: &Rect) -> (f32, f32) {
        (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
    }

    #[test]
    fn desired_cursor_icon_maps_search_overlay_hits_to_cursor_kinds() {
        let mut app = cursor_test_app_with_search_overlay();
        let input = cursor_test_center(
            &app.interaction
                .layout
                .search
                .as_ref()
                .expect("search overlay")
                .input,
        );
        let close = cursor_test_center(
            &app.interaction
                .layout
                .search
                .as_ref()
                .expect("search overlay")
                .close,
        );

        cursor_test_point(&mut app, input);
        assert_eq!(app.desired_cursor_icon(), CursorKind::Text);
        cursor_test_point(&mut app, close);
        assert_eq!(app.desired_cursor_icon(), CursorKind::Pointer);
        cursor_test_point(&mut app, (-1000.0, -1000.0));
        assert_eq!(app.desired_cursor_icon(), CursorKind::Default);
    }

    #[test]
    fn desired_cursor_icon_close_confirm_suppresses_search_hit() {
        let mut app = cursor_test_app_with_search_overlay();
        let input = cursor_test_center(
            &app.interaction
                .layout
                .search
                .as_ref()
                .expect("search overlay")
                .input,
        );
        cursor_test_point(&mut app, input);
        assert_eq!(app.desired_cursor_icon(), CursorKind::Text);

        app.close_confirm = Some(CloseConfirm {
            target: CloseTarget::Tab(0),
            title: "Закрыть вкладку?",
            rows: vec!["tab — vim".to_string()],
            more: None,
            hovered: CloseConfirmHit::None,
        });
        assert_eq!(app.desired_cursor_icon(), CursorKind::Default);
    }

    #[test]
    fn desired_cursor_icon_open_settings_suppresses_search_hit() {
        let mut app = cursor_test_app_with_search_overlay();
        let input = cursor_test_center(
            &app.interaction
                .layout
                .search
                .as_ref()
                .expect("search overlay")
                .input,
        );
        cursor_test_point(&mut app, input);
        app.settings_open = true;
        assert_eq!(app.desired_cursor_icon(), CursorKind::Default);
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
        let settings_pane = settings_app.issue_pane_id();
        push_test_terminal(
            &mut settings_app,
            Terminal::new_for_test(8, 2, 1, settings_pane),
        );
        settings_app.active_terminal_presented = true;
        settings_app
            .interaction
            .set_pressed_mouse_buttons_for_test(1);
        settings_app.toggle_settings();
        assert_eq!(settings_app.interaction.pressed_mouse_buttons_for_test(), 0);

        let mut focus_app = App::new();
        let focus_pane = focus_app.issue_pane_id();
        push_test_terminal(&mut focus_app, Terminal::new_for_test(8, 2, 1, focus_pane));
        focus_app.active_terminal_presented = true;
        focus_app.interaction.set_pressed_mouse_buttons_for_test(1);
        focus_app.on_focus_changed(false);
        assert_eq!(focus_app.interaction.pressed_mouse_buttons_for_test(), 0);
    }

    #[test]
    fn ctrl_hover_link_change_marks_app_dirty_for_idle_redraw() {
        use crate::renderer::TerminalUiLayout;

        let mut app = App::new();
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(80, 30, 1, pane));
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
        let pane = app.issue_pane_id();
        push_test_terminal(&mut app, Terminal::new_for_test(80, 30, 1, pane));
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
    fn tab_strip_shows_one_label_per_tab_with_active_pane_title() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "t6-tab-labels",
                2,
            );
        // A second pane in tab 1 must not add a third strip label.
        let second_pane = attach_test_pane(&mut app, 1, 3);
        app.activate_pane(second_pane, false);
        assert_eq!(app.on_frame(), Ok(true));

        assert_eq!(app.tab_labels.len(), 2);
        assert_eq!(app.tab_labels.len(), app.tabs.len());
        let tab = &app.tabs[1];
        let label = &app.tab_labels[1];
        assert_eq!(label.id, tab.id);
        assert_eq!(label.number, tab.title_number);
        assert_eq!(label.leaf_count, 2);
        // The title comes from the active pane of the tab, not from its founding terminal.
        assert_eq!(app.terminals[label.title_terminal].pane_id(), second_pane);

        // The renderer keeps one hitbox per tab: the third terminal has none.
        let renderer = app.runtime.as_ref().expect("headless runtime").renderer();
        assert!(renderer.terminal_tab_body(0).is_some());
        assert!(renderer.terminal_tab_body(1).is_some());
        assert!(renderer.terminal_tab_body(2).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tab_hit_test_and_drag_use_tab_indices() {
        let (mut app, _receiver, _profile, root) =
            crate::app::headless_run::test_support::started_headless_with_test_terminals(
                "t6-tab-drag",
                2,
            );
        let moved_tab = app.tabs[1].id;
        let founding_pane = app.tabs[1].active_pane;
        let attached_pane = attach_test_pane(&mut app, 1, 3);
        let terminal_count = app.terminals.len();
        assert_eq!(app.on_frame(), Ok(true));

        // Two tabs but three terminals: the second tab's hitbox carries tab index 1.
        let second = app
            .runtime
            .as_ref()
            .expect("headless runtime")
            .renderer()
            .terminal_tab_body(1)
            .expect("second tab body");
        let (x, y) = (second[0] + second[2] * 0.5, second[1] + second[3] * 0.5);
        let hit = app
            .runtime
            .as_ref()
            .expect("headless runtime")
            .terminal_tab_hit_test(x, y);
        assert_eq!(hit, TerminalTabHit::Body(1));

        app.on_pointer_motion(PointerPosition { x, y });
        app.on_pointer_button(KeyState::Pressed, PointerButton::Left);
        assert_eq!(app.active_tab, 1);
        // Drag the second tab left of the first tab's centre.
        app.on_pointer_motion(PointerPosition { x: 0.0, y });
        app.on_pointer_button(KeyState::Released, PointerButton::Left);

        assert_eq!(app.tabs[0].id, moved_tab);
        assert!(split_contains_pane(&app.tabs[0].layout, founding_pane));
        assert!(split_contains_pane(&app.tabs[0].layout, attached_pane));
        assert_eq!(leaf_count(&app.tabs[0].layout), 2);
        assert_eq!(app.active_tab, 0);
        assert_eq!(app.terminals.len(), terminal_count);
        assert_eq!(app.on_frame(), Ok(true));
        assert_eq!(app.panels.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
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
