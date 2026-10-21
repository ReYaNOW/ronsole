//! `vte::Perform` for the terminal: CSI, ESC, control characters, DECSET/DECRST and replies.

use crate::cell::{
    Cell, apply_ansi_sgr,
    is_terminal_zero_width_format, terminal_char_width, terminal_is_emoji_modifier,
    terminal_presentation_selector,
};
use crate::grid::{
    LineMeta, csi_count, insert_line_with_meta, remove_line_with_meta,
    repair_wide_line_with_blank,
};
use crate::input::MouseTrackingMode;
use crate::osc::terminal_programmed_title;
use crate::term::Performer;
use vte::{Params, Perform};

impl Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        if let Some(presentation) = terminal_presentation_selector(c) {
            self.term.apply_presentation_selector(presentation, c);
            return;
        }
        if c == '\u{200D}' {
            self.term.attach_zero_width(c);
            self.term.join_next = true;
            return;
        }
        if self.term.join_next {
            self.term.attach_zero_width(c);
            self.term.join_next = false;
            return;
        }
        if is_terminal_zero_width_format(c) {
            self.term.attach_zero_width(c);
            return;
        }
        if terminal_is_emoji_modifier(c) || terminal_char_width(c) == 0 {
            self.term.attach_zero_width(c);
            return;
        }
        self.term.put_char(c);
        if !self.term.presentation_ready && !c.is_whitespace() {
            self.term.mark_presentation_ready();
        }
    }
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\x0B' | b'\x0C' => {
                self.term.wrap_pending = false;
                self.term.join_next = false;
                self.term.newline();
            }
            b'\r' => {
                self.term.cur_x = 0;
                self.term.wrap_pending = false;
                self.term.join_next = false;
            }
            b'\x08' => {
                self.term.wrap_pending = false;
                self.term.join_next = false;
                self.term.cur_x = self.term.cur_x.saturating_sub(1);
                if self.term
                    .lines
                    .get(self.term.cur_y)
                    .and_then(|line| line.get(self.term.cur_x))
                    .is_some_and(Cell::is_wide_spacer)
                {
                    self.term.cur_x = self.term.cur_x.saturating_sub(1);
                }
            }
            b'\t' => self.term.move_to_tab_stop(1, false),
            _ => {}
        }
    }
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.first().is_some_and(|selector| *selector == b"104") {
            // Ronsole does not expose a mutable OSC 4 palette; reset is therefore a no-op.
            return;
        }
        if params
            .first()
            .is_some_and(|selector| *selector == b"0" || *selector == b"2")
            && let Some(title) = terminal_programmed_title(&params[1..])
        {
            self.sink.title(&title);
        }

        if params.len() >= 2 && params[1] == b"?" {
            if params[0] == b"10" || params[0] == b"11" {
                let prefix = std::str::from_utf8(params[0]).unwrap_or("10");
                let msg = format!("\x1B]{};rgb:ffff/ffff/ffff\x1B\\", prefix);
                self.term.queue_reply(msg.as_bytes());
            }
        }
    }
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        self.term.wrap_pending = false;
        self.term.join_next = false;
        match byte {
            b'7' => self.term.saved_cursor = Some((self.term.cur_x, self.term.cur_y)),
            b'8' => {
                if let Some((x, y)) = self.term.saved_cursor {
                    self.term.cur_x = x;
                    self.term.cur_y = y;
                }
            }
            b'D' => self.term.newline(),
            b'E' => {
                self.term.newline();
                self.term.cur_x = 0;
            }
            b'H' => {
                if let Some(stop) = self.term.tab_stops.get_mut(self.term.cur_x) {
                    *stop = true;
                }
            }
            b'M' => {
                if self.term.cur_y == self.term.scroll_region.0 {
                    self.term.scroll_region_down(1);
                } else {
                    self.term.cur_y = self.term.cur_y.saturating_sub(1);
                }
            }
            b'c' => self.term.hard_reset_terminal_state(),
            _ => {}
        }
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Queries do not move the cursor, so they keep a pending wrap.
        let query = match action {
            'c' | 'q' => intermediates == b">",
            'p' => intermediates.contains(&b'$'),
            _ => false,
        };
        if action != 'm' && !query {
            self.term.wrap_pending = false;
            self.term.join_next = false;
        }
        if action == 'p' && intermediates.contains(&b'!') {
            self.term.soft_reset_terminal_state();
            return;
        }
        match action {
            'h' | 'l' => {
                let enable = action == 'h';
                let is_private = intermediates.contains(&b'?');
                if is_private {
                    for param in params.iter() {
                        let Some(value) = param.first().copied() else {
                            continue;
                        };
                        if value == 1049 || value == 47 || value == 1047 {
                            if enable && !self.term.is_alt {
                                self.term.is_alt = true;
                                self.term.alt_saved_cursor = Some((self.term.cur_x, self.term.cur_y));
                                let mut alt = std::collections::VecDeque::new();
                                let mut alt_meta = std::collections::VecDeque::new();
                                for _ in 0..self.term.visible_rows {
                                    let mut line = self.term
                                        .pool
                                        .pop()
                                        .unwrap_or_else(|| Vec::with_capacity(self.term.cols));
                                    line.resize(self.term.cols, Cell::default());
                                    line.fill(Cell::default());
                                    alt.push_back(line);
                                    alt_meta.push_back(LineMeta::default());
                                }
                                self.term.alt_lines = Some(std::mem::replace(&mut self.term.lines, alt));
                                self.term.alt_line_meta =
                                    Some(std::mem::replace(&mut self.term.line_meta, alt_meta));
                                self.term.cur_x = 0;
                                self.term.cur_y = 0;
                                self.term.wrap_pending = false;
                                self.term.dirty = true;
                            } else if !enable && self.term.is_alt {
                                self.term.is_alt = false;
                                if let Some(alt) = self.term.alt_lines.take() {
                                    let old_lines = std::mem::replace(&mut self.term.lines, alt);
                                    for mut line in old_lines {
                                        if self.term.pool.len() < 128 {
                                            line.clear();
                                            self.term.pool.push(line);
                                        }
                                    }
                                }
                                if let Some(normal_meta) = self.term.alt_line_meta.take() {
                                    self.term.line_meta = normal_meta;
                                }
                                if let Some((x, y)) = self.term.alt_saved_cursor.take() {
                                    self.term.cur_x = x;
                                    self.term.cur_y = y;
                                }
                                self.term.cur_x = self.term.cur_x.min(self.term.cols.saturating_sub(1));
                                self.term.cur_y = self.term.cur_y.min(self.term.visible_rows.saturating_sub(1));
                                self.term.wrap_pending = false;
                                self.term.dirty = true;
                            }
                        } else {
                            match value {
                                1 => self.term.app_cursor_keys = enable,
                                6 => {
                                    self.term.origin_mode = enable;
                                    self.term.cursor_home();
                                }
                                7 => self.term.autowrap = enable,
                                25 => self.term.cursor_visible = enable,
                                1000 => self.term.set_mouse_tracking_mode(MouseTrackingMode::Press, enable),
                                1002 => self.term.set_mouse_tracking_mode(MouseTrackingMode::ButtonMotion, enable),
                                1003 => self.term.set_mouse_tracking_mode(MouseTrackingMode::AnyMotion, enable),
                                1004 => self.term.focus_reporting = enable,
                                1006 => self.term.mouse_sgr = enable,
                                2004 => self.term.bracketed_paste = enable,
                                _ => {}
                            }
                        }
                    }
                } else if params.iter().any(|param| param.first() == Some(&4)) {
                    self.term.insert_mode = enable;
                }
            }
            's' => self.term.saved_cursor = Some((self.term.cur_x, self.term.cur_y)),
            'u' => {
                if let Some((x, y)) = self.term.saved_cursor {
                    self.term.cur_x = x.min(self.term.cols.saturating_sub(1));
                    self.term.cur_y = y.min(self.term.visible_rows.saturating_sub(1));
                }
            }
            'G' | '`' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.term.cur_x = p.saturating_sub(1).min(self.term.cols.saturating_sub(1));
            }
            'd' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.term.set_cursor_position(p.saturating_sub(1), self.term.cur_x);
            }
            'c' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                match intermediates {
                    [] if param == 0 => self.term.queue_reply(b"\x1B[?62c"),
                    [b'>'] if param == 0 => self.term.queue_da2(),
                    _ => {}
                }
            }
            'n' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let private = intermediates.contains(&b'?');
                if param == 5 && !private {
                    self.term.queue_reply(b"\x1B[0n");
                } else if param == 6 {
                    let row = if self.term.origin_mode {
                        self.term.cur_y.saturating_sub(self.term.scroll_region.0) + 1
                    } else {
                        self.term.cur_y + 1
                    };
                    let prefix = if private { "?" } else { "" };
                    let msg = format!("\x1B[{prefix}{row};{}R", self.term.cur_x + 1);
                    self.term.queue_reply(msg.as_bytes());
                }
            }
            'p' => {
                // vte reads an omitted parameter as 0, and mode 0 is never a
                // real mode, so a DECRQM without a mode number gets no reply.
                let mode = params.iter().next().map(|p| p[0]).filter(|&mode| mode != 0);
                match (intermediates, mode) {
                    ([b'?', b'$'], Some(mode)) => self.term.queue_decrqm_private(mode),
                    ([b'$'], Some(mode)) => self.term.queue_decrqm_ansi(mode),
                    _ => {}
                }
            }
            'q' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                if intermediates == b">" && param == 0 {
                    self.term.queue_xtversion();
                }
            }
            'H' | 'f' => {
                let mut iter = params.iter();
                let y = iter.next().map(|p| p[0]).unwrap_or(1) as usize;
                let x = iter.next().map(|p| p[0]).unwrap_or(1) as usize;
                let y = if y == 0 { 1 } else { y };
                let x = if x == 0 { 1 } else { x };
                self.term.set_cursor_position(y.saturating_sub(1), x.saturating_sub(1));
            }
            'J' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let blank = Cell::blank_with_background(self.term.cur_bg);
                match param {
                    0 => {
                        if let Some(line) = self.term.lines.get_mut(self.term.cur_y) {
                            if self.term.cur_x < line.len() {
                                line[self.term.cur_x..].fill(blank.clone());
                                repair_wide_line_with_blank(line, &blank);
                            }
                        }
                        if let Some(state) = self.term.line_meta.get_mut(self.term.cur_y) {
                            state.used_cols = state.used_cols.min(self.term.cur_x);
                            state.soft_wrapped = false;
                        }
                        for i in (self.term.cur_y + 1)..self.term.visible_rows {
                            if let Some(line) = self.term.lines.get_mut(i) {
                                line.fill(blank.clone());
                            }
                            if let Some(state) = self.term.line_meta.get_mut(i) {
                                *state = LineMeta::default();
                            }
                        }
                    }
                    1 => {
                        for i in 0..self.term.cur_y {
                            if let Some(line) = self.term.lines.get_mut(i) {
                                line.fill(blank.clone());
                            }
                            if let Some(state) = self.term.line_meta.get_mut(i) {
                                *state = LineMeta::default();
                            }
                        }
                        if let Some(line) = self.term.lines.get_mut(self.term.cur_y) {
                            let end = (self.term.cur_x + 1).min(line.len());
                            line[..end].fill(blank.clone());
                            repair_wide_line_with_blank(line, &blank);
                        }
                    }
                    2 => {
                        for (line, state) in self.term.lines.iter_mut().zip(self.term.line_meta.iter_mut()) {
                            line.fill(blank.clone());
                            *state = LineMeta::default();
                        }
                    }
                    3 => {
                        self.term.scrollback.clear();
                        self.term.scrollback_meta.clear();
                        self.term.scrollback_storage_cols = self.term.cols;
                        self.term.selection = None;
                    }
                    _ => {}
                }
            }
            'K' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let blank = Cell::blank_with_background(self.term.cur_bg);
                if let Some(line) = self.term.lines.get_mut(self.term.cur_y) {
                    match param {
                        0 => {
                            if self.term.cur_x < line.len() {
                                line[self.term.cur_x..].fill(blank.clone());
                                repair_wide_line_with_blank(line, &blank);
                            }
                        }
                        1 => {
                            let end = (self.term.cur_x + 1).min(line.len());
                            line[..end].fill(blank.clone());
                            repair_wide_line_with_blank(line, &blank);
                        }
                        2 => {
                            line.fill(blank);
                        }
                        _ => {}
                    }
                }
                if let Some(state) = self.term.line_meta.get_mut(self.term.cur_y) {
                    match param {
                        0 => {
                            state.used_cols = state.used_cols.min(self.term.cur_x);
                            state.soft_wrapped = false;
                        }
                        1 if self.term.cur_x + 1 >= state.used_cols => {
                            *state = LineMeta::default();
                        }
                        2 => *state = LineMeta::default(),
                        _ => {}
                    }
                }
            }
            'r' => {
                let mut iter = params.iter();
                let top = iter.next().map(|p| p[0]).unwrap_or(1) as usize;
                let bottom = iter
                    .next()
                    .map(|p| p[0])
                    .unwrap_or(self.term.visible_rows as u16) as usize;
                let top = if top == 0 { 1 } else { top };
                let bottom = if bottom == 0 {
                    self.term.visible_rows
                } else {
                    bottom
                };
                let top_idx = top
                    .saturating_sub(1)
                    .min(self.term.visible_rows.saturating_sub(1));
                let bottom_idx = bottom
                    .saturating_sub(1)
                    .min(self.term.visible_rows.saturating_sub(1));
                if bottom_idx >= top_idx {
                    self.term.scroll_region = (top_idx, bottom_idx);
                }
                self.term.cursor_home();
            }
            'L' => {
                let (top, bottom) = self.term.scroll_region;
                if self.term.cur_y < top || self.term.cur_y > bottom || bottom >= self.term.lines.len() {
                    return;
                }
                let count = csi_count(params, bottom - self.term.cur_y + 1);
                let blank = Cell::blank_with_background(self.term.cur_bg);
                for _ in 0..count {
                    if let Some((mut line, _)) =
                        remove_line_with_meta(&mut self.term.lines, &mut self.term.line_meta, bottom)
                        && self.term.pool.len() < 128
                    {
                        line.clear();
                        self.term.pool.push(line);
                    }
                    let mut new_line = self.term
                        .pool
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(self.term.cols));
                    new_line.resize(self.term.cols, blank.clone());
                    new_line.fill(blank.clone());
                    insert_line_with_meta(
                        &mut self.term.lines,
                        &mut self.term.line_meta,
                        self.term.cur_y,
                        new_line,
                        LineMeta::default(),
                    );
                }
            }
            'M' => {
                let (top, bottom) = self.term.scroll_region;
                if self.term.cur_y < top || self.term.cur_y > bottom || bottom >= self.term.lines.len() {
                    return;
                }
                let count = csi_count(params, bottom - self.term.cur_y + 1);
                let blank = Cell::blank_with_background(self.term.cur_bg);
                for _ in 0..count {
                    if let Some((mut line, _)) =
                        remove_line_with_meta(&mut self.term.lines, &mut self.term.line_meta, self.term.cur_y)
                        && self.term.pool.len() < 128
                    {
                        line.clear();
                        self.term.pool.push(line);
                    }
                    let mut new_line = self.term
                        .pool
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(self.term.cols));
                    new_line.resize(self.term.cols, blank.clone());
                    new_line.fill(blank.clone());
                    insert_line_with_meta(
                        &mut self.term.lines,
                        &mut self.term.line_meta,
                        bottom,
                        new_line,
                        LineMeta::default(),
                    );
                }
            }
            'P' => {
                let blank = Cell::blank_with_background(self.term.cur_bg);
                let mut deleted = 0usize;
                if let Some(line) = self.term.lines.get_mut(self.term.cur_y) {
                    let count = csi_count(params, line.len().saturating_sub(self.term.cur_x));
                    deleted = count;
                    if count > 0 {
                        let tail = &mut line[self.term.cur_x..];
                        tail.rotate_left(count);
                        let fill_start = tail.len() - count;
                        tail[fill_start..].fill(blank.clone());
                    }
                    repair_wide_line_with_blank(line, &blank);
                }
                if deleted > 0
                    && let Some(state) = self.term.line_meta.get_mut(self.term.cur_y)
                    && self.term.cur_x < state.used_cols
                {
                    let removed = deleted.min(state.used_cols - self.term.cur_x);
                    state.used_cols = state.used_cols.saturating_sub(removed);
                }
            }
            'X' => {
                let blank = Cell::blank_with_background(self.term.cur_bg);
                let mut erased_end = None;
                if let Some(line) = self.term.lines.get_mut(self.term.cur_y) {
                    let start = self.term.cur_x.min(line.len());
                    let count = csi_count(params, line.len().saturating_sub(start));
                    let end = start + count;
                    if start < end {
                        line[start..end].fill(blank.clone());
                        repair_wide_line_with_blank(line, &blank);
                        erased_end = Some((start, end));
                    }
                }
                if let Some((start, end)) = erased_end
                    && let Some(state) = self.term.line_meta.get_mut(self.term.cur_y)
                    && end >= state.used_cols
                {
                    state.used_cols = state.used_cols.min(start);
                    if end >= self.term.cols {
                        state.soft_wrapped = false;
                    }
                }
            }
            'm' => {
                apply_ansi_sgr(
                    params,
                    &mut self.term.cur_fg,
                    &mut self.term.cur_bg,
                    &mut self.term.cur_bold,
                    &mut self.term.cur_dim,
                    &mut self.term.cur_underline,
                    &mut self.term.cur_inverse,
                );
            }
            'C' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.term.cur_x = (self.term.cur_x + p).min(self.term.cols.saturating_sub(1));
            }
            'D' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.term.cur_x = self.term.cur_x.saturating_sub(p);
            }
            'A' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                let lower = if self.term.origin_mode { self.term.scroll_region.0 } else { 0 };
                self.term.cur_y = self.term.cur_y.saturating_sub(p).max(lower);
            }
            'B' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                let upper = if self.term.origin_mode {
                    self.term.scroll_region.1
                } else {
                    self.term.visible_rows.saturating_sub(1)
                };
                self.term.cur_y = (self.term.cur_y + p).min(upper);
            }
            'E' | 'F' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                if action == 'E' {
                    self.term.cur_y = (self.term.cur_y + p).min(self.term.visible_rows.saturating_sub(1));
                } else {
                    self.term.cur_y = self.term.cur_y.saturating_sub(p);
                }
                self.term.cur_x = 0;
            }
            'I' | 'Z' => {
                let count = csi_count(params, self.term.cols.max(1));
                self.term.move_to_tab_stop(count, action == 'Z');
            }
            'g' => {
                let value = params.iter().next().map(|p| p[0]).unwrap_or(0);
                if value == 0 {
                    if let Some(stop) = self.term.tab_stops.get_mut(self.term.cur_x) {
                        *stop = false;
                    }
                } else if value == 3 {
                    self.term.tab_stops.fill(false);
                }
            }
            '@' => {
                let count = csi_count(params, self.term.cols.saturating_sub(self.term.cur_x));
                let blank = Cell::blank_with_background(self.term.cur_bg);
                if count > 0
                    && let Some(line) = self.term.lines.get_mut(self.term.cur_y)
                {
                    let tail = &mut line[self.term.cur_x..];
                    tail.rotate_right(count.min(tail.len()));
                    tail[..count].fill(blank.clone());
                    repair_wide_line_with_blank(line, &blank);
                }
                if count > 0
                    && let Some(state) = self.term.line_meta.get_mut(self.term.cur_y)
                    && self.term.cur_x < state.used_cols
                {
                    state.used_cols = state.used_cols.saturating_add(count).min(self.term.cols);
                }
            }
            'S' => {
                let (top, bottom) = self.term.scroll_region;
                let p = csi_count(params, bottom.saturating_sub(top) + 1);
                self.term.scroll_region_up(p);
            }
            'T' => {
                let (top, bottom) = self.term.scroll_region;
                let p = csi_count(params, bottom.saturating_sub(top) + 1);
                self.term.scroll_region_down(p);
            }
            _ => {}
        }
    }
}
