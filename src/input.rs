use crate::platform::Clipboard;
use crate::renderer::{TerminalUiLayout, terminal_scrollbar_drag_target};
use crate::search::{SearchRefreshCause, TerminalSearchState};
use crate::terminal::{MouseTrackingMode, Terminal};
use std::path::{Path, PathBuf};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};

pub(crate) struct TerminalInteraction {
    pub search: TerminalSearchState,
    pub layout: TerminalUiLayout,
    modifiers: ModifiersState,
    clipboard: Option<Clipboard>,
    mouse_x: f32,
    mouse_y: f32,
    pressed_mouse_buttons: u8,
    pointer_capture: PointerCapture,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PointerCapture {
    #[default]
    None,
    TerminalSelection,
    TerminalScrollbar,
    SearchInput,
    SearchControl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalCtrlCAction {
    CopySelection,
    SendInterrupt,
}

#[inline]
fn terminal_ctrl_c_action(has_selection: bool) -> TerminalCtrlCAction {
    if has_selection {
        TerminalCtrlCAction::CopySelection
    } else {
        TerminalCtrlCAction::SendInterrupt
    }
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
            modifiers: ModifiersState::empty(),
            clipboard: None,
            mouse_x: 0.0,
            mouse_y: 0.0,
            pressed_mouse_buttons: 0,
            pointer_capture: PointerCapture::None,
        }
    }
}

impl TerminalInteraction {
    pub(crate) fn set_modifiers(&mut self, modifiers: ModifiersState) {
        self.modifiers = modifiers;
    }

    #[cfg(test)]
    pub(crate) fn modifiers_for_test(&self) -> ModifiersState {
        self.modifiers
    }

    pub(crate) fn update_layout(&mut self, layout: TerminalUiLayout) {
        self.layout = layout;
    }

    pub(crate) fn animation_active(&self, terminal: &Terminal) -> bool {
        !terminal.scroll_y.is_settled()
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
        self.pressed_mouse_buttons = 0;
        self.pointer_capture = PointerCapture::None;
    }

    pub(crate) fn cancel_pointer_interaction(&mut self, terminal: &mut Terminal) {
        self.cancel_pointer_interaction_state(&mut terminal.scroll_y);
    }

    fn cancel_pointer_interaction_state(&mut self, scroll: &mut crate::scroll::ScrollState) {
        self.pressed_mouse_buttons = 0;
        let capture = release_pointer_capture(&mut self.pointer_capture);
        if capture == PointerCapture::TerminalScrollbar {
            scroll.end_drag();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_pressed_mouse_buttons_for_test(&mut self, buttons: u8) {
        self.pressed_mouse_buttons = buttons;
    }

    #[cfg(test)]
    pub(crate) fn pressed_mouse_buttons_for_test(&self) -> u8 {
        self.pressed_mouse_buttons
    }

    fn clipboard(&mut self) -> Option<&mut Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = Clipboard::new().ok();
        }
        self.clipboard.as_mut()
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
        let total_lines = if grid.is_alt {
            grid.lines.len()
        } else {
            grid.scrollback.len() + grid.lines.len()
        };
        let offset_from_bottom = total_lines.saturating_sub(1).saturating_sub(found.y);
        let target = offset_from_bottom as f32 * self.layout.char_h;
        drop(grid);
        terminal
            .scroll_y
            .animate_to(target.clamp(0.0, self.layout.max_scroll));
    }

    fn close_search(&mut self, terminal: &mut Terminal) {
        self.search.close();
        crate::platform::lock_recover(&terminal.grid).selection = None;
    }

    fn handle_search_key(&mut self, key_event: &KeyEvent, terminal: &mut Terminal) -> bool {
        if !search_owns_keyboard(&self.search) || key_event.state != ElementState::Pressed {
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
        let super_key = self.modifiers.super_key();
        let mut edited = false;
        match key_event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => self.close_search(terminal),
            PhysicalKey::Code(KeyCode::Enter) => {
                if shift { self.search.previous(); } else { self.search.next(); }
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
                let end = self.search.query.chars().count();
                self.search.move_cursor(end, shift);
            }
            PhysicalKey::Code(KeyCode::Backspace) => { self.search.backspace(); edited = true; }
            PhysicalKey::Code(KeyCode::Delete) => { self.search.delete_forward(); edited = true; }
            PhysicalKey::Code(KeyCode::KeyA) if ctrl => self.search.select_all(),
            PhysicalKey::Code(KeyCode::KeyC) if ctrl => {
                if let Some(text) = self.search.selected_text()
                    && let Some(clipboard) = self.clipboard()
                {
                    let _ = clipboard.set_text(text);
                }
            }
            PhysicalKey::Code(KeyCode::KeyX) if ctrl => {
                if let Some(text) = self.search.selected_text()
                    && let Some(clipboard) = self.clipboard()
                {
                    let _ = clipboard.set_text(text);
                    self.search.insert_text("");
                    edited = true;
                }
            }
            PhysicalKey::Code(KeyCode::KeyV) if ctrl => {
                if let Some(text) = self.clipboard().and_then(|clipboard| clipboard.get_text().ok()) {
                    let clean = text.replace(['\r', '\n'], "");
                    if !clean.is_empty() {
                        self.search.insert_text(&clean);
                        edited = true;
                    }
                }
            }
            _ if !ctrl && !super_key => {
                if let Some(text) = key_event.logical_key.to_text()
                    && !text.contains(['\r', '\n'])
                {
                    self.search.insert_text(text);
                    edited = true;
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
            let _ = terminal.write_input(text.as_bytes());
            return true;
        }
        false
    }

    pub(crate) fn handle_key_event(&mut self, key_event: &KeyEvent, terminal: &mut Terminal) -> bool {
        if self.handle_search_key(key_event, terminal) {
            return true;
        }
        if key_event.state != ElementState::Pressed {
            return false;
        }
        let ctrl = self.modifiers.control_key();
        let shift = self.modifiers.shift_key();
        let alt = self.modifiers.alt_key();
        let super_key = self.modifiers.super_key();

        if ctrl && key_event.physical_key == PhysicalKey::Code(KeyCode::KeyV) {
            let bracketed_paste = crate::platform::lock_recover(&terminal.grid).bracketed_paste;
            let file_list = self.clipboard().and_then(|clipboard| clipboard.get_file_list().ok());
            let paste = terminal_clipboard_paste_bytes(file_list.as_deref(), None).or_else(|| {
                self.clipboard()
                    .and_then(|clipboard| clipboard.get_text().ok())
                    .and_then(|text| terminal_clipboard_paste_bytes(None, Some(&text)))
            });
            if let Some(bytes) = paste {
                let bytes = terminal_bracketed_paste_bytes(bytes, bracketed_paste);
                let _ = terminal.write_input(&bytes);
            }
            return true;
        }

        let mut grid = crate::platform::lock_recover(&terminal.grid);
        if ctrl && key_event.physical_key == PhysicalKey::Code(KeyCode::KeyC) {
            match terminal_ctrl_c_action(grid.selection.is_some()) {
                TerminalCtrlCAction::CopySelection => {
                    let text = grid.get_selection_text();
                    grid.selection = None;
                    drop(grid);
                    if !text.is_empty() && let Some(clipboard) = self.clipboard() {
                        let _ = clipboard.set_text(text);
                    }
                }
                TerminalCtrlCAction::SendInterrupt => {
                    drop(grid);
                    let _ = terminal.write_input(&[0x03]);
                }
            }
            return true;
        }
        let app_cursor = grid.app_cursor_keys;
        drop(grid);
        if let Some(bytes) = terminal_key_sequence(
            key_event.physical_key,
            key_event.logical_key.to_text(),
            shift,
            ctrl,
            alt,
            super_key,
            app_cursor,
        ) {
            let _ = terminal.write_input(&bytes);
            return true;
        }
        false
    }

    pub(crate) fn cursor_moved(
        &mut self,
        x: f32,
        y: f32,
        terminal: &mut Terminal,
        search_cursor_from_x: impl FnOnce(&str, f32, f32) -> usize,
    ) -> bool {
        self.mouse_x = x;
        self.mouse_y = y;
        match self.pointer_capture {
            PointerCapture::TerminalSelection => {
                if let Some((cell_x, cell_y)) = terminal_cell_at(self.layout, terminal, x, y) {
                    let mut grid = crate::platform::lock_recover(&terminal.grid);
                    if let Some((sx, sy, _, _)) = grid.selection {
                        let next = (sx, sy, cell_x, cell_y);
                        if grid.selection != Some(next) {
                            grid.selection = Some(next);
                            return true;
                        }
                    }
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
                        search_cursor_from_x(&self.search.query, local_x, self.search.scroll_x);
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
                (grid.mouse_tracking_mode, grid.mouse_sgr)
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
        state: ElementState,
        button: MouseButton,
        terminal: &mut Terminal,
        search_cursor_from_x: impl FnOnce(&str, f32, f32) -> usize,
    ) -> bool {
        let x = self.mouse_x;
        let y = self.mouse_y;
        update_pressed_mouse_buttons(&mut self.pressed_mouse_buttons, button, state);

        if state == ElementState::Released && self.pointer_capture != PointerCapture::None {
            let capture = release_pointer_capture(&mut self.pointer_capture);
            if capture == PointerCapture::TerminalScrollbar {
                terminal.scroll_y.end_drag();
            }
            return true;
        }

        if let Some(search) = self.layout.search
            && state == ElementState::Pressed
            && button == MouseButton::Left
        {
            if search.close.contains(x, y) {
                self.pointer_capture = PointerCapture::SearchControl;
                self.close_search(terminal);
                return true;
            }
            if search.next.contains(x, y) {
                self.pointer_capture = PointerCapture::SearchControl;
                self.search.next();
                self.jump_to_search_match(terminal);
                return true;
            }
            if search.previous.contains(x, y) {
                self.pointer_capture = PointerCapture::SearchControl;
                self.search.previous();
                self.jump_to_search_match(terminal);
                return true;
            }
            if search.case_toggle.contains(x, y) {
                self.pointer_capture = PointerCapture::SearchControl;
                self.search.toggle_case();
                self.refresh_search(terminal, SearchRefreshCause::User);
                return true;
            }
            if search.input.contains(x, y) {
                let local_x = x - search.input.x - 5.0 * self.layout.scale;
                let cursor =
                    search_cursor_from_x(&self.search.query, local_x, self.search.scroll_x);
                begin_search_pointer_selection(&mut self.search, cursor);
                self.pointer_capture = PointerCapture::SearchInput;
                return true;
            }
            if search.outer.contains(x, y) {
                self.pointer_capture = PointerCapture::SearchControl;
                return true;
            }
        }

        if let Some(scrollbar) = self.layout.scrollbar
            && state == ElementState::Pressed
            && button == MouseButton::Left
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
        if state == ElementState::Pressed {
            terminal_body_takes_search_focus(&mut self.search);
        }

        let (tracking_mode, mouse_sgr) = {
            let mut grid = crate::platform::lock_recover(&terminal.grid);
            let tracking = grid.mouse_tracking_mode.enabled();
            clear_selection_for_tracked_mouse_press(&mut grid.selection, tracking, state);
            (grid.mouse_tracking_mode, grid.mouse_sgr)
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

        if !tracking && button == MouseButton::Left && state == ElementState::Pressed
            && let Some((cell_x, cell_y)) = terminal_cell_at(self.layout, terminal, x, y)
        {
            crate::platform::lock_recover(&terminal.grid).selection =
                Some((cell_x, cell_y, cell_x, cell_y));
            self.pointer_capture = PointerCapture::TerminalSelection;
            return true;
        }
        false
    }

    pub(crate) fn mouse_wheel(&mut self, delta: MouseScrollDelta, terminal: &mut Terminal) -> bool {
        if !self.layout.body.contains(self.mouse_x, self.mouse_y) { return false; }
        let dy = match delta {
            MouseScrollDelta::LineDelta(_, y) => y * 40.0,
            MouseScrollDelta::PixelDelta(position) => position.y as f32,
        };
        let grid = crate::platform::lock_recover(&terminal.grid);
        let is_alt = grid.is_alt;
        let tracking_mode = grid.mouse_tracking_mode;
        let mouse_sgr = grid.mouse_sgr;
        let app_cursor = grid.app_cursor_keys;
        drop(grid);
        if tracking_mode.enabled() {
            if let Some(input) = terminal_mouse_wheel_sequence(
                self.layout,
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
            let _ = terminal.write_input(&input);
            return true;
        }
        terminal.scroll_y.anim_speed = 7.0;
        terminal.scroll_y.scroll_by(dy);
        terminal.scroll_y.clamp_target(0.0, self.layout.max_scroll);
        true
    }
}

fn terminal_cell_at(
    layout: TerminalUiLayout,
    terminal: &Terminal,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    if layout.char_w <= 0.0 || layout.char_h <= 0.0 || !layout.body.contains(x, y) {
        return None;
    }
    let grid = crate::platform::lock_recover(&terminal.grid);
    let total_lines = if grid.is_alt { grid.lines.len() } else { grid.scrollback.len() + grid.lines.len() };
    if total_lines == 0 { return None; }
    let bottom_pad = layout.bottom_pad;
    let offset_from_bottom =
        (layout.body.y + layout.body.h - bottom_pad - y + layout.scroll_offset) / layout.char_h;
    let row = total_lines.saturating_sub(1).saturating_sub(offset_from_bottom.max(0.0).floor() as usize);
    let mut col = ((x - layout.text_x).max(0.0) / layout.char_w).floor() as usize;
    col = col.min(grid.cols.saturating_sub(1));
    let row = row.min(total_lines.saturating_sub(1));
    let row_cells = if grid.is_alt {
        grid.lines.get(row)
    } else if row < grid.scrollback.len() {
        grid.scrollback.get(row)
    } else {
        grid.lines.get(row - grid.scrollback.len())
    };
    if row_cells
        .and_then(|cells| cells.get(col))
        .is_some_and(crate::terminal::Cell::is_wide_spacer)
    {
        col = col.saturating_sub(1);
    }
    Some((col, row))
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
    modifiers: ModifiersState,
) -> bool {
    if !search_owns_keyboard(search) {
        return false;
    }
    let ctrl = modifiers.control_key();
    let shift = modifiers.shift_key();
    let alt = modifiers.alt_key();
    let super_key = modifiers.super_key();
    match physical_key {
        PhysicalKey::Code(KeyCode::F1) => !ctrl && !shift && !alt && !super_key,
        PhysicalKey::Code(KeyCode::KeyT) => ctrl && !shift && !alt && !super_key,
        _ => false,
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
        && !layout.search.is_some_and(|search| search.outer.contains(x, y))
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

#[inline]
fn clear_selection_for_tracked_mouse_press(
    selection: &mut Option<(usize, usize, usize, usize)>,
    tracking: bool,
    state: ElementState,
) {
    if tracking && state == ElementState::Pressed {
        *selection = None;
    }
}

fn terminal_mouse_event_sequence(
    layout: TerminalUiLayout,
    capture: PointerCapture,
    tracking: MouseTrackingMode,
    sgr: bool,
    state: ElementState,
    button: MouseButton,
    x: f32,
    y: f32,
) -> Option<Vec<u8>> {
    if capture != PointerCapture::None || !tracking.enabled() {
        return None;
    }
    let (cell_x, cell_y) = terminal_mouse_protocol_cell(layout, x, y)?;
    let code = terminal_mouse_button_code(button)?;
    Some(terminal_mouse_protocol_sequence(
        code,
        cell_x,
        cell_y,
        state == ElementState::Pressed,
        sgr,
    ))
}

fn terminal_mouse_motion_sequence(
    layout: TerminalUiLayout,
    tracking: MouseTrackingMode,
    sgr: bool,
    pressed_buttons: u8,
    x: f32,
    y: f32,
) -> Option<Vec<u8>> {
    let base_code = match tracking {
        MouseTrackingMode::AnyMotion if pressed_buttons == 0 => 3,
        MouseTrackingMode::AnyMotion | MouseTrackingMode::ButtonMotion => {
            terminal_first_pressed_button_code(pressed_buttons)?
        }
        MouseTrackingMode::None | MouseTrackingMode::Press => return None,
    };
    let (cell_x, cell_y) = terminal_mouse_protocol_cell(layout, x, y)?;
    Some(terminal_mouse_protocol_sequence(
        base_code + 32,
        cell_x,
        cell_y,
        true,
        sgr,
    ))
}

fn terminal_mouse_wheel_sequence(
    layout: TerminalUiLayout,
    tracking: MouseTrackingMode,
    sgr: bool,
    x: f32,
    y: f32,
    dy: f32,
) -> Option<Vec<u8>> {
    if !tracking.enabled() {
        return None;
    }
    let (cell_x, cell_y) = terminal_mouse_protocol_cell(layout, x, y)?;
    let steps = (dy.abs() / 20.0).max(1.0) as usize;
    let button = if dy > 0.0 { 64 } else { 65 };
    let sequence = terminal_mouse_protocol_sequence(button, cell_x, cell_y, true, sgr);
    let mut input = Vec::with_capacity(sequence.len().saturating_mul(steps.min(3)));
    for _ in 0..steps.min(3) {
        input.extend_from_slice(&sequence);
    }
    Some(input)
}

fn terminal_mouse_button_code(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => None,
    }
}

fn terminal_mouse_protocol_sequence(
    code: u8,
    x: usize,
    y: usize,
    pressed: bool,
    sgr: bool,
) -> Vec<u8> {
    if sgr {
        return format!("\x1b[<{code};{x};{y}{}", if pressed { 'M' } else { 'm' }).into_bytes();
    }
    let legacy_code = if pressed { code } else { 3 };
    vec![
        0x1b,
        b'[',
        b'M',
        legacy_code.saturating_add(32),
        (x.min(223) as u8).saturating_add(32),
        (y.min(223) as u8).saturating_add(32),
    ]
}

fn update_pressed_mouse_buttons(buttons: &mut u8, button: MouseButton, state: ElementState) {
    let Some(code) = terminal_mouse_button_code(button) else {
        return;
    };
    let bit = 1u8 << code;
    if state == ElementState::Pressed {
        *buttons |= bit;
    } else {
        *buttons &= !bit;
    }
}

fn terminal_first_pressed_button_code(buttons: u8) -> Option<u8> {
    if buttons & 1 != 0 {
        Some(0)
    } else if buttons & 2 != 0 {
        Some(1)
    } else if buttons & 4 != 0 {
        Some(2)
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

fn terminal_bracketed_paste_bytes(payload: Vec<u8>, enabled: bool) -> Vec<u8> {
    if !enabled {
        return payload;
    }
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    let mut out = Vec::with_capacity(START.len() + payload.len() + END.len());
    out.extend_from_slice(START);
    out.extend_from_slice(&payload);
    out.extend_from_slice(END);
    out
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

fn terminal_key_sequence(
    physical_key: PhysicalKey,
    logical_text: Option<&str>,
    shift: bool,
    ctrl: bool,
    alt: bool,
    super_key: bool,
    app_cursor_keys: bool,
) -> Option<Vec<u8>> {
    let seq = match physical_key {
        PhysicalKey::Code(KeyCode::Enter) | PhysicalKey::Code(KeyCode::NumpadEnter) => terminal_alt_prefixed(b"\r", alt),
        PhysicalKey::Code(KeyCode::Backspace) if ctrl => terminal_alt_prefixed(b"\x17", alt),
        PhysicalKey::Code(KeyCode::Backspace) if shift => terminal_alt_prefixed(b"\x08", alt),
        PhysicalKey::Code(KeyCode::Backspace) => terminal_alt_prefixed(b"\x7f", alt),
        PhysicalKey::Code(KeyCode::Tab) if shift => b"\x1b[Z".to_vec(),
        PhysicalKey::Code(KeyCode::Tab) => terminal_alt_prefixed(b"\t", alt),
        PhysicalKey::Code(KeyCode::Escape) => b"\x1b".to_vec(),
        PhysicalKey::Code(KeyCode::Insert) => terminal_tilde_key(2, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::Delete) => terminal_tilde_key(3, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::PageUp) => terminal_tilde_key(5, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::PageDown) => terminal_tilde_key(6, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::Home) => terminal_cursor_key(b'H', app_cursor_keys, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::End) => terminal_cursor_key(b'F', app_cursor_keys, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::ArrowUp) => terminal_cursor_key(b'A', app_cursor_keys, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::ArrowDown) => terminal_cursor_key(b'B', app_cursor_keys, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::ArrowRight) => terminal_cursor_key(b'C', app_cursor_keys, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::ArrowLeft) => terminal_cursor_key(b'D', app_cursor_keys, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F1) => terminal_function_key(11, b'P', shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F2) => terminal_function_key(12, b'Q', shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F3) => terminal_function_key(13, b'R', shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F4) => terminal_function_key(14, b'S', shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F5) => terminal_function_key(15, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F6) => terminal_function_key(17, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F7) => terminal_function_key(18, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F8) => terminal_function_key(19, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F9) => terminal_function_key(20, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F10) => terminal_function_key(21, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F11) => terminal_function_key(23, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::F12) => terminal_function_key(24, 0, shift, alt, ctrl),
        PhysicalKey::Code(KeyCode::Space) | PhysicalKey::Code(KeyCode::Digit2) if ctrl => terminal_alt_prefixed(b"\x00", alt),
        PhysicalKey::Code(KeyCode::Digit6) if ctrl => terminal_alt_prefixed(b"\x1e", alt),
        PhysicalKey::Code(KeyCode::Minus) | PhysicalKey::Code(KeyCode::Slash) if ctrl => terminal_alt_prefixed(b"\x1f", alt),
        PhysicalKey::Code(KeyCode::BracketLeft) if ctrl => terminal_alt_prefixed(b"\x1b", alt),
        PhysicalKey::Code(KeyCode::Backslash) if ctrl => terminal_alt_prefixed(b"\x1c", alt),
        PhysicalKey::Code(KeyCode::BracketRight) if ctrl => terminal_alt_prefixed(b"\x1d", alt),
        PhysicalKey::Code(code) if ctrl => {
            let control = match code {
                KeyCode::KeyA => 0x01, KeyCode::KeyB => 0x02, KeyCode::KeyC => 0x03,
                KeyCode::KeyD => 0x04, KeyCode::KeyE => 0x05, KeyCode::KeyG => 0x07,
                KeyCode::KeyH => 0x08, KeyCode::KeyI => 0x09, KeyCode::KeyJ => 0x0a,
                KeyCode::KeyK => 0x0b, KeyCode::KeyL => 0x0c, KeyCode::KeyM => 0x0d,
                KeyCode::KeyN => 0x0e, KeyCode::KeyO => 0x0f, KeyCode::KeyP => 0x10,
                KeyCode::KeyQ => 0x11, KeyCode::KeyR => 0x12, KeyCode::KeyS => 0x13,
                KeyCode::KeyT => 0x14, KeyCode::KeyU => 0x15, KeyCode::KeyW => 0x17,
                KeyCode::KeyX => 0x18, KeyCode::KeyY => 0x19, KeyCode::KeyZ => 0x1a,
                _ => return None,
            };
            terminal_alt_prefixed(&[control], alt)
        }
        _ if !ctrl && !super_key => {
            let text = logical_text?;
            if alt { let mut out = Vec::with_capacity(text.len() + 1); out.push(0x1b); out.extend_from_slice(text.as_bytes()); out }
            else { text.as_bytes().to_vec() }
        }
        _ => return None,
    };
    Some(seq)
}

fn terminal_modifier(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let value = 1 + u8::from(shift) + u8::from(alt) * 2 + u8::from(ctrl) * 4;
    (value != 1).then_some(value)
}

fn terminal_alt_prefixed(bytes: &[u8], alt: bool) -> Vec<u8> {
    if !alt { return bytes.to_vec(); }
    let mut out = Vec::with_capacity(bytes.len() + 1); out.push(0x1b); out.extend_from_slice(bytes); out
}

fn terminal_cursor_key(final_byte: u8, app: bool, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(8); out.push(0x1b);
    if let Some(modifier) = terminal_modifier(shift, alt, ctrl) {
        out.extend_from_slice(b"[1;"); out.push(b'0' + modifier); out.push(final_byte);
    } else { out.push(if app { b'O' } else { b'[' }); out.push(final_byte); }
    out
}

fn terminal_tilde_key(code: u8, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(8); out.extend_from_slice(b"\x1b["); out.push(b'0' + code);
    if let Some(modifier) = terminal_modifier(shift, alt, ctrl) { out.push(b';'); out.push(b'0' + modifier); }
    out.push(b'~'); out
}

fn terminal_function_key(code: u8, ss3: u8, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    if ss3 != 0 && terminal_modifier(shift, alt, ctrl).is_none() { return vec![0x1b, b'O', ss3]; }
    let mut out = Vec::with_capacity(9); out.extend_from_slice(b"\x1b[");
    if ss3 != 0 { out.push(b'1'); } else { if code >= 10 { out.push(b'0' + code / 10); } out.push(b'0' + code % 10); }
    if let Some(modifier) = terminal_modifier(shift, alt, ctrl) { out.push(b';'); out.push(b'0' + modifier); }
    if ss3 != 0 { out.push(ss3); } else { out.push(b'~'); } out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn plain_f1_reaches_pty_and_ctrl_t_remains_control_character() {
        assert_eq!(terminal_key_sequence(PhysicalKey::Code(KeyCode::F1), None, false, false, false, false, false), Some(b"\x1bOP".to_vec()));
        assert_eq!(terminal_key_sequence(PhysicalKey::Code(KeyCode::KeyT), Some("t"), false, true, false, false, false), Some(vec![0x14]));
    }

    #[test]
    fn focused_search_falls_through_only_for_plain_f1_and_ctrl_t_terminal_contract() {
        let mut search = TerminalSearchState::default();
        search.open();
        assert!(search_owns_keyboard(&search));

        let none = ModifiersState::empty();
        let ctrl = ModifiersState::CONTROL;
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;

        assert!(search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::F1),
            none,
        ));
        assert_eq!(
            terminal_key_sequence(
                PhysicalKey::Code(KeyCode::F1),
                None,
                false,
                false,
                false,
                false,
                false,
            ),
            Some(b"\x1bOP".to_vec()),
        );

        assert!(search_key_falls_through_to_terminal(
            &search,
            PhysicalKey::Code(KeyCode::KeyT),
            ctrl,
        ));
        assert_eq!(
            terminal_key_sequence(
                PhysicalKey::Code(KeyCode::KeyT),
                Some("t"),
                false,
                true,
                false,
                false,
                false,
            ),
            Some(vec![0x14]),
        );

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
            MouseButton::Left,
            ElementState::Pressed,
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
        assert_eq!(interaction.search.query, "needle");
    }

    #[test]
    fn modified_f1_remains_available_to_terminal_layer() {
        assert_eq!(
            terminal_key_sequence(
                PhysicalKey::Code(KeyCode::F1),
                None,
                false,
                true,
                true,
                false,
                false,
            ),
            Some(b"\x1b[1;7P".to_vec()),
        );
    }

    #[test]
    fn terminal_key_sequences_cover_navigation_and_modifiers() {
        assert_eq!(terminal_key_sequence(PhysicalKey::Code(KeyCode::ArrowUp), None, false, false, false, false, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(terminal_key_sequence(PhysicalKey::Code(KeyCode::ArrowUp), None, false, false, false, false, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(terminal_key_sequence(PhysicalKey::Code(KeyCode::Delete), None, true, true, false, false, false), Some(b"\x1b[3;6~".to_vec()));
        assert_eq!(terminal_key_sequence(PhysicalKey::Code(KeyCode::F12), None, false, false, false, false, false), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn ctrl_c_copies_selection_or_sends_sigint_byte() {
        assert_eq!(terminal_ctrl_c_action(true), TerminalCtrlCAction::CopySelection);
        assert_eq!(terminal_ctrl_c_action(false), TerminalCtrlCAction::SendInterrupt);
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
            terminal_bracketed_paste_bytes(b"plain".to_vec(), false),
            b"plain"
        );
        assert_eq!(
            terminal_bracketed_paste_bytes(b"plain".to_vec(), true),
            b"\x1b[200~plain\x1b[201~"
        );

        let paths = [PathBuf::from("/tmp/a b"), PathBuf::from("/tmp/界")];
        let payload = terminal_clipboard_paste_bytes(Some(&paths), Some("ignored")).unwrap();
        assert_eq!(
            terminal_bracketed_paste_bytes(payload, true),
            "\u{1b}[200~/tmp/a\\ b /tmp/界\u{1b}[201~".as_bytes()
        );
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
            terminal_mouse_protocol_sequence(0, 3, 4, true, true),
            b"\x1b[<0;3;4M"
        );
        assert_eq!(
            terminal_mouse_protocol_sequence(2, 1, 1, false, true),
            b"\x1b[<2;1;1m"
        );
        assert_eq!(
            terminal_mouse_protocol_sequence(0, 3, 4, true, false),
            vec![0x1b, b'[', b'M', 32, 35, 36]
        );
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
        assert_eq!(search.query, "needle");
        assert_eq!(
            terminal_key_sequence(
                PhysicalKey::Code(KeyCode::KeyA),
                Some("a"),
                false,
                false,
                false,
                false,
                false,
            ),
            Some(b"a".to_vec())
        );
        assert_eq!(search.query, "needle");
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
                ElementState::Pressed,
                MouseButton::Left,
                x,
                y,
            )
            .is_none());
            assert!(terminal_mouse_event_sequence(
                layout,
                capture,
                MouseTrackingMode::Press,
                true,
                ElementState::Released,
                MouseButton::Left,
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
            ElementState::Pressed,
            MouseButton::Left,
            x,
            y,
        )
        .unwrap();
        let release = terminal_mouse_event_sequence(
            layout,
            PointerCapture::None,
            MouseTrackingMode::Press,
            true,
            ElementState::Released,
            MouseButton::Left,
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
        for button in [MouseButton::Back, MouseButton::Forward, MouseButton::Other(9)] {
            assert_eq!(terminal_mouse_button_code(button), None);
            assert!(terminal_mouse_event_sequence(
                layout,
                PointerCapture::None,
                MouseTrackingMode::Press,
                true,
                ElementState::Pressed,
                button,
                x,
                y,
            )
            .is_none());
            let mut pressed = 0u8;
            update_pressed_mouse_buttons(&mut pressed, button, ElementState::Pressed);
            assert_eq!(pressed, 0);
        }
    }

    #[test]
    fn ronsole_overlays_own_mouse_motion_and_wheel_before_terminal_protocol() {
        let mut layout = mouse_test_layout(0.0);
        let search = crate::search::terminal_search_geometry(800.0, layout.body.y, 1.0);
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

    #[test]
    fn tracked_terminal_press_clears_stale_selection_only_on_press() {
        let mut selection = Some((1, 2, 3, 4));
        clear_selection_for_tracked_mouse_press(
            &mut selection,
            true,
            ElementState::Pressed,
        );
        assert_eq!(selection, None);

        selection = Some((1, 2, 3, 4));
        clear_selection_for_tracked_mouse_press(
            &mut selection,
            true,
            ElementState::Released,
        );
        assert!(selection.is_some());

        clear_selection_for_tracked_mouse_press(
            &mut selection,
            false,
            ElementState::Pressed,
        );
        assert!(selection.is_some());
    }
}
