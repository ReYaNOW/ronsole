use crate::input_types::{
    KeyCode, KeyInput, KeyState, Modifiers, PhysicalKey, PointerButton, ScrollDelta,
};
use crate::platform::Clipboard;
use crate::renderer::{TerminalUiLayout, terminal_scrollbar_drag_target, visible_row_range};
use crate::search::{SearchRefreshCause, TerminalSearchHit, TerminalSearchState};
use crate::terminal::{Cell, MouseTrackingMode, Terminal};
use crate::wayland_input::us_base_char_from_evdev;
use ronsole_term::{
    KeyEvent, KeyKind, KeyMods, KeyOutcome, KpKey, LinkTarget, MouseButton, MouseEventKind, Term,
    TermKey, bracketed_paste_bytes, mouse_report, ModKey,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Whether the smooth scroll of `terminal` still moves. Shared by the active
/// tab's animation check and the frame loop that keeps grid tiles animating.
pub(crate) fn terminal_scroll_animating(terminal: &Terminal) -> bool {
    !terminal.scroll_y.is_settled()
}

pub(crate) struct TerminalInteraction {
    pub search: TerminalSearchState,
    pub layout: TerminalUiLayout,
    modifiers: Modifiers,
    clipboard: ClipboardOwner,
    mouse_x: f32,
    mouse_y: f32,
    pressed_mouse_buttons: u8,
    pub(crate) pointer_capture: PointerCapture,
    terminal_selection_anchor: Option<(usize, usize)>,
    /// Unit that a drag from `terminal_selection_anchor` extends the selection by.
    terminal_selection_unit: SelectionUnit,
    /// Word or line span selected by the press, which a drag keeps as its origin.
    terminal_selection_base: SelectionSpan,
    /// Recent left presses, used to count double and triple clicks.
    click_counter: ClickCounter,
    scroll_sensitivity: f32,
    /// URI of the last Ctrl+click on a link, until `take_link_uri` hands it out.
    link_uri: Option<Box<str>>,
    /// True from a Ctrl+press on a link until the matching release.
    link_press_pending: bool,
    /// Key whose press the host consumed as a copy; its repeat and release stay away
    /// from the encoder until that key is released.
    held_host_key: Option<PhysicalKey>,
    /// Physical modifier keys held now, one bit per key as given by `modifier_key_bits`.
    held_modifier_keys: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PointerCapture {
    #[default]
    None,
    TerminalSelection,
    TerminalScrollbar,
    SearchInput,
    SearchControl,
}

/// Owns clipboard text and file lists, so every access goes through one place.
enum ClipboardOwner {
    /// The Wayland clipboard, created on first use. A failed creation leaves the
    /// slot empty, and the next access tries again.
    System(Option<Clipboard>),
    /// Headless buffer that keeps the last written text and has no file list.
    Memory { text: Option<String> },
}

impl ClipboardOwner {
    fn get_text(&mut self) -> Option<String> {
        match self {
            Self::System(slot) => system_clipboard(slot)?.get_text().ok(),
            Self::Memory { text } => text.clone(),
        }
    }

    fn set_text(&mut self, text: String) -> bool {
        match self {
            Self::System(slot) => {
                system_clipboard(slot).is_some_and(|clipboard| clipboard.set_text(text).is_ok())
            }
            Self::Memory { text: slot } => {
                *slot = Some(text);
                true
            }
        }
    }

    /// Reports whether a clipboard exists, creating the system one on first use.
    /// A failed write does not make it unavailable.
    fn is_available(&mut self) -> bool {
        match self {
            Self::System(slot) => system_clipboard(slot).is_some(),
            Self::Memory { .. } => true,
        }
    }

    fn get_file_list(&mut self) -> Option<Vec<PathBuf>> {
        match self {
            Self::System(slot) => system_clipboard(slot)?.get_file_list().ok(),
            Self::Memory { .. } => None,
        }
    }
}

/// Creates the Wayland clipboard on first use. Returns `None` when creation fails.
fn system_clipboard(slot: &mut Option<Clipboard>) -> Option<&mut Clipboard> {
    if slot.is_none() {
        *slot = Clipboard::new().ok();
    }
    slot.as_mut()
}

#[inline]
fn search_refresh_should_jump(changed: bool, cause: SearchRefreshCause) -> bool {
    changed && cause == SearchRefreshCause::User
}

impl Default for TerminalInteraction {
    fn default() -> Self {
        Self {
            search: TerminalSearchState::default(),
            layout: TerminalUiLayout::default(),
            modifiers: Modifiers::empty(),
            clipboard: ClipboardOwner::System(None),
            mouse_x: 0.0,
            mouse_y: 0.0,
            pressed_mouse_buttons: 0,
            pointer_capture: PointerCapture::None,
            terminal_selection_anchor: None,
            terminal_selection_unit: SelectionUnit::Cell,
            terminal_selection_base: (0, 0, 0, 0),
            click_counter: ClickCounter::default(),
            scroll_sensitivity: crate::config::DEFAULT_SCROLL_SENSITIVITY,
            link_uri: None,
            link_press_pending: false,
            held_host_key: None,
            held_modifier_keys: 0,
        }
    }
}

impl TerminalInteraction {
    pub(crate) fn set_modifiers(&mut self, modifiers: Modifiers) {
        // No host modifier means no modifier key is down, so a release that never
        // arrived (for example after focus loss) must not leave its key marked held.
        if modifiers.is_empty() {
            self.held_modifier_keys = 0;
        }
        self.modifiers = modifiers;
    }

    /// Records a physical modifier key as held or released. `App::on_key` calls it before
    /// any of its early returns (settings modal, global shortcuts), because the held state is physical.
    pub(crate) fn note_modifier_key(&mut self, key: &KeyInput) {
        let Some(TermKey::Modifier(modifier)) = key.named else {
            return;
        };
        let Some((bit, _, _)) = modifier_key_bits(modifier) else {
            return;
        };
        if key.state.is_pressed() {
            self.held_modifier_keys |= bit;
        } else {
            self.held_modifier_keys &= !bit;
        }
    }

    /// Modifiers for the encoder event of `key`. The host learns the state after a key
    /// only from the following `wl_keyboard.modifiers`, so a modifier key's own flag is
    /// set here: on press and repeat, and on release only while its other key is held.
    fn event_modifiers(&self, key: &KeyInput) -> Modifiers {
        let Some(TermKey::Modifier(modifier)) = key.named else {
            return self.modifiers;
        };
        let Some((_, flag, family)) = modifier_key_bits(modifier) else {
            return self.modifiers;
        };
        let on = key.state.is_pressed() || self.held_modifier_keys & family != 0;
        let own = |candidate: Modifiers| candidate == flag;
        let current = self.modifiers;
        Modifiers::new(
            if own(Modifiers::SHIFT) { on } else { current.shift_key() },
            if own(Modifiers::CONTROL) { on } else { current.control_key() },
            if own(Modifiers::ALT) { on } else { current.alt_key() },
            if own(Modifiers::SUPER) { on } else { current.super_key() },
        )
    }

    /// Forgets a Ctrl+press on a link whose release will never be delivered,
    /// for example after a focus change.
    pub(crate) fn cancel_link_press(&mut self) {
        self.link_press_pending = false;
    }

    /// Recomputes the hovered link for the pointer at (`x`, `y`). The link is
    /// kept only while Ctrl is held and the pointer is over the terminal body.
    /// Returns whether the hovered link changed.
    pub(crate) fn update_link_hover(
        &self,
        terminal: &mut Terminal,
        x: f32,
        y: f32,
        layout: TerminalUiLayout,
    ) -> bool {
        let link = if self.modifiers.control_key() && layout.body.contains(x, y) {
            link_under_pointer(layout, terminal, x, y)
        } else {
            None
        };
        terminal.set_hovered_link(link)
    }

    /// Takes the URI of the last Ctrl+click on a link; the caller decides
    /// whether to open it.
    pub(crate) fn take_link_uri(&mut self) -> Option<Box<str>> {
        self.link_uri.take()
    }

    pub(crate) fn set_scroll_sensitivity(&mut self, sensitivity: f32) {
        self.scroll_sensitivity = crate::config::normalize_scroll_sensitivity(sensitivity);
    }

    #[cfg(test)]
    pub(crate) fn modifiers_for_test(&self) -> Modifiers {
        self.modifiers
    }

    pub(crate) fn update_layout(&mut self, layout: TerminalUiLayout) {
        self.layout = layout;
    }

    pub(crate) fn animation_active(&self, terminal: &Terminal) -> bool {
        terminal_scroll_animating(terminal) || self.selection_autoscroll_active(terminal)
    }

    pub(crate) fn search_refresh_deadline(&self) -> Option<std::time::Instant> {
        self.search.pending_passive_refresh_deadline()
    }

    pub(crate) fn open_search(&mut self, terminal: &mut Terminal) {
        self.search.open();
        self.search.select_all();
        self.refresh_search(terminal, SearchRefreshCause::User);
    }

    pub(crate) fn reset_for_terminal_switch(&mut self, terminal: &mut Terminal) {
        if self.search.shown {
            self.close_search(terminal);
        } else {
            self.search.close();
        }
        self.layout = TerminalUiLayout::default();
        // Ends a scrollbar drag of the left terminal too, so its smooth scroll settles.
        self.cancel_pointer_interaction_state(&mut terminal.scroll_y);
        // Hover is only refreshed for the active tab, so drop the left tab's
        // link to keep it from showing a stale pointer when it is reactivated.
        terminal.set_hovered_link(None);
    }

    pub(crate) fn cancel_pointer_interaction(&mut self, terminal: &mut Terminal) {
        self.finish_terminal_selection(terminal);
        self.cancel_pointer_interaction_state(&mut terminal.scroll_y);
    }

    fn cancel_pointer_interaction_state(&mut self, scroll: &mut crate::scroll::ScrollState) {
        self.pressed_mouse_buttons = 0;
        let capture = release_pointer_capture(&mut self.pointer_capture);
        self.terminal_selection_anchor = None;
        if capture == PointerCapture::TerminalScrollbar {
            scroll.end_drag();
        }
    }

    pub(crate) fn clear_text_selection(&mut self, terminal: &mut Terminal) -> bool {
        if self.search.owns_grid_selection() {
            return false;
        }
        let mut term = crate::platform::lock_recover(&terminal.grid);
        let had_selection = term.selection().is_some();
        term.set_selection(None);
        had_selection
    }

    pub(crate) fn terminal_selection_active(&self) -> bool {
        self.pointer_capture == PointerCapture::TerminalSelection
    }

    pub(crate) fn cursor_left(
        &mut self,
        window_w: f32,
        window_h: f32,
        terminal: &mut Terminal,
    ) -> bool {
        if self.pointer_capture != PointerCapture::TerminalSelection {
            return false;
        }
        let was_autoscrolling = self.selection_autoscroll_active(terminal);
        let (x, y) =
            project_cursor_outside_window_on_leave(self.mouse_x, self.mouse_y, window_w, window_h);
        self.mouse_x = x;
        self.mouse_y = y;
        self.update_terminal_selection_endpoint(terminal, self.layout.scroll_offset)
            || was_autoscrolling != self.selection_autoscroll_active(terminal)
    }

    pub(crate) fn update_selection_autoscroll(&mut self, dt: f32, terminal: &mut Terminal) -> bool {
        if self.pointer_capture != PointerCapture::TerminalSelection
            || self.terminal_selection_anchor.is_none()
            || !dt.is_finite()
            || dt <= 0.0
        {
            return false;
        }
        let delta = selection_drag_autoscroll_delta(
            self.mouse_y,
            self.layout.body.y,
            self.layout.body.y + self.layout.body.h,
        );
        if delta == 0.0 || self.layout.max_scroll <= 0.0 {
            return false;
        }
        let old_target = terminal.scroll_y.target.clamp(0.0, self.layout.max_scroll);
        let speed = crate::scroll::drag_autoscroll_speed(delta, delta < 0.0);
        let new_target =
            (old_target - delta.signum() * speed * dt).clamp(0.0, self.layout.max_scroll);
        if new_target == old_target {
            return false;
        }
        terminal.scroll_y.target = new_target;
        terminal.scroll_y.anim_speed = 15.0;
        let _ = self.update_terminal_selection_endpoint(terminal, new_target.round());
        true
    }

    fn selection_autoscroll_active(&self, terminal: &Terminal) -> bool {
        if self.pointer_capture != PointerCapture::TerminalSelection
            || self.terminal_selection_anchor.is_none()
            || self.layout.max_scroll <= 0.0
        {
            return false;
        }
        let delta = selection_drag_autoscroll_delta(
            self.mouse_y,
            self.layout.body.y,
            self.layout.body.y + self.layout.body.h,
        );
        let target = terminal.scroll_y.target;
        target.is_finite()
            && if delta < 0.0 {
                target < self.layout.max_scroll
            } else if delta > 0.0 {
                target > 0.0
            } else {
                false
            }
    }

    fn update_terminal_selection_endpoint(
        &mut self,
        terminal: &mut Terminal,
        scroll_offset: f32,
    ) -> bool {
        let Some((anchor_x, anchor_y)) = self.terminal_selection_anchor else {
            return false;
        };
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        let Some((cell_x, cell_y)) = terminal_cell_from_grid(
            self.layout,
            &grid,
            self.mouse_x,
            self.mouse_y,
            scroll_offset,
            true,
        ) else {
            return false;
        };
        let next = match self.terminal_selection_unit {
            SelectionUnit::Cell => (anchor_x, anchor_y, cell_x, cell_y),
            unit => extend_unit_span(&grid, unit, self.terminal_selection_base, (cell_x, cell_y)),
        };
        let selection = grid.selection();
        if selection.is_none() && (cell_x, cell_y) == (anchor_x, anchor_y) {
            return false;
        }
        if selection != Some(next) {
            grid.set_selection(Some(next));
            return true;
        }
        false
    }

    fn finish_terminal_selection(&mut self, terminal: &mut Terminal) {
        if self.pointer_capture != PointerCapture::TerminalSelection {
            return;
        }
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        // A word or line selection is kept even when it covers a single cell.
        if self.terminal_selection_unit == SelectionUnit::Cell
            && grid
                .selection()
                .is_some_and(|(sx, sy, ex, ey)| sx == ex && sy == ey)
        {
            grid.set_selection(None);
        }
        self.terminal_selection_anchor = None;
    }

    /// Starts a selection from a left press at `pos`. A double click selects the word
    /// under it and a triple click the logical line; any other count is a single cell.
    fn begin_terminal_selection_at(
        &mut self,
        terminal: &mut Terminal,
        pos: (usize, usize),
        click_count: u8,
    ) {
        self.terminal_selection_anchor = Some(pos);
        self.pointer_capture = PointerCapture::TerminalSelection;
        self.terminal_selection_unit = match click_count {
            2 => SelectionUnit::Word,
            3 => SelectionUnit::Line,
            _ => SelectionUnit::Cell,
        };
        if self.terminal_selection_unit == SelectionUnit::Cell {
            return;
        }
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        let span = unit_span(&grid, self.terminal_selection_unit, pos);
        self.terminal_selection_base = span;
        grid.set_selection(Some(span));
    }

    #[cfg(test)]
    pub(crate) fn set_pressed_mouse_buttons_for_test(&mut self, buttons: u8) {
        self.pressed_mouse_buttons = buttons;
    }

    #[cfg(test)]
    pub(crate) fn pressed_mouse_buttons_for_test(&self) -> u8 {
        self.pressed_mouse_buttons
    }

    /// Clipboard backed by an in-process buffer instead of the Wayland clipboard.
    pub(crate) fn new_with_memory_clipboard() -> Self {
        Self {
            clipboard: ClipboardOwner::Memory { text: None },
            ..Self::default()
        }
    }

    pub(crate) fn clipboard_text(&mut self) -> Option<String> {
        self.clipboard.get_text()
    }

    pub(crate) fn set_clipboard_text(&mut self, text: String) -> bool {
        self.clipboard.set_text(text)
    }

    fn clipboard_file_list(&mut self) -> Option<Vec<PathBuf>> {
        self.clipboard.get_file_list()
    }

    pub(crate) fn clipboard_is_memory(&self) -> bool {
        matches!(self.clipboard, ClipboardOwner::Memory { .. })
    }

    pub(crate) fn refresh_search(
        &mut self,
        terminal: &mut Terminal,
        cause: SearchRefreshCause,
    ) {
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        let changed = self.search.refresh_for_grid(&mut grid, cause);
        drop(grid);
        if search_refresh_should_jump(changed, cause) {
            self.jump_to_search_match(terminal);
        }
    }

    fn jump_to_search_match(&mut self, terminal: &mut Terminal) {
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        let Some(found) = self.search.set_active_grid_selection(&mut grid) else {
            return;
        };
        let total_lines = grid.total_rows();
        let offset_from_bottom = total_lines.saturating_sub(1).saturating_sub(found.y);
        let target = offset_from_bottom as f32 * self.layout.char_h;
        drop(grid);
        terminal
            .scroll_y
            .animate_to(target.clamp(0.0, self.layout.max_scroll));
    }

    fn close_search(&mut self, terminal: &mut Terminal) {
        self.search.close();
        crate::platform::lock_recover(&terminal.grid).set_selection(None);
    }

    fn handle_search_key(&mut self, key_event: &KeyInput, terminal: &mut Terminal) -> bool {
        if !search_owns_keyboard(&self.search) || !key_event.state.is_pressed() {
            return false;
        }
        if search_key_falls_through_to_terminal(
            &self.search,
            key_event.physical_key,
            self.modifiers,
        ) {
            return false;
        }
        let ctrl = self.modifiers.control_key();
        let shift = self.modifiers.shift_key();
        let mut edited = false;
        match key_event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => self.close_search(terminal),
            PhysicalKey::Code(KeyCode::Enter) => {
                if shift {
                    self.search.previous();
                } else {
                    self.search.next();
                }
                self.jump_to_search_match(terminal);
            }
            PhysicalKey::Code(KeyCode::ArrowUp) => {
                self.search.previous();
                self.jump_to_search_match(terminal);
            }
            PhysicalKey::Code(KeyCode::ArrowDown) => {
                self.search.next();
                self.jump_to_search_match(terminal);
            }
            PhysicalKey::Code(KeyCode::ArrowLeft) => self.search.move_left(shift),
            PhysicalKey::Code(KeyCode::ArrowRight) => self.search.move_right(shift),
            PhysicalKey::Code(KeyCode::Home) => self.search.move_cursor(0, shift),
            PhysicalKey::Code(KeyCode::End) => {
                let end = self.search.text.chars().count();
                self.search.move_cursor(end, shift);
            }
            PhysicalKey::Code(KeyCode::Backspace) => {
                self.search.backspace();
                edited = true;
            }
            PhysicalKey::Code(KeyCode::Delete) => {
                self.search.delete_forward();
                edited = true;
            }
            PhysicalKey::Code(KeyCode::KeyA) if ctrl => self.search.select_all(),
            PhysicalKey::Code(KeyCode::KeyC) if ctrl => {
                if let Some(text) = self.search.selected_text() {
                    self.set_clipboard_text(text);
                }
            }
            PhysicalKey::Code(KeyCode::KeyX) if ctrl => {
                // Cuts whenever a clipboard exists, even if the write fails, as before 40ddd7a.
                if let Some(text) = self.search.selected_text()
                    && self.clipboard.is_available()
                {
                    let _ = self.clipboard.set_text(text);
                    self.search.insert_text("");
                    edited = true;
                }
            }
            PhysicalKey::Code(KeyCode::KeyV) if ctrl => {
                if let Some(text) = self.clipboard_text() {
                    let clean = text.replace(['\r', '\n'], "");
                    if !clean.is_empty() {
                        self.search.insert_text(&clean);
                        edited = true;
                    }
                }
            }
            _ => {}
        }
        if edited {
            self.refresh_search(terminal, SearchRefreshCause::User);
        }
        true
    }

    pub(crate) fn handle_ime_commit(&mut self, text: &str, terminal: &mut Terminal) -> bool {
        if search_owns_keyboard(&self.search) {
            let clean = text.replace(['\r', '\n'], "");
            if !clean.is_empty() {
                self.search.insert_text(&clean);
                self.refresh_search(terminal, SearchRefreshCause::User);
            }
            return true;
        }
        if !text.is_empty() {
            terminal.write_user_input(text.as_bytes());
            return true;
        }
        false
    }

    pub(crate) fn handle_key_event(
        &mut self,
        key_event: &KeyInput,
        terminal: &mut Terminal,
    ) -> bool {
        if self.handle_search_key(key_event, terminal) {
            if let Some(text) = key_event.text.as_deref() {
                self.handle_text(text, terminal);
            }
            return true;
        }
        // Pressed covers auto-repeat. A release still reaches the encoder while search
        // owns the keyboard: it is ignored unless its press was sent to the terminal.
        let pressed = key_event.state.is_pressed();
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let super_key = self.modifiers.super_key();

        // Ctrl+V pastes text or a file list on press and repeat. With nothing to paste
        // (an image, say) the key falls through to the encoder, so the app reads the clipboard itself.
        // A release is never a paste and always falls through; the encoder ignores it
        // when its press was not sent.
        if ctrl && key_event.physical_key == PhysicalKey::Code(KeyCode::KeyV) && pressed {
            let file_list = self.clipboard_file_list();
            let paste = terminal_clipboard_paste_bytes(file_list.as_deref(), None).or_else(|| {
                self.clipboard_text()
                    .and_then(|text| terminal_clipboard_paste_bytes(None, Some(&text)))
            });
            if let Some(bytes) = paste {
                write_terminal_paste(terminal, bytes);
                return true;
            }
        }

        // A key whose press the host consumed keeps out of the encoder until its release.
        // A fresh press means an earlier release was lost, so it drops the old hold.
        if self.held_host_key == Some(key_event.physical_key) {
            match key_event.state {
                KeyState::Repeat => return false,
                KeyState::Released => {
                    self.held_host_key = None;
                    return false;
                }
                KeyState::Pressed => self.held_host_key = None,
            }
        }

        let mut grid = crate::platform::lock_recover(&terminal.grid);
        if ctrl
            && key_event.physical_key == PhysicalKey::Code(KeyCode::KeyC)
            && grid.selection().is_some()
        {
            let copied = if pressed {
                let text = grid.selection_text();
                grid.set_selection(None);
                self.held_host_key = Some(key_event.physical_key);
                Some(text)
            } else {
                None
            };
            drop(grid);
            if let Some(text) = copied
                && !text.is_empty()
            {
                self.set_clipboard_text(text);
            }
            return pressed;
        }

        // A key consumed by the IME composition never reaches the encoder. Its press text
        // still goes to the PTY as plain text, because the IME may pass the key through.
        if key_event.ime_preedit {
            if key_event.state == KeyState::Pressed
                && let Some(text) = key_event.text.as_deref()
            {
                drop(grid);
                return self.handle_text(text, terminal);
            }
            return false;
        }
        let Some(key) = terminal_key_event(key_event, self.event_modifiers(key_event)) else {
            return false;
        };
        let mut out = Vec::new();
        let outcome = grid.encode_key(&key, &mut out);
        drop(grid);
        match outcome {
            KeyOutcome::Sent => {
                terminal.write_user_input(&out);
                true
            }
            KeyOutcome::Text => {
                let Some(bytes) = key
                    .text
                    .and_then(|text| terminal_text_sequence(text, ctrl, alt, super_key))
                else {
                    return false;
                };
                terminal.write_user_input(&bytes);
                true
            }
            KeyOutcome::Ignore => false,
        }
    }

    pub(crate) fn handle_text(&mut self, text: &str, terminal: &mut Terminal) -> bool {
        if text.is_empty() {
            return false;
        }
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let super_key = self.modifiers.super_key();
        if search_owns_keyboard(&self.search) {
            if ctrl || super_key || text.contains(['\r', '\n']) {
                return false;
            }
            self.search.insert_text(text);
            self.refresh_search(terminal, SearchRefreshCause::User);
            return true;
        }
        let Some(bytes) = terminal_text_sequence(text, ctrl, alt, super_key) else {
            return false;
        };
        terminal.write_user_input(&bytes);
        true
    }

    pub(crate) fn cursor_moved(
        &mut self,
        x: f32,
        y: f32,
        terminal: &mut Terminal,
        search_cursor_from_x: impl FnOnce(&str, f32, f32) -> usize,
    ) -> bool {
        let was_selection_autoscrolling = self.selection_autoscroll_active(terminal);
        self.mouse_x = x;
        self.mouse_y = y;
        match self.pointer_capture {
            PointerCapture::TerminalSelection => {
                let changed =
                    self.update_terminal_selection_endpoint(terminal, self.layout.scroll_offset);
                if changed
                    || was_selection_autoscrolling != self.selection_autoscroll_active(terminal)
                {
                    return true;
                }
            }
            PointerCapture::TerminalScrollbar => {
                if let Some(scrollbar) = self.layout.scrollbar
                    && let Some((_, target)) = terminal_scrollbar_drag_target(
                        y,
                        scrollbar,
                        Some(terminal.scroll_y.drag_offset),
                    )
                    && (terminal.scroll_y.current - target).abs() > f32::EPSILON
                {
                    terminal.scroll_y.current = target;
                    terminal.scroll_y.target = target;
                    terminal.scroll_y.velocity = 0.0;
                    return true;
                }
            }
            PointerCapture::SearchInput => {
                if let Some(search) = self.layout.search {
                    let local_x = x - search.input.x - 5.0 * self.layout.scale;
                    let cursor =
                        search_cursor_from_x(&self.search.text, local_x, self.search.scroll_x);
                    return update_search_pointer_selection(
                        &mut self.search,
                        self.pointer_capture,
                        cursor,
                    );
                }
            }
            PointerCapture::SearchControl | PointerCapture::None => {}
        }
        if self.pointer_capture == PointerCapture::None {
            let (mode, sgr) = {
                let grid = crate::platform::lock_recover(&terminal.grid);
                (grid.mouse_tracking(), grid.mouse_sgr())
            };
            if let Some(sequence) = terminal_mouse_motion_sequence(
                self.layout,
                mode,
                sgr,
                self.pressed_mouse_buttons,
                x,
                y,
            ) {
                let _ = terminal.write_input(&sequence);
                return true;
            }
        }
        false
    }

    pub(crate) fn mouse_input(
        &mut self,
        state: KeyState,
        button: PointerButton,
        terminal: &mut Terminal,
        search_cursor_from_x: impl FnOnce(&str, f32, f32) -> usize,
    ) -> bool {
        let x = self.mouse_x;
        let y = self.mouse_y;
        update_pressed_mouse_buttons(&mut self.pressed_mouse_buttons, button, state);
        if state == KeyState::Pressed && button == PointerButton::Left {
            // A new press supersedes a link press whose release never arrived.
            self.link_press_pending = false;
        }

        if state == KeyState::Released && self.pointer_capture != PointerCapture::None {
            self.finish_terminal_selection(terminal);
            let capture = release_pointer_capture(&mut self.pointer_capture);
            self.terminal_selection_anchor = None;
            if capture == PointerCapture::TerminalScrollbar {
                terminal.scroll_y.end_drag();
            }
            return true;
        }

        if state == KeyState::Released && button == PointerButton::Left && self.link_press_pending {
            self.link_press_pending = false;
            return true;
        }

        if let Some(search) = self.layout.search
            && state == KeyState::Pressed
            && button == PointerButton::Left
        {
            match search.hit_test(x, y) {
                TerminalSearchHit::Close => {
                    self.pointer_capture = PointerCapture::SearchControl;
                    self.close_search(terminal);
                    return true;
                }
                TerminalSearchHit::Next => {
                    self.pointer_capture = PointerCapture::SearchControl;
                    self.search.next();
                    self.jump_to_search_match(terminal);
                    return true;
                }
                TerminalSearchHit::Previous => {
                    self.pointer_capture = PointerCapture::SearchControl;
                    self.search.previous();
                    self.jump_to_search_match(terminal);
                    return true;
                }
                TerminalSearchHit::CaseToggle => {
                    self.pointer_capture = PointerCapture::SearchControl;
                    self.search.toggle_case();
                    self.refresh_search(terminal, SearchRefreshCause::User);
                    return true;
                }
                TerminalSearchHit::Input => {
                    let local_x = x - search.input.x - 5.0 * self.layout.scale;
                    let cursor =
                        search_cursor_from_x(&self.search.text, local_x, self.search.scroll_x);
                    begin_search_pointer_selection(&mut self.search, cursor);
                    self.pointer_capture = PointerCapture::SearchInput;
                    return true;
                }
                TerminalSearchHit::Chrome => {
                    self.pointer_capture = PointerCapture::SearchControl;
                    return true;
                }
                TerminalSearchHit::None => {}
            }
        }

        if let Some(scrollbar) = self.layout.scrollbar
            && state == KeyState::Pressed
            && button == PointerButton::Left
            && scrollbar.track.contains(x, y)
            && let Some((offset, target)) = terminal_scrollbar_drag_target(y, scrollbar, None)
        {
            terminal.scroll_y.drag_offset = offset;
            terminal.scroll_y.current = target;
            terminal.scroll_y.target = target;
            terminal.scroll_y.velocity = 0.0;
            terminal.scroll_y.is_dragging = true;
            self.pointer_capture = PointerCapture::TerminalScrollbar;
            return true;
        }

        if !self.layout.body.contains(x, y) {
            return false;
        }
        let left_press = state == KeyState::Pressed && button == PointerButton::Left;
        let press_cell = if left_press {
            terminal_cell_at(self.layout, terminal, x, y)
        } else {
            None
        };
        let click_count = if left_press {
            self.click_counter.register(Instant::now(), press_cell)
        } else {
            1
        };
        // Ctrl+click on a link opens it and never reaches the PTY, even under
        // mouse tracking, and never starts a selection. Only a single click opens
        // it; a double or triple click falls through to the selection.
        if left_press
            && click_count == 1
            && self.modifiers.control_key()
            && let Some((link, _)) = link_under_pointer(self.layout, terminal, x, y)
        {
            self.link_uri = Some(link.uri);
            self.link_press_pending = true;
            // The press is not forwarded to the PTY, so the button must not
            // count as held for motion reports (drag) until it is released.
            update_pressed_mouse_buttons(
                &mut self.pressed_mouse_buttons,
                PointerButton::Left,
                KeyState::Released,
            );
            return true;
        }
        if state == KeyState::Pressed {
            let search_owned_selection = self.search.owns_grid_selection();
            terminal_body_takes_search_focus(&mut self.search);
            if search_owned_selection {
                let _ = self.clear_text_selection(terminal);
            }
        }

        let (tracking_mode, mouse_sgr) = {
            let grid = crate::platform::lock_recover(&terminal.grid);
            (grid.mouse_tracking(), grid.mouse_sgr())
        };
        let tracking = tracking_mode.enabled();
        if let Some(sequence) = terminal_mouse_event_sequence(
            self.layout,
            PointerCapture::None,
            tracking_mode,
            mouse_sgr,
            state,
            button,
            x,
            y,
        ) {
            let _ = terminal.write_input(&sequence);
            return true;
        }

        if !tracking
            && left_press
            && let Some(pos) = press_cell
        {
            self.begin_terminal_selection_at(terminal, pos, click_count);
            return true;
        }
        false
    }

    pub(crate) fn mouse_wheel(
        &mut self,
        delta: ScrollDelta,
        terminal: &mut Terminal,
        layout: TerminalUiLayout,
    ) -> bool {
        if !layout.body.contains(self.mouse_x, self.mouse_y) { return false; }
        let dy = terminal_wheel_delta(delta, layout.char_h, self.scroll_sensitivity);
        let grid = crate::platform::lock_recover(&terminal.grid);
        let is_alt = grid.is_alt_screen();
        let tracking_mode = grid.mouse_tracking();
        let mouse_sgr = grid.mouse_sgr();
        let app_cursor = grid.app_cursor_keys();
        drop(grid);
        if tracking_mode.enabled() {
            if let Some(input) = terminal_mouse_wheel_sequence(
                layout,
                tracking_mode,
                mouse_sgr,
                self.mouse_x,
                self.mouse_y,
                dy,
            ) {
                let _ = terminal.write_input(&input);
            }
            return true;
        }
        if is_alt {
            let steps = (dy.abs() / 20.0).max(1.0) as usize;
            let mut input = Vec::with_capacity(24);
            {
                let sequence: &[u8] = if dy > 0.0 {
                    if app_cursor { b"\x1bOA" } else { b"\x1b[A" }
                } else if app_cursor { b"\x1bOB" } else { b"\x1b[B" };
                for _ in 0..steps.min(3) { input.extend_from_slice(sequence); }
            }
            // Alternate-scroll arrows act as cursor keys (history recall), so they are user input.
            terminal.write_user_input(&input);
            return true;
        }
        terminal.scroll_y.anim_speed = 7.0;
        terminal.scroll_y.scroll_by(dy);
        terminal.scroll_y.clamp_target(0.0, layout.max_scroll);
        true
    }
}

/// Link under (`x`, `y`) among the visible rows, with the grid's content
/// generation at search time. The grid lock is held only for the search.
fn link_under_pointer(
    layout: TerminalUiLayout,
    terminal: &Terminal,
    x: f32,
    y: f32,
) -> Option<(LinkTarget, u64)> {
    let grid = crate::platform::lock_recover(&terminal.grid);
    let (col, row) = terminal_cell_from_grid(layout, &grid, x, y, layout.scroll_offset, false)?;
    let rows = visible_row_range(
        grid.total_rows(),
        layout.visible_rows,
        layout.scroll_offset,
        layout.char_h,
    );
    let link = grid.link_at(row, col, rows)?;
    Some((link, grid.content_generation()))
}

fn terminal_cell_at(
    layout: TerminalUiLayout,
    terminal: &Terminal,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    let grid = crate::platform::lock_recover(&terminal.grid);
    terminal_cell_from_grid(layout, &grid, x, y, layout.scroll_offset, false)
}

fn terminal_cell_from_grid(
    layout: TerminalUiLayout,
    grid: &Term,
    x: f32,
    y: f32,
    scroll_offset: f32,
    clamp_to_body: bool,
) -> Option<(usize, usize)> {
    if layout.char_w <= 0.0
        || layout.char_h <= 0.0
        || layout.body.w <= 0.0
        || layout.body.h <= 0.0
        || !x.is_finite()
        || !y.is_finite()
        || !scroll_offset.is_finite()
        || (!clamp_to_body && !layout.body.contains(x, y))
    {
        return None;
    }
    let x = if clamp_to_body {
        x.clamp(layout.body.x, layout.body.x + layout.body.w)
    } else {
        x
    };
    let y = if clamp_to_body {
        y.clamp(layout.body.y, layout.body.y + layout.body.h)
    } else {
        y
    };
    let total_lines = grid.total_rows();
    if total_lines == 0 {
        return None;
    }
    let bottom_pad = layout.bottom_pad;
    let offset_from_bottom =
        (layout.body.y + layout.body.h - bottom_pad - y + scroll_offset) / layout.char_h;
    let row = total_lines
        .saturating_sub(1)
        .saturating_sub(offset_from_bottom.max(0.0).floor() as usize);
    let mut col = ((x - layout.text_x).max(0.0) / layout.char_w).floor() as usize;
    col = col.min(grid.cols().saturating_sub(1));
    let row = row.min(total_lines.saturating_sub(1));
    let row_cells = grid.row(row);
    if row_cells
        .and_then(|cells| cells.get(col))
        .is_some_and(crate::terminal::Cell::is_wide_spacer)
    {
        col = col.saturating_sub(1);
    }
    Some((col, row))
}

/// Selection as `(start_x, start_y, end_x, end_y)`, the layout of `Term::selection`.
type SelectionSpan = (usize, usize, usize, usize);

/// What a drag extends the selection by: single cells, whole words, or whole lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SelectionUnit {
    #[default]
    Cell,
    Word,
    Line,
}

/// Counts left presses for double and triple clicks. Times are passed in, so tests
/// need no sleeps.
#[derive(Clone, Copy, Debug, Default)]
struct ClickCounter {
    last: Option<(Instant, (usize, usize))>,
    count: u8,
}

/// Largest gap between two presses that still counts as one multi-click.
pub(crate) const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(300);

impl ClickCounter {
    /// Returns 1, 2 or 3 for a press at `cell` at time `now`; the fourth press starts over at 1.
    fn register(&mut self, now: Instant, cell: Option<(usize, usize)>) -> u8 {
        let continues = match (self.last, cell) {
            (Some((at, previous)), Some(cell)) => {
                previous == cell && now.saturating_duration_since(at) <= MULTI_CLICK_WINDOW
            }
            _ => false,
        };
        self.count = if continues && self.count < 3 {
            self.count + 1
        } else {
            1
        };
        self.last = cell.map(|cell| (now, cell));
        self.count
    }
}

/// Characters that end a word, following Alacritty's default `semantic_escape_chars`.
fn is_word_separator(cell: &Cell) -> bool {
    let ch = cell.ch();
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | '│' | '`' | '|' | ':' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        )
}

fn cell_at(grid: &Term, (x, y): (usize, usize)) -> Option<&Cell> {
    grid.row(y).and_then(|cells| cells.get(x))
}

/// Cell before `pos` in reading order, crossing a soft wrap.
fn previous_cell(grid: &Term, (x, y): (usize, usize)) -> Option<(usize, usize)> {
    if x > 0 {
        Some((x - 1, y))
    } else if y > 0 && grid.row_soft_wrapped(y - 1) {
        Some((grid.cols().saturating_sub(1), y - 1))
    } else {
        None
    }
}

/// Cell after `pos` in reading order, crossing a soft wrap.
fn next_cell(grid: &Term, (x, y): (usize, usize)) -> Option<(usize, usize)> {
    if x + 1 < grid.cols() {
        Some((x + 1, y))
    } else if y + 1 < grid.total_rows() && grid.row_soft_wrapped(y) {
        Some((0, y + 1))
    } else {
        None
    }
}

/// Whether `pos` belongs to a word. A wide spacer takes the class of its wide cell.
fn is_word_pos(grid: &Term, pos: (usize, usize)) -> bool {
    let owner = if cell_at(grid, pos).is_some_and(Cell::is_wide_spacer) {
        (pos.0.saturating_sub(1), pos.1)
    } else {
        pos
    };
    cell_at(grid, owner).is_some_and(|cell| !is_word_separator(cell))
}

/// First and last cell of the word under `pos`, crossing soft wraps. A separator
/// selects only its own cell (and wide spacer).
fn word_extent(grid: &Term, pos: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    // A wide spacer is part of the wide cell before it.
    let pos = if cell_at(grid, pos).is_some_and(Cell::is_wide_spacer) {
        (pos.0.saturating_sub(1), pos.1)
    } else {
        pos
    };
    let mut start = pos;
    let mut end = pos;
    if is_word_pos(grid, pos) {
        while let Some(prev) = previous_cell(grid, start) {
            if !is_word_pos(grid, prev) {
                break;
            }
            start = prev;
        }
        while let Some(next) = next_cell(grid, end) {
            if !is_word_pos(grid, next) {
                break;
            }
            end = next;
        }
    }
    // A separator is one cell, but a wide one still spans its spacer.
    if let Some(next) = next_cell(grid, end)
        && cell_at(grid, next).is_some_and(Cell::is_wide_spacer)
    {
        end = next;
    }
    (start, end)
}

/// Whole logical line that contains `row`, from its first soft-wrapped row to the
/// last non-blank cell of its last row.
fn line_extent(grid: &Term, row: usize) -> SelectionSpan {
    let mut first = row;
    while first > 0 && grid.row_soft_wrapped(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < grid.total_rows() && grid.row_soft_wrapped(last) {
        last += 1;
    }
    let cells = grid.row(last);
    let end_x = cells
        .and_then(|cells| cells.iter().rposition(|cell| cell.ch() != ' '))
        // A wide character at the end of the line also covers its blank spacer.
        .map(|x| {
            if cells.and_then(|cells| cells.get(x + 1)).is_some_and(Cell::is_wide_spacer) {
                x + 1
            } else {
                x
            }
        })
        .unwrap_or(0);
    (0, first, end_x, last)
}

/// Span of the unit under `pos`: the cell itself, its word, or its logical line.
fn unit_span(grid: &Term, unit: SelectionUnit, pos: (usize, usize)) -> SelectionSpan {
    match unit {
        SelectionUnit::Cell => (pos.0, pos.1, pos.0, pos.1),
        SelectionUnit::Word => {
            let (start, end) = word_extent(grid, pos);
            (start.0, start.1, end.0, end.1)
        }
        SelectionUnit::Line => line_extent(grid, pos.1),
    }
}

/// Span of a drag that started on the word or line `base` and now reaches `pos`.
/// The unit under `pos` is added on the side the pointer moved to.
fn extend_unit_span(
    grid: &Term,
    unit: SelectionUnit,
    base: SelectionSpan,
    pos: (usize, usize),
) -> SelectionSpan {
    // Row-major order, so tuples compare in reading order.
    let base_start = (base.1, base.0);
    let base_end = (base.3, base.2);
    let target = (pos.1, pos.0);
    if target < base_start {
        let under = unit_span(grid, unit, pos);
        (under.0, under.1, base.2, base.3)
    } else if target > base_end {
        let under = unit_span(grid, unit, pos);
        (base.0, base.1, under.2, under.3)
    } else {
        base
    }
}

#[inline]
fn selection_drag_autoscroll_delta(pos: f32, start: f32, end: f32) -> f32 {
    if pos < start {
        pos - start
    } else if pos > end {
        pos - end
    } else {
        0.0
    }
}

#[inline]
fn project_cursor_outside_window_on_leave(
    x: f32,
    y: f32,
    window_w: f32,
    window_h: f32,
) -> (f32, f32) {
    if !x.is_finite()
        || !y.is_finite()
        || !window_w.is_finite()
        || !window_h.is_finite()
        || window_w <= 0.0
        || window_h <= 0.0
        || x < 0.0
        || x > window_w
        || y < 0.0
        || y > window_h
    {
        return (x, y);
    }

    let left = x;
    let right = window_w - x;
    let top = y;
    let bottom = window_h - y;

    if left <= right && left <= top && left <= bottom {
        (-1.0, y)
    } else if right <= top && right <= bottom {
        (window_w + 1.0, y)
    } else if top <= bottom {
        (x, -1.0)
    } else {
        (x, window_h + 1.0)
    }
}

#[inline]
fn begin_search_pointer_selection(search: &mut TerminalSearchState, cursor: usize) {
    search.focused = true;
    search.move_cursor(cursor, false);
    search.selection_anchor = Some(cursor);
}

#[inline]
fn update_search_pointer_selection(
    search: &mut TerminalSearchState,
    capture: PointerCapture,
    cursor: usize,
) -> bool {
    if capture != PointerCapture::SearchInput {
        return false;
    }
    let previous_cursor = search.cursor;
    let previous_anchor = search.selection_anchor;
    search.move_cursor(cursor, true);
    search.cursor != previous_cursor || search.selection_anchor != previous_anchor
}

#[inline]
fn release_pointer_capture(capture: &mut PointerCapture) -> PointerCapture {
    std::mem::take(capture)
}
#[inline]
fn search_owns_keyboard(search: &TerminalSearchState) -> bool {
    search.shown && search.focused
}

#[inline]
fn search_key_falls_through_to_terminal(
    search: &TerminalSearchState,
    physical_key: PhysicalKey,
    modifiers: Modifiers,
) -> bool {
    if !search_owns_keyboard(search) {
        return false;
    }
    let ctrl = modifiers.control_key();
    let shift = modifiers.shift_key();
    let alt = modifiers.alt_key();
    let super_key = modifiers.super_key();
    match physical_key {
        PhysicalKey::Code(KeyCode::F1) => ctrl || shift || alt || super_key,
        PhysicalKey::Code(KeyCode::KeyT) => ctrl && !shift && !alt && !super_key,
        _ => false,
    }
}

#[inline]
fn terminal_wheel_delta(delta: ScrollDelta, char_h: f32, sensitivity: f32) -> f32 {
    let sensitivity = crate::config::normalize_scroll_sensitivity(sensitivity);
    match delta {
        ScrollDelta::Line { y, .. } => y * 4.0 * char_h.max(0.0) * sensitivity,
        ScrollDelta::Pixel { y, .. } => y * sensitivity,
    }
}

#[inline]
fn terminal_body_takes_search_focus(search: &mut TerminalSearchState) {
    if search.shown {
        search.focused = false;
        search.release_grid_selection_ownership();
    }
}

fn terminal_mouse_cell_y(layout: TerminalUiLayout, y: f32) -> usize {
    let offset_from_bottom = (layout.body.y + layout.body.h - layout.bottom_pad - y
        + layout.scroll_offset)
        / layout.char_h.max(0.0001);
    layout
        .visible_rows
        .saturating_sub(1)
        .saturating_sub(offset_from_bottom.max(0.0).floor() as usize)
        + 1
}

#[inline]
fn terminal_protocol_pointer_target(layout: TerminalUiLayout, x: f32, y: f32) -> bool {
    layout.body.contains(x, y)
        && !layout
            .search
            .is_some_and(|search| search.hit_test(x, y) != TerminalSearchHit::None)
        && !layout
            .scrollbar
            .is_some_and(|scrollbar| scrollbar.track.contains(x, y))
}

fn terminal_mouse_protocol_cell(
    layout: TerminalUiLayout,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    if layout.char_w <= 0.0
        || layout.char_h <= 0.0
        || layout.visible_rows == 0
        || layout.cols == 0
        || !terminal_protocol_pointer_target(layout, x, y)
    {
        return None;
    }
    let col = (((x - layout.text_x).max(0.0) / layout.char_w).floor() as usize + 1)
        .clamp(1, layout.cols);
    Some((col, terminal_mouse_cell_y(layout, y)))
}

fn terminal_mouse_event_sequence(
    layout: TerminalUiLayout,
    capture: PointerCapture,
    tracking: MouseTrackingMode,
    sgr: bool,
    state: KeyState,
    button: PointerButton,
    x: f32,
    y: f32,
) -> Option<Vec<u8>> {
    if capture != PointerCapture::None {
        return None;
    }
    let (cell_x, cell_y) = terminal_mouse_protocol_cell(layout, x, y)?;
    let button = terminal_mouse_button(button)?;
    let kind = if state == KeyState::Pressed {
        MouseEventKind::Press(button)
    } else {
        MouseEventKind::Release(button)
    };
    // Protocol cells are one-based; the crate takes zero-based coordinates.
    mouse_report(tracking, sgr, kind, cell_x - 1, cell_y - 1)
}

fn terminal_mouse_motion_sequence(
    layout: TerminalUiLayout,
    tracking: MouseTrackingMode,
    sgr: bool,
    pressed_buttons: u8,
    x: f32,
    y: f32,
) -> Option<Vec<u8>> {
    let (cell_x, cell_y) = terminal_mouse_protocol_cell(layout, x, y)?;
    let kind = MouseEventKind::Motion {
        held: terminal_first_pressed_button(pressed_buttons),
    };
    mouse_report(tracking, sgr, kind, cell_x - 1, cell_y - 1)
}

fn terminal_mouse_wheel_sequence(
    layout: TerminalUiLayout,
    tracking: MouseTrackingMode,
    sgr: bool,
    x: f32,
    y: f32,
    dy: f32,
) -> Option<Vec<u8>> {
    let (cell_x, cell_y) = terminal_mouse_protocol_cell(layout, x, y)?;
    let steps = (dy.abs() / 20.0).max(1.0) as usize;
    let kind = if dy > 0.0 {
        MouseEventKind::WheelUp
    } else {
        MouseEventKind::WheelDown
    };
    let sequence = mouse_report(tracking, sgr, kind, cell_x - 1, cell_y - 1)?;
    let mut input = Vec::with_capacity(sequence.len().saturating_mul(steps.min(3)));
    for _ in 0..steps.min(3) {
        input.extend_from_slice(&sequence);
    }
    Some(input)
}

fn terminal_mouse_button(button: PointerButton) -> Option<MouseButton> {
    match button {
        PointerButton::Left => Some(MouseButton::Left),
        PointerButton::Middle => Some(MouseButton::Middle),
        PointerButton::Right => Some(MouseButton::Right),
        PointerButton::Back | PointerButton::Forward | PointerButton::Other(_) => None,
    }
}

fn update_pressed_mouse_buttons(buttons: &mut u8, button: PointerButton, state: KeyState) {
    // Bit per button: left 1, middle 2, right 4.
    let bit = match button {
        PointerButton::Left => 1u8,
        PointerButton::Middle => 2,
        PointerButton::Right => 4,
        PointerButton::Back | PointerButton::Forward | PointerButton::Other(_) => return,
    };
    if state == KeyState::Pressed {
        *buttons |= bit;
    } else {
        *buttons &= !bit;
    }
}

fn terminal_first_pressed_button(buttons: u8) -> Option<MouseButton> {
    if buttons & 1 != 0 {
        Some(MouseButton::Left)
    } else if buttons & 2 != 0 {
        Some(MouseButton::Middle)
    } else if buttons & 4 != 0 {
        Some(MouseButton::Right)
    } else {
        None
    }
}

fn terminal_clipboard_paste_bytes(
    file_list: Option<&[PathBuf]>,
    text: Option<&str>,
) -> Option<Vec<u8>> {
    if let Some(paths) = file_list.filter(|paths| !paths.is_empty()) {
        let mut out = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            if index > 0 { out.push(b' '); }
            terminal_shell_escape_path(path, &mut out);
        }
        return Some(out);
    }
    text.map(|text| text.as_bytes().to_vec())
}

/// Delivers a paste payload as user input, wrapped when bracketed paste is on.
fn write_terminal_paste(terminal: &mut Terminal, payload: Vec<u8>) {
    let bracketed_paste = crate::platform::lock_recover(&terminal.grid).bracketed_paste();
    terminal.write_user_input(&bracketed_paste_bytes(payload, bracketed_paste));
}

fn terminal_shell_escape_path(path: &Path, out: &mut Vec<u8>) {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() { out.extend_from_slice(b"''"); return; }
    for &byte in bytes {
        match byte {
            b'\n' | b'\r' => { out.extend_from_slice(&[b'\'', byte, b'\'']); continue; }
            b' ' | b'\t' | b'\\' | b'\'' | b'"' | b'`' | b'$' | b'&' | b';' | b'|'
            | b'<' | b'>' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'*' | b'?' | b'!'
            | b'#' | b'~' | b'^' | b'%' | b'=' => out.push(b'\\'),
            _ => {}
        }
        out.push(byte);
    }
}

fn terminal_text_sequence(text: &str, ctrl: bool, alt: bool, super_key: bool) -> Option<Vec<u8>> {
    if text.is_empty() || ctrl || super_key {
        return None;
    }
    Some(terminal_alt_prefixed(text.as_bytes(), alt))
}

/// Held-key bit, modifier flag, and the bits of both physical keys setting that flag,
/// for a physical modifier key. Keys that set no `Modifiers` flag give `None`.
fn modifier_key_bits(modifier: ModKey) -> Option<(u8, Modifiers, u8)> {
    const SHIFT_KEYS: u8 = 0b0001_0001;
    const CONTROL_KEYS: u8 = 0b0010_0010;
    const ALT_KEYS: u8 = 0b0100_0100;
    const SUPER_KEYS: u8 = 0b1000_1000;
    Some(match modifier {
        ModKey::LeftShift => (0b0000_0001, Modifiers::SHIFT, SHIFT_KEYS),
        ModKey::RightShift => (0b0001_0000, Modifiers::SHIFT, SHIFT_KEYS),
        ModKey::LeftCtrl => (0b0000_0010, Modifiers::CONTROL, CONTROL_KEYS),
        ModKey::RightCtrl => (0b0010_0000, Modifiers::CONTROL, CONTROL_KEYS),
        ModKey::LeftAlt => (0b0000_0100, Modifiers::ALT, ALT_KEYS),
        ModKey::RightAlt => (0b0100_0000, Modifiers::ALT, ALT_KEYS),
        ModKey::LeftSuper => (0b0000_1000, Modifiers::SUPER, SUPER_KEYS),
        ModKey::RightSuper => (0b1000_0000, Modifiers::SUPER, SUPER_KEYS),
        _ => return None,
    })
}

/// Builds the encoder event for one host key. The named key wins over the
/// physical key, which wins over the layout character.
pub(crate) fn terminal_key_event(key: &KeyInput, mods: Modifiers) -> Option<KeyEvent<'_>> {
    // A key with no named, physical, or layout identity has nothing to encode,
    // so it is dropped instead of being encoded as a placeholder character.
    let named = key
        .named
        .or_else(|| terminal_named_key(key.physical_key))
        .or(key.unshifted.map(TermKey::Char))?;
    Some(KeyEvent {
        physical: u64::from(key.keycode),
        key: named,
        base: us_base_char_from_evdev(key.keycode),
        mods: KeyMods {
            shift: mods.shift_key(),
            ctrl: mods.control_key(),
            alt: mods.alt_key(),
            super_key: mods.super_key(),
        },
        kind: match key.state {
            KeyState::Pressed => KeyKind::Press,
            KeyState::Repeat => KeyKind::Repeat,
            KeyState::Released => KeyKind::Release,
        },
        text: key.text.as_deref(),
    })
}

/// Named keys that have a fixed terminal meaning independent of the layout.
fn terminal_named_key(key: PhysicalKey) -> Option<TermKey> {
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    Some(match code {
        KeyCode::Enter => TermKey::Enter,
        KeyCode::NumpadEnter => TermKey::Kp(KpKey::Enter),
        KeyCode::Tab => TermKey::Tab,
        KeyCode::Backspace => TermKey::Backspace,
        KeyCode::Escape => TermKey::Escape,
        KeyCode::Insert => TermKey::Insert,
        KeyCode::Delete => TermKey::Delete,
        KeyCode::PageUp => TermKey::PageUp,
        KeyCode::PageDown => TermKey::PageDown,
        KeyCode::Home => TermKey::Home,
        KeyCode::End => TermKey::End,
        KeyCode::ArrowUp => TermKey::Up,
        KeyCode::ArrowDown => TermKey::Down,
        KeyCode::ArrowLeft => TermKey::Left,
        KeyCode::ArrowRight => TermKey::Right,
        KeyCode::F1 => TermKey::F(1),
        KeyCode::F2 => TermKey::F(2),
        KeyCode::F3 => TermKey::F(3),
        KeyCode::F4 => TermKey::F(4),
        KeyCode::F5 => TermKey::F(5),
        KeyCode::F6 => TermKey::F(6),
        KeyCode::F7 => TermKey::F(7),
        KeyCode::F8 => TermKey::F(8),
        KeyCode::F9 => TermKey::F(9),
        KeyCode::F10 => TermKey::F(10),
        KeyCode::F11 => TermKey::F(11),
        KeyCode::F12 => TermKey::F(12),
        _ => return None,
    })
}

fn terminal_alt_prefixed(bytes: &[u8], alt: bool) -> Vec<u8> {
    if !alt { return bytes.to_vec(); }
    let mut out = Vec::with_capacity(bytes.len() + 1); out.push(0x1b); out.extend_from_slice(bytes); out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn project_owned_keyboard_contract_separates_physical_keys_from_text() {
        assert_eq!(
            terminal_text_sequence("Ж界", false, false, false),
            Some("Ж界".as_bytes().to_vec()),
        );
        assert_eq!(
            terminal_text_sequence("x", false, true, false),
            Some(b"\x1bx".to_vec()),
        );
        assert_eq!(terminal_text_sequence("a", true, false, false), None);
        assert_eq!(terminal_text_sequence("a", false, false, true), None);
    }

    #[test]
    fn focused_search_falls_through_for_modified_f1_and_ctrl_t_terminal_contract() {
        let mut search = TerminalSearchState::default();
        search.open();
        assert!(search_owns_keyboard(&search));

        let none = Modifiers::empty();
        let ctrl = Modifiers::CONTROL;
        let alt = Modifiers::ALT;
        let shift = Modifiers::SHIFT;
        let super_key = Modifiers::SUPER;
        let ctrl_shift = Modifiers::CONTROL | Modifiers::SHIFT;

        assert!(!search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::F1),
            none,
        ));
        for modifiers in [ctrl, alt, shift, super_key, ctrl_shift] {
            assert!(search_key_falls_through_to_terminal(
                &search,
                PhysicalKey::Code(KeyCode::F1),
                modifiers,
            ));
        }

        assert!(search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::KeyT),
            ctrl,
        ));
        assert!(!search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::KeyA),
            none,
        ));
        assert!(!search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::ArrowLeft),
            none,
        ));
        assert!(!search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::KeyV),
            ctrl,
        ));
        assert!(!search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::KeyT),
            ctrl_shift,
        ));
    }

    #[test]
    fn terminal_wheel_delta_matches_donor_line_height_and_sensitivity() {
        let line = ScrollDelta::Line { x: 0.0, y: 1.0 };
        assert_eq!(terminal_wheel_delta(line, 24.0, 1.0), 96.0);
        assert_eq!(terminal_wheel_delta(line, 24.0, 0.5), 48.0);
        assert_eq!(terminal_wheel_delta(line, 24.0, 2.0), 192.0);

        let pixel = ScrollDelta::Pixel { x: 0.0, y: -12.5 };
        assert_eq!(terminal_wheel_delta(pixel, 24.0, 1.0), -12.5);
        assert_eq!(terminal_wheel_delta(pixel, 24.0, 2.0), -25.0);
    }

    #[test]
    fn cancelling_scrollbar_pointer_capture_ends_drag_without_moving_scroll() {
        let mut interaction = TerminalInteraction::default();
        interaction.pointer_capture = PointerCapture::TerminalScrollbar;
        let mut scroll = crate::scroll::ScrollState::new(7.0);
        scroll.current = 42.0;
        scroll.target = 42.0;
        scroll.is_dragging = true;
        scroll.drag_offset = 9.0;

        interaction.cancel_pointer_interaction_state(&mut scroll);

        assert_eq!(interaction.pointer_capture, PointerCapture::None);
        assert!(!scroll.is_dragging);
        assert_eq!(scroll.drag_offset, 0.0);
        assert_eq!(scroll.current, 42.0);
        assert_eq!(scroll.target, 42.0);
    }

    #[test]
    fn reset_for_terminal_switch_ends_scrollbar_drag_of_left_terminal() {
        let mut terminal = Terminal::new_for_test(8, 2, 1);
        let mut interaction = TerminalInteraction::default();
        interaction.pointer_capture = PointerCapture::TerminalScrollbar;
        terminal.scroll_y.is_dragging = true;

        interaction.reset_for_terminal_switch(&mut terminal);

        assert_eq!(interaction.pointer_capture, PointerCapture::None);
        assert!(!terminal.scroll_y.is_dragging);
    }

    #[test]
    fn pointer_cancellation_clears_tracked_physical_buttons_even_without_capture() {
        let mut interaction = TerminalInteraction::default();
        interaction.pressed_mouse_buttons = 1;
        interaction.pointer_capture = PointerCapture::None;
        let mut scroll = crate::scroll::ScrollState::new(7.0);

        interaction.cancel_pointer_interaction_state(&mut scroll);

        assert_eq!(interaction.pressed_mouse_buttons, 0);
        assert!(terminal_mouse_motion_sequence(
            mouse_test_layout(0.0),
            MouseTrackingMode::ButtonMotion,
            true,
            interaction.pressed_mouse_buttons,
            30.0,
            140.0,
        )
        .is_none());

        update_pressed_mouse_buttons(
            &mut interaction.pressed_mouse_buttons,
            PointerButton::Left,
            KeyState::Pressed,
        );
        assert_eq!(interaction.pressed_mouse_buttons, 1);
        assert!(terminal_mouse_motion_sequence(
            mouse_test_layout(0.0),
            MouseTrackingMode::ButtonMotion,
            true,
            interaction.pressed_mouse_buttons,
            30.0,
            140.0,
        )
        .is_some());
    }

    #[test]
    fn cancelling_search_input_capture_preserves_search_and_stops_pointer_selection_drag() {
        let mut interaction = TerminalInteraction::default();
        interaction.search.open();
        interaction.search.insert_text("abcdef");
        begin_search_pointer_selection(&mut interaction.search, 1);
        assert!(update_search_pointer_selection(
            &mut interaction.search,
            PointerCapture::SearchInput,
            4,
        ));
        interaction.pointer_capture = PointerCapture::SearchInput;
        let before = (interaction.search.cursor, interaction.search.selection_anchor);
        let mut scroll = crate::scroll::ScrollState::new(7.0);

        interaction.cancel_pointer_interaction_state(&mut scroll);

        assert_eq!(interaction.pointer_capture, PointerCapture::None);
        assert!(interaction.search.shown);
        assert!(interaction.search.focused);
        assert!(!update_search_pointer_selection(
            &mut interaction.search,
            interaction.pointer_capture,
            6,
        ));
        assert_eq!(
            (interaction.search.cursor, interaction.search.selection_anchor),
            before,
        );
    }

    #[test]
    fn cancelling_terminal_selection_capture_prevents_followup_drag_ownership() {
        let mut interaction = TerminalInteraction::default();
        interaction.pointer_capture = PointerCapture::TerminalSelection;
        let mut scroll = crate::scroll::ScrollState::new(7.0);

        interaction.cancel_pointer_interaction_state(&mut scroll);

        assert_eq!(interaction.pointer_capture, PointerCapture::None);
        assert!(!scroll.is_dragging);
    }

    #[test]
    fn cancelling_search_control_capture_only_releases_pointer_ownership() {
        let mut interaction = TerminalInteraction::default();
        interaction.search.open();
        interaction.search.insert_text("needle");
        interaction.pointer_capture = PointerCapture::SearchControl;
        let mut scroll = crate::scroll::ScrollState::new(7.0);

        interaction.cancel_pointer_interaction_state(&mut scroll);

        assert_eq!(interaction.pointer_capture, PointerCapture::None);
        assert!(interaction.search.shown);
        assert_eq!(interaction.search.text, "needle");
    }

    fn raw_key(state: KeyState, physical_key: PhysicalKey, keycode: u32) -> KeyInput {
        KeyInput {
            state,
            physical_key,
            keycode,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        }
    }

    #[test]
    fn key_event_maps_russian_ctrl_c() {
        let mut key = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyC), 46);
        key.unshifted = Some('с');
        let event = terminal_key_event(&key, Modifiers::CONTROL).expect("identified key");
        assert_eq!(event.physical, 46);
        assert_eq!(event.key, TermKey::Char('с'));
        assert_eq!(event.base, Some('c'));
        assert!(event.mods.ctrl);
        assert_eq!(event.kind, KeyKind::Press);
    }

    #[test]
    fn key_event_base_for_punctuation() {
        let mut semicolon = raw_key(KeyState::Pressed, PhysicalKey::Unidentified, 39);
        semicolon.unshifted = Some('ж');
        let event = terminal_key_event(&semicolon, Modifiers::empty()).expect("identified key");
        assert_eq!(event.key, TermKey::Char('ж'));
        assert_eq!(event.base, Some(';'));

        // Any layout's level-0 character for the digit key is '3', so the fixture carries it.
        let mut digit = raw_key(KeyState::Pressed, PhysicalKey::Unidentified, 4);
        digit.unshifted = Some('3');
        assert_eq!(
            terminal_key_event(&digit, Modifiers::empty()).expect("identified key").base,
            Some('3')
        );
    }

    #[test]
    fn key_event_prefers_named_over_keycode() {
        let mut enter = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::NumpadEnter), 96);
        enter.named = Some(TermKey::Kp(KpKey::Enter));
        assert_eq!(
            terminal_key_event(&enter, Modifiers::empty()).expect("identified key").key,
            TermKey::Kp(KpKey::Enter)
        );
    }

    #[test]
    fn ctrl_c_without_selection_sends_etx_flags0() {
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut key = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyC), 46);
        key.unshifted = Some('c');
        assert!(interaction.handle_key_event(&key, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), vec![0x03]);
    }

    #[test]
    fn ctrl_c_with_selection_release_not_sent() {
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        crate::platform::lock_recover(&terminal.grid).set_selection(Some((1, 1, 4, 1)));
        let mut release = raw_key(KeyState::Released, PhysicalKey::Code(KeyCode::KeyC), 46);
        release.unshifted = Some('c');
        assert!(!interaction.handle_key_event(&release, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
        assert!(crate::platform::lock_recover(&terminal.grid).selection().is_some());
    }

    #[test]
    fn ctrl_c_with_selection_press_copies_and_sends_nothing() {
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        // The selection lies past the grid rows, so its copied text is empty and
        // the clipboard write is skipped; only selection state and encoder output are checked.
        crate::platform::lock_recover(&terminal.grid).set_selection(Some((1, 1000, 4, 1000)));
        let mut press = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyC), 46);
        press.unshifted = Some('c');
        assert!(interaction.handle_key_event(&press, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
        assert!(crate::platform::lock_recover(&terminal.grid).selection().is_none());
    }

    #[test]
    fn ctrl_v_release_not_sent() {
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let release = raw_key(KeyState::Released, PhysicalKey::Code(KeyCode::KeyV), 47);
        assert!(!interaction.handle_key_event(&release, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn alt_a_flag1_sends_no_separate_text() {
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::ALT);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        // Kitty push of flag 1 (disambiguate escape codes) through the grid parser.
        crate::platform::lock_recover(&terminal.grid).advance(
            b"\x1b[>1u",
            std::time::Instant::now(),
            &mut sink,
        );
        let mut key = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyA), 30);
        key.unshifted = Some('a');
        key.text = Some("a".to_string());
        // Sent means the encoder owns the key, so the text is not handled separately.
        assert!(interaction.handle_key_event(&key, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[97;3u");
    }

    #[test]
    fn memory_clipboard_round_trip() {
        // Expected values come from the Task 3 brief: Memory keeps the last written text.
        let mut interaction = TerminalInteraction::new_with_memory_clipboard();
        assert_eq!(interaction.clipboard_text(), None);
        assert!(interaction.set_clipboard_text("x".to_string()));
        assert_eq!(interaction.clipboard_text().as_deref(), Some("x"));
        assert_eq!(interaction.clipboard_file_list(), None);
    }

    #[test]
    fn ctrl_v_repeat_pastes_release_not_sent() {
        let mut interaction = TerminalInteraction::new_with_memory_clipboard();
        assert!(interaction.set_clipboard_text("pasted".to_string()));
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let press = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyV), 47);
        let repeat = raw_key(KeyState::Repeat, PhysicalKey::Code(KeyCode::KeyV), 47);
        let release = raw_key(KeyState::Released, PhysicalKey::Code(KeyCode::KeyV), 47);
        interaction.handle_key_event(&press, &mut terminal);
        interaction.handle_key_event(&repeat, &mut terminal);
        interaction.handle_key_event(&release, &mut terminal);
        // Press and repeat each paste the seeded Memory text once; the release sends nothing.
        // Bracketed paste is off in `new_for_test`, so the payload is written verbatim.
        let bytes = terminal.input_bytes_for_test();
        assert!(!bytes.contains(&0x16));
        assert!(!bytes.windows(8).any(|window| window == b"\x1b[118;5u"));
        assert_eq!(bytes, b"pastedpasted");
    }

    #[test]
    fn ctrl_v_with_nothing_to_paste_reaches_encoder() {
        // Expected bytes come from the legacy Ctrl+V contract: the encoder sends 0x16 on press,
        // and the release emits nothing in legacy mode.
        let mut interaction = TerminalInteraction::new_with_memory_clipboard();
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut press = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyV), 47);
        press.unshifted = Some('v');
        let release = raw_key(KeyState::Released, PhysicalKey::Code(KeyCode::KeyV), 47);
        assert!(interaction.handle_key_event(&press, &mut terminal));
        interaction.handle_key_event(&release, &mut terminal);
        assert_eq!(terminal.input_bytes_for_test(), [0x16]);
    }

    #[test]
    fn search_target_gets_text_after_key() {
        let mut interaction = TerminalInteraction::default();
        interaction.search.open();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut key = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyA), 30);
        key.unshifted = Some('a');
        key.text = Some("a".to_string());
        assert!(interaction.handle_key_event(&key, &mut terminal));
        assert_eq!(interaction.search.text, "a");
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn repeated_letter_reaches_pty_as_text() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut repeat = raw_key(KeyState::Repeat, PhysicalKey::Code(KeyCode::KeyA), 30);
        repeat.unshifted = Some('a');
        repeat.text = Some("a".to_string());
        assert!(interaction.handle_key_event(&repeat, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"a");
    }

    #[test]
    fn ime_preedit_key_not_encoded() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let enter = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::Enter), 28);
        assert!(interaction.handle_key_event(&enter, &mut terminal));
        assert!(!terminal.input_bytes_for_test().is_empty());

        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut composing = enter.clone();
        composing.ime_preedit = true;
        assert!(!interaction.handle_key_event(&composing, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn ime_preedit_press_text_reaches_pty() {
        // Expected value comes from the I1 ruling: press text still reaches the PTY under preedit.
        let mut interaction = TerminalInteraction::default();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut press = raw_key(KeyState::Pressed, PhysicalKey::Code(KeyCode::KeyA), 30);
        press.unshifted = Some('a');
        press.text = Some("a".to_string());
        press.ime_preedit = true;
        assert!(interaction.handle_key_event(&press, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"a");
    }

    #[test]
    fn ime_preedit_repeat_sends_nothing() {
        // Expected value comes from the I1 ruling: repeat under preedit stays suppressed.
        let mut interaction = TerminalInteraction::default();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut repeat = raw_key(KeyState::Repeat, PhysicalKey::Code(KeyCode::KeyA), 30);
        repeat.unshifted = Some('a');
        repeat.text = Some("a".to_string());
        repeat.ime_preedit = true;
        assert!(!interaction.handle_key_event(&repeat, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn ctrl_c_with_selection_press_repeat_release_sends_nothing() {
        // Expected behaviour comes from the G2-1 ruling: a host-intercepted press keeps
        // its key away from the encoder until that key's release.
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::CONTROL);
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        crate::platform::lock_recover(&terminal.grid).set_selection(Some((0, 0, 0, 2)));
        let ctrl_c = |state| {
            let mut key = raw_key(state, PhysicalKey::Code(KeyCode::KeyC), 46);
            key.unshifted = Some('c');
            key
        };
        let press = ctrl_c(KeyState::Pressed);
        let repeat = ctrl_c(KeyState::Repeat);
        let release = ctrl_c(KeyState::Released);
        assert!(interaction.handle_key_event(&press, &mut terminal));
        assert!(crate::platform::lock_recover(&terminal.grid).selection().is_none());
        assert!(!interaction.handle_key_event(&repeat, &mut terminal));
        assert!(!interaction.handle_key_event(&release, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());

        // A fresh Ctrl+C press without selection goes to the encoder again (ETX, flags 0).
        assert!(interaction.handle_key_event(&press, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x03");
    }

    #[test]
    fn release_without_flag2_sends_nothing() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let up = raw_key(KeyState::Released, PhysicalKey::Code(KeyCode::ArrowUp), 103);
        assert!(!interaction.handle_key_event(&up, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn unidentified_key_without_text_sends_nothing() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let key = raw_key(KeyState::Pressed, PhysicalKey::Unidentified, 0);
        interaction.handle_key_event(&key, &mut terminal);
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn unidentified_alt_key_sends_nothing_with_or_without_kitty_flags() {
        let mut interaction = TerminalInteraction::default();
        interaction.set_modifiers(Modifiers::ALT);
        let key = raw_key(KeyState::Pressed, PhysicalKey::Unidentified, 0);

        // Flags 0: an encoded placeholder would arrive as ESC NUL.
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        assert!(!interaction.handle_key_event(&key, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());

        // Kitty flag 8 (report all keys as escape codes) through the grid parser.
        let mut terminal = Terminal::new_for_test(80, 30, 1);
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        crate::platform::lock_recover(&terminal.grid).advance(
            b"\x1b[>8u",
            std::time::Instant::now(),
            &mut sink,
        );
        assert!(!interaction.handle_key_event(&key, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    fn modifier_key(state: KeyState, modifier: ModKey, keycode: u32) -> KeyInput {
        KeyInput {
            named: Some(TermKey::Modifier(modifier)),
            ..raw_key(state, PhysicalKey::Unidentified, keycode)
        }
    }

    /// Terminal with Kitty flags 10 (report all keys and report event types) pushed.
    fn modifier_test_terminal() -> Terminal {
        let terminal = Terminal::new_for_test(80, 30, 1);
        let mut sink = crate::terminal_process::SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        crate::platform::lock_recover(&terminal.grid).advance(
            b"\x1b[>10u",
            std::time::Instant::now(),
            &mut sink,
        );
        terminal
    }

    // Expected bytes follow the reference "Modifiers" rule (protocol ref line 121) and
    // the encoder tests in crates/ronsole-term/src/tests/keys.rs, not the host output.
    #[test]
    fn left_ctrl_press_sets_own_ctrl_bit_with_empty_host_modifiers() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        let press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        assert!(interaction.handle_key_event(&press, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[57442;5u");
    }

    #[test]
    fn left_ctrl_release_clears_ctrl_bit_when_no_other_ctrl_held() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        let press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        let release = modifier_key(KeyState::Released, ModKey::LeftCtrl, 29);
        assert!(interaction.handle_key_event(&press, &mut terminal));
        assert!(interaction.handle_key_event(&release, &mut terminal));
        assert_eq!(
            terminal.input_bytes_for_test(),
            b"\x1b[57442;5u\x1b[57442;1:3u"
        );
    }

    #[test]
    fn left_ctrl_release_keeps_ctrl_bit_while_right_ctrl_held() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        let left_press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        let right_press = modifier_key(KeyState::Pressed, ModKey::RightCtrl, 97);
        let left_release = modifier_key(KeyState::Released, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &left_press, &mut terminal));
        assert!(send_key(&mut interaction, &right_press, &mut terminal));
        assert!(send_key(&mut interaction, &left_release, &mut terminal));
        assert_eq!(
            terminal.input_bytes_for_test(),
            b"\x1b[57442;5u\x1b[57448;5u\x1b[57442;5:3u"
        );
    }

    #[test]
    fn modifier_release_without_sent_press_sends_nothing() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        let release = modifier_key(KeyState::Released, ModKey::LeftCtrl, 29);
        assert!(!interaction.handle_key_event(&release, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());

        // A focus change clears the sent set, so the release of a Ctrl pressed before it is dropped.
        let press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        assert!(interaction.handle_key_event(&press, &mut terminal));
        let before = terminal.input_bytes_for_test();
        let _ = crate::platform::lock_recover(&terminal.grid).focus_changed(true);
        assert!(!interaction.handle_key_event(&release, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), before);
    }

    /// Feeds one key the way `App::on_key` does: modifier state is noted first.
    fn send_key(interaction: &mut TerminalInteraction, key: &KeyInput, terminal: &mut Terminal) -> bool {
        interaction.note_modifier_key(key);
        interaction.handle_key_event(key, terminal)
    }

    /// A Ctrl press sent before search opened still gets its release while search owns the keyboard.
    #[test]
    fn ctrl_release_while_search_owns_keyboard_reaches_encoder() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        interaction.set_modifiers(Modifiers::CONTROL);
        let press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &press, &mut terminal));
        interaction.search.open();
        interaction.set_modifiers(Modifiers::empty());
        let release = modifier_key(KeyState::Released, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &release, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[57442;5u\x1b[57442;1:3u");
    }

    /// A release of a key whose press was never sent stays ignored while search owns the keyboard.
    #[test]
    fn letter_release_while_search_owns_keyboard_sends_nothing() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        interaction.search.open();
        let release = raw_key(KeyState::Released, PhysicalKey::Code(KeyCode::KeyA), 30);
        assert!(!send_key(&mut interaction, &release, &mut terminal));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    /// Modifier events that the settings modal swallows still update the held state,
    /// so a later Ctrl release does not keep a stale ctrl bit.
    #[test]
    fn modifier_state_noted_while_modal_swallows_keys() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        let both = Modifiers::new(true, true, false, false);
        interaction.set_modifiers(both);
        let left_press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &left_press, &mut terminal));
        // The modal returns before the encoder: only the held-state update runs.
        interaction.set_modifiers(Modifiers::SHIFT);
        interaction.note_modifier_key(&modifier_key(KeyState::Released, ModKey::LeftCtrl, 29));
        interaction.set_modifiers(both);
        let right_press = modifier_key(KeyState::Pressed, ModKey::RightCtrl, 97);
        assert!(send_key(&mut interaction, &right_press, &mut terminal));
        interaction.set_modifiers(Modifiers::SHIFT);
        let right_release = modifier_key(KeyState::Released, ModKey::RightCtrl, 97);
        assert!(send_key(&mut interaction, &right_release, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[57442;6u\x1b[57448;6u\x1b[57448;2:3u");
    }

    /// Empty host modifiers mean no modifier key is down, so the held state is cleared.
    #[test]
    fn set_modifiers_empty_clears_held_modifier_keys() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        interaction.set_modifiers(Modifiers::CONTROL);
        let press = modifier_key(KeyState::Pressed, ModKey::RightCtrl, 97);
        assert!(send_key(&mut interaction, &press, &mut terminal));
        interaction.set_modifiers(Modifiers::empty());
        assert_eq!(interaction.held_modifier_keys, 0);
        let release = modifier_key(KeyState::Released, ModKey::RightCtrl, 97);
        assert!(send_key(&mut interaction, &release, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[57448;5u\x1b[57448;1:3u");
    }

    /// Releasing one Ctrl while the other Ctrl and Shift are held keeps shift and drops ctrl.
    #[test]
    fn release_of_one_ctrl_keeps_shift_when_both_ctrl_and_shift_held() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        interaction.set_modifiers(Modifiers::SHIFT);
        let shift_press = modifier_key(KeyState::Pressed, ModKey::LeftShift, 42);
        assert!(send_key(&mut interaction, &shift_press, &mut terminal));
        interaction.set_modifiers(Modifiers::new(true, true, false, false));
        let ctrl_press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &ctrl_press, &mut terminal));
        interaction.set_modifiers(Modifiers::SHIFT);
        let ctrl_release = modifier_key(KeyState::Released, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &ctrl_release, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[57441;2u\x1b[57442;6u\x1b[57442;2:3u");
    }

    /// A repeat of a held modifier is a repeat event (type 2) and keeps its own bit.
    #[test]
    fn modifier_repeat_is_repeat_event_with_own_bit() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = modifier_test_terminal();
        interaction.set_modifiers(Modifiers::CONTROL);
        let press = modifier_key(KeyState::Pressed, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &press, &mut terminal));
        let repeat = modifier_key(KeyState::Repeat, ModKey::LeftCtrl, 29);
        assert!(send_key(&mut interaction, &repeat, &mut terminal));
        assert_eq!(terminal.input_bytes_for_test(), b"\x1b[57442;5u\x1b[57442;5:2u");
    }

    #[test]
    fn passive_search_refresh_never_requests_a_scroll_jump() {
        assert!(search_refresh_should_jump(true, SearchRefreshCause::User));
        assert!(!search_refresh_should_jump(true, SearchRefreshCause::Grid));
        assert!(!search_refresh_should_jump(false, SearchRefreshCause::User));

        let mut scroll_target = 137.0f32;
        let before = scroll_target;
        if search_refresh_should_jump(true, SearchRefreshCause::Grid) {
            scroll_target = 999.0;
        }
        assert_eq!(scroll_target, before);
    }

    #[test]
    fn dolphin_file_list_has_priority_and_shell_escapes_only_special_bytes() {
        let paths = vec![PathBuf::from("/home/reyan/Загрузки/file.patch"), PathBuf::from("/tmp/a b;$c")];
        let bytes = terminal_clipboard_paste_bytes(Some(&paths), Some("ignored")).unwrap();
        assert_eq!(bytes, "/home/reyan/Загрузки/file.patch /tmp/a\\ b\\;\\$c".as_bytes());
    }

    #[test]
    fn shell_escape_preserves_non_utf8_and_quotes_newline_cr_safely() {
        let raw = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff, b' ', b'x']);
        let mut out = Vec::new(); terminal_shell_escape_path(Path::new(&raw), &mut out);
        assert_eq!(out, vec![b'/', b't', b'm', b'p', b'/', 0xff, b'\\', b' ', b'x']);
        let mut special = Vec::new(); terminal_shell_escape_path(Path::new(OsString::from_vec(b"a\rb\nc".to_vec()).as_os_str()), &mut special);
        assert_eq!(special, b"a'\r'b'\n'c");
    }

    #[test]
    fn bracketed_paste_wraps_text_and_shell_escaped_file_lists_through_one_path() {
        assert_eq!(
            bracketed_paste_bytes(b"plain".to_vec(), false),
            b"plain"
        );
        assert_eq!(
            bracketed_paste_bytes(b"plain".to_vec(), true),
            b"\x1b[200~plain\x1b[201~"
        );

        let paths = [PathBuf::from("/tmp/a b"), PathBuf::from("/tmp/界")];
        let payload = terminal_clipboard_paste_bytes(Some(&paths), Some("ignored")).unwrap();
        assert_eq!(
            bracketed_paste_bytes(payload, true),
            "\u{1b}[200~/tmp/a\\ b /tmp/界\u{1b}[201~".as_bytes()
        );
    }

    fn enter_key() -> KeyInput {
        KeyInput {
            state: KeyState::Pressed,
            physical_key: PhysicalKey::Code(KeyCode::Enter),
            keycode: 28,
            text: None,
            unshifted: None,
            named: None,
            ime_preedit: false,
        }
    }

    #[test]
    fn user_input_routes_set_mark_and_programmatic_writes_do_not() {
        let mut interaction = TerminalInteraction::default();

        let mut terminal = selection_test_terminal();
        let _ = terminal.write_input(b"programmatic");
        assert!(terminal.last_user_input().is_none());
        interaction.layout = mouse_test_layout(0.0);
        interaction.mouse_x = 25.0;
        interaction.mouse_y = 400.0;
        {
            let mut term = crate::platform::lock_recover(&terminal.grid);
            term.advance(
                b"\x1b[?1000h\x1b[?1006h",
                std::time::Instant::now(),
                &mut crate::terminal_process::SessionSink {
                    reply_tx: None,
                    title_cache: None,
                    pending: None,
                    wake: None,
                },
            );
        }
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(terminal.last_user_input().is_none());

        let mut terminal = selection_test_terminal();
        assert!(interaction.handle_key_event(&enter_key(), &mut terminal));
        assert!(terminal.last_user_input().is_some());

        let mut terminal = selection_test_terminal();
        assert!(interaction.handle_text("a", &mut terminal));
        assert!(terminal.last_user_input().is_some());

        let mut terminal = selection_test_terminal();
        assert!(interaction.handle_ime_commit("б", &mut terminal));
        assert!(terminal.last_user_input().is_some());

        let mut terminal = selection_test_terminal();
        write_terminal_paste(&mut terminal, b"xy".to_vec());
        assert!(terminal.last_user_input().is_some());
    }

    #[test]
    fn held_user_input_reaches_pty_only_after_hold_ends_in_order() {
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ronsole-input-hold-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let out = root.join("out");
        let launch = crate::launch::TerminalLaunchSpec {
            shell: None,
            working_directory: Some(root.clone()),
            inherited_directory: None,
            command: vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("stty raw -echo; printf __READY__; exec cat > out"),
            ],
            hold: false,
            bridge_launch_id: None,
        };
        let mut terminal = Terminal::spawn(
            None,
            1,
            launch,
            crate::renderer::terminal_palette(&crate::config::AppConfig::default()),
        );
        let read_out = || std::fs::read(&out).unwrap_or_default();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let term = crate::platform::lock_recover(&terminal.grid);
            let ready = (0..term.rows())
                .filter_map(|row| term.visible_row(row))
                .flatten()
                .map(|cell| cell.ch())
                .collect::<String>()
                .contains("__READY__");
            drop(term);
            if ready && out.exists() {
                break;
            }
            assert!(Instant::now() < deadline, "PTY fixture did not become ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(terminal.process_group_leader().is_some());

        let mut interaction = TerminalInteraction::default();
        terminal.begin_input_hold();
        assert!(interaction.handle_text("a", &mut terminal));
        assert!(interaction.handle_ime_commit("б", &mut terminal));
        write_terminal_paste(&mut terminal, b"xy".to_vec());
        assert!(interaction.handle_key_event(&enter_key(), &mut terminal));
        assert!(terminal.last_user_input().is_some());
        std::thread::sleep(Duration::from_millis(150));
        assert!(read_out().is_empty(), "held input leaked: {:?}", read_out());

        terminal.end_input_hold();
        let expected = "aбxy\r".as_bytes();
        let deadline = Instant::now() + Duration::from_secs(3);
        while read_out().len() < expected.len() {
            assert!(Instant::now() < deadline, "held input not flushed: {:?}", read_out());
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(read_out(), expected);

        terminal.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dec_mouse_motion_modes_distinguish_button_and_any_motion() {
        let layout = mouse_test_layout(0.0);
        let x = layout.text_x + layout.char_w * 2.5;
        let y = layout.body.y + 100.0;

        assert!(terminal_mouse_motion_sequence(
            layout,
            MouseTrackingMode::ButtonMotion,
            true,
            0,
            x,
            y,
        )
        .is_none());
        assert_eq!(
            terminal_mouse_motion_sequence(
                layout,
                MouseTrackingMode::ButtonMotion,
                true,
                1,
                x,
                y,
            )
            .unwrap(),
            b"\x1b[<32;3;5M"
        );
        assert_eq!(
            terminal_mouse_motion_sequence(
                layout,
                MouseTrackingMode::AnyMotion,
                true,
                0,
                x,
                y,
            )
            .unwrap(),
            b"\x1b[<35;3;5M"
        );
    }

    #[test]
    fn sgr_mouse_protocol_uses_one_based_cells_and_release_marker() {
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, true, MouseEventKind::Press(MouseButton::Left), 2, 3),
            Some(b"\x1b[<0;3;4M".to_vec())
        );
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, true, MouseEventKind::Release(MouseButton::Right), 0, 0),
            Some(b"\x1b[<2;1;1m".to_vec())
        );
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, false, MouseEventKind::Press(MouseButton::Left), 2, 3),
            Some(vec![0x1b, b'[', b'M', 32, 35, 36])
        );
    }

    /// Test terminal whose bottom row holds a URL at columns 4..25. The point
    /// (73, 682) is on that URL: column 6, bottom row of the body. With
    /// `tracking`, SGR mouse tracking (`ESC[?1000h`, `ESC[?1006h`) is on.
    fn link_test_terminal(tracking: bool) -> Terminal {
        let mode: &[u8] = if tracking {
            b"\x1b[?1000h\x1b[?1006h"
        } else {
            b""
        };
        link_test_terminal_with_modes(mode)
    }

    /// Like `link_test_terminal`, but writes `modes` before the URL row.
    fn link_test_terminal_with_modes(modes: &[u8]) -> Terminal {
        let terminal = selection_test_terminal();
        let bytes = [modes, b"\x1b[30;1Hsee https://example.com/x"].concat();
        crate::platform::lock_recover(&terminal.grid).advance(
            &bytes,
            std::time::Instant::now(),
            &mut crate::terminal_process::SessionSink {
                reply_tx: None,
                title_cache: None,
                pending: None,
                wake: None,
            },
        );
        terminal
    }

    #[test]
    fn ctrl_click_with_tracking_not_sent_to_pty() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(true);
        interaction.layout = mouse_test_layout(0.0);
        interaction.mouse_x = 73.0;
        interaction.mouse_y = 682.0;
        interaction.set_modifiers(Modifiers::CONTROL);
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(terminal.input_bytes_for_test().is_empty());
        assert!(!interaction.terminal_selection_active());
        assert_eq!(
            interaction.take_link_uri().as_deref(),
            Some("https://example.com/x")
        );
        assert!(interaction.take_link_uri().is_none());
    }

    #[test]
    fn ctrl_click_off_link_with_tracking_still_reports_to_pty() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(true);
        interaction.layout = mouse_test_layout(0.0);
        // Column 60 of the same row: no link there.
        interaction.mouse_x = 613.0;
        interaction.mouse_y = 682.0;
        interaction.set_modifiers(Modifiers::CONTROL);
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(terminal.input_bytes_for_test().starts_with(b"\x1b[<"));
        assert!(interaction.take_link_uri().is_none());
    }

    #[test]
    fn ctrl_hover_sets_pointer_and_range() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(false);
        interaction.layout = mouse_test_layout(0.0);
        interaction.set_modifiers(Modifiers::CONTROL);
        interaction.update_link_hover(&mut terminal, 73.0, 682.0, interaction.layout);
        let (link, generation) = terminal.hovered_link().expect("link under pointer");
        let term = crate::platform::lock_recover(&terminal.grid);
        // The URL is written on the bottom row, which is the last row of the grid.
        assert_eq!(&*link.uri, "https://example.com/x");
        assert_eq!(link.cells, vec![(term.total_rows() - 1, 4..25)]);
        assert_eq!(*generation, term.content_generation());
    }

    #[test]
    fn hover_without_ctrl_keeps_default_cursor() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(false);
        interaction.layout = mouse_test_layout(0.0);
        interaction.update_link_hover(&mut terminal, 73.0, 682.0, interaction.layout);
        assert!(terminal.hovered_link().is_none());
    }

    #[test]
    fn releasing_ctrl_clears_hovered_link() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(false);
        interaction.layout = mouse_test_layout(0.0);
        interaction.set_modifiers(Modifiers::CONTROL);
        interaction.update_link_hover(&mut terminal, 73.0, 682.0, interaction.layout);
        assert!(terminal.hovered_link().is_some());
        interaction.set_modifiers(Modifiers::empty());
        interaction.update_link_hover(&mut terminal, 73.0, 682.0, interaction.layout);
        assert!(terminal.hovered_link().is_none());
    }

    #[test]
    fn ctrl_press_on_link_under_button_event_mode_does_not_leak_drag_to_pty() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal_with_modes(b"\x1b[?1002h\x1b[?1006h");
        interaction.layout = mouse_test_layout(0.0);
        interaction.mouse_x = 73.0;
        interaction.mouse_y = 682.0;
        interaction.set_modifiers(Modifiers::CONTROL);
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        // The pointer moves one column while the Ctrl+press is still held down.
        assert!(!interaction.cursor_moved(83.0, 682.0, &mut terminal, |_, _, _| 0));
        assert!(terminal.input_bytes_for_test().is_empty());
        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn new_left_press_clears_stale_link_press_so_next_release_reaches_pty() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(true);
        interaction.layout = mouse_test_layout(0.0);
        interaction.mouse_x = 73.0;
        interaction.mouse_y = 682.0;
        interaction.set_modifiers(Modifiers::CONTROL);
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        // The Release of the link press never reached mouse_input (for example
        // a modal swallowed it), so the next plain press must clear the pending state.
        interaction.set_modifiers(Modifiers::empty());
        interaction.mouse_x = 300.0;
        interaction.mouse_y = 300.0;
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        // SGR reports: one press (final byte `M`) and one release (final byte `m`).
        let input = String::from_utf8_lossy(&terminal.input_bytes_for_test()).into_owned();
        assert_eq!(
            input.matches("\x1b[<").count(),
            2,
            "press and release must both reach the PTY: {input:?}"
        );
        assert!(input.ends_with('m'), "release report expected: {input:?}");
    }

    #[test]
    fn switching_terminal_clears_hovered_link_of_terminal_left() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(false);
        interaction.layout = mouse_test_layout(0.0);
        interaction.set_modifiers(Modifiers::CONTROL);
        interaction.update_link_hover(&mut terminal, 73.0, 682.0, interaction.layout);
        assert!(terminal.hovered_link().is_some());
        interaction.reset_for_terminal_switch(&mut terminal);
        assert!(terminal.hovered_link().is_none());
    }

    fn mouse_test_layout(scroll_offset: f32) -> TerminalUiLayout {
        TerminalUiLayout {
            body: crate::search::Rect { x: 0.0, y: 100.0, w: 800.0, h: 600.0 },
            text_x: 8.0,
            char_w: 10.0,
            char_h: 20.0,
            visible_rows: 29,
            cols: 80,
            scroll_offset,
            bottom_pad: 8.0,
            scale: 1.0,
            ..TerminalUiLayout::default()
        }
    }

    fn selection_test_terminal() -> Terminal {
        Terminal::new_for_test(80, 30, 1)
    }

    fn begin_terminal_selection(
        interaction: &mut TerminalInteraction,
        terminal: &mut Terminal,
        x: f32,
        y: f32,
    ) {
        // Each helper press is a fresh single click, not a continuation of the last one.
        interaction.click_counter = ClickCounter::default();
        interaction.layout = mouse_test_layout(0.0);
        interaction.mouse_x = x;
        interaction.mouse_y = y;
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            terminal,
            |_, _, _| 0,
        ));
        assert!(interaction.terminal_selection_active());
    }

    /// Writes raw bytes into the test terminal's grid, like PTY output would.
    fn feed_test_text(terminal: &Terminal, bytes: &[u8]) {
        crate::platform::lock_recover(&terminal.grid).advance(
            bytes,
            Instant::now(),
            &mut crate::terminal_process::SessionSink {
                reply_tx: None,
                title_cache: None,
                pending: None,
                wake: None,
            },
        );
    }

    /// Pointer position at the middle of cell (`col`, `row`) under `mouse_test_layout(0.0)`.
    fn cell_point(col: usize, row: usize) -> (f32, f32) {
        (8.0 + 10.0 * col as f32 + 5.0, 107.0 + 20.0 * row as f32)
    }

    fn grid_selection(terminal: &Terminal) -> Option<SelectionSpan> {
        crate::platform::lock_recover(&terminal.grid).selection()
    }

    #[test]
    fn word_extent_stops_at_separators_and_keeps_wide_cells_whole() {
        let terminal = selection_test_terminal();
        feed_test_text(
            &terminal,
            "\x1b[1;1Halpha|beta:gamma\x1b[2;1Hx中y z\x1b[3;1Ha,b`c|d:e\"f'g(h)i[j]k{l}m<n>o"
                .as_bytes(),
        );
        let grid = crate::platform::lock_recover(&terminal.grid);
        assert_eq!(word_extent(&grid, (1, 0)), ((0, 0), (4, 0)));
        assert_eq!(word_extent(&grid, (5, 0)), ((5, 0), (5, 0)));
        assert_eq!(word_extent(&grid, (7, 0)), ((6, 0), (9, 0)));
        assert_eq!(word_extent(&grid, (10, 0)), ((10, 0), (10, 0)));
        assert_eq!(word_extent(&grid, (12, 0)), ((11, 0), (15, 0)));
        // Wide cell at column 1 with its spacer at column 2: the whole "x中y" is one word.
        assert_eq!(word_extent(&grid, (1, 1)), ((0, 1), (3, 1)));
        assert_eq!(word_extent(&grid, (2, 1)), ((0, 1), (3, 1)));
        assert_eq!(word_extent(&grid, (4, 1)), ((4, 1), (4, 1)));
        assert_eq!(word_extent(&grid, (5, 1)), ((5, 1), (5, 1)));
        // Every character of the third line is a separator or a one-letter word.
        for col in 0..=28 {
            assert_eq!(word_extent(&grid, (col, 2)), ((col, 2), (col, 2)), "col {col}");
        }
    }

    #[test]
    fn word_extent_crosses_soft_wrap() {
        let terminal = selection_test_terminal();
        let text = format!("\x1b[1;1H{}abcd ef\r\n", "x".repeat(78));
        feed_test_text(&terminal, text.as_bytes());
        let grid = crate::platform::lock_recover(&terminal.grid);
        // "xxx...xxxab" wraps, so "ab" and "cd" are one word.
        assert_eq!(word_extent(&grid, (79, 0)), ((0, 0), (1, 1)));
        assert_eq!(word_extent(&grid, (1, 1)), ((0, 0), (1, 1)));
        assert_eq!(word_extent(&grid, (3, 1)), ((3, 1), (4, 1)));
    }

    #[test]
    fn line_extent_includes_soft_wrapped_rows() {
        let terminal = selection_test_terminal();
        let text = format!("\x1b[1;1H{}abcd ef\r\nnext", "x".repeat(78));
        feed_test_text(&terminal, text.as_bytes());
        let grid = crate::platform::lock_recover(&terminal.grid);
        assert_eq!(line_extent(&grid, 1), (0, 0, 4, 1));
        assert_eq!(line_extent(&grid, 2), (0, 2, 3, 2));
    }

    #[test]
    fn triple_click_selects_soft_wrapped_logical_line() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        let text = format!("\x1b[1;1H{}abcd ef\r\nnext", "x".repeat(78));
        feed_test_text(&terminal, text.as_bytes());
        interaction.layout = mouse_test_layout(0.0);
        interaction.begin_terminal_selection_at(&mut terminal, (2, 1), 3);
        assert_eq!(grid_selection(&terminal), Some((0, 0, 4, 1)));
    }

    #[test]
    fn triple_click_on_line_ending_with_wide_char_covers_its_spacer() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        let text = format!("\x1b[1;1H{}中", "x".repeat(78));
        feed_test_text(&terminal, text.as_bytes());
        interaction.layout = mouse_test_layout(0.0);
        interaction.begin_terminal_selection_at(&mut terminal, (2, 0), 3);
        // The wide cell occupies columns 78-79, so its spacer at 79 ends the line.
        assert_eq!(grid_selection(&terminal), Some((0, 0, 79, 0)));
    }

    #[test]
    fn triple_click_drag_extends_selection_by_whole_lines_forward() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        feed_test_text(&terminal, b"\x1b[1;1Halpha\x1b[2;1Hbeta gamma");
        interaction.layout = mouse_test_layout(0.0);
        interaction.begin_terminal_selection_at(&mut terminal, (1, 0), 3);
        assert_eq!(grid_selection(&terminal), Some((0, 0, 4, 0)));

        let (x, y) = cell_point(2, 1);
        assert!(interaction.cursor_moved(x, y, &mut terminal, |_, _, _| 0));
        assert_eq!(grid_selection(&terminal), Some((0, 0, 9, 1)));

        // Back onto the anchor line keeps the anchor line selected.
        let (x, y) = cell_point(2, 0);
        assert!(interaction.cursor_moved(x, y, &mut terminal, |_, _, _| 0));
        assert_eq!(grid_selection(&terminal), Some((0, 0, 4, 0)));
    }

    #[test]
    fn triple_click_drag_extends_selection_by_whole_lines_backward() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        feed_test_text(&terminal, b"\x1b[1;1Halpha\x1b[2;1Hbeta gamma");
        interaction.layout = mouse_test_layout(0.0);
        interaction.begin_terminal_selection_at(&mut terminal, (1, 1), 3);
        assert_eq!(grid_selection(&terminal), Some((0, 1, 9, 1)));

        let (x, y) = cell_point(2, 0);
        assert!(interaction.cursor_moved(x, y, &mut terminal, |_, _, _| 0));
        assert_eq!(grid_selection(&terminal), Some((0, 0, 9, 1)));
    }

    #[test]
    fn click_counter_counts_fast_presses_in_one_cell() {
        let start = Instant::now();
        let mut counter = ClickCounter::default();
        let cell = Some((3, 4));
        assert_eq!(counter.register(start, cell), 1);
        assert_eq!(counter.register(start + Duration::from_millis(299), cell), 2);
        assert_eq!(counter.register(start + Duration::from_millis(598), cell), 3);
        // The fourth press starts over at 1.
        assert_eq!(counter.register(start + Duration::from_millis(897), cell), 1);
        // A press in another cell starts over.
        assert_eq!(
            counter.register(start + Duration::from_millis(1000), Some((4, 4))),
            1
        );
        assert_eq!(
            counter.register(start + Duration::from_millis(1100), Some((4, 4))),
            2
        );
        // A press more than 300 ms after the previous one starts over.
        assert_eq!(
            counter.register(start + Duration::from_millis(1401), Some((4, 4))),
            1
        );
    }

    #[test]
    fn word_drag_extends_selection_by_whole_words_both_ways() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        feed_test_text(&terminal, b"\x1b[1;1Halpha|beta:gamma");
        interaction.layout = mouse_test_layout(0.0);
        interaction.begin_terminal_selection_at(&mut terminal, (7, 0), 2);
        assert_eq!(grid_selection(&terminal), Some((6, 0, 9, 0)));

        let (x, y) = cell_point(12, 0);
        assert!(interaction.cursor_moved(x, y, &mut terminal, |_, _, _| 0));
        assert_eq!(grid_selection(&terminal), Some((6, 0, 15, 0)));

        let (x, y) = cell_point(1, 0);
        assert!(interaction.cursor_moved(x, y, &mut terminal, |_, _, _| 0));
        assert_eq!(grid_selection(&terminal), Some((0, 0, 9, 0)));
    }

    #[test]
    fn double_click_on_separator_selects_one_cell_and_keeps_it_on_release() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        feed_test_text(&terminal, b"\x1b[1;1Halpha|beta");
        interaction.layout = mouse_test_layout(0.0);
        interaction.begin_terminal_selection_at(&mut terminal, (5, 0), 2);
        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert_eq!(grid_selection(&terminal), Some((5, 0, 5, 0)));
    }

    #[test]
    fn ctrl_click_as_second_click_on_link_selects_word_without_opening_link() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(false);
        interaction.layout = mouse_test_layout(0.0);
        interaction.set_modifiers(Modifiers::CONTROL);
        // An earlier press in the same cell makes this press the second click.
        interaction
            .click_counter
            .register(Instant::now(), Some((6, 29)));
        interaction.mouse_x = 73.0;
        interaction.mouse_y = 682.0;
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(interaction.take_link_uri().is_none());
        // "https" on the URL row is cols 4..=8, followed by the separator ':'.
        assert_eq!(grid_selection(&terminal), Some((4, 29, 8, 29)));
    }

    #[test]
    fn ctrl_triple_click_on_link_selects_line_without_opening_link() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = link_test_terminal(false);
        interaction.layout = mouse_test_layout(0.0);
        interaction.set_modifiers(Modifiers::CONTROL);
        // Two earlier presses in the same cell make this press the third click.
        interaction
            .click_counter
            .register(Instant::now(), Some((6, 29)));
        interaction
            .click_counter
            .register(Instant::now(), Some((6, 29)));
        interaction.mouse_x = 73.0;
        interaction.mouse_y = 682.0;
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(interaction.take_link_uri().is_none());
        // "see https://example.com/x" is 25 cells, so the line ends at column 24.
        assert_eq!(grid_selection(&terminal), Some((0, 29, 24, 29)));
    }

    #[test]
    fn terminal_selection_geometry_clamps_outside_body_to_grid_edges() {
        let terminal = selection_test_terminal();
        let layout = mouse_test_layout(0.0);
        let grid = crate::platform::lock_recover(&terminal.grid);

        assert_eq!(
            terminal_cell_from_grid(layout, &grid, -50.0, 400.0, 0.0, true),
            Some((0, 15)),
        );
        assert_eq!(
            terminal_cell_from_grid(layout, &grid, 900.0, 400.0, 0.0, true),
            Some((79, 15)),
        );
        assert_eq!(
            terminal_cell_from_grid(layout, &grid, 400.0, 20.0, 0.0, true),
            Some((39, 0)),
        );
        assert_eq!(
            terminal_cell_from_grid(layout, &grid, 400.0, 760.0, 0.0, true),
            Some((39, 29)),
        );
    }

    #[test]
    fn cursor_leave_projection_crosses_nearest_window_edge_only_for_active_selection() {
        assert_eq!(
            project_cursor_outside_window_on_leave(400.0, 599.0, 800.0, 600.0),
            (400.0, 601.0),
        );
        assert_eq!(
            project_cursor_outside_window_on_leave(400.0, 1.0, 800.0, 600.0),
            (400.0, -1.0),
        );
        assert_eq!(
            project_cursor_outside_window_on_leave(1.0, 300.0, 800.0, 600.0),
            (-1.0, 300.0),
        );
        assert_eq!(
            project_cursor_outside_window_on_leave(799.0, 300.0, 800.0, 600.0),
            (801.0, 300.0),
        );

        let mut interaction = TerminalInteraction::default();
        interaction.mouse_x = 400.0;
        interaction.mouse_y = 599.0;
        let mut terminal = selection_test_terminal();
        assert!(!interaction.cursor_left(800.0, 600.0, &mut terminal));
        assert_eq!((interaction.mouse_x, interaction.mouse_y), (400.0, 599.0));
    }

    #[test]
    fn selection_autoscroll_uses_outside_distance_real_dt_and_bounded_speed() {
        assert_eq!(selection_drag_autoscroll_delta(100.0, 100.0, 700.0), 0.0);
        assert_eq!(selection_drag_autoscroll_delta(99.0, 100.0, 700.0), -1.0);
        assert_eq!(selection_drag_autoscroll_delta(740.0, 100.0, 700.0), 40.0);
        assert!(
            crate::scroll::drag_autoscroll_speed(-30.0, true)
                > crate::scroll::drag_autoscroll_speed(30.0, false)
        );
        assert!(
            crate::scroll::drag_autoscroll_speed(3000.0, false)
                <= crate::scroll::drag_autoscroll_speed(3000.0, true)
        );

        let run = |dt: f32| {
            let mut interaction = TerminalInteraction::default();
            let mut terminal = selection_test_terminal();
            interaction.layout = mouse_test_layout(0.0);
            interaction.layout.max_scroll = 1000.0;
            interaction.pointer_capture = PointerCapture::TerminalSelection;
            interaction.terminal_selection_anchor = Some((10, 15));
            interaction.mouse_x = 100.0;
            interaction.mouse_y = 50.0;
            terminal.scroll_y.current = 500.0;
            terminal.scroll_y.target = 500.0;
            assert!(interaction.update_selection_autoscroll(dt, &mut terminal));
            terminal.scroll_y.target - 500.0
        };
        let one = run(0.01);
        let two = run(0.02);
        assert!((two - one * 2.0).abs() < 0.01);
    }

    #[test]
    fn selection_autoscroll_clamps_at_history_bounds_without_permanent_animation() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        interaction.layout = mouse_test_layout(0.0);
        interaction.layout.max_scroll = 1000.0;
        interaction.pointer_capture = PointerCapture::TerminalSelection;
        interaction.terminal_selection_anchor = Some((10, 15));
        interaction.mouse_x = 100.0;

        interaction.mouse_y = 50.0;
        terminal.scroll_y.current = 995.0;
        terminal.scroll_y.target = 995.0;
        assert!(interaction.update_selection_autoscroll(1.0, &mut terminal));
        assert_eq!(terminal.scroll_y.target, 1000.0);
        terminal.scroll_y.current = 1000.0;
        assert!(!interaction.animation_active(&terminal));

        interaction.mouse_y = 750.0;
        terminal.scroll_y.current = 5.0;
        terminal.scroll_y.target = 5.0;
        assert!(interaction.update_selection_autoscroll(1.0, &mut terminal));
        assert_eq!(terminal.scroll_y.target, 0.0);
        terminal.scroll_y.current = 0.0;
        assert!(!interaction.animation_active(&terminal));
    }

    #[test]
    fn single_click_clears_selection_while_drag_keeps_nonempty_selection() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        crate::platform::lock_recover(&terminal.grid).set_selection(Some((1, 1, 4, 1)));
        assert!(interaction.clear_text_selection(&mut terminal));

        begin_terminal_selection(&mut interaction, &mut terminal, 25.0, 400.0);
        assert!(
            crate::platform::lock_recover(&terminal.grid)
                .selection()
                .is_none()
        );
        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(
            crate::platform::lock_recover(&terminal.grid)
                .selection()
                .is_none()
        );

        begin_terminal_selection(&mut interaction, &mut terminal, 25.0, 400.0);
        assert!(interaction.cursor_moved(75.0, 400.0, &mut terminal, |_, _, _| 0));
        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        let selection = crate::platform::lock_recover(&terminal.grid).selection();
        assert!(selection.is_some_and(|(sx, sy, ex, ey)| sx != ex || sy != ey));
    }

    #[test]
    fn cursor_left_top_and_bottom_autoscroll_continue_selection_and_return_uses_real_position() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        interaction.layout = mouse_test_layout(0.0);
        interaction.layout.max_scroll = 1000.0;
        terminal.scroll_y.current = 500.0;
        terminal.scroll_y.target = 500.0;

        begin_terminal_selection(&mut interaction, &mut terminal, 100.0, 400.0);
        interaction.layout.max_scroll = 1000.0;
        interaction.mouse_x = 400.0;
        interaction.mouse_y = 1.0;
        assert!(interaction.cursor_left(800.0, 600.0, &mut terminal));
        assert_eq!(interaction.mouse_y, -1.0);
        assert!(interaction.update_selection_autoscroll(0.016, &mut terminal));
        assert!(terminal.scroll_y.target > 500.0);
        assert!(
            crate::platform::lock_recover(&terminal.grid)
                .selection()
                .is_some()
        );

        let _ = interaction.cursor_moved(400.0, 300.0, &mut terminal, |_, _, _| 0);
        assert_eq!((interaction.mouse_x, interaction.mouse_y), (400.0, 300.0));

        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        interaction.layout = mouse_test_layout(0.0);
        interaction.layout.max_scroll = 1000.0;
        terminal.scroll_y.current = 500.0;
        terminal.scroll_y.target = 500.0;

        begin_terminal_selection(&mut interaction, &mut terminal, 100.0, 400.0);
        interaction.layout.max_scroll = 1000.0;
        interaction.mouse_x = 400.0;
        interaction.mouse_y = 699.0;
        assert!(interaction.cursor_left(800.0, 700.0, &mut terminal));
        assert_eq!(interaction.mouse_y, 701.0);
        assert!(interaction.update_selection_autoscroll(0.016, &mut terminal));
        assert!(terminal.scroll_y.target < 500.0);
        assert!(
            crate::platform::lock_recover(&terminal.grid)
                .selection()
                .is_some()
        );
    }

    #[test]
    fn release_and_cancellation_stop_selection_autoscroll() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        interaction.layout = mouse_test_layout(0.0);
        interaction.layout.max_scroll = 1000.0;
        terminal.scroll_y.current = 500.0;
        terminal.scroll_y.target = 500.0;
        interaction.pointer_capture = PointerCapture::TerminalSelection;
        interaction.terminal_selection_anchor = Some((2, 2));
        interaction.mouse_y = 50.0;
        assert!(interaction.animation_active(&terminal));

        assert!(interaction.mouse_input(
            KeyState::Released,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        assert!(!interaction.terminal_selection_active());
        assert!(!interaction.animation_active(&terminal));

        interaction.pointer_capture = PointerCapture::TerminalSelection;
        interaction.terminal_selection_anchor = Some((2, 2));
        interaction.cancel_pointer_interaction(&mut terminal);
        assert!(!interaction.terminal_selection_active());
        assert!(!interaction.animation_active(&terminal));
    }

    #[test]
    fn tracked_mouse_press_still_reports_after_stale_selection_is_cleared() {
        let mut interaction = TerminalInteraction::default();
        let mut terminal = selection_test_terminal();
        interaction.layout = mouse_test_layout(0.0);
        interaction.mouse_x = 25.0;
        interaction.mouse_y = 400.0;
        {
            let mut term = crate::platform::lock_recover(&terminal.grid);
            term.set_selection(Some((1, 1, 4, 1)));
            term.advance(
                b"\x1b[?1000h\x1b[?1006h",
                std::time::Instant::now(),
                &mut crate::terminal_process::SessionSink {
                    reply_tx: None,
                    title_cache: None,
                    pending: None,
                    wake: None,
                },
            );
        }
        assert!(interaction.clear_text_selection(&mut terminal));
        assert!(interaction.mouse_input(
            KeyState::Pressed,
            PointerButton::Left,
            &mut terminal,
            |_, _, _| 0,
        ));
        let term = crate::platform::lock_recover(&terminal.grid);
        assert!(term.selection().is_none());
        assert_eq!(term.mouse_tracking(), MouseTrackingMode::Press);
    }

    #[test]
    fn terminal_body_click_defocuses_search_without_closing_it() {
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search_owns_keyboard(&search));

        terminal_body_takes_search_focus(&mut search);

        assert!(search.shown);
        assert!(!search.focused);
        assert!(!search_owns_keyboard(&search));
        assert_eq!(search.text, "needle");
        assert_eq!(
            terminal_text_sequence("a", false, false, false),
            Some(b"a".to_vec())
        );
        assert_eq!(search.text, "needle");
    }

    #[test]
    fn search_mouse_drag_selection_uses_unicode_character_indices_both_directions() {
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("ab界cd");

        begin_search_pointer_selection(&mut search, 1);
        assert!(update_search_pointer_selection(
            &mut search,
            PointerCapture::SearchInput,
            3,
        ));
        assert_eq!(search.selected_text().as_deref(), Some("b界"));

        begin_search_pointer_selection(&mut search, 3);
        assert!(update_search_pointer_selection(
            &mut search,
            PointerCapture::SearchInput,
            1,
        ));
        assert_eq!(search.selected_text().as_deref(), Some("b界"));

        let mut capture = PointerCapture::SearchInput;
        assert_eq!(release_pointer_capture(&mut capture), PointerCapture::SearchInput);
        assert_eq!(capture, PointerCapture::None);
        let before = (search.cursor, search.selection_anchor);
        assert!(!update_search_pointer_selection(&mut search, capture, 5));
        assert_eq!((search.cursor, search.selection_anchor), before);
    }

    #[test]
    fn ui_pointer_capture_blocks_sgr_press_and_release_packets() {
        let layout = mouse_test_layout(0.0);
        let x = layout.text_x + layout.char_w * 2.5;
        let y = layout.body.y + 100.0;
        for capture in [
            PointerCapture::TerminalScrollbar,
            PointerCapture::SearchInput,
            PointerCapture::SearchControl,
        ] {
            assert!(terminal_mouse_event_sequence(
                layout,
                capture,
                MouseTrackingMode::Press,
                true,
                KeyState::Pressed,
                PointerButton::Left,
                x,
                y,
            )
            .is_none());
            assert!(terminal_mouse_event_sequence(
                layout,
                capture,
                MouseTrackingMode::Press,
                true,
                KeyState::Released,
                PointerButton::Left,
                x,
                y,
            )
            .is_none());
        }

        let press = terminal_mouse_event_sequence(
            layout,
            PointerCapture::None,
            MouseTrackingMode::Press,
            true,
            KeyState::Pressed,
            PointerButton::Left,
            x,
            y,
        )
        .unwrap();
        let release = terminal_mouse_event_sequence(
            layout,
            PointerCapture::None,
            MouseTrackingMode::Press,
            true,
            KeyState::Released,
            PointerButton::Left,
            x,
            y,
        )
        .unwrap();
        assert!(press.ends_with(b"M"));
        assert!(release.ends_with(b"m"));
    }

    #[test]
    fn terminal_mouse_x_clamps_to_actual_grid_columns_not_body_padding() {
        let layout = mouse_test_layout(0.0);
        let first =
            terminal_mouse_protocol_cell(layout, layout.text_x, layout.body.y + 100.0).unwrap();
        assert_eq!(first.0, 1);
        let right_padding = terminal_mouse_protocol_cell(
            layout,
            layout.body.x + layout.body.w - 1.0,
            layout.body.y + 100.0,
        )
        .unwrap();
        assert_eq!(right_padding.0, 80);
    }

    #[test]
    fn unsupported_mouse_buttons_never_become_left_click_or_pressed_state() {
        let layout = mouse_test_layout(0.0);
        let x = layout.text_x + 5.0;
        let y = layout.body.y + 100.0;
        for button in [PointerButton::Back, PointerButton::Forward, PointerButton::Other(9)] {
            assert_eq!(terminal_mouse_button(button), None);
            assert!(terminal_mouse_event_sequence(
                layout,
                PointerCapture::None,
                MouseTrackingMode::Press,
                true,
                KeyState::Pressed,
                button,
                x,
                y,
            )
            .is_none());
            let mut pressed = 0u8;
            update_pressed_mouse_buttons(&mut pressed, button, KeyState::Pressed);
            assert_eq!(pressed, 0);
        }
    }

    #[test]
    fn ronsole_overlays_own_mouse_motion_and_wheel_before_terminal_protocol() {
        let mut layout = mouse_test_layout(0.0);
        let window_body = crate::search::Rect {
            x: 0.0,
            y: layout.body.y,
            w: 800.0,
            h: 600.0,
        };
        let search = crate::search::terminal_search_geometry(window_body, 1.0);
        layout.search = Some(search);
        layout.scrollbar = Some(Default::default());
        let scrollbar = layout.scrollbar.as_mut().unwrap();
        scrollbar.track = crate::search::Rect {
            x: 780.0,
            y: 100.0,
            w: 12.0,
            h: 600.0,
        };
        scrollbar.thumb = crate::search::Rect {
            x: 780.0,
            y: 200.0,
            w: 12.0,
            h: 80.0,
        };
        scrollbar.max_scroll = 1000.0;

        for (x, y) in [
            (search.input.x + 2.0, search.input.y + 2.0),
            (search.close.x + 2.0, search.close.y + 2.0),
            (search.outer.x + 2.0, search.outer.y + search.outer.h - 2.0),
            (786.0, 400.0),
        ] {
            assert!(terminal_mouse_motion_sequence(
                layout,
                MouseTrackingMode::AnyMotion,
                true,
                0,
                x,
                y,
            )
            .is_none());
            assert!(terminal_mouse_wheel_sequence(
                layout,
                MouseTrackingMode::AnyMotion,
                true,
                x,
                y,
                40.0,
            )
            .is_none());
        }

        let body_x = layout.text_x + 20.0;
        let body_y = layout.body.y + 200.0;
        assert!(terminal_mouse_motion_sequence(
            layout,
            MouseTrackingMode::AnyMotion,
            true,
            0,
            body_x,
            body_y,
        )
        .is_some());
        assert!(terminal_mouse_wheel_sequence(
            layout,
            MouseTrackingMode::AnyMotion,
            true,
            body_x,
            body_y,
            40.0,
        )
        .is_some());

        layout.search = None;
        let formerly_covered = (search.input.x + 2.0, search.input.y + 2.0);
        assert!(terminal_mouse_motion_sequence(
            layout,
            MouseTrackingMode::AnyMotion,
            true,
            0,
            formerly_covered.0,
            formerly_covered.1,
        )
        .is_some());
    }

    #[test]
    fn sgr_mouse_y_is_visible_viewport_coordinate_even_with_scrollback() {
        for scroll_offset in [0.0, 7.0, 83.0] {
            let layout = mouse_test_layout(scroll_offset);
            let top_center = layout.body.y + layout.body.h - layout.bottom_pad
                - layout.visible_rows as f32 * layout.char_h
                + layout.char_h * 0.5
                + scroll_offset;
            let bottom_center = layout.body.y + layout.body.h - layout.bottom_pad
                - layout.char_h * 0.5
                + scroll_offset;
            assert_eq!(terminal_mouse_cell_y(layout, top_center), 1);
            assert_eq!(terminal_mouse_cell_y(layout, bottom_center), layout.visible_rows);

            let middle_y = top_center + 12.0 * layout.char_h;
            let (_, row) = terminal_mouse_protocol_cell(
                layout,
                layout.text_x + 5.0,
                middle_y,
            )
            .unwrap();
            assert_eq!(row, 13);
        }
    }
}
