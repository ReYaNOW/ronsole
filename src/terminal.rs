use std::io;
use std::sync::{Arc, Mutex};
use vte::{Params, Parser, Perform};

pub(crate) use crate::terminal_compat::{
    CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT, Cell,
    MouseTrackingMode, TerminalColor, apply_ansi_sgr, is_terminal_zero_width_format,
    terminal_char_width, terminal_color_rgba, terminal_effective_foreground,
    terminal_is_emoji_modifier, terminal_presentation_selector, terminal_should_render_zero_width,
};
#[cfg(test)]
use crate::terminal_compat::CELL_PRESENTATION_AUTO;

#[inline(always)]
pub(crate) fn normalized_selection_bounds(
    sx: usize,
    sy: usize,
    ex: usize,
    ey: usize,
) -> (usize, usize, usize, usize) {
    let start_y = sy.min(ey);
    let end_y = sy.max(ey);
    let start_x = if sy < ey {
        sx
    } else if sy > ey {
        ex
    } else {
        sx.min(ex)
    };
    let end_x = if sy < ey {
        ex
    } else if sy > ey {
        sx
    } else {
        sx.max(ex)
    };
    (start_x, start_y, end_x, end_y)
}

fn default_tab_stops(cols: usize) -> Vec<bool> {
    let mut stops = vec![false; cols];
    for column in (8..cols).step_by(8) {
        stops[column] = true;
    }
    stops
}

fn clear_wide_footprint(line: &mut [Cell], column: usize, blank: &Cell) {
    let Some(cell) = line.get(column) else {
        return;
    };
    if cell.is_wide_spacer() {
        if column > 0 && line[column - 1].is_wide() {
            line[column - 1] = blank.clone();
        }
    } else if cell.is_wide() && column + 1 < line.len() && line[column + 1].is_wide_spacer() {
        line[column + 1] = blank.clone();
    }
    line[column] = blank.clone();
}

fn compact_line_capacity_after_resize(line: &mut Vec<Cell>, cols: usize) {
    let target = cols.max(1).saturating_mul(2);
    let threshold = target.saturating_mul(2).max(512);
    if line.capacity() <= threshold || line.len() > target {
        return;
    }
    let mut compact = Vec::with_capacity(target.max(line.len()));
    compact.extend(line.iter().cloned());
    *line = compact;
}

#[inline]
fn should_compact_scrollback_storage(storage_cols: usize, new_cols: usize) -> bool {
    new_cols < storage_cols && storage_cols >= new_cols.max(1).saturating_mul(2)
}

fn compact_scrollback_line_after_major_shrink(line: &mut Vec<Cell>, cols: usize) {
    line.truncate(cols);
    repair_wide_line(line);
    let target = cols.max(1);
    let threshold = target.saturating_mul(2).max(128);
    if line.capacity() <= threshold {
        return;
    }
    let mut compact = Vec::with_capacity(target.max(line.len()));
    compact.extend(line.iter().cloned());
    *line = compact;
}

fn repair_wide_line_with_blank(line: &mut [Cell], blank: &Cell) {
    let mut column = 0usize;
    while column < line.len() {
        if line[column].is_wide() {
            if column + 1 >= line.len() || !line[column + 1].is_wide_spacer() {
                line[column] = blank.clone();
            } else {
                column += 2;
                continue;
            }
        } else if line[column].is_wide_spacer()
            && (column == 0 || !line[column - 1].is_wide())
        {
            line[column] = blank.clone();
        }
        column += 1;
    }
}

fn repair_wide_line(line: &mut [Cell]) {
    repair_wide_line_with_blank(line, &Cell::default());
}

#[inline]
fn csi_count(params: &Params, max: usize) -> usize {
    let raw = params
        .iter()
        .next()
        .and_then(|param| param.first())
        .copied()
        .unwrap_or(1);
    usize::from(raw.max(1)).min(max)
}

pub struct TermGrid {
    pub scrollback: std::collections::VecDeque<Vec<Cell>>,
    pub lines: std::collections::VecDeque<Vec<Cell>>,
    pub alt_lines: Option<std::collections::VecDeque<Vec<Cell>>>,
    pub alt_saved_cursor: Option<(usize, usize)>,
    pub is_alt: bool,
    pub cols: usize,
    pub visible_rows: usize,
    pub cur_x: usize,
    pub cur_y: usize,
    pub(crate) cur_fg: TerminalColor,
    pub(crate) cur_bg: TerminalColor,
    pub cur_bold: bool,
    pub(crate) cur_dim: bool,
    pub(crate) cur_underline: bool,
    pub(crate) cur_inverse: bool,
    pub dirty: bool,
    pub content_generation: u64,
    pub(crate) presentation_ready: bool,
    presentation_layout_ready: bool,
    pub selection: Option<(usize, usize, usize, usize)>,
    pub reply_tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    pub saved_cursor: Option<(usize, usize)>,
    pub scroll_region: (usize, usize),
    pub cursor_visible: bool,
    pub app_cursor_keys: bool,
    pub(crate) mouse_tracking_mode: MouseTrackingMode,
    mouse_tracking_mask: u8,
    pub(crate) mouse_sgr: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) focus_reporting: bool,
    autowrap: bool,
    wrap_pending: bool,
    insert_mode: bool,
    origin_mode: bool,
    tab_stops: Vec<bool>,
    join_next: bool,
    scrollback_storage_cols: usize,
    pub pool: Vec<Vec<Cell>>,
    title_cache: Option<crate::terminal_process::TerminalTitleCache>,
}

impl TermGrid {
    pub fn new(cols: usize, visible_rows: usize) -> Self {
        let mut lines = std::collections::VecDeque::new();
        for _ in 0..visible_rows {
            lines.push_back(vec![Cell::default(); cols]);
        }
        Self {
            scrollback: std::collections::VecDeque::new(),
            lines,
            alt_lines: None,
            alt_saved_cursor: None,
            is_alt: false,
            cols,
            visible_rows,
            cur_x: 0,
            cur_y: 0,
            cur_fg: TerminalColor::default_foreground(),
            cur_bg: TerminalColor::default_background(),
            cur_bold: false,
            cur_dim: false,
            cur_underline: false,
            cur_inverse: false,
            dirty: true,
            content_generation: 0,
            presentation_ready: false,
            presentation_layout_ready: false,
            selection: None,
            reply_tx: None,
            saved_cursor: None,
            scroll_region: (0, visible_rows.saturating_sub(1)),
            cursor_visible: true,
            app_cursor_keys: false,
            mouse_tracking_mode: MouseTrackingMode::None,
            mouse_tracking_mask: 0,
            mouse_sgr: false,
            bracketed_paste: false,
            focus_reporting: false,
            autowrap: true,
            wrap_pending: false,
            insert_mode: false,
            origin_mode: false,
            tab_stops: default_tab_stops(cols),
            join_next: false,
            scrollback_storage_cols: cols,
            pool: Vec::with_capacity(128),
            title_cache: None,
        }
    }

    pub(crate) fn new_with_title_cache(
        cols: usize,
        visible_rows: usize,
        title_cache: crate::terminal_process::TerminalTitleCache,
    ) -> Self {
        let mut grid = Self::new(cols, visible_rows);
        grid.title_cache = Some(title_cache);
        grid
    }

    #[inline]
    fn try_queue_reply(&self, reply: Vec<u8>) {
        if let Some(tx) = &self.reply_tx {
            let _ = tx.try_send(reply);
        }
    }

    #[inline]
    pub(crate) fn mark_presentation_ready(&mut self) {
        self.presentation_ready = true;
        self.dirty = true;
    }

    #[inline]
    pub fn mark_presentation_layout_ready(&mut self) {
        self.presentation_layout_ready = true;
    }

    #[inline]
    pub fn presentation_visible(&self) -> bool {
        self.presentation_ready && self.presentation_layout_ready
    }

    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        if new_cols == self.cols && new_rows == self.visible_rows {
            return;
        }

        if new_cols != self.cols {
            let old_cols = self.cols;
            self.selection = None;
            if should_compact_scrollback_storage(self.scrollback_storage_cols, new_cols) {
                for line in &mut self.scrollback {
                    compact_scrollback_line_after_major_shrink(line, new_cols);
                }
                self.scrollback_storage_cols = new_cols;
            }
            for line in self.lines.iter_mut() {
                line.resize(new_cols, Cell::default());
                repair_wide_line(line);
                compact_line_capacity_after_resize(line, new_cols);
            }
            if let Some(alt) = &mut self.alt_lines {
                for line in alt.iter_mut() {
                    line.resize(new_cols, Cell::default());
                    repair_wide_line(line);
                    compact_line_capacity_after_resize(line, new_cols);
                }
            }
            for line in &mut self.pool {
                compact_line_capacity_after_resize(line, new_cols);
            }
            self.tab_stops.resize(new_cols, false);
            if new_cols > old_cols {
                for column in old_cols..new_cols {
                    self.tab_stops[column] = column != 0 && column % 8 == 0;
                }
            }
            self.cols = new_cols;
        }

        let current_rows = self.lines.len();
        let was_full_region = self.scroll_region.1 >= current_rows.saturating_sub(1);

        if new_rows < current_rows {
            let diff = current_rows - new_rows;
            let rows_below_cursor = current_rows.saturating_sub(self.cur_y + 1);
            let drop_bottom = rows_below_cursor.min(diff);
            let drop_top = diff - drop_bottom;

            for _ in 0..drop_bottom {
                if let Some(mut line) = self.lines.pop_back() {
                    if self.pool.len() < 128 {
                        line.clear();
                        self.pool.push(line);
                    }
                }
            }
            for _ in 0..drop_top {
                if let Some(top) = self.lines.pop_front() {
                    if !self.is_alt {
                        self.scrollback_storage_cols = self.scrollback_storage_cols.max(top.len());
                        self.scrollback.push_back(top);
                    }
                }
            }

            self.cur_y = self.cur_y.saturating_sub(drop_top);
            if let Some((_, ref mut sy)) = self.saved_cursor {
                *sy = sy.saturating_sub(drop_top);
            }
        } else if new_rows > current_rows {
            let diff = new_rows - current_rows;

            if !self.is_alt {
                let from_scrollback = diff.min(self.scrollback.len());

                for _ in 0..from_scrollback {
                    if let Some(mut row) = self.scrollback.pop_back() {
                        row.resize(self.cols, Cell::default());
                        repair_wide_line(&mut row);
                        compact_line_capacity_after_resize(&mut row, self.cols);
                        self.lines.push_front(row);
                    }
                }

                let blanks = diff - from_scrollback;
                for _ in 0..blanks {
                    let mut line = self
                        .pool
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(self.cols));
                    line.resize(self.cols, Cell::default());
                    line.fill(Cell::default());
                    self.lines.push_back(line);
                }

                self.cur_y += from_scrollback;
                if let Some((_, ref mut sy)) = self.saved_cursor {
                    *sy += from_scrollback;
                }
            } else {
                for _ in 0..diff {
                    self.lines.push_back(vec![Cell::default(); self.cols]);
                }
            }
        }

        if let Some(alt) = &mut self.alt_lines {
            let alt_current_rows = alt.len();
            if new_rows < alt_current_rows {
                let diff = alt_current_rows - new_rows;
                for _ in 0..diff {
                    if let Some(mut line) = alt.pop_back() {
                        if self.pool.len() < 128 {
                            line.clear();
                            self.pool.push(line);
                        }
                    }
                }
            } else if new_rows > alt_current_rows {
                let diff = new_rows - alt_current_rows;
                for _ in 0..diff {
                    let mut line = self
                        .pool
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(self.cols));
                    line.resize(self.cols, Cell::default());
                    line.fill(Cell::default());
                    alt.push_back(line);
                }
            }
            if let Some((_, ref mut sy)) = self.alt_saved_cursor {
                *sy = (*sy).min(new_rows.saturating_sub(1));
            }
        }

        while self.scrollback.len() > 10000 {
            self.scrollback.pop_front();
        }
        self.visible_rows = new_rows;
        self.cur_x = self.cur_x.min(self.cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(self.visible_rows.saturating_sub(1));
        self.wrap_pending = false;
        self.join_next = false;

        if was_full_region {
            self.scroll_region = (0, new_rows.saturating_sub(1));
        } else {
            let (sr_top, sr_bot) = self.scroll_region;
            self.scroll_region = (
                sr_top.min(new_rows.saturating_sub(1)),
                sr_bot.min(new_rows.saturating_sub(1)),
            );
        }
        self.content_generation = self.content_generation.wrapping_add(1);
        self.dirty = true;
    }

    pub fn put_char(&mut self, c: char) {
        let width = terminal_char_width(c);
        if width == 0 {
            self.attach_zero_width(c);
            return;
        }
        if self.wrap_pending {
            if self.autowrap {
                self.newline();
                self.cur_x = 0;
            }
            self.wrap_pending = false;
        }
        if self.cur_y >= self.visible_rows {
            self.cur_y = self.visible_rows.saturating_sub(1);
        }
        if width == 2 {
            if self.cols < 2 {
                self.join_next = false;
                return;
            }
            if self.cur_x + 1 >= self.cols {
                if !self.autowrap {
                    self.join_next = false;
                    return;
                }
                let blank = Cell::blank_with_background(self.cur_bg);
                if let Some(line) = self.lines.get_mut(self.cur_y) {
                    clear_wide_footprint(line, self.cur_x, &blank);
                }
                self.newline();
                self.cur_x = 0;
            }
        }
        let fg = terminal_effective_foreground(self.cur_fg, self.cur_bold);
        let bg = self.cur_bg;
        let blank = Cell::blank_with_background(bg);
        if let Some(line) = self.lines.get_mut(self.cur_y) {
            if self.insert_mode {
                let tail = &mut line[self.cur_x..];
                tail.rotate_right(width.min(tail.len()));
                for cell in tail.iter_mut().take(width) {
                    *cell = blank.clone();
                }
                repair_wide_line_with_blank(line, &blank);
            } else {
                clear_wide_footprint(line, self.cur_x, &blank);
                if width == 2 {
                    clear_wide_footprint(line, self.cur_x + 1, &blank);
                }
            }
            if let Some(cell) = line.get_mut(self.cur_x) {
                cell.set_char(c, fg, bg, width == 2);
                cell.set_sgr_style(self.cur_inverse, self.cur_underline, self.cur_dim);
            }
            if width == 2
                && let Some(cell) = line.get_mut(self.cur_x + 1)
            {
                cell.set_wide_spacer(fg, bg);
                cell.set_sgr_style(self.cur_inverse, self.cur_underline, self.cur_dim);
            }
        }
        let next = self.cur_x.saturating_add(width);
        if next >= self.cols {
            self.cur_x = self.cols.saturating_sub(1);
            self.wrap_pending = self.autowrap;
        } else {
            self.cur_x = next;
        }
        self.join_next = false;
    }

    fn previous_base_cell_mut(&mut self) -> Option<&mut Cell> {
        let line = self.lines.get_mut(self.cur_y)?;
        let mut cell_x = if self.wrap_pending {
            self.cur_x
        } else {
            self.cur_x.checked_sub(1)?
        };
        if line.get(cell_x)?.is_wide_spacer() {
            cell_x = cell_x.checked_sub(1)?;
        }
        line.get_mut(cell_x)
    }

    fn attach_zero_width(&mut self, c: char) {
        if let Some(cell) = self.previous_base_cell_mut()
            && cell.c != ' '
        {
            cell.push_zero_width(c);
        }
    }

    pub fn apply_presentation_selector(&mut self, presentation: u8, selector: char) {
        let Some(cell) = self.previous_base_cell_mut() else {
            return;
        };
        if cell.c != ' ' {
            cell.presentation = if presentation == CELL_PRESENTATION_EMOJI
                && crate::renderer::terminal_force_text_presentation(cell.c)
            {
                CELL_PRESENTATION_TEXT
            } else {
                presentation
            };
            cell.push_zero_width(selector);
        }
    }

    fn next_tab_stop(&self, from: usize) -> usize {
        ((from + 1)..self.cols)
            .find(|&column| self.tab_stops.get(column).copied().unwrap_or(false))
            .unwrap_or_else(|| self.cols.saturating_sub(1))
    }

    fn previous_tab_stop(&self, from: usize) -> usize {
        (0..from)
            .rev()
            .find(|&column| self.tab_stops.get(column).copied().unwrap_or(false))
            .unwrap_or(0)
    }

    fn move_to_tab_stop(&mut self, count: usize, backwards: bool) {
        self.wrap_pending = false;
        for _ in 0..count.max(1).min(self.cols.max(1)) {
            let next = if backwards {
                self.previous_tab_stop(self.cur_x)
            } else {
                self.next_tab_stop(self.cur_x)
            };
            if next == self.cur_x {
                break;
            }
            self.cur_x = next;
        }
    }

    fn set_mouse_tracking_mode(&mut self, mode: MouseTrackingMode, enabled: bool) {
        let bit = match mode {
            MouseTrackingMode::Press => 1,
            MouseTrackingMode::ButtonMotion => 2,
            MouseTrackingMode::AnyMotion => 4,
            MouseTrackingMode::None => return,
        };
        if enabled {
            self.mouse_tracking_mask |= bit;
        } else {
            self.mouse_tracking_mask &= !bit;
        }
        self.mouse_tracking_mode = if self.mouse_tracking_mask & 4 != 0 {
            MouseTrackingMode::AnyMotion
        } else if self.mouse_tracking_mask & 2 != 0 {
            MouseTrackingMode::ButtonMotion
        } else if self.mouse_tracking_mask & 1 != 0 {
            MouseTrackingMode::Press
        } else {
            MouseTrackingMode::None
        };
    }

    fn set_cursor_position(&mut self, row: usize, column: usize) {
        let (top, bottom) = self.scroll_region;
        self.cur_y = if self.origin_mode {
            top.saturating_add(row).min(bottom)
        } else {
            row.min(self.visible_rows.saturating_sub(1))
        };
        self.cur_x = column.min(self.cols.saturating_sub(1));
        self.wrap_pending = false;
        self.join_next = false;
    }

    fn cursor_home(&mut self) {
        self.set_cursor_position(0, 0);
    }

    fn reset_terminal_modes(&mut self) {
        self.cur_fg = TerminalColor::default_foreground();
        self.cur_bg = TerminalColor::default_background();
        self.cur_bold = false;
        self.cur_dim = false;
        self.cur_underline = false;
        self.cur_inverse = false;
        self.saved_cursor = None;
        self.alt_saved_cursor = None;
        self.scroll_region = (0, self.visible_rows.saturating_sub(1));
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.mouse_tracking_mode = MouseTrackingMode::None;
        self.mouse_tracking_mask = 0;
        self.mouse_sgr = false;
        self.bracketed_paste = false;
        self.focus_reporting = false;
        self.autowrap = true;
        self.wrap_pending = false;
        self.insert_mode = false;
        self.origin_mode = false;
        self.join_next = false;
    }

    fn soft_reset_terminal_state(&mut self) {
        self.reset_terminal_modes();
        self.cur_x = self.cur_x.min(self.cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(self.visible_rows.saturating_sub(1));
        self.dirty = true;
    }

    fn hard_reset_terminal_state(&mut self) {
        if self.is_alt
            && let Some(normal_lines) = self.alt_lines.take()
        {
            self.lines = normal_lines;
        }
        self.is_alt = false;
        self.alt_lines = None;

        self.lines.truncate(self.visible_rows);
        while self.lines.len() < self.visible_rows {
            self.lines.push_back(vec![Cell::default(); self.cols]);
        }
        for line in &mut self.lines {
            line.resize(self.cols, Cell::default());
            line.fill(Cell::default());
            repair_wide_line(line);
        }

        self.scrollback.clear();
        self.scrollback_storage_cols = self.cols;
        self.selection = None;
        self.tab_stops = default_tab_stops(self.cols);
        self.reset_terminal_modes();
        self.cur_x = 0;
        self.cur_y = 0;
        self.content_generation = self.content_generation.wrapping_add(1);
        self.dirty = true;
    }

    pub fn newline(&mut self) {
        if self.cur_y == self.scroll_region.1 {
            self.scroll_region_up(1);
        } else if self.cur_y + 1 < self.visible_rows {
            self.cur_y += 1;
        }
    }

    pub fn scroll_region_up(&mut self, rows: usize) {
        let (top, bottom) = self.scroll_region;
        if bottom >= self.lines.len() || top >= bottom {
            return;
        }
        let rows = rows.min(bottom - top + 1);
        let blank = Cell::blank_with_background(self.cur_bg);
        for _ in 0..rows {
            let mut removed = self
                .lines
                .remove(top)
                .unwrap_or_else(|| vec![blank.clone(); self.cols]);
            if top == 0 && bottom == self.visible_rows.saturating_sub(1) {
                if !self.is_alt {
                    self.scrollback_storage_cols = self.scrollback_storage_cols.max(removed.len());
                    self.scrollback.push_back(removed);
                    if self.scrollback.len() > 10000 {
                        if let Some(mut old) = self.scrollback.pop_front() {
                            if self.pool.len() < 128 {
                                old.clear();
                                self.pool.push(old);
                            }
                        }
                    }
                } else {
                    if self.pool.len() < 128 {
                        removed.clear();
                        self.pool.push(removed);
                    }
                }
            } else {
                if self.pool.len() < 128 {
                    removed.clear();
                    self.pool.push(removed);
                }
            }
            let mut new_line = self
                .pool
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(self.cols));
            new_line.resize(self.cols, blank.clone());
            new_line.fill(blank.clone());
            self.lines.insert(bottom, new_line);
        }
        self.dirty = true;
    }

    pub fn scroll_region_down(&mut self, rows: usize) {
        let (top, bottom) = self.scroll_region;
        if bottom >= self.lines.len() || top >= bottom {
            return;
        }
        let rows = rows.min(bottom - top + 1);
        let blank = Cell::blank_with_background(self.cur_bg);
        for _ in 0..rows {
            if let Some(mut removed) = self.lines.remove(bottom) {
                if self.pool.len() < 128 {
                    removed.clear();
                    self.pool.push(removed);
                }
            }
            let mut new_line = self
                .pool
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(self.cols));
            new_line.resize(self.cols, blank.clone());
            new_line.fill(blank.clone());
            self.lines.insert(top, new_line);
        }
        self.dirty = true;
    }

    pub fn get_selection_text(&self) -> String {
        if let Some((sx, sy, ex, ey)) = self.selection {
            let mut res = String::new();
            let total_lines = self.scrollback.len() + self.lines.len();
            let (start_x, start_y, end_x, end_y) = normalized_selection_bounds(sx, sy, ex, ey);

            for y in start_y..=end_y {
                if y >= total_lines {
                    continue;
                }
                let row = if y < self.scrollback.len() {
                    &self.scrollback[y]
                } else {
                    &self.lines[y - self.scrollback.len()]
                };
                let logical_len = row.len().min(self.cols);
                if logical_len > 0 {
                    let line_start = if y == start_y { start_x } else { 0 };
                    let line_end = if y == end_y {
                        end_x.min(logical_len - 1)
                    } else {
                        logical_len - 1
                    };
                    if line_start < logical_len && line_start <= line_end {
                        for cell in &row[line_start..=line_end] {
                            cell.append_text_to(&mut res);
                        }
                    }
                }
                if y != end_y {
                    res.push('\n');
                }
            }
            res.truncate(res.trim_end().len());
            res
        } else {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(grid: &mut TermGrid, bytes: &[u8]) {
        let mut parser = Parser::new();
        parser.advance(grid, bytes);
    }

    fn set_line(grid: &mut TermGrid, row: usize, text: &str) {
        for (x, ch) in text.chars().enumerate() {
            grid.lines[row][x].c = ch;
        }
    }

    fn assert_wide_line_invariant(line: &[Cell]) {
        for (column, cell) in line.iter().enumerate() {
            if cell.is_wide() {
                assert!(
                    column + 1 < line.len() && line[column + 1].is_wide_spacer(),
                    "wide base at column {column} has no spacer"
                );
            }
            if cell.is_wide_spacer() {
                assert!(
                    column > 0 && line[column - 1].is_wide(),
                    "wide spacer at column {column} has no base"
                );
            }
            if terminal_char_width(cell.c) == 2 && !cell.is_wide_spacer() {
                assert!(
                    cell.is_wide(),
                    "width-2 glyph {:?} survived as a one-cell glyph at column {column}",
                    cell.c
                );
            }
        }
    }

    #[test]
    fn terminal_spawn_error_is_ready_and_keeps_error_text_visible() {
        let title_cache = Arc::new(Mutex::new(
            crate::terminal_process::TerminalTitleState::new("terminal error".to_string()),
        ));
        let mut grid = TermGrid::new_with_title_cache(48, 3, title_cache.clone());
        write_terminal_spawn_error(&mut grid, &io::Error::other("spawn failed"));
        let grid = Arc::new(Mutex::new(grid));
        let mut terminal = Terminal {
            grid: grid.clone(),
            process: None,
            scroll_y: crate::scroll::ScrollState::new(7.0),
            presentation_intent: TerminalPresentationIntent::None,
            reveal_right_tail_when_presented: false,
            title_cache,
        };

        assert!(!terminal.is_closed());
        let mut grid = crate::platform::lock_recover(&grid);
        assert!(grid.presentation_ready);
        assert!(!grid.presentation_visible());
        grid.mark_presentation_layout_ready();
        assert!(grid.presentation_visible());
        let text = grid
            .lines
            .iter()
            .flat_map(|line| line.iter().map(|cell| cell.c))
            .collect::<String>();
        assert!(text.contains("Ronsole terminal error: spawn failed"));
    }

    #[test]
    fn terminal_grid_print_scroll_resize_and_selection_end_to_end() {
        let mut grid = TermGrid::new(4, 2);
        for ch in "abcd".chars() {
            grid.put_char(ch);
        }
        grid.put_char('e');

        assert_eq!(grid.lines[0][0].c, 'a');
        assert_eq!(grid.lines[0][3].c, 'd');
        assert_eq!(grid.lines[1][0].c, 'e');

        grid.newline();
        grid.execute(b'\r');
        grid.put_char('z');
        assert_eq!(grid.scrollback.len(), 1);
        assert_eq!(grid.lines[1][0].c, 'z');

        grid.selection = Some((0, 2, 0, 2));
        assert_eq!(grid.get_selection_text(), "z");

        grid.resize(6, 3);
        assert_eq!(grid.cols, 6);
        assert_eq!(grid.visible_rows, 3);
        assert!(grid.lines.iter().all(|line| line.len() == 6));
    }

    #[test]
    fn terminal_scrollback_and_line_pool_remain_bounded() {
        let mut grid = TermGrid::new(2, 2);
        for _ in 0..10_512 {
            grid.newline();
        }

        assert_eq!(grid.scrollback.len(), 10_000);
        assert!(grid.pool.len() <= 128);
    }

    #[test]
    fn major_column_shrink_drops_obsolete_visible_line_capacity() {
        let mut grid = TermGrid::new(4096, 2);
        let before = grid.lines[0].capacity();
        grid.resize(64, 2);
        assert!(grid.lines.iter().all(|line| line.len() == 64));
        assert!(grid.lines.iter().all(|line| line.capacity() < before));
    }

    #[test]
    fn scrollback_compaction_runs_only_for_major_column_shrink() {
        assert!(!should_compact_scrollback_storage(120, 119));
        assert!(!should_compact_scrollback_storage(120, 61));
        assert!(should_compact_scrollback_storage(120, 60));

        let mut grid = TermGrid::new(4096, 1);
        grid.scrollback.push_back(vec![Cell::default(); 4096]);
        let before_capacity = grid.scrollback[0].capacity();
        grid.resize(64, 1);
        assert_eq!(grid.scrollback[0].len(), 64);
        assert!(grid.scrollback[0].capacity() < before_capacity);
        assert!(grid.scrollback[0].capacity() <= 128);
    }

    #[test]
    fn gradual_column_shrink_uses_cumulative_scrollback_storage_watermark() {
        let mut grid = TermGrid::new(4096, 1);
        let mut history = vec![Cell::default(); 4096];
        history[0].c = 'A';
        history[79].c = 'Z';
        grid.scrollback.push_back(history);
        let original_capacity = grid.scrollback[0].capacity();
        let widths = [3000, 2000, 1200, 700, 400, 240, 140, 80];
        let mut expected_storage_cols = 4096;
        let mut expected_compactions = 0;
        let mut actual_compactions = 0;

        for width in widths {
            if should_compact_scrollback_storage(expected_storage_cols, width) {
                expected_compactions += 1;
                expected_storage_cols = width;
            }
            let before_storage_cols = grid.scrollback_storage_cols;
            grid.resize(width, 1);
            actual_compactions += usize::from(grid.scrollback_storage_cols != before_storage_cols);
        }

        assert_eq!(expected_compactions, 4);
        assert_eq!(actual_compactions, 4);
        assert_eq!(grid.scrollback_storage_cols, 80);
        assert_eq!(grid.scrollback[0].len(), 80);
        assert!(grid.scrollback[0].capacity() < original_capacity);
        assert!(grid.scrollback[0].capacity() <= 160);
        assert_eq!(grid.scrollback[0][0].c, 'A');
        assert_eq!(grid.scrollback[0][79].c, 'Z');
    }

    #[test]
    fn minor_column_shrink_does_not_reallocate_scrollback_rows() {
        let mut grid = TermGrid::new(120, 1);
        grid.scrollback.push_back(vec![Cell::default(); 120]);
        let before_len = grid.scrollback[0].len();
        let before_capacity = grid.scrollback[0].capacity();

        grid.resize(119, 1);

        assert_eq!(grid.scrollback[0].len(), before_len);
        assert_eq!(grid.scrollback[0].capacity(), before_capacity);
    }

    #[test]
    fn terminal_print_applies_text_selector_but_keeps_text_default_symbols_text() {
        let mut grid = TermGrid::new(8, 2);

        feed(&mut grid, "✔\u{FE0F}X✅\u{FE0E}Y".as_bytes());

        let mut reconstructed = String::new();
        for cell in &grid.lines[0][0..5] {
            cell.append_text_to(&mut reconstructed);
        }
        assert_eq!(reconstructed, "✔\u{FE0F}X✅\u{FE0E}Y");
        assert_eq!(grid.cur_x, 5);
        assert_eq!(grid.lines[0][0].presentation, CELL_PRESENTATION_TEXT);
        assert_eq!(grid.lines[0][1].presentation, CELL_PRESENTATION_AUTO);
        assert_eq!(grid.lines[0][2].presentation, CELL_PRESENTATION_TEXT);
        assert!(grid.lines[0][2].is_wide());
        assert!(grid.lines[0][3].is_wide_spacer());
        assert_eq!(grid.lines[0][4].presentation, CELL_PRESENTATION_AUTO);
        assert_eq!(
            terminal_presentation_selector('\u{FE0F}'),
            Some(CELL_PRESENTATION_EMOJI)
        );
        assert_eq!(
            terminal_presentation_selector('\u{FE0E}'),
            Some(CELL_PRESENTATION_TEXT)
        );
        assert!(is_terminal_zero_width_format('\u{FE0F}'));
        assert!(is_terminal_zero_width_format('\u{FE0E}'));
        assert!(is_terminal_zero_width_format('\u{200D}'));
        assert!(!is_terminal_zero_width_format('✔'));
    }

    #[test]
    fn terminal_cell_presentation_flag_keeps_cell_size_tight() {
        assert!(std::mem::size_of::<Cell>() <= 24);
        assert!(Cell::default().zero_width().is_empty());
    }

    #[test]
    fn terminal_csi_cursor_modes_colors_and_replies_end_to_end() {
        let mut grid = TermGrid::new(8, 3);
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        grid.reply_tx = Some(tx);

        feed(
            &mut grid,
            b"abc\x1b[2D!\x1b[s\x1b[3;8H?\x1b[u\x1b[31;44;1mX\x1b[22mY\x1b[38;5;200;48;5;17mZ",
        );

        assert_eq!(grid.lines[0][0].c, 'a');
        assert_eq!(grid.lines[0][1].c, '!');
        assert_eq!(grid.lines[2][7].c, '?');
        assert_eq!(
            (grid.lines[0][2].c, grid.lines[0][2].fg, grid.lines[0][2].bg),
            ('X', TerminalColor::indexed(9), TerminalColor::indexed(4))
        );
        assert_eq!(
            (grid.lines[0][3].c, grid.lines[0][3].fg, grid.lines[0][3].bg),
            ('Y', TerminalColor::indexed(1), TerminalColor::indexed(4))
        );
        assert_eq!(
            (grid.lines[0][4].c, grid.lines[0][4].fg, grid.lines[0][4].bg),
            ('Z', TerminalColor::indexed(200), TerminalColor::indexed(17))
        );

        feed(&mut grid, b"\x1b[?25l\x1b[?1h\x1b[?1000h");
        assert!(!grid.cursor_visible);
        assert!(grid.app_cursor_keys);
        assert!(grid.mouse_tracking_mode.enabled());

        feed(&mut grid, b"\x1b[6n\x1b[c\x1b]10;?\x1b\\");
        let reply_pos = rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let reply_device = rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let reply_color = rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        assert!(
            String::from_utf8(reply_pos)
                .unwrap()
                .starts_with("\x1B[1;6R")
        );
        assert_eq!(reply_device, b"\x1B[?62c");
        assert_eq!(reply_color, b"\x1B]10;rgb:ffff/ffff/ffff\x1B\\");

        feed(&mut grid, b"\x1b[?1049hALT\x1b[?1049l");
        assert!(!grid.is_alt);
        assert!(grid.alt_lines.is_none());
        assert_eq!(grid.lines[0][0].c, 'a');
    }

    #[test]
    fn full_terminal_reply_queue_never_blocks_parser_and_stays_bounded() {
        const CAPACITY: usize = 2;
        const QUERY_BATCHES: usize = 10_000;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(CAPACITY);
        reply_tx.try_send(vec![1]).unwrap();
        reply_tx.try_send(vec![2]).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);

        let worker = std::thread::spawn(move || {
            let mut grid = TermGrid::new(8, 3);
            grid.reply_tx = Some(reply_tx);
            let mut parser = Parser::new();
            for _ in 0..QUERY_BATCHES {
                parser.advance(&mut grid, b"\x1b[6n\x1b[c\x1b]10;?\x1b\\");
            }
            done_tx.send(()).unwrap();
        });

        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("full reply queue blocked terminal parser");
        worker.join().unwrap();
        assert_eq!(reply_rx.try_iter().count(), CAPACITY);
    }

    #[test]
    fn terminal_csi_erases_scroll_region_and_line_mutation_end_to_end() {
        let mut grid = TermGrid::new(5, 4);
        set_line(&mut grid, 0, "abcde");
        set_line(&mut grid, 1, "fghij");
        set_line(&mut grid, 2, "klmno");
        set_line(&mut grid, 3, "pqrst");

        feed(&mut grid, b"\x1b[2;2H\x1b[K");
        assert_eq!(grid.lines[1][0].c, 'f');
        assert_eq!(grid.lines[1][1].c, ' ');

        feed(&mut grid, b"\x1b[1;3H\x1b[1K");
        assert_eq!(grid.lines[0][0].c, ' ');
        assert_eq!(grid.lines[0][3].c, 'd');

        feed(&mut grid, b"\x1b[2J");
        assert!(
            grid.lines
                .iter()
                .flat_map(|line| line.iter())
                .all(|cell| cell.c == ' ')
        );

        set_line(&mut grid, 0, "11111");
        set_line(&mut grid, 1, "22222");
        set_line(&mut grid, 2, "33333");
        set_line(&mut grid, 3, "44444");
        feed(&mut grid, b"\x1b[2;3r\x1b[2;1H\x1b[LAAAAA\x1b[2;1H\x1b[M");
        assert_eq!(grid.scroll_region, (1, 2));
        assert_eq!(grid.cur_y, 1);

        feed(&mut grid, b"\x1b[1;1H12345\x1b[1;3H\x1b[2P");
        let top: String = grid.lines[0].iter().map(|cell| cell.c).collect();
        assert_eq!(top, "125  ");

        feed(&mut grid, b"\x1b[1;1Habcde\x1b[1;2H\x1b[3X");
        let top: String = grid.lines[0].iter().map(|cell| cell.c).collect();
        assert_eq!(top, "a   e");

        feed(&mut grid, b"\x1b[3J");
        assert!(grid.scrollback.is_empty());
    }

    #[test]
    fn csi_saved_erase_clears_history_without_touching_viewport_or_cursor() {
        let mut grid = TermGrid::new(7, 2);
        let mut old = vec![Cell::default(); 7];
        for (index, c) in "old    ".chars().enumerate() {
            old[index].c = c;
        }
        grid.scrollback.push_back(old);
        set_line(&mut grid, 0, "visible");
        set_line(&mut grid, 1, "screen ");
        grid.cur_x = 3;
        grid.cur_y = 1;
        grid.selection = Some((0, 0, 2, 0));
        let before_viewport = grid.lines.clone();
        let before_cursor = (grid.cur_x, grid.cur_y);

        feed(&mut grid, b"\x1b[3J");

        assert!(grid.scrollback.is_empty());
        assert_eq!(grid.lines, before_viewport);
        assert_eq!((grid.cur_x, grid.cur_y), before_cursor);
        assert!(grid.selection.is_none());

        feed(&mut grid, b"\x1b[2J");
        assert!(
            grid.lines
                .iter()
                .flat_map(|line| line.iter())
                .all(|cell| cell.c == ' ')
        );
    }

    #[test]
    fn insert_delete_lines_are_noop_outside_scroll_region() {
        let mut grid = TermGrid::new(5, 5);
        for (row, text) in ["11111", "22222", "33333", "44444", "55555"]
            .into_iter()
            .enumerate()
        {
            set_line(&mut grid, row, text);
        }
        feed(&mut grid, b"\x1b[2;4r\x1b[1;1H");
        let before = grid.lines.clone();
        feed(&mut grid, b"\x1b[L");
        assert_eq!(grid.lines, before);
        feed(&mut grid, b"\x1b[M");
        assert_eq!(grid.lines, before);

        feed(&mut grid, b"\x1b[2;1H\x1b[L");
        assert_eq!(
            grid.lines[0].iter().map(|cell| cell.c).collect::<String>(),
            "11111"
        );
        assert_eq!(
            grid.lines[1].iter().map(|cell| cell.c).collect::<String>(),
            "     "
        );
        assert_eq!(
            grid.lines[2].iter().map(|cell| cell.c).collect::<String>(),
            "22222"
        );
        assert_eq!(
            grid.lines[4].iter().map(|cell| cell.c).collect::<String>(),
            "55555"
        );

        feed(&mut grid, b"\x1b[2;1H\x1b[M");
        assert_eq!(
            grid.lines[0].iter().map(|cell| cell.c).collect::<String>(),
            "11111"
        );
        assert_eq!(
            grid.lines[4].iter().map(|cell| cell.c).collect::<String>(),
            "55555"
        );
    }

    #[test]
    fn terminal_resize_preserves_scrollback_saved_cursor_and_alt_buffer() {
        let mut grid = TermGrid::new(4, 4);
        set_line(&mut grid, 0, "aaaa");
        set_line(&mut grid, 1, "bbbb");
        set_line(&mut grid, 2, "cccc");
        set_line(&mut grid, 3, "dddd");
        grid.cur_y = 3;
        grid.saved_cursor = Some((1, 3));

        grid.resize(4, 2);

        assert_eq!(grid.scrollback.len(), 2);
        assert_eq!(grid.cur_y, 1);
        assert_eq!(grid.saved_cursor, Some((1, 1)));
        let row0: String = grid.lines[0].iter().map(|cell| cell.c).collect();
        let row1: String = grid.lines[1].iter().map(|cell| cell.c).collect();
        assert_eq!(row0, "cccc");
        assert_eq!(row1, "dddd");

        grid.resize(4, 4);

        assert!(grid.scrollback.is_empty());
        assert_eq!(grid.cur_y, 3);
        assert_eq!(grid.saved_cursor, Some((1, 3)));
        let row0: String = grid.lines[0].iter().map(|cell| cell.c).collect();
        let row3: String = grid.lines[3].iter().map(|cell| cell.c).collect();
        assert_eq!(row0, "aaaa");
        assert_eq!(row3, "dddd");

        feed(&mut grid, b"\x1b[?1049h");
        assert!(grid.is_alt);
        set_line(&mut grid, 0, "1111");
        set_line(&mut grid, 1, "2222");
        set_line(&mut grid, 2, "3333");
        set_line(&mut grid, 3, "4444");
        grid.cur_y = 3;

        grid.resize(6, 2);

        assert_eq!(grid.cols, 6);
        assert_eq!(grid.visible_rows, 2);
        assert!(grid.scrollback.is_empty());
        assert!(grid.lines.iter().all(|line| line.len() == 6));
        let row0: String = grid.lines[0].iter().map(|cell| cell.c).collect();
        let row1: String = grid.lines[1].iter().map(|cell| cell.c).collect();
        assert_eq!(row0, "3333  ");
        assert_eq!(row1, "4444  ");

        feed(&mut grid, b"\x1b[?1049l");

        assert!(!grid.is_alt);
        assert!(grid.alt_lines.is_none());
        let row0: String = grid.lines[0].iter().map(|cell| cell.c).collect();
        let row1: String = grid.lines[1].iter().map(|cell| cell.c).collect();
        assert_eq!(row0, "aaaa  ");
        assert_eq!(row1, "bbbb  ");
        assert_eq!(grid.cur_y, 1);
    }

    #[test]
    fn terminal_csi_defaults_scroll_modes_and_sgr_edges_end_to_end() {
        let mut grid = TermGrid::new(6, 3);

        feed(&mut grid, b"\t\x08\x08Z");
        assert_eq!((grid.cur_x, grid.cur_y), (4, 0));
        assert_eq!(grid.lines[0][3].c, 'Z');

        feed(&mut grid, b"\x1b[0G");
        assert_eq!(grid.cur_x, 0);
        feed(&mut grid, b"\x1b[99G");
        assert_eq!(grid.cur_x, 5);
        feed(&mut grid, b"\x1b[0d");
        assert_eq!(grid.cur_y, 0);
        feed(&mut grid, b"\x1b[99d");
        assert_eq!(grid.cur_y, 2);
        feed(&mut grid, b"\x1b[0;0f");
        assert_eq!((grid.cur_x, grid.cur_y), (0, 0));

        feed(
            &mut grid,
            b"\x1b[m\x1b[1;34;104mA\x1b[39;49mB\x1b[38;2;1;2;3;48;2;4;5;6mC\x1b[90;107mD\x1b[0mE\x1b[38;5;13;48;5;6mF",
        );
        assert_eq!(
            (grid.lines[0][0].c, grid.lines[0][0].fg, grid.lines[0][0].bg),
            ('A', TerminalColor::indexed(12), TerminalColor::indexed(12))
        );
        assert_eq!(
            (grid.lines[0][1].c, grid.lines[0][1].fg, grid.lines[0][1].bg),
            ('B', TerminalColor::default_foreground(), TerminalColor::default_background())
        );
        assert_eq!(grid.lines[0][2].c, 'C');
        assert_eq!(grid.lines[0][2].fg.rgb_value(), Some((1, 2, 3)));
        assert_eq!(grid.lines[0][2].bg.rgb_value(), Some((4, 5, 6)));
        assert_eq!(
            (grid.lines[0][3].c, grid.lines[0][3].fg, grid.lines[0][3].bg),
            ('D', TerminalColor::indexed(8), TerminalColor::indexed(15))
        );
        assert_eq!(
            (grid.lines[0][4].c, grid.lines[0][4].fg, grid.lines[0][4].bg),
            ('E', TerminalColor::default_foreground(), TerminalColor::default_background())
        );
        assert_eq!(
            (grid.lines[0][5].c, grid.lines[0][5].fg, grid.lines[0][5].bg),
            ('F', TerminalColor::indexed(13), TerminalColor::indexed(6))
        );
        assert!(!grid.cur_bold);

        feed(&mut grid, b"\x1b[?25l\x1b[?1h\x1b[?1002h");
        assert!(!grid.cursor_visible);
        assert!(grid.app_cursor_keys);
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::ButtonMotion);
        feed(&mut grid, b"\x1b[?1006h");
        assert!(grid.mouse_sgr);
        feed(&mut grid, b"\x1b[?25h\x1b[?1l\x1b[?1006l\x1b[?1002l");
        assert!(grid.cursor_visible);
        assert!(!grid.app_cursor_keys);
        assert!(!grid.mouse_sgr);
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::None);

        let mut scroll = TermGrid::new(4, 3);
        set_line(&mut scroll, 0, "aaaa");
        set_line(&mut scroll, 1, "bbbb");
        set_line(&mut scroll, 2, "cccc");

        feed(&mut scroll, b"\x1b[2;3r\x1b[S");
        let row0: String = scroll.lines[0].iter().map(|cell| cell.c).collect();
        let row1: String = scroll.lines[1].iter().map(|cell| cell.c).collect();
        let row2: String = scroll.lines[2].iter().map(|cell| cell.c).collect();
        assert_eq!(row0, "aaaa");
        assert_eq!(row1, "cccc");
        assert_eq!(row2, "    ");

        feed(&mut scroll, b"\x1b[T");
        let row1: String = scroll.lines[1].iter().map(|cell| cell.c).collect();
        let row2: String = scroll.lines[2].iter().map(|cell| cell.c).collect();
        assert_eq!(row1, "    ");
        assert_eq!(row2, "cccc");
    }

    #[test]
    fn terminal_osc_insert_delete_and_truecolor_edges_end_to_end() {
        let mut grid = TermGrid::new(8, 4);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(4);
        grid.reply_tx = Some(reply_tx);

        feed(&mut grid, b"\x1b]10;?\x1b\\");
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            b"\x1B]10;rgb:ffff/ffff/ffff\x1B\\".to_vec()
        );

        feed(&mut grid, b"\x1b[2;1Habcdefgh\x1b[2;3H\x1b[2P");
        let row1: String = grid.lines[1].iter().map(|cell| cell.c).collect();
        assert_eq!(row1, "abefgh  ");

        feed(&mut grid, b"\x1b[2;2H\x1b[3X");
        let row1: String = grid.lines[1].iter().map(|cell| cell.c).collect();
        assert_eq!(row1, "a   gh  ");

        set_line(&mut grid, 1, "11111111");
        set_line(&mut grid, 2, "22222222");
        set_line(&mut grid, 3, "33333333");
        feed(&mut grid, b"\x1b[2;4r\x1b[3;1H\x1b[L");
        assert_eq!(
            grid.lines[2].iter().map(|cell| cell.c).collect::<String>(),
            "        "
        );
        assert_eq!(
            grid.lines[3].iter().map(|cell| cell.c).collect::<String>(),
            "22222222"
        );

        feed(&mut grid, b"\x1b[2;1H\x1b[M");
        assert_eq!(
            grid.lines[1].iter().map(|cell| cell.c).collect::<String>(),
            "        "
        );

        feed(&mut grid, b"\x1b[38;2;1;2;3m\x1b[48;5;42mX\x1b[39;49m");
        assert!(grid.cur_bg.is_default_background());
        assert!(grid.cur_fg.is_default_foreground());
        assert_eq!(grid.lines[1][0].bg, 42);
        assert_eq!(grid.lines[1][0].fg.rgb_value(), Some((1, 2, 3)));

        let before = (grid.cols, grid.visible_rows, grid.lines.len());
        grid.resize(before.0, before.1);
        assert_eq!((grid.cols, grid.visible_rows, grid.lines.len()), before);

        grid.scroll_region = (2, 2);
        grid.scroll_region_up(1);
        grid.scroll_region_down(1);
        assert_eq!(grid.scroll_region, (2, 2));
    }

    #[test]
    fn terminal_resize_selection_and_alt_growth_edges() {
        let mut grid = TermGrid::new(3, 2);
        set_line(&mut grid, 0, "abc");
        set_line(&mut grid, 1, "def");
        let mut scrollback_cell = Cell::default();
        scrollback_cell.c = 's';
        grid.scrollback.push_back(vec![scrollback_cell; 3]);
        grid.saved_cursor = Some((2, 1));

        grid.resize(3, 4);
        assert_eq!(grid.visible_rows, 4);
        assert_eq!(grid.cur_y, 1);
        assert_eq!(grid.saved_cursor, Some((2, 2)));
        assert!(grid.scrollback.is_empty());
        assert_eq!(
            grid.lines[0].iter().map(|cell| cell.c).collect::<String>(),
            "sss"
        );

        grid.selection = Some((2, 2, 1, 0));
        assert_eq!(grid.get_selection_text(), "ss\nabc\ndef");
        grid.selection = Some((0, 99, 2, 100));
        assert_eq!(grid.get_selection_text(), "");
        grid.selection = None;
        assert_eq!(grid.get_selection_text(), "");

        feed(&mut grid, b"\x1b[?1049h");
        assert!(grid.is_alt);
        grid.alt_saved_cursor = Some((9, 9));
        grid.resize(5, 5);
        assert_eq!(grid.alt_saved_cursor, Some((9, 4)));
        assert_eq!(grid.lines.len(), 5);
        assert!(grid.lines.iter().all(|line| line.len() == 5));
        grid.resize(5, 3);
        assert_eq!(grid.lines.len(), 3);
        assert_eq!(grid.visible_rows, 3);
    }

    #[test]
    fn terminal_csi_more_erase_cursor_and_reply_edges() {
        let mut grid = TermGrid::new(5, 3);
        set_line(&mut grid, 0, "abcde");
        set_line(&mut grid, 1, "fghij");
        set_line(&mut grid, 2, "klmno");

        feed(&mut grid, b"\x1b[2;3H\x1b[J");
        assert_eq!(
            grid.lines[1].iter().map(|cell| cell.c).collect::<String>(),
            "fg   "
        );
        assert_eq!(
            grid.lines[2].iter().map(|cell| cell.c).collect::<String>(),
            "     "
        );

        set_line(&mut grid, 0, "abcde");
        set_line(&mut grid, 1, "fghij");
        set_line(&mut grid, 2, "klmno");
        feed(&mut grid, b"\x1b[2;3H\x1b[1J");
        assert_eq!(
            grid.lines[0].iter().map(|cell| cell.c).collect::<String>(),
            "     "
        );
        assert_eq!(
            grid.lines[1].iter().map(|cell| cell.c).collect::<String>(),
            "   ij"
        );

        set_line(&mut grid, 1, "fghij");
        feed(&mut grid, b"\x1b[2;3H\x1b[2K");
        assert_eq!(
            grid.lines[1].iter().map(|cell| cell.c).collect::<String>(),
            "     "
        );

        feed(&mut grid, b"\x1b[1;1H\x1b[10C\x1b[2D\x1b[2B\x1b[A");
        assert_eq!((grid.cur_x, grid.cur_y), (2, 1));

        feed(&mut grid, b"\x1b7\x1b[3;5H\x1b8");
        assert_eq!((grid.cur_x, grid.cur_y), (2, 1));

        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        grid.reply_tx = Some(tx);
        feed(&mut grid, b"\x1b]11;?\x1b\\");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_millis(50))
                .unwrap(),
            b"\x1B]11;rgb:ffff/ffff/ffff\x1B\\".to_vec()
        );

        feed(&mut grid, b"\x1b]10;?\x07");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_millis(50))
                .unwrap(),
            b"\x1B]10;rgb:ffff/ffff/ffff\x1B\\".to_vec()
        );

        feed(&mut grid, b"\x1b[38;5;123m\x1b[48;2;9;8;7mQ\x1b[999m");
        assert_eq!(grid.lines[1][2].c, 'Q');
        assert_eq!(grid.lines[1][2].fg, 123);
        assert_eq!(grid.lines[1][2].bg.rgb_value(), Some((9, 8, 7)));
    }

    #[test]
    fn wide_combining_and_common_emoji_clusters_preserve_terminal_columns() {
        let mut grid = TermGrid::new(24, 2);
        let text = "A界e\u{0301}👍🏽👨\u{200D}👩\u{200D}👧X";
        feed(&mut grid, text.as_bytes());

        assert_eq!(grid.cur_x, 9);
        assert!(grid.lines[0][1].is_wide());
        assert!(grid.lines[0][2].is_wide_spacer());
        assert_eq!(grid.lines[0][3].zero_width(), &['\u{0301}']);
        assert!(grid.lines[0][4].is_wide());
        assert!(grid.lines[0][5].is_wide_spacer());
        assert_eq!(grid.lines[0][4].zero_width(), &['🏽']);
        assert!(grid.lines[0][6].is_wide());
        assert!(grid.lines[0][7].is_wide_spacer());
        assert_eq!(
            grid.lines[0][6].zero_width(),
            &['\u{200D}', '👩', '\u{200D}', '👧']
        );

        grid.selection = Some((0, 0, 8, 0));
        assert_eq!(grid.get_selection_text(), text);

        feed(&mut grid, b"\x1b[1;3HZ");
        assert_eq!(grid.lines[0][1].c, ' ');
        assert!(!grid.lines[0][1].is_wide());
        assert_eq!(grid.lines[0][2].c, 'Z');
        assert!(!grid.lines[0][2].is_wide_spacer());
    }

    #[test]
    fn pathological_combining_cluster_is_bounded_and_safe_to_reconstruct_and_search() {
        let mut grid = TermGrid::new(8, 1);
        grid.put_char('A');
        for _ in 0..100_000 {
            grid.attach_zero_width('\u{0301}');
        }
        assert_eq!(
            grid.lines[0][0].zero_width().len(),
            crate::terminal_compat::TERMINAL_CELL_EXTRA_MAX_CHARS
        );

        grid.selection = Some((0, 0, 0, 0));
        let selected = grid.get_selection_text();
        assert_eq!(
            selected.chars().count(),
            1 + crate::terminal_compat::TERMINAL_CELL_EXTRA_MAX_CHARS
        );

        grid.content_generation = grid.content_generation.wrapping_add(1);
        let mut search = crate::search::TerminalSearchState::default();
        search.open();
        search.insert_text("A\u{0301}");
        assert!(search.recompute_if_needed(
            &grid,
            crate::search::SearchRefreshCause::User
        ));
        assert_eq!(search.results.len(), 1);
        assert_eq!(search.results[0].start_x, 0);
    }

    #[test]
    fn partial_wide_mutations_never_leave_orphan_bases_or_spacers() {
        for sequence in [
            b"\x1b[1;2H\x1b[X".as_slice(),
            b"\x1b[1;2H\x1b[P".as_slice(),
            b"\x1b[1;2H\x1b[@".as_slice(),
            b"\x1b[1;2H\x1b[4hZ\x1b[4l".as_slice(),
            b"\x1b[1;2H\x1b[K".as_slice(),
            b"\x1b[1;2H\x1b[J".as_slice(),
        ] {
            let mut grid = TermGrid::new(4, 1);
            feed(&mut grid, "界X".as_bytes());
            feed(&mut grid, sequence);
            assert_wide_line_invariant(&grid.lines[0]);
            assert_ne!(grid.lines[0][0].c, '界');
        }

        let mut resized = TermGrid::new(4, 1);
        feed(&mut resized, "AB界".as_bytes());
        assert!(resized.lines[0][2].is_wide());
        assert!(resized.lines[0][3].is_wide_spacer());
        resized.resize(3, 1);
        assert_wide_line_invariant(&resized.lines[0]);
        assert_eq!(resized.lines[0][2].c, ' ');
    }

    #[test]
    fn wide_line_repair_blanks_broken_footprints_instead_of_downgrading_width() {
        let mut orphan_base = vec![Cell::default(); 2];
        orphan_base[0].set_char(
            '界',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            true,
        );
        repair_wide_line(&mut orphan_base);
        assert_eq!(orphan_base[0].c, ' ');
        assert_wide_line_invariant(&orphan_base);

        let mut orphan_spacer = vec![Cell::default(); 2];
        orphan_spacer[1].set_wide_spacer(
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
        );
        repair_wide_line(&mut orphan_spacer);
        assert_eq!(orphan_spacer[1].c, ' ');
        assert_wide_line_invariant(&orphan_spacer);
    }

    #[test]
    fn width_two_glyph_at_right_edge_never_degrades_to_one_cell() {
        let mut no_wrap = TermGrid::new(4, 2);
        feed(&mut no_wrap, b"\x1b[?7l\x1b[1;4H");
        feed(&mut no_wrap, "界".as_bytes());
        assert_eq!((no_wrap.cur_x, no_wrap.cur_y), (3, 0));
        assert!(!no_wrap.lines[0].iter().any(|cell| cell.c == '界'));
        assert_wide_line_invariant(&no_wrap.lines[0]);

        let mut wrap = TermGrid::new(4, 2);
        wrap.lines[0][3].c = 'Q';
        feed(&mut wrap, b"\x1b[41m\x1b[1;4H");
        feed(&mut wrap, "界".as_bytes());
        assert_eq!(wrap.lines[0][3].c, ' ');
        assert_eq!(wrap.lines[0][3].bg, TerminalColor::indexed(1));
        assert_eq!(wrap.lines[1][0].c, '界');
        assert!(wrap.lines[1][0].is_wide());
        assert!(wrap.lines[1][1].is_wide_spacer());
        assert_wide_line_invariant(&wrap.lines[0]);
        assert_wide_line_invariant(&wrap.lines[1]);

        let mut one_col = TermGrid::new(1, 2);
        feed(&mut one_col, "界".as_bytes());
        assert!(!one_col.lines.iter().flatten().any(|cell| cell.c == '界'));
        for line in &one_col.lines {
            assert_wide_line_invariant(line);
        }
    }

    #[test]
    fn linefeed_below_decstbm_region_is_physical_bottom_noop() {
        let mut grid = TermGrid::new(5, 6);
        for (row, text) in ["00000", "11111", "22222", "33333", "44444", "55555"]
            .into_iter()
            .enumerate()
        {
            set_line(&mut grid, row, text);
        }
        feed(&mut grid, b"\x1b[3;5r\x1b[6;1H");
        let before = grid.lines.clone();
        feed(&mut grid, b"\n");
        assert_eq!(grid.lines, before);
        assert_eq!(grid.cur_y, 5);

        feed(&mut grid, b"\x1b[5;1H\n");
        assert_eq!(grid.lines[0].iter().map(|cell| cell.c).collect::<String>(), "00000");
        assert_eq!(grid.lines[1].iter().map(|cell| cell.c).collect::<String>(), "11111");
        assert_eq!(grid.lines[2].iter().map(|cell| cell.c).collect::<String>(), "33333");
        assert_eq!(grid.lines[3].iter().map(|cell| cell.c).collect::<String>(), "44444");
        assert_eq!(grid.lines[4].iter().map(|cell| cell.c).collect::<String>(), "     ");
        assert_eq!(grid.lines[5].iter().map(|cell| cell.c).collect::<String>(), "55555");
        assert_eq!(grid.cur_y, 4);

        let region_after_scroll = grid.lines.clone();
        feed(&mut grid, b"\x1b[2;1H\n");
        assert_eq!(grid.lines, region_after_scroll);
        assert_eq!(grid.cur_y, 2);
    }

    #[test]
    fn esc_ind_uses_same_decstbm_linefeed_semantics() {
        let mut grid = TermGrid::new(5, 6);
        for (row, text) in ["00000", "11111", "22222", "33333", "44444", "55555"]
            .into_iter()
            .enumerate()
        {
            set_line(&mut grid, row, text);
        }
        feed(&mut grid, b"\x1b[3;5r\x1b[6;1H");
        let before = grid.lines.clone();
        feed(&mut grid, b"\x1bD");
        assert_eq!(grid.lines, before);
        assert_eq!(grid.cur_y, 5);

        feed(&mut grid, b"\x1b[5;1H\x1bD");
        assert_eq!(grid.lines[2].iter().map(|cell| cell.c).collect::<String>(), "33333");
        assert_eq!(grid.lines[3].iter().map(|cell| cell.c).collect::<String>(), "44444");
        assert_eq!(grid.lines[4].iter().map(|cell| cell.c).collect::<String>(), "     ");
        assert_eq!(grid.cur_y, 4);
    }

    #[test]
    fn bce_erase_insert_delete_char_paths_use_current_background() {
        let red = TerminalColor::indexed(1);

        let mut erase_display = TermGrid::new(5, 2);
        set_line(&mut erase_display, 0, "ABCDE");
        set_line(&mut erase_display, 1, "FGHIJ");
        feed(&mut erase_display, b"\x1b[41m\x1b[2J");
        assert!(erase_display.lines.iter().flatten().all(|cell| cell.c == ' ' && cell.bg == red));

        let mut erase_line = TermGrid::new(5, 1);
        set_line(&mut erase_line, 0, "ABCDE");
        feed(&mut erase_line, b"\x1b[41m\x1b[1;3H\x1b[K");
        assert_eq!(erase_line.lines[0][1].c, 'B');
        assert!(erase_line.lines[0][2..].iter().all(|cell| cell.c == ' ' && cell.bg == red));

        let mut ech = TermGrid::new(5, 1);
        set_line(&mut ech, 0, "ABCDE");
        feed(&mut ech, b"\x1b[41m\x1b[1;2H\x1b[2X");
        assert!(ech.lines[0][1..3].iter().all(|cell| cell.c == ' ' && cell.bg == red));

        let mut dch = TermGrid::new(5, 1);
        set_line(&mut dch, 0, "ABCDE");
        feed(&mut dch, b"\x1b[41m\x1b[1;2H\x1b[P");
        assert_eq!(dch.lines[0].iter().map(|cell| cell.c).collect::<String>(), "ACDE ");
        assert_eq!(dch.lines[0][4].bg, red);

        let mut ich = TermGrid::new(5, 1);
        set_line(&mut ich, 0, "ABCDE");
        feed(&mut ich, b"\x1b[41m\x1b[1;2H\x1b[@");
        assert_eq!(ich.lines[0][1].c, ' ');
        assert_eq!(ich.lines[0][1].bg, red);
    }

    #[test]
    fn bce_line_insert_delete_and_scroll_blanks_use_current_background() {
        let red = TerminalColor::indexed(1);

        let mut il = TermGrid::new(4, 3);
        for (row, text) in ["1111", "2222", "3333"].into_iter().enumerate() {
            set_line(&mut il, row, text);
        }
        feed(&mut il, b"\x1b[41m\x1b[2;1H\x1b[L");
        assert!(il.lines[1].iter().all(|cell| cell.c == ' ' && cell.bg == red));

        let mut dl = TermGrid::new(4, 3);
        for (row, text) in ["1111", "2222", "3333"].into_iter().enumerate() {
            set_line(&mut dl, row, text);
        }
        feed(&mut dl, b"\x1b[41m\x1b[2;1H\x1b[M");
        assert!(dl.lines[2].iter().all(|cell| cell.c == ' ' && cell.bg == red));

        let mut scroll = TermGrid::new(4, 3);
        for (row, text) in ["1111", "2222", "3333"].into_iter().enumerate() {
            set_line(&mut scroll, row, text);
        }
        feed(&mut scroll, b"\x1b[41m\x1b[3;1H\n");
        assert!(scroll.lines[2].iter().all(|cell| cell.c == ' ' && cell.bg == red));
        assert_eq!(scroll.scrollback.len(), 1);
    }

    #[test]
    fn sgr_intensity_preserves_default_and_explicit_bright_identity() {
        let mut grid = TermGrid::new(8, 1);
        feed(
            &mut grid,
            b"\x1b[1mA\x1b[31;1mB\x1b[22mC\x1b[91mD\x1b[22mE\x1b[1;91mF",
        );
        assert!(grid.lines[0][0].fg.is_default_foreground());
        assert_eq!(grid.lines[0][1].fg, TerminalColor::indexed(9));
        assert_eq!(grid.lines[0][2].fg, TerminalColor::indexed(1));
        assert_eq!(grid.lines[0][3].fg, TerminalColor::indexed(9));
        assert_eq!(grid.lines[0][4].fg, TerminalColor::indexed(9));
        assert_eq!(grid.lines[0][5].fg, TerminalColor::indexed(9));
    }

    #[test]
    fn sgr_inverse_underline_dim_and_reset_are_stored_on_cells_and_wide_spacers() {
        let mut grid = TermGrid::new(12, 1);
        feed(
            &mut grid,
            b"\x1b[7mA\x1b[27mB\x1b[4mC\x1b[24mD\x1b[2mE\x1b[22mF\x1b[2;4;7m",
        );
        feed(&mut grid, "界".as_bytes());
        feed(&mut grid, b"\x1b[0mG");
        assert!(grid.lines[0][0].is_inverse());
        assert!(!grid.lines[0][1].is_inverse());
        assert!(grid.lines[0][2].is_underlined());
        assert!(!grid.lines[0][3].is_underlined());
        assert!(grid.lines[0][4].is_dim());
        assert!(!grid.lines[0][5].is_dim());
        assert!(grid.lines[0][6].is_wide());
        assert!(grid.lines[0][6].is_inverse());
        assert!(grid.lines[0][6].is_underlined());
        assert!(grid.lines[0][6].is_dim());
        assert!(grid.lines[0][7].is_wide_spacer());
        assert!(grid.lines[0][7].is_inverse());
        assert!(grid.lines[0][7].is_underlined());
        assert!(grid.lines[0][7].is_dim());
        assert!(!grid.lines[0][8].is_inverse());
        assert!(!grid.lines[0][8].is_underlined());
        assert!(!grid.lines[0][8].is_dim());
        assert_wide_line_invariant(&grid.lines[0]);
    }

    #[test]
    fn minor_column_shrink_clears_stale_selection_and_copy_never_reads_hidden_tail() {
        let mut grid = TermGrid::new(13, 1);
        let mut row = vec![Cell::default(); 13];
        for (cell, c) in row.iter_mut().zip("hit....SECRET".chars()) {
            cell.c = c;
        }
        grid.scrollback.push_back(row);
        let before_len = grid.scrollback[0].len();
        let before_capacity = grid.scrollback[0].capacity();
        grid.selection = Some((7, 0, 12, 0));

        grid.resize(7, 1);

        assert_eq!(grid.scrollback[0].len(), before_len);
        assert_eq!(grid.scrollback[0].capacity(), before_capacity);
        assert!(grid.selection.is_none());
        grid.selection = Some((7, 0, 12, 0));
        assert_eq!(grid.get_selection_text(), "");
        grid.selection = Some((0, 0, 2, 0));
        assert_eq!(grid.get_selection_text(), "hit");
    }

    #[test]
    fn bce_wide_mutations_blank_broken_footprints_with_current_background() {
        let red = TerminalColor::indexed(1);
        let mut erase = TermGrid::new(4, 1);
        feed(&mut erase, "界X".as_bytes());
        feed(&mut erase, b"\x1b[41m\x1b[1;2H\x1b[X");
        assert_eq!(erase.lines[0][0].c, ' ');
        assert_eq!(erase.lines[0][1].c, ' ');
        assert_eq!(erase.lines[0][0].bg, red);
        assert_eq!(erase.lines[0][1].bg, red);
        assert_wide_line_invariant(&erase.lines[0]);

        let mut overwrite = TermGrid::new(4, 1);
        feed(&mut overwrite, "界X".as_bytes());
        feed(&mut overwrite, b"\x1b[41m\x1b[1;2HZ");
        assert_eq!(overwrite.lines[0][0].c, ' ');
        assert_eq!(overwrite.lines[0][0].bg, red);
        assert_eq!(overwrite.lines[0][1].c, 'Z');
        assert_eq!(overwrite.lines[0][1].bg, red);
        assert_wide_line_invariant(&overwrite.lines[0]);
    }

    #[test]
    fn common_dec_modes_cover_wrap_origin_insert_tabs_mouse_focus_and_paste() {
        let mut grid = TermGrid::new(20, 4);

        feed(&mut grid, b"\t");
        assert_eq!(grid.cur_x, 8);
        feed(&mut grid, b"\x1b[I");
        assert_eq!(grid.cur_x, 16);
        feed(&mut grid, b"\x1b[Z");
        assert_eq!(grid.cur_x, 8);
        feed(&mut grid, b"\x1b[3g\r\t");
        assert_eq!(grid.cur_x, 19);
        feed(&mut grid, b"\r\x1b[5G\x1bH\r\t");
        assert_eq!(grid.cur_x, 4);

        feed(&mut grid, b"\x1b[2;4r\x1b[?6h\x1b[2;3H");
        assert_eq!((grid.cur_x, grid.cur_y), (2, 2));
        feed(&mut grid, b"\x1b[?6l");
        assert_eq!((grid.cur_x, grid.cur_y), (0, 0));

        feed(&mut grid, b"abc\x1b[1;2H\x1b[4hX\x1b[4l");
        assert_eq!(
            grid.lines[0][..4].iter().map(|cell| cell.c).collect::<String>(),
            "aXbc"
        );

        feed(
            &mut grid,
            b"\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h",
        );
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::ButtonMotion);
        assert!(grid.mouse_sgr);
        assert!(grid.focus_reporting);
        assert!(grid.bracketed_paste);
        feed(&mut grid, b"\x1b[?1003h");
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::AnyMotion);
        feed(&mut grid, b"\x1b[?1003l");
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::ButtonMotion);
        feed(&mut grid, b"\x1b[?1002l\x1b[?1006l\x1b[?1004l\x1b[?2004l");
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::None);
        assert!(!grid.mouse_sgr);
        assert!(!grid.focus_reporting);
        assert!(!grid.bracketed_paste);

        let mut wrap = TermGrid::new(3, 2);
        feed(&mut wrap, b"\x1b[?7l\x1b[1;3HAB");
        assert_eq!(wrap.lines[0][2].c, 'B');
        assert_eq!((wrap.cur_x, wrap.cur_y), (2, 0));
        feed(&mut wrap, b"\x1b[?7h\x1b[1;3HCD");
        assert_eq!(wrap.lines[0][2].c, 'C');
        assert_eq!(wrap.lines[1][0].c, 'D');
        assert_eq!((wrap.cur_x, wrap.cur_y), (1, 1));
    }

    fn assert_terminal_modes_reset(grid: &TermGrid) {
        assert!(grid.cur_fg.is_default_foreground());
        assert!(grid.cur_bg.is_default_background());
        assert!(!grid.cur_bold);
        assert!(!grid.cur_dim);
        assert!(!grid.cur_underline);
        assert!(!grid.cur_inverse);
        assert!(!grid.insert_mode);
        assert!(!grid.origin_mode);
        assert!(grid.autowrap);
        assert!(!grid.wrap_pending);
        assert!(!grid.join_next);
        assert!(!grid.app_cursor_keys);
        assert!(grid.cursor_visible);
        assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::None);
        assert_eq!(grid.mouse_tracking_mask, 0);
        assert!(!grid.mouse_sgr);
        assert!(!grid.bracketed_paste);
        assert!(!grid.focus_reporting);
        assert_eq!(grid.scroll_region, (0, grid.visible_rows.saturating_sub(1)));
        assert!(grid.saved_cursor.is_none());
        assert!(grid.alt_saved_cursor.is_none());
    }

    #[test]
    fn ris_hard_reset_restores_startup_terminal_state_without_rehiding_presentation() {
        let mut grid = TermGrid::new(16, 4);
        grid.mark_presentation_ready();
        grid.mark_presentation_layout_ready();
        grid.scrollback.push_back(vec![Cell::default(); 16]);
        grid.selection = Some((0, 0, 2, 0));
        feed(
            &mut grid,
            b"\x1b[31;44;1;2;4;7m\x1b[4h\x1b[2;3r\x1b[?6h\x1b[?7l\x1b[?1h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[2;4H\x1b[s\x1bH\x1b[?1049hALT",
        );
        assert!(grid.is_alt);
        assert!(grid.presentation_visible());
        assert!(!grid.scrollback.is_empty());

        feed(&mut grid, b"\x1bc");

        assert!(!grid.is_alt);
        assert!(grid.alt_lines.is_none());
        assert!(grid.scrollback.is_empty());
        assert!(grid.selection.is_none());
        assert_eq!((grid.cur_x, grid.cur_y), (0, 0));
        assert!(grid
            .lines
            .iter()
            .flatten()
            .all(|cell| cell.c == ' ' && cell.bg.is_default_background()));
        assert_terminal_modes_reset(&grid);
        assert_eq!(grid.tab_stops, default_tab_stops(grid.cols));
        assert!(grid.presentation_ready);
        assert!(grid.presentation_layout_ready);
        assert!(grid.presentation_visible());
        assert_eq!(grid.scrollback_storage_cols, grid.cols);
    }

    #[test]
    fn decstr_soft_reset_preserves_content_and_history_but_resets_mutable_modes() {
        let mut grid = TermGrid::new(16, 4);
        let mut history = vec![Cell::default(); 16];
        history[0].c = 'H';
        grid.scrollback.push_back(history);
        feed(
            &mut grid,
            b"VISIBLE\x1b[31;44;1;2;4;7m\x1b[4h\x1b[2;3r\x1b[?6h\x1b[?7l\x1b[?1h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[3;7H\x1b[s",
        );
        let before_lines = grid.lines.clone();
        let before_history = grid.scrollback.clone();
        let before_cursor = (grid.cur_x, grid.cur_y);

        feed(&mut grid, b"\x1b[!p");

        assert_eq!(grid.lines, before_lines);
        assert_eq!(grid.scrollback, before_history);
        assert_eq!((grid.cur_x, grid.cur_y), before_cursor);
        assert_terminal_modes_reset(&grid);
    }

    #[test]
    fn local_xterm_256color_tput_reset_sequence_resets_supported_state() {
        const XTERM_256COLOR_RESET: &[u8] =
            b"\x1bc\x1b]104\x07\x1b[!p\x1b[?3;4l\x1b[4l\x1b>\x1b[?69l";
        let mut grid = TermGrid::new(16, 4);
        grid.scrollback.push_back(vec![Cell::default(); 16]);
        feed(
            &mut grid,
            b"dirty\x1b[31;44;1;2;4;7m\x1b[4h\x1b[2;3r\x1b[?6h\x1b[?7l\x1b[?1h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?1049hALT",
        );

        feed(&mut grid, XTERM_256COLOR_RESET);

        assert_terminal_modes_reset(&grid);
        assert!(!grid.is_alt);
        assert!(grid.scrollback.is_empty());
        assert_eq!((grid.cur_x, grid.cur_y), (0, 0));
        assert!(grid.lines.iter().flatten().all(|cell| cell.c == ' '));
    }

    #[test]
    fn hostile_csi_counts_match_maximum_meaningful_geometry() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"\x1b[65535P", b"\x1b[7P"),
            (b"\x1b[65535L", b"\x1b[3L"),
            (b"\x1b[65535M", b"\x1b[3M"),
            (b"\x1b[65535S", b"\x1b[4S"),
            (b"\x1b[65535T", b"\x1b[4T"),
            (b"\x1b[65535I", b"\x1b[8I"),
            (b"\x1b[65535Z", b"\x1b[8Z"),
        ];
        for &(hostile_sequence, maximum_sequence) in cases {
            let mut hostile_grid = TermGrid::new(8, 4);
            let mut maximum_grid = TermGrid::new(8, 4);
            for grid in [&mut hostile_grid, &mut maximum_grid] {
                for (row, text) in ["abcdefgh", "ijklmnop", "qrstuvwx", "yzABCDEF"]
                    .into_iter()
                    .enumerate()
                {
                    set_line(grid, row, text);
                }
                feed(grid, b"\x1b[41m\x1b[2;2H");
            }
            feed(&mut hostile_grid, hostile_sequence);
            feed(&mut maximum_grid, maximum_sequence);
            assert_eq!(
                hostile_grid.lines, maximum_grid.lines,
                "sequence {hostile_sequence:?}"
            );
            assert_eq!(
                (hostile_grid.cur_x, hostile_grid.cur_y),
                (maximum_grid.cur_x, maximum_grid.cur_y),
                "sequence {hostile_sequence:?}"
            );
            for line in &hostile_grid.lines {
                assert_wide_line_invariant(line);
            }
        }
    }

    #[test]
    fn real_pty_shell_emits_output_and_observes_resize() {
        use std::time::{Duration, Instant};

        let mut terminal = Terminal::spawn(None, 1);
        terminal.resize_pty(120, 40);
        terminal
            .write_input(b"printf '__RONS_PTY__\\n'; stty size; exit\r")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let text = loop {
            let text = {
                let grid = crate::platform::lock_recover(&terminal.grid);
                grid.scrollback
                    .iter()
                    .chain(grid.lines.iter())
                    .flat_map(|line| line.iter().map(|cell| cell.c))
                    .collect::<String>()
            };
            if text.contains("__RONS_PTY__") && text.contains("40 120") {
                break text;
            }
            assert!(Instant::now() < deadline, "PTY output did not reach the terminal grid: {text:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(text.contains("__RONS_PTY__"));
        assert!(text.contains("40 120"));

        let deadline = Instant::now() + Duration::from_secs(3);
        while !terminal.is_closed() {
            assert!(Instant::now() < deadline, "shell did not exit after the scripted command");
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.shutdown();
    }

    #[test]
    fn bug_70_poisoned_terminal_mutex_recovers_without_ui_thread_panic() {
        let grid = Mutex::new(TermGrid::new(4, 2));
        let mut guard = crate::platform::lock_recover(&grid);
        guard.put_char('R');
        assert_eq!(guard.lines[0][0].c, 'R');
        drop(guard);

        let source = include_str!("platform.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(production.contains("unwrap_or_else(|poisoned| poisoned.into_inner())"));
        assert!(!production.contains("grid.lock().unwrap()"));
    }
}

impl Perform for TermGrid {
    fn print(&mut self, c: char) {
        if let Some(presentation) = terminal_presentation_selector(c) {
            self.apply_presentation_selector(presentation, c);
            return;
        }
        if c == '\u{200D}' {
            self.attach_zero_width(c);
            self.join_next = true;
            return;
        }
        if self.join_next {
            self.attach_zero_width(c);
            self.join_next = false;
            return;
        }
        if is_terminal_zero_width_format(c) {
            self.attach_zero_width(c);
            return;
        }
        if terminal_is_emoji_modifier(c) || terminal_char_width(c) == 0 {
            self.attach_zero_width(c);
            return;
        }
        self.put_char(c);
        if !self.presentation_ready && !c.is_whitespace() {
            self.mark_presentation_ready();
        }
    }
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\x0B' | b'\x0C' => {
                self.wrap_pending = false;
                self.join_next = false;
                self.newline();
            }
            b'\r' => {
                self.cur_x = 0;
                self.wrap_pending = false;
                self.join_next = false;
            }
            b'\x08' => {
                self.wrap_pending = false;
                self.join_next = false;
                self.cur_x = self.cur_x.saturating_sub(1);
                if self
                    .lines
                    .get(self.cur_y)
                    .and_then(|line| line.get(self.cur_x))
                    .is_some_and(Cell::is_wide_spacer)
                {
                    self.cur_x = self.cur_x.saturating_sub(1);
                }
            }
            b'\t' => self.move_to_tab_stop(1, false),
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
            && let Some(title) =
                crate::terminal_process::terminal_programmed_title(&params[1..])
            && let Some(cache) = &self.title_cache
        {
            crate::platform::lock_recover(cache).set_programmed(title);
        }

        if params.len() >= 2 && params[1] == b"?" {
            if params[0] == b"10" || params[0] == b"11" {
                let prefix = std::str::from_utf8(params[0]).unwrap_or("10");
                let msg = format!("\x1B]{};rgb:ffff/ffff/ffff\x1B\\", prefix);
                self.try_queue_reply(msg.into_bytes());
            }
        }
    }
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        self.wrap_pending = false;
        self.join_next = false;
        match byte {
            b'7' => self.saved_cursor = Some((self.cur_x, self.cur_y)),
            b'8' => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cur_x = x;
                    self.cur_y = y;
                }
            }
            b'D' => self.newline(),
            b'E' => {
                self.newline();
                self.cur_x = 0;
            }
            b'H' => {
                if let Some(stop) = self.tab_stops.get_mut(self.cur_x) {
                    *stop = true;
                }
            }
            b'M' => {
                if self.cur_y == self.scroll_region.0 {
                    self.scroll_region_down(1);
                } else {
                    self.cur_y = self.cur_y.saturating_sub(1);
                }
            }
            b'c' => self.hard_reset_terminal_state(),
            _ => {}
        }
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if action != 'm' {
            self.wrap_pending = false;
            self.join_next = false;
        }
        if action == 'p' && intermediates.contains(&b'!') {
            self.soft_reset_terminal_state();
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
                            if enable && !self.is_alt {
                                self.is_alt = true;
                                self.alt_saved_cursor = Some((self.cur_x, self.cur_y));
                                let mut alt = std::collections::VecDeque::new();
                                for _ in 0..self.visible_rows {
                                    let mut line = self
                                        .pool
                                        .pop()
                                        .unwrap_or_else(|| Vec::with_capacity(self.cols));
                                    line.resize(self.cols, Cell::default());
                                    line.fill(Cell::default());
                                    alt.push_back(line);
                                }
                                self.alt_lines = Some(std::mem::replace(&mut self.lines, alt));
                                self.cur_x = 0;
                                self.cur_y = 0;
                                self.wrap_pending = false;
                                self.dirty = true;
                            } else if !enable && self.is_alt {
                                self.is_alt = false;
                                if let Some(alt) = self.alt_lines.take() {
                                    let old_lines = std::mem::replace(&mut self.lines, alt);
                                    for mut line in old_lines {
                                        if self.pool.len() < 128 {
                                            line.clear();
                                            self.pool.push(line);
                                        }
                                    }
                                }
                                if let Some((x, y)) = self.alt_saved_cursor.take() {
                                    self.cur_x = x;
                                    self.cur_y = y;
                                }
                                self.cur_x = self.cur_x.min(self.cols.saturating_sub(1));
                                self.cur_y = self.cur_y.min(self.visible_rows.saturating_sub(1));
                                self.wrap_pending = false;
                                self.dirty = true;
                            }
                        } else {
                            match value {
                                1 => self.app_cursor_keys = enable,
                                6 => {
                                    self.origin_mode = enable;
                                    self.cursor_home();
                                }
                                7 => self.autowrap = enable,
                                25 => self.cursor_visible = enable,
                                1000 => self.set_mouse_tracking_mode(MouseTrackingMode::Press, enable),
                                1002 => self.set_mouse_tracking_mode(MouseTrackingMode::ButtonMotion, enable),
                                1003 => self.set_mouse_tracking_mode(MouseTrackingMode::AnyMotion, enable),
                                1004 => self.focus_reporting = enable,
                                1006 => self.mouse_sgr = enable,
                                2004 => self.bracketed_paste = enable,
                                _ => {}
                            }
                        }
                    }
                } else if params.iter().any(|param| param.first() == Some(&4)) {
                    self.insert_mode = enable;
                }
            }
            's' => self.saved_cursor = Some((self.cur_x, self.cur_y)),
            'u' => {
                if let Some((x, y)) = self.saved_cursor {
                    self.cur_x = x.min(self.cols.saturating_sub(1));
                    self.cur_y = y.min(self.visible_rows.saturating_sub(1));
                }
            }
            'G' | '`' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.cur_x = p.saturating_sub(1).min(self.cols.saturating_sub(1));
            }
            'd' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.set_cursor_position(p.saturating_sub(1), self.cur_x);
            }
            'c' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                if param == 0 {
                    self.try_queue_reply(b"\x1B[?62c".to_vec());
                }
            }
            'n' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let private = intermediates.contains(&b'?');
                if param == 5 && !private {
                    self.try_queue_reply(b"\x1B[0n".to_vec());
                } else if param == 6 {
                    let row = if self.origin_mode {
                        self.cur_y.saturating_sub(self.scroll_region.0) + 1
                    } else {
                        self.cur_y + 1
                    };
                    let prefix = if private { "?" } else { "" };
                    let msg = format!("\x1B[{prefix}{row};{}R", self.cur_x + 1);
                    self.try_queue_reply(msg.into_bytes());
                }
            }
            'H' | 'f' => {
                let mut iter = params.iter();
                let y = iter.next().map(|p| p[0]).unwrap_or(1) as usize;
                let x = iter.next().map(|p| p[0]).unwrap_or(1) as usize;
                let y = if y == 0 { 1 } else { y };
                let x = if x == 0 { 1 } else { x };
                self.set_cursor_position(y.saturating_sub(1), x.saturating_sub(1));
            }
            'J' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let blank = Cell::blank_with_background(self.cur_bg);
                match param {
                    0 => {
                        if let Some(line) = self.lines.get_mut(self.cur_y) {
                            if self.cur_x < line.len() {
                                line[self.cur_x..].fill(blank.clone());
                                repair_wide_line_with_blank(line, &blank);
                            }
                        }
                        for i in (self.cur_y + 1)..self.visible_rows {
                            if let Some(line) = self.lines.get_mut(i) {
                                line.fill(blank.clone());
                            }
                        }
                    }
                    1 => {
                        for i in 0..self.cur_y {
                            if let Some(line) = self.lines.get_mut(i) {
                                line.fill(blank.clone());
                            }
                        }
                        if let Some(line) = self.lines.get_mut(self.cur_y) {
                            let end = (self.cur_x + 1).min(line.len());
                            line[..end].fill(blank.clone());
                            repair_wide_line_with_blank(line, &blank);
                        }
                    }
                    2 => {
                        for line in self.lines.iter_mut() {
                            line.fill(blank.clone());
                        }
                    }
                    3 => {
                        self.scrollback.clear();
                        self.scrollback_storage_cols = self.cols;
                        self.selection = None;
                    }
                    _ => {}
                }
            }
            'K' => {
                let param = params.iter().next().map(|p| p[0]).unwrap_or(0);
                let blank = Cell::blank_with_background(self.cur_bg);
                if let Some(line) = self.lines.get_mut(self.cur_y) {
                    match param {
                        0 => {
                            if self.cur_x < line.len() {
                                line[self.cur_x..].fill(blank.clone());
                                repair_wide_line_with_blank(line, &blank);
                            }
                        }
                        1 => {
                            let end = (self.cur_x + 1).min(line.len());
                            line[..end].fill(blank.clone());
                            repair_wide_line_with_blank(line, &blank);
                        }
                        2 => {
                            line.fill(blank);
                        }
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
                    .unwrap_or(self.visible_rows as u16) as usize;
                let top = if top == 0 { 1 } else { top };
                let bottom = if bottom == 0 {
                    self.visible_rows
                } else {
                    bottom
                };
                let top_idx = top
                    .saturating_sub(1)
                    .min(self.visible_rows.saturating_sub(1));
                let bottom_idx = bottom
                    .saturating_sub(1)
                    .min(self.visible_rows.saturating_sub(1));
                if bottom_idx >= top_idx {
                    self.scroll_region = (top_idx, bottom_idx);
                }
                self.cursor_home();
            }
            'L' => {
                let (top, bottom) = self.scroll_region;
                if self.cur_y < top || self.cur_y > bottom || bottom >= self.lines.len() {
                    return;
                }
                let count = csi_count(params, bottom - self.cur_y + 1);
                let blank = Cell::blank_with_background(self.cur_bg);
                for _ in 0..count {
                    if let Some(mut line) = self.lines.remove(bottom) {
                        if self.pool.len() < 128 {
                            line.clear();
                            self.pool.push(line);
                        }
                    }
                    let mut new_line = self
                        .pool
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(self.cols));
                    new_line.resize(self.cols, blank.clone());
                    new_line.fill(blank.clone());
                    self.lines.insert(self.cur_y, new_line);
                }
            }
            'M' => {
                let (top, bottom) = self.scroll_region;
                if self.cur_y < top || self.cur_y > bottom || bottom >= self.lines.len() {
                    return;
                }
                let count = csi_count(params, bottom - self.cur_y + 1);
                let blank = Cell::blank_with_background(self.cur_bg);
                for _ in 0..count {
                    if let Some(mut line) = self.lines.remove(self.cur_y) {
                        if self.pool.len() < 128 {
                            line.clear();
                            self.pool.push(line);
                        }
                    }
                    let mut new_line = self
                        .pool
                        .pop()
                        .unwrap_or_else(|| Vec::with_capacity(self.cols));
                    new_line.resize(self.cols, blank.clone());
                    new_line.fill(blank.clone());
                    self.lines.insert(bottom, new_line);
                }
            }
            'P' => {
                let blank = Cell::blank_with_background(self.cur_bg);
                if let Some(line) = self.lines.get_mut(self.cur_y) {
                    let count = csi_count(params, line.len().saturating_sub(self.cur_x));
                    if count > 0 {
                        let tail = &mut line[self.cur_x..];
                        tail.rotate_left(count);
                        let fill_start = tail.len() - count;
                        tail[fill_start..].fill(blank.clone());
                    }
                    repair_wide_line_with_blank(line, &blank);
                }
            }
            'X' => {
                let blank = Cell::blank_with_background(self.cur_bg);
                if let Some(line) = self.lines.get_mut(self.cur_y) {
                    let start = self.cur_x.min(line.len());
                    let count = csi_count(params, line.len().saturating_sub(start));
                    let end = start + count;
                    if start < end {
                        line[start..end].fill(blank.clone());
                        repair_wide_line_with_blank(line, &blank);
                    }
                }
            }
            'm' => {
                apply_ansi_sgr(
                    params,
                    &mut self.cur_fg,
                    &mut self.cur_bg,
                    &mut self.cur_bold,
                    &mut self.cur_dim,
                    &mut self.cur_underline,
                    &mut self.cur_inverse,
                );
            }
            'C' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.cur_x = (self.cur_x + p).min(self.cols.saturating_sub(1));
            }
            'D' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                self.cur_x = self.cur_x.saturating_sub(p);
            }
            'A' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                let lower = if self.origin_mode { self.scroll_region.0 } else { 0 };
                self.cur_y = self.cur_y.saturating_sub(p).max(lower);
            }
            'B' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                let upper = if self.origin_mode {
                    self.scroll_region.1
                } else {
                    self.visible_rows.saturating_sub(1)
                };
                self.cur_y = (self.cur_y + p).min(upper);
            }
            'E' | 'F' => {
                let p = params.iter().next().map(|p| p[0]).unwrap_or(1) as usize;
                let p = if p == 0 { 1 } else { p };
                if action == 'E' {
                    self.cur_y = (self.cur_y + p).min(self.visible_rows.saturating_sub(1));
                } else {
                    self.cur_y = self.cur_y.saturating_sub(p);
                }
                self.cur_x = 0;
            }
            'I' | 'Z' => {
                let count = csi_count(params, self.cols.max(1));
                self.move_to_tab_stop(count, action == 'Z');
            }
            'g' => {
                let value = params.iter().next().map(|p| p[0]).unwrap_or(0);
                if value == 0 {
                    if let Some(stop) = self.tab_stops.get_mut(self.cur_x) {
                        *stop = false;
                    }
                } else if value == 3 {
                    self.tab_stops.fill(false);
                }
            }
            '@' => {
                let count = csi_count(params, self.cols.saturating_sub(self.cur_x));
                let blank = Cell::blank_with_background(self.cur_bg);
                if count > 0
                    && let Some(line) = self.lines.get_mut(self.cur_y)
                {
                    let tail = &mut line[self.cur_x..];
                    tail.rotate_right(count.min(tail.len()));
                    tail[..count].fill(blank.clone());
                    repair_wide_line_with_blank(line, &blank);
                }
            }
            'S' => {
                let (top, bottom) = self.scroll_region;
                let p = csi_count(params, bottom.saturating_sub(top) + 1);
                self.scroll_region_up(p);
            }
            'T' => {
                let (top, bottom) = self.scroll_region;
                let p = csi_count(params, bottom.saturating_sub(top) + 1);
                self.scroll_region_down(p);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalPresentationIntent {
    #[default]
    None,
    ActivateWhenReady,
}

pub struct Terminal {
    pub grid: Arc<Mutex<TermGrid>>,
    process: Option<crate::terminal_process::TerminalProcess>,
    pub scroll_y: crate::scroll::ScrollState,
    pub(crate) presentation_intent: TerminalPresentationIntent,
    pub(crate) reveal_right_tail_when_presented: bool,
    title_cache: crate::terminal_process::TerminalTitleCache,
}

fn write_terminal_spawn_error(grid: &mut TermGrid, error: &io::Error) {
    let message = format!("Ronsole terminal error: {error}\r\n");
    let mut parser = Parser::new();
    parser.advance(grid, message.as_bytes());
    grid.mark_presentation_ready();
}

impl Terminal {
    #[cfg(test)]
    pub(crate) fn new_for_test(cols: usize, rows: usize, display_number: u64) -> Self {
        let title_cache = Arc::new(Mutex::new(
            crate::terminal_process::TerminalTitleState::new_numbered(
                "terminal".to_string(),
                display_number,
            ),
        ));
        Self {
            grid: Arc::new(Mutex::new(TermGrid::new_with_title_cache(
                cols,
                rows,
                title_cache.clone(),
            ))),
            process: None,
            scroll_y: crate::scroll::ScrollState::new(7.0),
            presentation_intent: TerminalPresentationIntent::None,
            reveal_right_tail_when_presented: false,
            title_cache,
        }
    }

    pub fn spawn(
        window: Option<std::sync::Arc<winit::window::Window>>,
        display_number: u64,
    ) -> Self {
        let title_cache = Arc::new(Mutex::new(
            crate::terminal_process::TerminalTitleState::new_numbered(
                "terminal".to_string(),
                display_number,
            ),
        ));
        let grid = Arc::new(Mutex::new(TermGrid::new_with_title_cache(
            200,
            60,
            title_cache.clone(),
        )));
        let process = match crate::terminal_process::TerminalProcess::spawn(
            grid.clone(),
            title_cache.clone(),
            window,
        ) {
            Ok(process) => Some(process),
            Err(error) => {
                crate::platform::lock_recover(&title_cache)
                    .set_fallback("terminal error".to_string());
                let mut grid = crate::platform::lock_recover(&grid);
                write_terminal_spawn_error(&mut grid, &error);
                None
            }
        };

        Self {
            grid,
            process,
            scroll_y: crate::scroll::ScrollState::new(7.0),
            presentation_intent: TerminalPresentationIntent::None,
            reveal_right_tail_when_presented: false,
            title_cache,
        }
    }

    pub fn write_display_title(&self, output: &mut String) {
        crate::platform::lock_recover(&self.title_cache).write_resolved(output);
    }

    pub(crate) fn presentation_ready(&self) -> bool {
        crate::platform::lock_recover(&self.grid).presentation_ready
    }

    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        self.process
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "terminal is not running"))?
            .write_input(bytes)
    }

    pub fn is_closed(&mut self) -> bool {
        self.process
            .as_mut()
            .is_some_and(|process| process.try_wait().unwrap_or(true))
    }

    pub fn shutdown(&mut self) {
        if let Some(process) = self.process.as_mut() {
            process.shutdown();
        }
        self.process = None;
    }

    pub(crate) fn take_process_for_cleanup(
        &mut self,
    ) -> Option<crate::terminal_process::TerminalProcess> {
        self.process.take()
    }

    pub(crate) fn restore_process_after_cleanup_backpressure(
        &mut self,
        process: crate::terminal_process::TerminalProcess,
    ) {
        debug_assert!(self.process.is_none());
        self.process = Some(process);
    }

    pub fn resize_pty(&self, cols: u16, rows: u16) {
        if let Some(process) = self.process.as_ref() {
            let _ = process.resize(cols, rows);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}
