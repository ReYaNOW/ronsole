use crate::single_line_input::SingleLineInput;
use crate::terminal::TermGrid;
use std::time::{Duration, Instant};
#[cfg(test)]
use unicode_width::UnicodeWidthChar;

const TERMINAL_SEARCH_MAX_RESULTS: usize = 65_536;
pub(crate) const TERMINAL_SEARCH_PASSIVE_REFRESH_INTERVAL: Duration = Duration::from_millis(75);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[inline]
    pub(crate) fn contains(self, x: f32, y: f32) -> bool {
        self.w > 0.0
            && self.h > 0.0
            && x >= self.x
            && x <= self.x + self.w
            && y >= self.y
            && y <= self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TerminalSearchMatch {
    pub start_x: usize,
    pub y: usize,
    pub end_x: usize,
}

#[inline]
fn terminal_search_match_key(item: &TerminalSearchMatch) -> (usize, usize, usize) {
    (item.y, item.start_x, item.end_x)
}

#[inline]
fn terminal_search_result_position(
    results: &[TerminalSearchMatch],
    target: TerminalSearchMatch,
) -> Result<usize, usize> {
    let key = terminal_search_match_key(&target);
    results.binary_search_by_key(&key, terminal_search_match_key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchRefreshCause {
    User,
    Grid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TerminalSearchGeometry {
    pub outer: Rect,
    pub input: Rect,
    pub close: Rect,
    pub previous: Rect,
    pub next: Rect,
    pub case_toggle: Rect,
    pub text_viewport_w: f32,
    pub show_nav: bool,
    pub show_case: bool,
    pub counter_reserve: f32,
}

pub(crate) fn terminal_search_geometry(
    window_w: f32,
    body_y: f32,
    scale: f32,
) -> TerminalSearchGeometry {
    let s = scale;
    let window_w = window_w.max(0.0);
    let w = (480.0 * s).min((window_w - 16.0 * s).max(0.0));
    let x = (window_w - w - 8.0 * s).max(0.0);
    let outer = Rect {
        x,
        y: body_y + 10.0 * s,
        w,
        h: 52.0 * s,
    };

    let btn_size = 36.0 * s;
    let btn_gap = (10.0 * s).min(w * 0.025);
    let show_nav = w >= 250.0 * s;
    let show_case = w >= 330.0 * s;
    let button_count = 1 + usize::from(show_nav) * 2 + usize::from(show_case);
    let controls_w = button_count as f32 * btn_size
        + button_count.saturating_sub(1) as f32 * btn_gap;
    let counter_reserve = if w >= 235.0 * s { 52.0 * s } else { 0.0 };
    let input_w = (w - 20.0 * s - controls_w - counter_reserve - 8.0 * s).max(0.0);

    let close_size = btn_size.min(w);
    let close_x = (x + w - close_size).max(x);
    let close_y = outer.y + 8.0 * s;
    let close = Rect {
        x: close_x,
        y: close_y,
        w: close_size,
        h: close_size,
    };

    let mut button_x = close_x - close_size - btn_gap;
    let next = if show_nav {
        let rect = Rect {
            x: button_x,
            y: close_y,
            w: btn_size,
            h: btn_size,
        };
        button_x -= btn_size + btn_gap;
        rect
    } else {
        Rect::default()
    };
    let previous = if show_nav {
        let rect = Rect {
            x: button_x,
            y: close_y,
            w: btn_size,
            h: btn_size,
        };
        button_x -= btn_size + btn_gap;
        rect
    } else {
        Rect::default()
    };
    let case_toggle = if show_case {
        Rect {
            x: button_x,
            y: close_y,
            w: btn_size,
            h: btn_size,
        }
    } else {
        Rect::default()
    };

    let input = Rect {
        x: outer.x + 10.0 * s,
        y: outer.y + 11.0 * s,
        w: input_w,
        h: 30.0 * s,
    };

    TerminalSearchGeometry {
        outer,
        input,
        close,
        previous,
        next,
        case_toggle,
        text_viewport_w: (input_w - 10.0 * s).max(0.0),
        show_nav,
        show_case,
        counter_reserve,
    }
}

#[cfg(test)]
pub(crate) fn terminal_match_cell_range(
    line: &str,
    match_start: usize,
    match_end: usize,
) -> Option<(usize, usize)> {
    if match_start >= match_end
        || match_end > line.len()
        || !line.is_char_boundary(match_start)
        || !line.is_char_boundary(match_end)
    {
        return None;
    }
    let start_col = line[..match_start]
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0).min(2))
        .sum::<usize>();
    let matched_width = line[match_start..match_end]
        .chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0).min(2))
        .sum::<usize>();
    let end_exclusive_col = start_col + matched_width.max(1);
    Some((start_col, end_exclusive_col.saturating_sub(1)))
}

fn terminal_match_cell_range_from_map(
    byte_cells: &[usize],
    match_start: usize,
    match_end: usize,
) -> Option<(usize, usize)> {
    if match_start >= match_end || match_end > byte_cells.len() {
        return None;
    }
    Some((byte_cells[match_start], byte_cells[match_end - 1]))
}

pub(crate) struct TerminalSearchState {
    pub shown: bool,
    pub focused: bool,
    pub case_sensitive: bool,
    input: SingleLineInput,
    pub results: Vec<TerminalSearchMatch>,
    pub current: Option<usize>,
    query_revision: u64,
    scanned_query_revision: u64,
    scanned_grid_generation: u64,
    last_passive_scan_at: Option<Instant>,
    pending_passive_refresh_at: Option<Instant>,
    grid_selection_owned: bool,
    line_scratch: String,
    line_byte_cells: Vec<usize>,
}

impl Default for TerminalSearchState {
    fn default() -> Self {
        Self {
            shown: false,
            focused: false,
            case_sensitive: false,
            input: SingleLineInput::with_capacity(128),
            results: Vec::with_capacity(128),
            current: None,
            query_revision: 1,
            scanned_query_revision: 0,
            scanned_grid_generation: u64::MAX,
            last_passive_scan_at: None,
            pending_passive_refresh_at: None,
            grid_selection_owned: false,
            line_scratch: String::with_capacity(512),
            line_byte_cells: Vec::with_capacity(512),
        }
    }
}

impl std::ops::Deref for TerminalSearchState {
    type Target = SingleLineInput;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

impl std::ops::DerefMut for TerminalSearchState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.input
    }
}

impl TerminalSearchState {
    pub(crate) fn open(&mut self) {
        self.shown = true;
        self.focused = true;
        self.scanned_query_revision = self.query_revision.wrapping_sub(1);
        self.last_passive_scan_at = None;
        self.pending_passive_refresh_at = None;
    }

    pub(crate) fn close(&mut self) {
        self.shown = false;
        self.focused = false;
        self.current = None;
        self.results.clear();
        self.grid_selection_owned = false;
        self.last_passive_scan_at = None;
        self.pending_passive_refresh_at = None;
    }

    fn touch_query(&mut self) {
        self.query_revision = self.query_revision.wrapping_add(1);
    }

    pub(crate) fn select_all(&mut self) {
        self.input.select_all();
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        self.input.selected_text()
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        if self.input.insert_text(text) {
            self.touch_query();
        }
    }

    pub(crate) fn backspace(&mut self) {
        if self.input.backspace() {
            self.touch_query();
        }
    }

    pub(crate) fn delete_forward(&mut self) {
        if self.input.delete_forward() {
            self.touch_query();
        }
    }

    pub(crate) fn move_cursor(&mut self, new_cursor: usize, selecting: bool) {
        let _ = self.input.move_cursor(new_cursor, selecting);
    }

    pub(crate) fn move_left(&mut self, selecting: bool) {
        let _ = self.input.move_left(selecting);
    }

    pub(crate) fn move_right(&mut self, selecting: bool) {
        let _ = self.input.move_right(selecting);
    }

    pub(crate) fn toggle_case(&mut self) {
        self.case_sensitive = !self.case_sensitive;
        self.touch_query();
    }

    pub(crate) fn recompute_if_needed(
        &mut self,
        grid: &TermGrid,
        cause: SearchRefreshCause,
    ) -> bool {
        if !self.shown
            || (self.scanned_query_revision == self.query_revision
                && self.scanned_grid_generation == grid.content_generation)
        {
            return false;
        }
        let previous_active = self.active_match();
        let previous_index = self.current;
        self.scanned_query_revision = self.query_revision;
        self.scanned_grid_generation = grid.content_generation;
        self.results.clear();
        self.current = None;
        if self.input.text.is_empty() {
            return true;
        }
        let escaped = regex::escape(&self.input.text);
        let Ok(regex) = regex::RegexBuilder::new(&escaped)
            .case_insensitive(!self.case_sensitive)
            .build()
        else {
            return true;
        };

        let scrollback_len = if grid.is_alt { 0 } else { grid.scrollback.len() };
        let total_lines = scrollback_len + grid.lines.len();
        'rows: for y in 0..total_lines {
            let row = if y < scrollback_len {
                &grid.scrollback[y]
            } else {
                &grid.lines[y - scrollback_len]
            };
            self.line_scratch.clear();
            self.line_byte_cells.clear();
            for (cell_index, cell) in row.iter().take(grid.cols).enumerate() {
                let before = self.line_scratch.len();
                cell.append_text_to(&mut self.line_scratch);
                self.line_byte_cells
                    .resize(self.line_scratch.len(), cell_index);
                debug_assert!(before <= self.line_scratch.len());
            }
            for found in regex.find_iter(&self.line_scratch) {
                if let Some((start_x, end_x)) =
                    terminal_match_cell_range_from_map(&self.line_byte_cells, found.start(), found.end())
                {
                    self.results.push(TerminalSearchMatch { start_x, y, end_x });
                    if self.results.len() >= TERMINAL_SEARCH_MAX_RESULTS {
                        break 'rows;
                    }
                }
            }
        }
        if !self.results.is_empty() {
            self.current = Some(match cause {
                SearchRefreshCause::User => self.results.len() - 1,
                SearchRefreshCause::Grid => previous_active
                    .map(|active| match terminal_search_result_position(&self.results, active) {
                        Ok(index) => index,
                        Err(insertion) => insertion.min(self.results.len() - 1),
                    })
                    .or_else(|| previous_index.map(|index| index.min(self.results.len() - 1)))
                    .unwrap_or(self.results.len() - 1),
            });
        }
        true
    }

    pub(crate) fn refresh_for_grid(
        &mut self,
        grid: &mut TermGrid,
        cause: SearchRefreshCause,
    ) -> bool {
        self.refresh_for_grid_at(grid, cause, Instant::now())
    }

    fn refresh_for_grid_at(
        &mut self,
        grid: &mut TermGrid,
        cause: SearchRefreshCause,
        now: Instant,
    ) -> bool {
        let stale_query = self.scanned_query_revision != self.query_revision;
        let stale_grid = self.scanned_grid_generation != grid.content_generation;
        if !self.shown || (!stale_query && !stale_grid) {
            self.pending_passive_refresh_at = None;
            return false;
        }

        if cause == SearchRefreshCause::Grid && !stale_query {
            if let Some(last_scan) = self.last_passive_scan_at {
                let deadline = last_scan + TERMINAL_SEARCH_PASSIVE_REFRESH_INTERVAL;
                if now < deadline {
                    self.pending_passive_refresh_at = Some(deadline);
                    return false;
                }
            }
        }

        let changed = self.recompute_if_needed(grid, cause);
        if changed {
            if cause == SearchRefreshCause::Grid {
                self.last_passive_scan_at = Some(now);
            }
            self.pending_passive_refresh_at = None;
        }
        if changed && self.grid_selection_owned {
            if let Some(found) = self.active_match() {
                grid.selection = Some((found.start_x, found.y, found.end_x, found.y));
            } else {
                grid.selection = None;
                self.grid_selection_owned = false;
            }
        }
        changed
    }

    pub(crate) fn pending_passive_refresh_deadline(&self) -> Option<Instant> {
        self.shown.then_some(self.pending_passive_refresh_at).flatten()
    }

    pub(crate) fn set_active_grid_selection(
        &mut self,
        grid: &mut TermGrid,
    ) -> Option<TerminalSearchMatch> {
        let found = self.active_match();
        if let Some(found) = found {
            grid.selection = Some((found.start_x, found.y, found.end_x, found.y));
            self.grid_selection_owned = true;
        } else if self.grid_selection_owned {
            grid.selection = None;
            self.grid_selection_owned = false;
        }
        found
    }

    pub(crate) fn release_grid_selection_ownership(&mut self) {
        self.grid_selection_owned = false;
    }

    pub(crate) fn owns_grid_selection(&self) -> bool {
        self.grid_selection_owned
    }

    #[cfg(test)]
    pub(crate) fn scanned_grid_generation_for_test(&self) -> u64 {
        self.scanned_grid_generation
    }

    pub(crate) fn next(&mut self) {
        if self.results.is_empty() {
            self.current = None;
            return;
        }
        self.current = Some((self.current.unwrap_or(0) + 1) % self.results.len());
    }

    pub(crate) fn previous(&mut self) {
        if self.results.is_empty() {
            self.current = None;
            return;
        }
        let current = self.current.unwrap_or(0);
        self.current = Some(if current == 0 { self.results.len() - 1 } else { current - 1 });
    }

    pub(crate) fn active_match(&self) -> Option<TerminalSearchMatch> {
        self.current.and_then(|index| self.results.get(index).copied())
    }

    pub(crate) fn row_matches(&self, row: usize) -> &[TerminalSearchMatch] {
        let start = self.results.partition_point(|item| item.y < row);
        let end = self.results[start..].partition_point(|item| item.y == row) + start;
        &self.results[start..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TermGrid;

    fn set_row(grid: &mut TermGrid, row: usize, text: &str) {
        for (x, c) in text.chars().enumerate() {
            grid.lines[row][x].c = c;
        }
        grid.content_generation = grid.content_generation.wrapping_add(1);
    }

    #[test]
    fn utf8_search_offsets_are_terminal_columns() {
        let line = "é界needle";
        let start = line.find("needle").unwrap();
        let end = start + "needle".len();
        assert_eq!(terminal_match_cell_range(line, start, end), Some((3, 8)));
    }

    #[test]
    fn search_maps_wide_and_combining_text_to_physical_grid_columns() {
        let mut grid = TermGrid::new(32, 1);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, "é界e\u{0301}needle".as_bytes());

        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(
            search.active_match(),
            Some(TerminalSearchMatch {
                start_x: 4,
                y: 0,
                end_x: 9,
            })
        );

        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("e\u{0301}");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(
            search.active_match(),
            Some(TerminalSearchMatch {
                start_x: 3,
                y: 0,
                end_x: 3,
            })
        );
    }

    #[test]
    fn search_uses_reflowed_scrollback_rows_after_width_change() {
        let mut grid = TermGrid::new(12, 1);
        let mut history = vec![crate::terminal::Cell::default(); 12];
        for (index, c) in "hit   needle".chars().enumerate() {
            history[index].c = c;
        }
        grid.push_scrollback_row_for_test(history, 12, false);

        grid.resize(7, 1);
        assert_eq!(grid.cols, 7);
        assert_eq!(grid.scrollback.len(), 2);
        assert!(grid.scrollback.iter().all(|row| row.len() == 7));

        let mut second_row = TerminalSearchState::default();
        second_row.open();
        second_row.insert_text("eedle");
        assert!(second_row.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(
            second_row.active_match(),
            Some(TerminalSearchMatch {
                start_x: 0,
                y: 1,
                end_x: 4,
            })
        );

        let mut first_row = TerminalSearchState::default();
        first_row.open();
        first_row.insert_text("hit");
        assert!(first_row.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(
            first_row.active_match(),
            Some(TerminalSearchMatch {
                start_x: 0,
                y: 0,
                end_x: 2,
            })
        );
    }

    #[test]
    fn search_reuses_results_until_query_or_grid_generation_changes() {
        let mut grid = TermGrid::new(32, 2);
        set_row(&mut grid, 0, "Alpha alpha");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("alpha");
        search.recompute_if_needed(&grid, SearchRefreshCause::User);
        assert_eq!(search.results.len(), 2);
        search.toggle_case();
        search.recompute_if_needed(&grid, SearchRefreshCause::User);
        assert_eq!(search.results.len(), 1);
        set_row(&mut grid, 1, "alpha");
        search.recompute_if_needed(&grid, SearchRefreshCause::User);
        assert_eq!(search.results.len(), 2);
    }

    #[test]
    fn reopening_search_rebuilds_results_after_close_clears_them() {
        let mut grid = TermGrid::new(16, 1);
        set_row(&mut grid, 0, "needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(search.results.len(), 1);
        search.close();
        assert!(search.results.is_empty());
        search.open();
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(search.results.len(), 1);
    }

    #[test]
    fn search_results_remain_bounded_under_match_flood() {
        let mut grid = TermGrid::new(256, 300);
        for row in &mut grid.lines {
            row.fill(crate::terminal::Cell::default());
            for cell in row {
                cell.c = 'a';
            }
        }
        grid.content_generation = grid.content_generation.wrapping_add(1);
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("a");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(search.results.len(), TERMINAL_SEARCH_MAX_RESULTS);
    }

    #[test]
    fn passive_grid_refresh_preserves_exact_active_match() {
        let mut grid = TermGrid::new(32, 4);
        set_row(&mut grid, 0, "needle");
        set_row(&mut grid, 1, "needle");
        set_row(&mut grid, 2, "needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        search.current = Some(1);
        let before = search.active_match().unwrap();
        assert_eq!(search.set_active_grid_selection(&mut grid), Some(before));

        set_row(&mut grid, 3, "unrelated output");
        assert!(search.refresh_for_grid(&mut grid, SearchRefreshCause::Grid));

        assert_eq!(search.active_match(), Some(before));
        assert_eq!(search.current, Some(1));
        assert_eq!(grid.selection, Some((before.start_x, before.y, before.end_x, before.y)));
    }

    #[test]
    fn passive_refresh_preserves_cross_row_match_when_start_x_decreases() {
        let mut grid = TermGrid::new(32, 3);
        set_row(&mut grid, 0, "..........needle");
        set_row(&mut grid, 1, "needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(
            search.results,
            vec![
                TerminalSearchMatch {
                    start_x: 10,
                    y: 0,
                    end_x: 15,
                },
                TerminalSearchMatch {
                    start_x: 0,
                    y: 1,
                    end_x: 5,
                },
            ]
        );

        search.current = Some(0);
        let active = search.active_match().unwrap();
        set_row(&mut grid, 2, "unrelated output");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::Grid));

        assert_eq!(search.active_match(), Some(active));
        assert_eq!(search.current, Some(0));
    }

    #[test]
    fn passive_refresh_fallback_uses_row_major_insertion_order() {
        let mut grid = TermGrid::new(40, 3);
        set_row(&mut grid, 0, "..........needle");
        set_row(&mut grid, 1, "needle");
        set_row(&mut grid, 2, "....................needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        search.current = Some(0);
        let removed = search.active_match().unwrap();
        assert_eq!(
            removed,
            TerminalSearchMatch {
                start_x: 10,
                y: 0,
                end_x: 15,
            }
        );

        grid.lines[0].fill(crate::terminal::Cell::default());
        grid.content_generation = grid.content_generation.wrapping_add(1);
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::Grid));

        assert_eq!(
            search.active_match(),
            Some(TerminalSearchMatch {
                start_x: 0,
                y: 1,
                end_x: 5,
            })
        );
        assert_eq!(search.current, Some(0));
    }

    #[test]
    fn terminal_search_match_key_is_explicitly_row_major() {
        let first = TerminalSearchMatch {
            start_x: 10,
            y: 0,
            end_x: 15,
        };
        let second = TerminalSearchMatch {
            start_x: 0,
            y: 1,
            end_x: 5,
        };
        assert!(terminal_search_match_key(&first) < terminal_search_match_key(&second));
        assert_eq!(terminal_search_match_key(&first), (0, 10, 15));
        assert_eq!(terminal_search_match_key(&second), (1, 0, 5));
    }

    #[test]
    fn passive_new_matching_output_does_not_steal_current_result() {
        let mut grid = TermGrid::new(32, 4);
        set_row(&mut grid, 0, "needle");
        set_row(&mut grid, 1, "needle");
        set_row(&mut grid, 2, "needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        search.current = Some(0);
        let before = search.active_match().unwrap();
        search.set_active_grid_selection(&mut grid);

        set_row(&mut grid, 3, "new needle");
        assert!(search.refresh_for_grid(&mut grid, SearchRefreshCause::Grid));

        assert_eq!(search.results.len(), 4);
        assert_eq!(search.active_match(), Some(before));
        assert_ne!(search.current, Some(search.results.len() - 1));
        assert_eq!(grid.selection, Some((before.start_x, before.y, before.end_x, before.y)));
    }

    #[test]
    fn passive_refresh_uses_nearby_fallback_and_clears_stale_selection_when_empty() {
        let mut grid = TermGrid::new(32, 3);
        set_row(&mut grid, 0, "needle");
        set_row(&mut grid, 1, "needle");
        set_row(&mut grid, 2, "needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        search.current = Some(1);
        let removed = search.active_match().unwrap();
        search.set_active_grid_selection(&mut grid);
        let now = Instant::now();

        grid.lines[1].fill(crate::terminal::Cell::default());
        grid.content_generation = grid.content_generation.wrapping_add(1);
        assert!(search.refresh_for_grid_at(&mut grid, SearchRefreshCause::Grid, now));
        let fallback = search.active_match().expect("nearby result should remain");
        assert_ne!(fallback, removed);
        assert_eq!(fallback.y, 2);
        assert!(search.current.is_some_and(|index| index < search.results.len()));
        assert_eq!(grid.selection, Some((fallback.start_x, fallback.y, fallback.end_x, fallback.y)));

        for row in &mut grid.lines {
            row.fill(crate::terminal::Cell::default());
        }
        grid.content_generation = grid.content_generation.wrapping_add(1);
        assert!(search.refresh_for_grid_at(
            &mut grid,
            SearchRefreshCause::Grid,
            now + TERMINAL_SEARCH_PASSIVE_REFRESH_INTERVAL,
        ));
        assert!(search.results.is_empty());
        assert_eq!(search.current, None);
        assert_eq!(grid.selection, None);
    }

    #[test]
    fn passive_search_refresh_coalesces_generation_flood_and_finishes_after_quiescence() {
        let mut grid = TermGrid::new(32, 2);
        set_row(&mut grid, 0, "needle");
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle");
        let base = Instant::now();
        assert!(search.refresh_for_grid_at(&mut grid, SearchRefreshCause::User, base));

        let mut passive_scan_count = 0usize;
        for update in 0..100u64 {
            grid.content_generation = grid.content_generation.wrapping_add(1);
            let now = base + Duration::from_micros(update * 100);
            passive_scan_count += usize::from(search.refresh_for_grid_at(
                &mut grid,
                SearchRefreshCause::Grid,
                now,
            ));
        }
        assert_eq!(passive_scan_count, 1);
        let latest_generation = grid.content_generation;
        let deadline = search
            .pending_passive_refresh_deadline()
            .expect("coalesced passive search needs a final wakeup");
        assert_eq!(deadline, base + TERMINAL_SEARCH_PASSIVE_REFRESH_INTERVAL);
        assert_ne!(search.scanned_grid_generation, latest_generation);

        assert!(search.refresh_for_grid_at(
            &mut grid,
            SearchRefreshCause::Grid,
            deadline,
        ));
        assert_eq!(search.scanned_grid_generation, latest_generation);
        assert!(search.pending_passive_refresh_deadline().is_none());

        search.insert_text("x");
        grid.content_generation = grid.content_generation.wrapping_add(1);
        assert!(search.refresh_for_grid_at(
            &mut grid,
            SearchRefreshCause::User,
            deadline + Duration::from_millis(1),
        ));
        assert_eq!(search.scanned_grid_generation, grid.content_generation);
        assert!(search.pending_passive_refresh_deadline().is_none());
    }

    #[test]
    fn search_editor_handles_utf8_selection_without_byte_cursor_mixing() {
        let mut search = TerminalSearchState::default();
        search.insert_text("a界b");
        search.move_cursor(1, false);
        search.move_right(true);
        assert_eq!(search.selected_text().as_deref(), Some("界"));
        search.insert_text("Z");
        assert_eq!(search.text, "aZb");
        assert_eq!(search.cursor, 2);
    }

    #[test]
    fn search_geometry_matches_rriter_thresholds_and_stays_inside_narrow_windows() {
        let widths = [0.0, 8.0, 20.0, 35.0, 50.0, 120.0, 180.0, 249.0, 250.0, 329.0, 330.0, 480.0, 900.0];
        for scale in [1.0, 1.333_333_3] {
            for window_w in widths {
                let geometry = terminal_search_geometry(window_w, 13.0, scale);
                let rects = [
                    geometry.outer,
                    geometry.input,
                    geometry.close,
                    geometry.previous,
                    geometry.next,
                    geometry.case_toggle,
                ];
                for rect in rects {
                    assert!(rect.x.is_finite());
                    assert!(rect.y.is_finite());
                    assert!(rect.w.is_finite());
                    assert!(rect.h.is_finite());
                    assert!(rect.w >= 0.0);
                    assert!(rect.h >= 0.0);
                }

                assert!(geometry.outer.x >= -0.001);
                assert!(geometry.outer.x + geometry.outer.w <= window_w.max(0.0) + 0.001);
                assert_eq!(geometry.show_nav, geometry.outer.w >= 250.0 * scale);
                assert_eq!(geometry.show_case, geometry.outer.w >= 330.0 * scale);

                for rect in [
                    geometry.input,
                    geometry.close,
                    geometry.previous,
                    geometry.next,
                    geometry.case_toggle,
                ] {
                    if rect.w > 0.0 && rect.h > 0.0 {
                        assert!(rect.x >= geometry.outer.x - 0.001);
                        assert!(rect.x + rect.w <= geometry.outer.x + geometry.outer.w + 0.001);
                    }
                }

                let control_left = [
                    geometry.close,
                    geometry.previous,
                    geometry.next,
                    geometry.case_toggle,
                ]
                .into_iter()
                .filter(|rect| rect.w > 0.0)
                .map(|rect| rect.x)
                .fold(geometry.outer.x + geometry.outer.w, f32::min);
                if geometry.input.w > 0.0 {
                    assert!(geometry.input.x + geometry.input.w <= control_left + 0.001);
                }
                assert!(geometry.close.x + geometry.close.w <= geometry.outer.x + geometry.outer.w + 0.001);
                assert!(geometry.text_viewport_w >= 0.0);
            }
        }
    }
}
