use crate::input::TerminalInteraction;
use crate::renderer::TerminalTabHit;
use crate::runtime::{TerminalRenderParams, WindowRuntime};
use crate::scroll::ScrollState;
use crate::tabs::{
    DRAG_AUTOSCROLL_EDGE_PX, TabDragState, active_index_after_move,
    active_index_after_remove, drag_autoscroll_delta, drag_autoscroll_speed,
    take_terminal_creation_number,
};
use crate::terminal::{Terminal, TerminalPresentationIntent};
use crate::terminal_process::{TerminalCleanupWorker, TerminalProcess};
use std::collections::VecDeque;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::WindowId;

const TERMINAL_CLEANUP_PENDING_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopMode {
    Wait,
    Poll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FramePlan {
    request_redraw: bool,
    loop_mode: LoopMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlobalShortcut {
    ToggleSettings,
    NewTab,
    CloseTab,
    Search,
}

#[inline(always)]
fn animation_dt(raw_dt: f32) -> f32 {
    raw_dt.min(0.016)
}

#[inline(always)]
fn frame_plan(renderable: bool, dirty: bool, animation_active: bool) -> FramePlan {
    if !renderable {
        return FramePlan {
            request_redraw: false,
            loop_mode: LoopMode::Wait,
        };
    }

    FramePlan {
        request_redraw: dirty || animation_active,
        loop_mode: if animation_active {
            LoopMode::Poll
        } else {
            LoopMode::Wait
        },
    }
}

#[inline]
fn exact_global_modifiers(modifiers: ModifiersState, shift: bool) -> bool {
    modifiers.control_key()
        && modifiers.shift_key() == shift
        && !modifiers.alt_key()
        && !modifiers.super_key()
}

fn global_shortcut(key: PhysicalKey, modifiers: ModifiersState) -> Option<GlobalShortcut> {
    match key {
        PhysicalKey::Code(KeyCode::F1) if exact_global_modifiers(modifiers, false) => {
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
fn tab_wheel_delta(delta: MouseScrollDelta, scale: f32) -> f32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => -y * 40.0 * scale.max(0.1),
        MouseScrollDelta::PixelDelta(position) => -(position.y as f32),
    }
}

fn ready_terminal_presentation<I>(states: I) -> Option<(usize, bool)>
where
    I: IntoIterator<Item = (usize, TerminalPresentationIntent, bool, bool)>,
{
    states.into_iter().find_map(|(index, intent, ready, reveal_tail)| {
        (intent == TerminalPresentationIntent::ActivateWhenReady && ready)
            .then_some((index, reveal_tail))
    })
}

pub struct App {
    runtime: Option<WindowRuntime>,
    terminals: Vec<Terminal>,
    active_terminal: usize,
    active_terminal_presented: bool,
    next_terminal_creation_number: u64,
    terminal_tab_scroll: ScrollState,
    terminal_tab_drag: Option<TabDragState>,
    pending_tab_reveal: Option<bool>,
    interaction: TerminalInteraction,
    terminal_cleanup: TerminalCleanupWorker,
    pending_terminal_cleanup: VecDeque<TerminalProcess>,
    modifiers: ModifiersState,
    pointer_x: f32,
    pointer_y: f32,
    settings_open: bool,
    settings_progress: f32,
    focused: bool,
    occluded: bool,
    zero_sized: bool,
    dirty: bool,
    animation_active: bool,
    last_frame: Instant,
}

impl App {
    pub fn new() -> Self {
        Self {
            runtime: None,
            terminals: Vec::new(),
            active_terminal: 0,
            active_terminal_presented: false,
            next_terminal_creation_number: 1,
            terminal_tab_scroll: ScrollState::new(7.0),
            terminal_tab_drag: None,
            pending_tab_reveal: None,
            interaction: TerminalInteraction::default(),
            terminal_cleanup: TerminalCleanupWorker::new(),
            pending_terminal_cleanup: VecDeque::with_capacity(TERMINAL_CLEANUP_PENDING_CAPACITY),
            modifiers: ModifiersState::empty(),
            pointer_x: 0.0,
            pointer_y: 0.0,
            settings_open: false,
            settings_progress: 0.0,
            focused: true,
            occluded: false,
            zero_sized: false,
            dirty: true,
            animation_active: false,
            last_frame: Instant::now(),
        }
    }

    #[inline(always)]
    fn renderable(&self) -> bool {
        self.focused && !self.occluded && !self.zero_sized && self.runtime.is_some()
    }

    #[inline(always)]
    fn settings_modal_active(&self) -> bool {
        self.settings_open || self.settings_progress > 0.001
    }

    #[inline(always)]
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn suspend_frame_clock(&mut self) {
        self.last_frame = Instant::now();
    }

    fn request_redraw(&mut self) {
        self.mark_dirty();
        if let Some(runtime) = self.runtime.as_ref() {
            runtime.window().request_redraw();
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
        let (focus_out, focus_in) = terminal_focus_transition_plan(
            self.focused,
            true,
            old_reporting,
            new_reporting,
        );
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
        let ready = ready_terminal_presentation(
            self.terminals.iter().enumerate().map(|(index, terminal)| {
                (
                    index,
                    terminal.presentation_intent,
                    terminal.presentation_ready(),
                    terminal.reveal_right_tail_when_presented,
                )
            }),
        );
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
        let window = self.runtime.as_ref()?.window_arc();
        self.cancel_terminal_presentation_intents();
        let display_number = take_terminal_creation_number(&mut self.next_terminal_creation_number);
        let terminal = Terminal::spawn(Some(window), display_number);
        let index = self.terminals.len();
        self.terminals.push(terminal);
        if self.terminals.len() == 1 {
            self.active_terminal = 0;
        }
        self.request_terminal_activation(index, true);
        self.request_redraw();
        Some(index)
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
        self.request_redraw();
        false
    }

    fn try_schedule_terminal_cleanup(
        &mut self,
        process: TerminalProcess,
    ) -> Result<(), TerminalProcess> {
        if !self.terminal_cleanup.is_available() {
            return Err(process);
        }
        let wake_window = self.runtime.as_ref().map(WindowRuntime::window_arc);
        match self.terminal_cleanup.try_enqueue(process, wake_window) {
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
        let wake_window = self.runtime.as_ref().map(WindowRuntime::window_arc);
        while let Some(process) = self.pending_terminal_cleanup.pop_front() {
            match self
                .terminal_cleanup
                .try_enqueue(process, wake_window.clone())
            {
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

    fn handle_focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        self.modifiers = ModifiersState::empty();
        self.interaction.set_modifiers(ModifiersState::empty());
        if self.active_terminal_presented {
            self.send_terminal_focus_state(self.active_terminal, focused);
        }
        if !focused {
            self.cancel_pointer_interactions();
        }
        self.suspend_frame_clock();
        if focused {
            self.mark_dirty();
        }
    }

    fn toggle_settings(&mut self) {
        let opening = !self.settings_open;
        if opening {
            self.cancel_pointer_interactions();
        }
        self.settings_open = !self.settings_open;
        self.request_redraw();
    }

    fn close_settings(&mut self) {
        if self.settings_open {
            self.settings_open = false;
            self.request_redraw();
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
        let settings_animation = (self.settings_progress - if self.settings_open { 1.0 } else { 0.0 })
            .abs()
            > 0.001;
        self.animation_active = terminal_animation || tab_animation || settings_animation;
    }

    fn apply_frame_plan(&self, event_loop: &ActiveEventLoop) {
        let plan = frame_plan(self.renderable(), self.dirty, self.animation_active);
        let now = Instant::now();
        let search_deadline = self
            .renderable()
            .then(|| self.interaction.search_refresh_deadline())
            .flatten();
        let search_due = search_deadline.is_some_and(|deadline| deadline <= now);
        if plan.request_redraw || search_due {
            if let Some(runtime) = self.runtime.as_ref() {
                runtime.window().request_redraw();
            }
        }
        match plan.loop_mode {
            LoopMode::Wait => {
                if let Some(deadline) = search_deadline.filter(|deadline| *deadline > now) {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                } else {
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
            LoopMode::Poll => event_loop.set_control_flow(ControlFlow::Poll),
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
        let new = (old + delta.signum() * drag_autoscroll_speed(delta) * dt)
            .clamp(0.0, strip.max_scroll);
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
                    self.request_redraw();
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.runtime.is_some() {
            return;
        }

        match WindowRuntime::bootstrap(event_loop) {
            Ok(runtime) => {
                self.zero_sized = runtime.window().inner_size().width == 0
                    || runtime.window().inner_size().height == 0;
                if std::env::var_os("RONSOLE_GL_DIAGNOSTICS")
                    .is_some_and(|value| value != std::ffi::OsStr::new("0"))
                {
                    eprintln!("Ronsole graphics:\n{}", runtime.diagnostics_report());
                }
                self.runtime = Some(runtime);
                self.last_frame = Instant::now();
                let _ = self.add_terminal();
                self.mark_dirty();
            }
            Err(error) => {
                eprintln!("Ronsole: {error}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        if runtime.window().id() != id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                self.shutdown_all();
                event_loop.exit();
            }
            WindowEvent::Focused(focused) => {
                self.handle_focus_changed(focused);
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                self.suspend_frame_clock();
                if !occluded {
                    self.mark_dirty();
                }
            }
            WindowEvent::Resized(size) => {
                self.zero_sized = size.width == 0 || size.height == 0;
                self.suspend_frame_clock();
                if !self.zero_sized {
                    if let Some(runtime) = self.runtime.as_mut() {
                        runtime.resize(size.width, size.height);
                    }
                    self.mark_dirty();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = self
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.window().inner_size());
                if let Some(runtime) = self.runtime.as_mut() {
                    runtime.update_scale_factor(scale_factor as f32);
                    if let Some(size) = size.filter(|size| size.width > 0 && size.height > 0) {
                        runtime.resize(size.width, size.height);
                    }
                }
                self.terminal_tab_scroll.reset();
                self.mark_dirty();
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                self.interaction.set_modifiers(self.modifiers);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if self.settings_modal_active() {
                    return;
                }
                let active = self.active_terminal;
                if let Some(terminal) = self.terminals.get_mut(active)
                    && self.interaction.handle_ime_commit(&text, terminal)
                {
                    self.sync_animation_state();
                    self.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if key_event.state == ElementState::Pressed
                    && let Some(shortcut) = global_shortcut(key_event.physical_key, self.modifiers)
                {
                    if self.settings_modal_active()
                        && shortcut != GlobalShortcut::ToggleSettings
                    {
                        return;
                    }
                    if self.handle_global_key(shortcut) {
                        event_loop.exit();
                        return;
                    }
                    self.sync_animation_state();
                    return;
                }
                if self.settings_modal_active() {
                    if key_event.state == ElementState::Pressed
                        && key_event.physical_key == PhysicalKey::Code(KeyCode::Escape)
                    {
                        self.close_settings();
                    }
                    return;
                }
                let active = self.active_terminal;
                if let Some(terminal) = self.terminals.get_mut(active)
                    && self.interaction.handle_key_event(&key_event, terminal)
                {
                    self.sync_animation_state();
                    self.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_x = position.x as f32;
                self.pointer_y = position.y as f32;
                if self.settings_modal_active() {
                    return;
                }
                if let Some(drag) = self.terminal_tab_drag.as_mut() {
                    drag.current_x = self.pointer_x;
                    if !drag.threshold_passed {
                        let scale = self.runtime.as_ref().map_or(1.0, WindowRuntime::scale_factor);
                        drag.threshold_passed =
                            drag_threshold_passed(drag.start_x, drag.current_x, scale);
                    }
                    self.sync_animation_state();
                    self.request_redraw();
                    return;
                }
                if self.runtime.as_ref().is_some_and(|runtime| {
                    runtime
                        .terminal_tab_strip_layout()
                        .rect
                        .contains(self.pointer_x, self.pointer_y)
                }) {
                    self.request_redraw();
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
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.settings_modal_active() {
                    return;
                }
                if button == MouseButton::Left && state == ElementState::Pressed {
                    let hit = self.runtime.as_ref().map_or(TerminalTabHit::None, |runtime| {
                        runtime.terminal_tab_hit_test(self.pointer_x, self.pointer_y)
                    });
                    match hit {
                        TerminalTabHit::Close(index) => {
                            if self.close_terminal_tab_at(index) {
                                event_loop.exit();
                            }
                            return;
                        }
                        TerminalTabHit::Add => {
                            let _ = self.add_terminal();
                            return;
                        }
                        TerminalTabHit::Body(index) => {
                            self.terminal_tab_drag = Some(TabDragState {
                                start_idx: index,
                                start_x: self.pointer_x,
                                current_x: self.pointer_x,
                                threshold_passed: false,
                            });
                            self.select_terminal_tab_from_user(index);
                            self.request_redraw();
                            return;
                        }
                        TerminalTabHit::None => {}
                    }
                }
                if button == MouseButton::Left && state == ElementState::Released {
                    if let Some(drag) = self.terminal_tab_drag.take() {
                        if drag.threshold_passed
                            && let Some(destination) = self
                                .runtime
                                .as_ref()
                                .and_then(|runtime| runtime.terminal_tab_drag_destination(&drag))
                        {
                            self.reorder_terminal_tab(drag.start_idx, destination);
                        }
                        self.sync_animation_state();
                        self.request_redraw();
                        return;
                    }
                }
                let active = self.active_terminal;
                if let (Some(runtime), Some(terminal)) =
                    (self.runtime.as_mut(), self.terminals.get_mut(active))
                {
                    let handled = self.interaction.mouse_input(
                        state,
                        button,
                        terminal,
                        |text, x, scroll| runtime.terminal_search_cursor_from_x(text, x, scroll),
                    );
                    if handled {
                        self.sync_animation_state();
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
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
                        self.request_redraw();
                        return;
                    }
                }
                let active = self.active_terminal;
                if let Some(terminal) = self.terminals.get_mut(active)
                    && self.interaction.mouse_wheel(delta, terminal)
                {
                    self.sync_animation_state();
                    self.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if !self.renderable() {
                    event_loop.set_control_flow(ControlFlow::Wait);
                    return;
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
                if let Some(terminal) = self.terminals.get_mut(active) {
                    terminal.scroll_y.update(dt);
                    terminal
                        .scroll_y
                        .clamp_target(0.0, self.interaction.layout.max_scroll);
                    terminal
                        .scroll_y
                        .clamp_current(0.0, self.interaction.layout.max_scroll);
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
                        self.sync_animation_state();
                        self.dirty = false;
                    }
                    Some(Err(error)) => {
                        eprintln!("Ronsole: frame presentation failed: {error}");
                        event_loop.exit();
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.shutdown_all();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_pending_terminal_cleanup();
        if self.remove_closed_terminals() {
            event_loop.exit();
            return;
        }
        if self.process_terminal_presentation_intents() {
            self.request_redraw();
        }
        if !self.renderable() {
            self.suspend_frame_clock();
        }
        self.sync_animation_state();
        self.apply_frame_plan(event_loop);
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
                request_redraw: false,
                loop_mode: LoopMode::Wait,
            }
        );
        assert_eq!(
            frame_plan(false, true, true),
            FramePlan {
                request_redraw: false,
                loop_mode: LoopMode::Wait,
            }
        );
    }

    #[test]
    fn redraw_plan_only_polls_for_live_animation() {
        assert_eq!(
            frame_plan(true, true, false),
            FramePlan {
                request_redraw: true,
                loop_mode: LoopMode::Wait,
            }
        );
        assert_eq!(
            frame_plan(true, false, true),
            FramePlan {
                request_redraw: true,
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
        assert_eq!(production.matches("self.add_terminal()").count(), 3);
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
            .and_then(|tail| tail.split("    fn process_terminal_presentation_intents").next())
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
            .write_input(
                format!("trap '' TERM; printf ready > {}\r", marker.display()).as_bytes(),
            )
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(Instant::now() < ready_deadline, "shell did not install SIGTERM trap");
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
    fn global_shortcuts_require_exact_application_modifiers() {
        let ctrl = ModifiersState::CONTROL;
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::F1), ctrl),
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
            ctrl | ModifiersState::ALT,
            ctrl | ModifiersState::SUPER,
            ctrl | ModifiersState::SHIFT,
        ] {
            assert_eq!(global_shortcut(PhysicalKey::Code(KeyCode::F1), modifiers), None);
        }
        for modifiers in [
            ctrl_shift | ModifiersState::ALT,
            ctrl_shift | ModifiersState::SUPER,
        ] {
            assert_eq!(global_shortcut(PhysicalKey::Code(KeyCode::KeyT), modifiers), None);
        }
        for key in [KeyCode::Digit4, KeyCode::KeyF] {
            for modifiers in [ctrl | ModifiersState::ALT, ctrl | ModifiersState::SUPER] {
                assert_eq!(global_shortcut(PhysicalKey::Code(key), modifiers), None);
            }
        }
        assert_eq!(
            global_shortcut(PhysicalKey::Code(KeyCode::F1), ModifiersState::empty()),
            None,
        );
        assert_eq!(global_shortcut(PhysicalKey::Code(KeyCode::KeyT), ctrl), None);
    }

    #[test]
    fn focus_reporting_uses_xterm_sequences_only_when_enabled() {
        assert_eq!(terminal_focus_report_sequence(false, true), None);
        assert_eq!(terminal_focus_report_sequence(false, false), None);
        assert_eq!(terminal_focus_report_sequence(true, true), Some(b"\x1b[I".as_slice()));
        assert_eq!(terminal_focus_report_sequence(true, false), Some(b"\x1b[O".as_slice()));
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
    fn settings_open_and_focus_loss_clear_tracked_physical_mouse_buttons() {
        let mut settings_app = App::new();
        settings_app.terminals.push(Terminal::new_for_test(8, 2, 1));
        settings_app.active_terminal_presented = true;
        settings_app
            .interaction
            .set_pressed_mouse_buttons_for_test(1);
        settings_app.toggle_settings();
        assert_eq!(
            settings_app.interaction.pressed_mouse_buttons_for_test(),
            0
        );

        let mut focus_app = App::new();
        focus_app.terminals.push(Terminal::new_for_test(8, 2, 1));
        focus_app.active_terminal_presented = true;
        focus_app.interaction.set_pressed_mouse_buttons_for_test(1);
        focus_app.handle_focus_changed(false);
        assert_eq!(focus_app.interaction.pressed_mouse_buttons_for_test(), 0);
    }

    #[test]
    fn focus_transition_clears_app_and_terminal_modifiers_and_stale_shortcuts() {
        let mut app = App::new();
        app.modifiers = ModifiersState::CONTROL;
        app.interaction.set_modifiers(ModifiersState::CONTROL);
        app.handle_focus_changed(false);
        assert_eq!(app.modifiers, ModifiersState::empty());
        assert_eq!(app.interaction.modifiers_for_test(), ModifiersState::empty());
        app.handle_focus_changed(true);
        assert_eq!(app.modifiers, ModifiersState::empty());
        for key in [KeyCode::KeyT, KeyCode::KeyF, KeyCode::Digit4, KeyCode::F1] {
            assert_eq!(
                global_shortcut(PhysicalKey::Code(key), app.modifiers),
                None,
            );
        }
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
        focus_app.handle_focus_changed(false);
        assert!(!focus_app.focused);
        assert!(focus_app.terminal_tab_drag.is_none());
    }

    #[test]
    fn settings_and_focus_loss_share_pointer_cancellation_path() {
        let source = include_str!("app.rs");
        let production = source.split("
#[cfg(test)]").next().unwrap_or(source);
        let shared = production
            .split("    fn cancel_pointer_interactions")
            .nth(1)
            .and_then(|tail| tail.split("    fn handle_focus_changed").next())
            .expect("shared pointer cancellation helper must remain present");
        assert!(shared.contains("self.terminal_tab_drag = None;"));
        assert!(shared.contains("self.interaction.cancel_pointer_interaction(terminal);"));

        let focus = production
            .split("    fn handle_focus_changed")
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
            tab_wheel_delta(MouseScrollDelta::LineDelta(0.0, 1.0), 1.0),
            -40.0
        );
        assert_eq!(
            tab_wheel_delta(
                MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, -12.5)),
                1.0,
            ),
            12.5
        );
    }
}
