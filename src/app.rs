use crate::config::{
    AppConfig, DEFAULT_SCROLL_SENSITIVITY, DEFAULT_TERMINAL_BACKGROUND, DEFAULT_TERMINAL_FONT_SIZE,
    RgbColor, SCROLL_SENSITIVITY_STEP, TERMINAL_FONT_SIZE_STEP,
};
use crate::input::TerminalInteraction;
use crate::input_types::{
    CursorKind, KeyCode, KeyInput, KeyState, Modifiers, PhysicalKey, PointerButton,
    PointerPosition, ScrollDelta,
};
use crate::platform::{KdeActivationWorker, kde_session_active};
use crate::renderer::{SettingsHit, SettingsTab, TerminalTabHit};
use crate::runtime::{TerminalRenderParams, WindowRuntime};
use crate::scroll::ScrollState;
use crate::single_line_input::SingleLineInput;
use crate::tabs::{
    DRAG_AUTOSCROLL_EDGE_PX, TabDragState, active_index_after_move, active_index_after_remove,
    drag_autoscroll_delta, drag_autoscroll_speed, take_terminal_creation_number,
};
use crate::terminal::{Terminal, TerminalPresentationIntent};
use crate::terminal_process::{TerminalCleanupWorker, TerminalProcess};
use std::collections::VecDeque;
use std::time::Instant;

mod direct_wayland;

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
    NewTab,
    CloseTab,
    Search,
}

#[derive(PartialEq, Eq)]
enum ExternalLaunchAction {
    OpenDefaultTab,
    WaylandXdgActivation { activation_token: String },
    KdeForceActivate,
    WaylandBestEffort,
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
        PhysicalKey::Code(KeyCode::KeyT) if exact_global_modifiers(modifiers, true) => {
            Some(GlobalShortcut::NewTab)
        }
        PhysicalKey::Code(KeyCode::Digit4) if exact_global_modifiers(modifiers, false) => {
            Some(GlobalShortcut::CloseTab)
        }
        PhysicalKey::Code(KeyCode::KeyF) if exact_global_modifiers(modifiers, false) => {
            Some(GlobalShortcut::Search)
        }
        _ => None,
    }
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
    tab_hit: TerminalTabHit,
    tab_dragging: bool,
) -> CursorKind {
    if settings_modal_active {
        return if settings_text_dragging || settings_hit == SettingsHit::BackgroundField {
            CursorKind::Text
        } else {
            CursorKind::Default
        };
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
fn external_launch_action(
    focused: bool,
    kde_session: bool,
    request: crate::platform::single_instance::ExternalLaunchRequest,
) -> ExternalLaunchAction {
    if focused {
        ExternalLaunchAction::OpenDefaultTab
    } else if let Some(activation_token) = request.activation_token {
        ExternalLaunchAction::WaylandXdgActivation { activation_token }
    } else if kde_session {
        ExternalLaunchAction::KdeForceActivate
    } else {
        ExternalLaunchAction::WaylandBestEffort
    }
}

#[inline]
fn terminal_focus_report_sequence(enabled: bool, focused: bool) -> Option<&'static [u8]> {
    enabled.then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

#[inline]
fn terminal_focus_transition_plan(
    window_focused: bool,
    identity_changed: bool,
    old_reporting: bool,
    new_reporting: bool,
) -> (Option<&'static [u8]>, Option<&'static [u8]>) {
    if !window_focused || !identity_changed {
        return (None, None);
    }
    (
        terminal_focus_report_sequence(old_reporting, false),
        terminal_focus_report_sequence(new_reporting, true),
    )
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

pub struct App {
    runtime: Option<WindowRuntime>,
    config: AppConfig,
    config_dirty: bool,
    config_save_attempted: bool,
    settings_font_value: String,
    settings_scroll_value: String,
    settings_background_input: SingleLineInput,
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
    focused: bool,
    unfocused_redraw_pending: bool,
    pending_external_launches: VecDeque<crate::platform::single_instance::ExternalLaunchRequest>,
    occluded: bool,
    zero_sized: bool,
    dirty: bool,
    animation_active: bool,
    last_frame: Instant,
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
        let mut interaction = TerminalInteraction::default();
        interaction.set_scroll_sensitivity(config.scroll_sensitivity);
        let settings_font_value = format!("{:.0}", config.terminal_font_size);
        let settings_scroll_value = format!("{:.2}", config.scroll_sensitivity);
        let settings_background_input =
            SingleLineInput::from_text(&config.terminal_background.to_hex());
        Self {
            runtime: None,
            config,
            config_dirty: false,
            config_save_attempted: false,
            settings_font_value,
            settings_scroll_value,
            settings_background_input,
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
            focused: true,
            unfocused_redraw_pending: false,
            pending_external_launches: VecDeque::with_capacity(PENDING_EXTERNAL_LAUNCH_CAPACITY),
            occluded: false,
            zero_sized: false,
            dirty: true,
            animation_active: false,
            last_frame: Instant::now(),
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

    fn send_terminal_focus_state(&self, index: usize, focused: bool) {
        let Some(terminal) = self.terminals.get(index) else {
            return;
        };
        let reporting = crate::platform::lock_recover(&terminal.grid).focus_reporting;
        if let Some(sequence) = terminal_focus_report_sequence(reporting, focused) {
            let _ = terminal.write_input(sequence);
        }
    }

    fn send_terminal_focus_transition(&self, old: Option<usize>, new: Option<usize>) {
        if !self.focused || old == new {
            return;
        }
        let old_reporting = old
            .and_then(|index| self.terminals.get(index))
            .is_some_and(|terminal| crate::platform::lock_recover(&terminal.grid).focus_reporting);
        let new_reporting = new
            .and_then(|index| self.terminals.get(index))
            .is_some_and(|terminal| crate::platform::lock_recover(&terminal.grid).focus_reporting);
        let (focus_out, focus_in) =
            terminal_focus_transition_plan(self.focused, true, old_reporting, new_reporting);
        if let (Some(index), Some(sequence)) = (old, focus_out)
            && let Some(terminal) = self.terminals.get(index)
        {
            let _ = terminal.write_input(sequence);
        }
        if let (Some(index), Some(sequence)) = (new, focus_in)
            && let Some(terminal) = self.terminals.get(index)
        {
            let _ = terminal.write_input(sequence);
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
        self.request_terminal_activation(index, false);
    }

    fn add_terminal(&mut self) -> Option<usize> {
        let wake = self.runtime.as_ref()?.wake_handle();
        self.cancel_terminal_presentation_intents();
        let display_number = take_terminal_creation_number(&mut self.next_terminal_creation_number);
        let terminal = Terminal::spawn(Some(wake), display_number);
        let index = self.terminals.len();
        self.terminals.push(terminal);
        if self.terminals.len() == 1 {
            self.active_terminal = 0;
        }
        self.request_terminal_activation(index, true);
        self.request_frame();
        Some(index)
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
        match external_launch_action(self.focused, kde_session, request) {
            ExternalLaunchAction::OpenDefaultTab => {
                let _ = self.add_terminal();
            }
            ExternalLaunchAction::WaylandXdgActivation { activation_token } => {
                if let Some(runtime) = self.runtime.as_mut() {
                    runtime.activate_existing_window(Some(activation_token));
                }
            }
            ExternalLaunchAction::KdeForceActivate => {
                let worker = self
                    .kde_activation_worker
                    .get_or_insert_with(KdeActivationWorker::new);
                worker.try_activate(std::process::id());
            }
            ExternalLaunchAction::WaylandBestEffort => {
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
            if self.terminals[self.active_terminal].presentation_ready() {
                self.activate_ready_terminal(self.active_terminal, false);
            } else {
                self.cancel_terminal_presentation_intents();
                self.terminals[self.active_terminal].presentation_intent =
                    TerminalPresentationIntent::ActivateWhenReady;
            }
        }
        self.pending_tab_reveal = Some(false);
        self.request_frame();
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
        self.close_terminal_tab_at(self.active_terminal)
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

    fn on_focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        self.unfocused_redraw_pending = !focused;
        self.modifiers = Modifiers::empty();
        self.interaction.set_modifiers(Modifiers::empty());
        if self.active_terminal_presented {
            self.send_terminal_focus_state(self.active_terminal, focused);
        }
        if !focused {
            self.cancel_pointer_interactions();
            self.settings_background_dragging = false;
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
        if let Some(runtime) = self.runtime.as_mut() {
            let _ = runtime.set_terminal_background(color);
        }
        true
    }

    fn apply_valid_settings_background_draft(&mut self) {
        if let Some(color) = RgbColor::parse_hex(&self.settings_background_input.text)
            && self.set_terminal_background(color)
        {
            self.mark_config_dirty();
        }
    }

    fn finish_settings_background_edit(&mut self) {
        if !self.settings_background_editing {
            return;
        }
        self.apply_valid_settings_background_draft();
        self.settings_background_input
            .set_text(&self.config.terminal_background.to_hex());
        self.settings_background_editing = false;
        self.settings_background_dragging = false;
        self.request_frame();
    }

    fn edit_settings_background_text(&mut self, text: &str) -> bool {
        if !self.settings_background_editing {
            return false;
        }
        if settings_hex_insert_text(&mut self.settings_background_input, text) {
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
        let outcome = settings_background_editor_key(
            &mut self.settings_background_input,
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
            SettingsHit::BackgroundReset => {
                let changed = self.set_terminal_background(DEFAULT_TERMINAL_BACKGROUND);
                self.settings_background_input
                    .set_text(&DEFAULT_TERMINAL_BACKGROUND.to_hex());
                changed
            }
            SettingsHit::None
            | SettingsHit::Outside
            | SettingsHit::Tab(_)
            | SettingsHit::BackgroundField => false,
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

    fn desired_cursor_icon(&self) -> CursorKind {
        let settings_modal_active = self.settings_modal_active();
        let settings_hit = if settings_modal_active {
            self.settings_hit_test(self.pointer_x, self.pointer_y)
        } else {
            SettingsHit::None
        };
        let tab_hit = if settings_modal_active {
            TerminalTabHit::None
        } else {
            self.runtime
                .as_ref()
                .map_or(TerminalTabHit::None, |runtime| {
                    runtime.terminal_tab_hit_test(self.pointer_x, self.pointer_y)
                })
        };
        ui_cursor_icon(
            settings_modal_active,
            settings_hit,
            self.settings_background_dragging,
            tab_hit,
            self.terminal_tab_drag.is_some(),
        )
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
        let plan = frame_plan(
            self.renderable(),
            self.dirty,
            self.focused && self.animation_active,
        );
        let now = Instant::now();
        let search_deadline = (self.focused && self.renderable())
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
                .map_or(AppLoopControl::Wait, AppLoopControl::WaitUntil),
            LoopMode::Poll => AppLoopControl::Poll,
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
        }
        false
    }

    fn shutdown_all(&mut self) {
        for terminal in &mut self.terminals {
            terminal.shutdown();
        }
        self.terminals.clear();
        while let Some(mut process) = self.pending_terminal_cleanup.pop_front() {
            process.shutdown();
        }
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
            if self.terminals[index].is_closed() && self.close_terminal_tab_at(index) {
                return true;
            }
        }
        false
    }
}

impl App {
    fn on_runtime_ready(&mut self, runtime: WindowRuntime, width: u32, height: u32) {
        self.zero_sized = width == 0 || height == 0;
        if std::env::var_os("RONSOLE_GL_DIAGNOSTICS")
            .is_some_and(|value| value != std::ffi::OsStr::new("0"))
        {
            eprintln!("Ronsole graphics:\n{}", runtime.diagnostics_report());
        }
        self.runtime = Some(runtime);
        self.last_frame = Instant::now();
        let _ = self.add_terminal();
        self.flush_pending_external_launches();
        self.mark_dirty();
    }

    fn on_close_requested(&mut self) {
        self.prepare_exit();
        self.shutdown_all();
    }

    fn on_resize_with_logical_size(
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
        self.mark_dirty();
    }

    fn on_scale_changed(&mut self, scale_factor: f32, width: u32, height: u32) {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.update_scale_factor(scale_factor);
            if width > 0 && height > 0 {
                runtime.resize(width, height);
            }
        }
        self.terminal_tab_scroll.reset();
        self.mark_dirty();
    }

    fn on_modifiers(&mut self, modifiers: Modifiers) {
        self.modifiers = modifiers;
        self.interaction.set_modifiers(modifiers);
    }

    fn on_ime_commit(&mut self, text: &str) {
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

    fn on_key(&mut self, key_input: KeyInput) -> bool {
        if key_input.state.is_pressed()
            && let Some(shortcut) = global_shortcut(key_input.physical_key, self.modifiers)
        {
            if self.settings_modal_active() && shortcut != GlobalShortcut::ToggleSettings {
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

    fn on_pointer_motion(&mut self, position: PointerPosition) {
        let previous_x = self.pointer_x;
        let previous_y = self.pointer_y;
        self.pointer_x = position.x;
        self.pointer_y = position.y;
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
        self.refresh_cursor_icon();
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

    fn on_pointer_leave(&mut self, width: u32, height: u32) {
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

    fn on_pointer_button(&mut self, state: KeyState, button: PointerButton) -> bool {
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
                TerminalTabHit::Close(index) => return self.close_terminal_tab_at(index),
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
        false
    }

    fn on_scroll(&mut self, delta: ScrollDelta) {
        if self.settings_modal_active() {
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
            && self.interaction.mouse_wheel(delta, terminal)
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

        let render_result = if let Some(runtime) = self.runtime.as_mut() {
            Some(runtime.render_terminal_and_present(TerminalRenderParams {
                terminals: &self.terminals,
                active_terminal: self.active_terminal,
                search: &mut self.interaction.search,
                focused: self.focused,
                tab_scroll_x: self.terminal_tab_scroll.current,
                drag: self.terminal_tab_drag.as_ref(),
                pointer_x: self.pointer_x,
                pointer_y: self.pointer_y,
                settings_progress: self.settings_progress,
                settings_tab: self.settings_active_tab,
                settings_font_value: &self.settings_font_value,
                settings_scroll_value: &self.settings_scroll_value,
                settings_background_input: &mut self.settings_background_input,
                settings_background_editing: self.settings_background_editing,
            }))
        } else {
            None
        };
        match render_result {
            Some(Ok(layout)) => {
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

    fn on_about_to_wait(&mut self) -> AppLoopControl {
        self.flush_pending_terminal_cleanup();
        if self.remove_closed_terminals() {
            self.prepare_exit();
            return AppLoopControl::Exit;
        }
        if self.process_terminal_presentation_intents() {
            self.request_frame();
        }
        if !self.renderable() {
            self.suspend_frame_clock();
        }
        self.sync_animation_state();
        self.loop_control()
    }

    fn on_exiting(&mut self) {
        self.persist_config_if_needed();
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(production.matches("self.add_terminal()").count(), 4);
        assert!(production.contains("GlobalShortcut::NewTab"));
        assert!(production.contains("TerminalTabHit::Add"));

        let close = production
            .split("    fn close_terminal_tab_at")
            .nth(1)
            .and_then(|tail| tail.split("    fn close_active_terminal").next())
            .expect("shared terminal close lifecycle must remain present");
        assert!(close.contains("self.terminals.remove(index)"));
        assert!(close.contains("if self.terminals.is_empty()"));
        assert!(close.contains("return true;"));
        assert!(!close.contains("add_terminal"));
        assert_eq!(production.matches("self.close_terminal_tab_at(").count(), 3);

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
        let terminal = Terminal::spawn(None, 1);
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
    fn global_shortcuts_use_plain_f1_and_exact_application_modifiers() {
        let none = Modifiers::empty();
        let ctrl = Modifiers::CONTROL;
        let ctrl_shift = Modifiers::CONTROL | Modifiers::SHIFT;
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::F1), none),
            Some(GlobalShortcut::ToggleSettings)
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
    fn focused_external_launch_with_token_opens_tab_without_using_token() {
        assert!(matches!(
            external_launch_action(
                true,
                true,
                crate::platform::single_instance::ExternalLaunchRequest {
                    activation_token: Some("one-shot-token".to_owned()),
                },
            ),
            ExternalLaunchAction::OpenDefaultTab
        ));
    }

    #[test]
    fn focused_external_launch_without_token_opens_tab() {
        assert!(matches!(
            external_launch_action(
                true,
                true,
                crate::platform::single_instance::ExternalLaunchRequest::default(),
            ),
            ExternalLaunchAction::OpenDefaultTab
        ));
    }

    #[test]
    fn unfocused_external_launch_with_supplied_token_uses_only_xdg_activation() {
        let action = external_launch_action(
            false,
            true,
            crate::platform::single_instance::ExternalLaunchRequest {
                activation_token: Some("one-shot-token".to_owned()),
            },
        );
        assert!(matches!(
            action,
            ExternalLaunchAction::WaylandXdgActivation {
                activation_token
            } if activation_token == "one-shot-token"
        ));
    }

    #[test]
    fn unfocused_kde_launch_without_token_uses_force_activation_worker() {
        assert!(matches!(
            external_launch_action(
                false,
                true,
                crate::platform::single_instance::ExternalLaunchRequest::default(),
            ),
            ExternalLaunchAction::KdeForceActivate
        ));
    }

    #[test]
    fn unfocused_non_kde_launch_without_token_keeps_generic_wayland_best_effort() {
        assert!(matches!(
            external_launch_action(
                false,
                false,
                crate::platform::single_instance::ExternalLaunchRequest::default(),
            ),
            ExternalLaunchAction::WaylandBestEffort
        ));
    }

    #[test]
    fn focus_reporting_uses_xterm_sequences_only_when_enabled() {
        assert_eq!(terminal_focus_report_sequence(false, true), None);
        assert_eq!(terminal_focus_report_sequence(false, false), None);
        assert_eq!(
            terminal_focus_report_sequence(true, true),
            Some(b"\x1b[I".as_slice())
        );
        assert_eq!(
            terminal_focus_report_sequence(true, false),
            Some(b"\x1b[O".as_slice())
        );
    }

    #[test]
    fn committed_tab_focus_transition_plans_xterm_out_then_in_only_when_app_is_focused() {
        assert_eq!(
            terminal_focus_transition_plan(true, true, true, true),
            (Some(b"\x1b[O".as_slice()), Some(b"\x1b[I".as_slice()))
        );
        assert_eq!(
            terminal_focus_transition_plan(true, true, false, true),
            (None, Some(b"\x1b[I".as_slice()))
        );
        assert_eq!(
            terminal_focus_transition_plan(true, false, true, true),
            (None, None)
        );
        assert_eq!(
            terminal_focus_transition_plan(false, true, true, true),
            (None, None)
        );
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
                TerminalTabHit::Add,
                false
            ),
            CursorKind::Text
        );
        assert_eq!(
            ui_cursor_icon(true, SettingsHit::None, false, TerminalTabHit::Add, false),
            CursorKind::Default
        );
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
                TerminalTabHit::Close(0),
                false
            ),
            CursorKind::Pointer
        );
        assert_eq!(
            ui_cursor_icon(false, SettingsHit::None, false, TerminalTabHit::Add, false),
            CursorKind::Pointer
        );
        assert_eq!(
            ui_cursor_icon(
                false,
                SettingsHit::None,
                false,
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
            .and_then(|tail| tail.split("    fn on_focus_changed").next())
            .expect("shared pointer cancellation helper must remain present");
        assert!(shared.contains("self.terminal_tab_drag = None;"));
        assert!(shared.contains("self.interaction.cancel_pointer_interaction(terminal);"));

        let focus = production
            .split("    fn on_focus_changed")
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
        crate::platform::lock_recover(&app.terminals[0].grid).selection = Some((1, 2, 5, 2));

        assert!(app.clear_active_terminal_text_selection());
        assert!(
            crate::platform::lock_recover(&app.terminals[0].grid)
                .selection
                .is_none()
        );
        assert!(!app.clear_active_terminal_text_selection());
    }

    #[test]
    fn left_press_clear_and_cursor_leave_are_routed_before_ui_short_circuits() {
        let source = include_str!("app.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        let mouse = production
            .split("    fn on_pointer_button")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_scroll").next())
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
            .split("    fn on_pointer_motion")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_pointer_leave").next())
            .expect("cursor moved routing must remain present");
        assert!(moved.contains("!self.interaction.terminal_selection_active()"));
        let left = production
            .split("    fn on_pointer_leave")
            .nth(1)
            .and_then(|tail| tail.split("    fn on_pointer_button").next())
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
