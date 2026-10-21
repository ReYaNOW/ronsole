//! Grid storage, scrollback, reflow, selection and mode resets for [`Term`].

use crate::cell::{
    CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT, Cell, Pen, terminal_char_width,
    terminal_effective_foreground, terminal_force_text_presentation,
};
use crate::input::MouseTrackingMode;
use crate::term::Term;
use vte::Params;

#[inline(always)]
pub fn normalized_selection_bounds(
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

pub(crate) fn default_tab_stops(cols: usize) -> Vec<bool> {
    let mut stops = vec![false; cols];
    for column in (8..cols).step_by(8) {
        stops[column] = true;
    }
    stops
}

pub(crate) fn clear_wide_footprint(line: &mut [Cell], column: usize, blank: &Cell) {
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

pub(crate) fn compact_line_capacity_after_resize(line: &mut Vec<Cell>, cols: usize) {
    let target = cols.max(1).saturating_mul(2);
    let threshold = target.saturating_mul(2).max(512);
    if line.capacity() <= threshold || line.len() > target {
        return;
    }
    let mut compact = Vec::with_capacity(target.max(line.len()));
    compact.extend(line.iter().cloned());
    *line = compact;
}

pub(crate) fn repair_wide_line_with_blank(line: &mut [Cell], blank: &Cell) {
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

pub(crate) fn repair_wide_line(line: &mut [Cell]) {
    repair_wide_line_with_blank(line, &Cell::default());
}

#[inline]
pub(crate) fn csi_count(params: &Params, max: usize) -> usize {
    let raw = params
        .iter()
        .next()
        .and_then(|param| param.first())
        .copied()
        .unwrap_or(1);
    usize::from(raw.max(1)).min(max)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineMeta {
    pub(crate) soft_wrapped: bool,
    pub(crate) used_cols: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReflowAnchor {
    row: usize,
    column: usize,
    wrap_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReflowedAnchor {
    row: usize,
    column: usize,
    wrap_pending: bool,
}

pub(crate) struct ReflowResult {
    scrollback: std::collections::VecDeque<Vec<Cell>>,
    scrollback_meta: std::collections::VecDeque<LineMeta>,
    lines: std::collections::VecDeque<Vec<Cell>>,
    line_meta: std::collections::VecDeque<LineMeta>,
    anchors: Vec<Option<ReflowedAnchor>>,
}

pub(crate) fn remove_line_with_meta(
    lines: &mut std::collections::VecDeque<Vec<Cell>>,
    meta: &mut std::collections::VecDeque<LineMeta>,
    index: usize,
) -> Option<(Vec<Cell>, LineMeta)> {
    debug_assert_eq!(lines.len(), meta.len());
    let line = lines.remove(index)?;
    let state = meta.remove(index).unwrap_or_default();
    debug_assert_eq!(lines.len(), meta.len());
    Some((line, state))
}

pub(crate) fn insert_line_with_meta(
    lines: &mut std::collections::VecDeque<Vec<Cell>>,
    meta: &mut std::collections::VecDeque<LineMeta>,
    index: usize,
    line: Vec<Cell>,
    state: LineMeta,
) {
    debug_assert_eq!(lines.len(), meta.len());
    let index = index.min(lines.len());
    lines.insert(index, line);
    meta.insert(index, state);
    debug_assert_eq!(lines.len(), meta.len());
}

pub(crate) fn resize_physical_columns(
    lines: &mut std::collections::VecDeque<Vec<Cell>>,
    meta: &mut std::collections::VecDeque<LineMeta>,
    new_cols: usize,
) {
    debug_assert_eq!(lines.len(), meta.len());
    for (line, state) in lines.iter_mut().zip(meta.iter_mut()) {
        line.resize(new_cols, Cell::default());
        repair_wide_line(line);
        compact_line_capacity_after_resize(line, new_cols);
        state.used_cols = state.used_cols.min(new_cols);
    }
}

pub(crate) fn semantic_used_cols(line: &[Cell], state: LineMeta, cols: usize) -> usize {
    let styled_used_cols = line
        .iter()
        .take(cols)
        .rposition(|cell| *cell != Cell::default())
        .map_or(0, |column| column + 1);
    state
        .used_cols
        .max(styled_used_cols)
        .min(cols)
        .min(line.len())
}

pub(crate) fn emit_reflowed_logical_line(
    mut cells: Vec<Cell>,
    anchors: &[(usize, usize, bool)],
    new_cols: usize,
    output_lines: &mut std::collections::VecDeque<Vec<Cell>>,
    output_meta: &mut std::collections::VecDeque<LineMeta>,
    mapped: &mut [Option<ReflowedAnchor>],
) {
    debug_assert!(new_cols >= 2);
    if let Some(max_anchor) = anchors.iter().map(|(_, offset, _)| *offset).max()
        && cells.len() < max_anchor
    {
        cells.resize(max_anchor, Cell::default());
    }

    let mut row = vec![Cell::default(); new_cols];
    let mut row_used = 0usize;
    let mut logical_offset = 0usize;
    let mut index = 0usize;

    let map_at = |mapped: &mut [Option<ReflowedAnchor>],
                  logical_offset: usize,
                  row: usize,
                  column: usize,
                  wrap_pending: bool,
                  pending_only: Option<bool>| {
        for &(anchor_index, offset, was_pending) in anchors {
            if offset != logical_offset || mapped[anchor_index].is_some() {
                continue;
            }
            if let Some(required_pending) = pending_only
                && was_pending != required_pending
            {
                continue;
            }
            mapped[anchor_index] = Some(ReflowedAnchor {
                row,
                column,
                wrap_pending: wrap_pending && was_pending,
            });
        }
    };

    if cells.is_empty() {
        let row_index = output_lines.len();
        map_at(mapped, 0, row_index, 0, false, None);
        output_lines.push_back(row);
        output_meta.push_back(LineMeta::default());
        return;
    }

    while index < cells.len() {
        let wide =
            cells[index].is_wide() && index + 1 < cells.len() && cells[index + 1].is_wide_spacer();

        if row_used == new_cols {
            let row_index = output_lines.len();
            map_at(
                mapped,
                logical_offset,
                row_index,
                new_cols - 1,
                true,
                Some(true),
            );
            output_lines.push_back(row);
            output_meta.push_back(LineMeta {
                soft_wrapped: true,
                used_cols: new_cols,
            });
            row = vec![Cell::default(); new_cols];
            row_used = 0;
            let row_index = output_lines.len();
            map_at(mapped, logical_offset, row_index, 0, false, None);
            continue;
        }

        if wide && row_used + 2 > new_cols {
            output_lines.push_back(row);
            output_meta.push_back(LineMeta {
                soft_wrapped: true,
                used_cols: row_used,
            });
            row = vec![Cell::default(); new_cols];
            row_used = 0;
            let row_index = output_lines.len();
            map_at(mapped, logical_offset, row_index, 0, false, None);
            continue;
        }

        let row_index = output_lines.len();
        map_at(mapped, logical_offset, row_index, row_used, false, None);

        if wide {
            row[row_used] = cells[index].clone();
            map_at(
                mapped,
                logical_offset + 1,
                row_index,
                row_used + 1,
                false,
                None,
            );
            row[row_used + 1] = cells[index + 1].clone();
            row_used += 2;
            logical_offset += 2;
            index += 2;
        } else {
            row[row_used] = cells[index].clone();
            row_used += 1;
            logical_offset += 1;
            index += 1;
        }
    }

    let row_index = output_lines.len();
    if row_used == new_cols {
        map_at(
            mapped,
            logical_offset,
            row_index,
            new_cols - 1,
            true,
            Some(true),
        );
        map_at(
            mapped,
            logical_offset,
            row_index,
            new_cols - 1,
            false,
            Some(false),
        );
    } else {
        map_at(mapped, logical_offset, row_index, row_used, false, None);
    }
    output_lines.push_back(row);
    output_meta.push_back(LineMeta {
        soft_wrapped: false,
        used_cols: row_used,
    });
}

pub(crate) fn reflow_normal_buffer(
    mut scrollback: std::collections::VecDeque<Vec<Cell>>,
    mut scrollback_meta: std::collections::VecDeque<LineMeta>,
    mut lines: std::collections::VecDeque<Vec<Cell>>,
    mut line_meta: std::collections::VecDeque<LineMeta>,
    columns: (usize, usize),
    visible_rows: usize,
    anchors: &[ReflowAnchor],
    scrollback_limit: usize,
) -> ReflowResult {
    let (old_cols, new_cols) = columns;
    debug_assert!(new_cols >= 2);
    debug_assert_eq!(scrollback.len(), scrollback_meta.len());
    debug_assert_eq!(lines.len(), line_meta.len());

    let old_scrollback_len = scrollback.len();
    let mut last_keep = anchors
        .iter()
        .filter_map(|anchor| anchor.row.checked_sub(old_scrollback_len))
        .filter(|row| *row < lines.len())
        .max()
        .unwrap_or(0);
    for row in 0..lines.len() {
        let state = line_meta.get(row).copied().unwrap_or_default();
        let continues_from_previous = row > 0
            && line_meta
                .get(row - 1)
                .is_some_and(|previous| previous.soft_wrapped);
        let used_cols = lines
            .get(row)
            .map_or(state.used_cols.min(old_cols), |line| {
                semantic_used_cols(line, state, old_cols)
            });
        if used_cols > 0 || state.soft_wrapped || continues_from_previous {
            last_keep = last_keep.max(row);
        }
    }
    let keep_lines = if lines.is_empty() {
        0
    } else {
        (last_keep + 1).min(lines.len())
    };
    while lines.len() > keep_lines {
        let index = lines.len() - 1;
        let _ = remove_line_with_meta(&mut lines, &mut line_meta, index);
    }

    scrollback.append(&mut lines);
    scrollback_meta.append(&mut line_meta);

    let mut output_lines = std::collections::VecDeque::new();
    let mut output_meta = std::collections::VecDeque::new();
    let mut mapped = vec![None; anchors.len()];
    let mut logical_cells = Vec::new();
    let mut logical_anchors = Vec::new();
    let mut input_row = 0usize;

    while !scrollback.is_empty() {
        let Some((mut line, state)) =
            remove_line_with_meta(&mut scrollback, &mut scrollback_meta, 0)
        else {
            break;
        };
        line.truncate(old_cols);
        repair_wide_line(&mut line);

        let logical_base = logical_cells.len();
        let mut used_cols = semantic_used_cols(&line, state, old_cols);
        for (anchor_index, anchor) in anchors.iter().enumerate() {
            if anchor.row == input_row {
                let column = anchor.column.min(old_cols);
                used_cols = used_cols.max(column.min(line.len()));
                logical_anchors.push((
                    anchor_index,
                    logical_base + column.min(line.len()),
                    anchor.wrap_pending,
                ));
            }
        }
        if used_cols > 0
            && line.get(used_cols - 1).is_some_and(Cell::is_wide)
            && used_cols < line.len()
            && line.get(used_cols).is_some_and(Cell::is_wide_spacer)
        {
            used_cols += 1;
        }
        logical_cells.extend(line.into_iter().take(used_cols));

        input_row += 1;
        if !state.soft_wrapped {
            emit_reflowed_logical_line(
                std::mem::take(&mut logical_cells),
                &logical_anchors,
                new_cols,
                &mut output_lines,
                &mut output_meta,
                &mut mapped,
            );
            logical_anchors.clear();
        }
    }
    if !logical_cells.is_empty() || !logical_anchors.is_empty() {
        emit_reflowed_logical_line(
            logical_cells,
            &logical_anchors,
            new_cols,
            &mut output_lines,
            &mut output_meta,
            &mut mapped,
        );
    }

    if output_lines.is_empty() && visible_rows > 0 {
        output_lines.push_back(vec![Cell::default(); new_cols]);
        output_meta.push_back(LineMeta::default());
    }

    let viewport_row = mapped
        .first()
        .and_then(|anchor| *anchor)
        .map_or(0, |anchor| anchor.row.min(output_lines.len()));
    while output_lines.len().saturating_sub(viewport_row) < visible_rows {
        output_lines.push_back(vec![Cell::default(); new_cols]);
        output_meta.push_back(LineMeta::default());
    }
    let scrollback_len = output_lines
        .len()
        .saturating_sub(visible_rows)
        .max(viewport_row);
    let mut visible = output_lines.split_off(scrollback_len.min(output_lines.len()));
    let mut visible_meta = output_meta.split_off(scrollback_len.min(output_meta.len()));

    while visible.len() < visible_rows {
        visible.push_back(vec![Cell::default(); new_cols]);
        visible_meta.push_back(LineMeta::default());
    }
    while output_lines.len() > scrollback_limit {
        let _ = remove_line_with_meta(&mut output_lines, &mut output_meta, 0);
    }

    for anchor in mapped.iter_mut().flatten() {
        anchor.row = anchor
            .row
            .saturating_sub(scrollback_len)
            .min(visible_rows.saturating_sub(1));
        anchor.column = anchor.column.min(new_cols.saturating_sub(1));
        anchor.wrap_pending &= anchor.column + 1 == new_cols;
    }

    ReflowResult {
        scrollback: output_lines,
        scrollback_meta: output_meta,
        lines: visible,
        line_meta: visible_meta,
        anchors: mapped,
    }
}

impl Term {

    #[inline]
    pub fn mark_presentation_ready(&mut self) {
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

    pub(crate) fn push_scrollback_row(&mut self, line: Vec<Cell>, state: LineMeta) {
        self.scrollback_storage_cols = self.scrollback_storage_cols.max(line.len());
        self.scrollback_pushed = self.scrollback_pushed.wrapping_add(1);
        let index = self.scrollback.len();
        insert_line_with_meta(
            &mut self.scrollback,
            &mut self.scrollback_meta,
            index,
            line,
            state,
        );
        if self.scrollback.len() > self.scrollback_limit
            && let Some((mut old, _)) =
                remove_line_with_meta(&mut self.scrollback, &mut self.scrollback_meta, 0)
            && self.pool.len() < 128
        {
            old.clear();
            self.pool.push(old);
        }
    }

    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        if new_cols == self.cols && new_rows == self.visible_rows {
            return;
        }

        let mut resized_wrap_pending = None;
        if new_cols != self.cols {
            let old_cols = self.cols;
            self.selection = None;
            let full_region = self.scroll_region == (0, self.visible_rows.saturating_sub(1));

            if new_cols >= 2 && full_region {
                if self.is_alt {
                    if let (Some(normal_lines), Some(normal_meta)) =
                        (self.alt_lines.take(), self.alt_line_meta.take())
                    {
                        let scrollback_len = self.scrollback.len();
                        let mut anchors = vec![ReflowAnchor {
                            row: scrollback_len,
                            column: 0,
                            wrap_pending: false,
                        }];
                        let normal_cursor_anchor = self.alt_saved_cursor.map(|(x, y)| {
                            anchors.push(ReflowAnchor {
                                row: scrollback_len + y.min(self.visible_rows.saturating_sub(1)),
                                column: x.min(old_cols.saturating_sub(1)),
                                wrap_pending: false,
                            });
                            anchors.len() - 1
                        });
                        let result = reflow_normal_buffer(
                            std::mem::take(&mut self.scrollback),
                            std::mem::take(&mut self.scrollback_meta),
                            normal_lines,
                            normal_meta,
                            (old_cols, new_cols),
                            self.visible_rows,
                            &anchors,
                            self.scrollback_limit,
                        );
                        self.scrollback = result.scrollback;
                        self.scrollback_meta = result.scrollback_meta;
                        self.alt_lines = Some(result.lines);
                        self.alt_line_meta = Some(result.line_meta);
                        if let Some(anchor_index) = normal_cursor_anchor
                            && let Some(anchor) = result.anchors[anchor_index]
                        {
                            self.alt_saved_cursor = Some((anchor.column, anchor.row));
                        }
                    }
                    resize_physical_columns(&mut self.lines, &mut self.line_meta, new_cols);
                    self.cur_x = self.cur_x.min(new_cols.saturating_sub(1));
                    self.wrap_pending = false;
                } else {
                    let scrollback_len = self.scrollback.len();
                    let mut anchors = vec![
                        ReflowAnchor {
                            row: scrollback_len,
                            column: 0,
                            wrap_pending: false,
                        },
                        ReflowAnchor {
                            row: scrollback_len
                                + self.cur_y.min(self.visible_rows.saturating_sub(1)),
                            column: if self.wrap_pending {
                                self.cur_x.saturating_add(1).min(old_cols)
                            } else {
                                self.cur_x.min(old_cols.saturating_sub(1))
                            },
                            wrap_pending: self.wrap_pending,
                        },
                    ];
                    let saved_anchor = self.saved_cursor.map(|(x, y)| {
                        anchors.push(ReflowAnchor {
                            row: scrollback_len + y.min(self.visible_rows.saturating_sub(1)),
                            column: x.min(old_cols.saturating_sub(1)),
                            wrap_pending: false,
                        });
                        anchors.len() - 1
                    });
                    let result = reflow_normal_buffer(
                        std::mem::take(&mut self.scrollback),
                        std::mem::take(&mut self.scrollback_meta),
                        std::mem::take(&mut self.lines),
                        std::mem::take(&mut self.line_meta),
                        (old_cols, new_cols),
                        self.visible_rows,
                        &anchors,
                        self.scrollback_limit,
                    );
                    self.scrollback = result.scrollback;
                    self.scrollback_meta = result.scrollback_meta;
                    self.lines = result.lines;
                    self.line_meta = result.line_meta;
                    if let Some(anchor) = result.anchors.get(1).and_then(|anchor| *anchor) {
                        self.cur_x = anchor.column;
                        self.cur_y = anchor.row;
                        resized_wrap_pending = Some(anchor.wrap_pending);
                    }
                    if let Some(anchor_index) = saved_anchor
                        && let Some(anchor) = result.anchors[anchor_index]
                    {
                        self.saved_cursor = Some((anchor.column, anchor.row));
                    }
                }
                self.scrollback_storage_cols = new_cols;
            } else {
                resize_physical_columns(&mut self.scrollback, &mut self.scrollback_meta, new_cols);
                resize_physical_columns(&mut self.lines, &mut self.line_meta, new_cols);
                if let (Some(alt), Some(alt_meta)) = (&mut self.alt_lines, &mut self.alt_line_meta)
                {
                    resize_physical_columns(alt, alt_meta, new_cols);
                }
                self.scrollback_storage_cols = new_cols;
                self.cur_x = self.cur_x.min(new_cols.saturating_sub(1));
                self.wrap_pending = false;
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
                let Some(index) = self.lines.len().checked_sub(1) else {
                    break;
                };
                if let Some((mut line, _)) =
                    remove_line_with_meta(&mut self.lines, &mut self.line_meta, index)
                    && self.pool.len() < 128
                {
                    line.clear();
                    self.pool.push(line);
                }
            }
            for _ in 0..drop_top {
                if let Some((top, state)) =
                    remove_line_with_meta(&mut self.lines, &mut self.line_meta, 0)
                    && !self.is_alt
                {
                    self.push_scrollback_row(top, state);
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
                    let Some(index) = self.scrollback.len().checked_sub(1) else {
                        break;
                    };
                    if let Some((mut row, mut state)) = remove_line_with_meta(
                        &mut self.scrollback,
                        &mut self.scrollback_meta,
                        index,
                    ) {
                        row.resize(self.cols, Cell::default());
                        repair_wide_line(&mut row);
                        compact_line_capacity_after_resize(&mut row, self.cols);
                        state.used_cols = state.used_cols.min(self.cols);
                        insert_line_with_meta(&mut self.lines, &mut self.line_meta, 0, row, state);
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
                    let index = self.lines.len();
                    insert_line_with_meta(
                        &mut self.lines,
                        &mut self.line_meta,
                        index,
                        line,
                        LineMeta::default(),
                    );
                }

                self.cur_y += from_scrollback;
                if let Some((_, ref mut sy)) = self.saved_cursor {
                    *sy += from_scrollback;
                }
            } else {
                for _ in 0..diff {
                    let index = self.lines.len();
                    insert_line_with_meta(
                        &mut self.lines,
                        &mut self.line_meta,
                        index,
                        vec![Cell::default(); self.cols],
                        LineMeta::default(),
                    );
                }
            }
        }

        if let (Some(alt), Some(alt_meta)) = (&mut self.alt_lines, &mut self.alt_line_meta) {
            let alt_current_rows = alt.len();
            if new_rows < alt_current_rows {
                let diff = alt_current_rows - new_rows;
                for _ in 0..diff {
                    let Some(index) = alt.len().checked_sub(1) else {
                        break;
                    };
                    if let Some((mut line, _)) = remove_line_with_meta(alt, alt_meta, index)
                        && self.pool.len() < 128
                    {
                        line.clear();
                        self.pool.push(line);
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
                    let index = alt.len();
                    insert_line_with_meta(alt, alt_meta, index, line, LineMeta::default());
                }
            }
            if let Some((_, ref mut sy)) = self.alt_saved_cursor {
                *sy = (*sy).min(new_rows.saturating_sub(1));
            }
        }

        while self.scrollback.len() > self.scrollback_limit {
            let _ = remove_line_with_meta(&mut self.scrollback, &mut self.scrollback_meta, 0);
        }
        self.visible_rows = new_rows;
        self.cur_x = self.cur_x.min(self.cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(self.visible_rows.saturating_sub(1));
        if let Some((ref mut sx, ref mut sy)) = self.saved_cursor {
            *sx = (*sx).min(self.cols.saturating_sub(1));
            *sy = (*sy).min(self.visible_rows.saturating_sub(1));
        }
        if let Some((ref mut sx, ref mut sy)) = self.alt_saved_cursor {
            *sx = (*sx).min(self.cols.saturating_sub(1));
            *sy = (*sy).min(self.visible_rows.saturating_sub(1));
        }
        self.wrap_pending = resized_wrap_pending.unwrap_or(false) && self.cur_x + 1 == self.cols;
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

    pub(crate) fn put_char(&mut self, c: char) {
        let width = terminal_char_width(c);
        if width == 0 {
            self.attach_zero_width(c);
            return;
        }
        if self.wrap_pending {
            if self.autowrap {
                self.soft_wrap_to_next_line();
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
                let blank = Cell::blank_with_background(self.pen.bg);
                if let Some(line) = self.lines.get_mut(self.cur_y) {
                    clear_wide_footprint(line, self.cur_x, &blank);
                }
                self.soft_wrap_to_next_line();
            }
        }
        let fg = terminal_effective_foreground(self.pen.fg, self.pen.bold);
        let bg = self.pen.bg;
        let blank = Cell::blank_with_background(bg);
        let previous_used = self
            .line_meta
            .get(self.cur_y)
            .map_or(0, |state| state.used_cols);
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
                cell.set_sgr_style(&self.pen);
            }
            if width == 2
                && let Some(cell) = line.get_mut(self.cur_x + 1)
            {
                cell.set_wide_spacer(fg, bg);
                cell.set_sgr_style(&self.pen);
            }
        }
        if let Some(state) = self.line_meta.get_mut(self.cur_y) {
            if self.insert_mode && self.cur_x < previous_used {
                state.used_cols = previous_used.saturating_add(width).min(self.cols);
            }
            state.used_cols = state
                .used_cols
                .max(self.cur_x.saturating_add(width).min(self.cols));
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

    pub(crate) fn previous_base_cell_mut(&mut self) -> Option<&mut Cell> {
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

    pub(crate) fn attach_zero_width(&mut self, c: char) {
        if let Some(cell) = self.previous_base_cell_mut()
            && cell.ch() != ' '
        {
            cell.push_zero_width(c);
        }
    }

    pub(crate) fn apply_presentation_selector(&mut self, presentation: u8, selector: char) {
        let Some(cell) = self.previous_base_cell_mut() else {
            return;
        };
        if cell.ch() != ' ' {
            cell.set_presentation(if presentation == CELL_PRESENTATION_EMOJI
                && terminal_force_text_presentation(cell.ch())
            {
                CELL_PRESENTATION_TEXT
            } else {
                presentation
            });
            cell.push_zero_width(selector);
        }
    }

    pub(crate) fn next_tab_stop(&self, from: usize) -> usize {
        ((from + 1)..self.cols)
            .find(|&column| self.tab_stops.get(column).copied().unwrap_or(false))
            .unwrap_or_else(|| self.cols.saturating_sub(1))
    }

    pub(crate) fn previous_tab_stop(&self, from: usize) -> usize {
        (0..from)
            .rev()
            .find(|&column| self.tab_stops.get(column).copied().unwrap_or(false))
            .unwrap_or(0)
    }

    pub(crate) fn move_to_tab_stop(&mut self, count: usize, backwards: bool) {
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

    pub(crate) fn set_mouse_tracking_mode(&mut self, mode: MouseTrackingMode, enabled: bool) {
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

    pub(crate) fn set_cursor_position(&mut self, row: usize, column: usize) {
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

    pub(crate) fn cursor_home(&mut self) {
        self.set_cursor_position(0, 0);
    }

    pub(crate) fn reset_terminal_modes(&mut self) {
        self.pen = Pen::default();
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

    pub(crate) fn soft_reset_terminal_state(&mut self) {
        self.reset_terminal_modes();
        self.cur_x = self.cur_x.min(self.cols.saturating_sub(1));
        self.cur_y = self.cur_y.min(self.visible_rows.saturating_sub(1));
        self.dirty = true;
    }

    pub(crate) fn hard_reset_terminal_state(&mut self) {
        if self.is_alt {
            if let Some(normal_lines) = self.alt_lines.take() {
                self.lines = normal_lines;
            }
            if let Some(normal_meta) = self.alt_line_meta.take() {
                self.line_meta = normal_meta;
            }
        }
        self.is_alt = false;
        self.alt_lines = None;
        self.alt_line_meta = None;

        while self.lines.len() > self.visible_rows {
            let index = self.lines.len() - 1;
            let _ = remove_line_with_meta(&mut self.lines, &mut self.line_meta, index);
        }
        while self.lines.len() < self.visible_rows {
            let index = self.lines.len();
            insert_line_with_meta(
                &mut self.lines,
                &mut self.line_meta,
                index,
                vec![Cell::default(); self.cols],
                LineMeta::default(),
            );
        }
        for (line, state) in self.lines.iter_mut().zip(self.line_meta.iter_mut()) {
            line.resize(self.cols, Cell::default());
            line.fill(Cell::default());
            repair_wide_line(line);
            *state = LineMeta::default();
        }

        self.scrollback.clear();
        self.scrollback_meta.clear();
        self.scrollback_storage_cols = self.cols;
        self.selection = None;
        self.tab_stops = default_tab_stops(self.cols);
        self.reset_terminal_modes();
        self.cur_x = 0;
        self.cur_y = 0;
        self.content_generation = self.content_generation.wrapping_add(1);
        self.dirty = true;
    }

    pub(crate) fn advance_to_next_line(&mut self) {
        if self.cur_y == self.scroll_region.1 {
            self.scroll_region_up(1);
        } else if self.cur_y + 1 < self.visible_rows {
            self.cur_y += 1;
        }
    }

    pub(crate) fn soft_wrap_to_next_line(&mut self) {
        if let Some(state) = self.line_meta.get_mut(self.cur_y) {
            state.soft_wrapped = true;
        }
        self.advance_to_next_line();
        self.cur_x = 0;
    }

    pub(crate) fn newline(&mut self) {
        if let Some(state) = self.line_meta.get_mut(self.cur_y) {
            state.soft_wrapped = false;
        }
        self.advance_to_next_line();
    }

    pub(crate) fn scroll_region_up(&mut self, rows: usize) {
        let (top, bottom) = self.scroll_region;
        if bottom >= self.lines.len() || top >= bottom {
            return;
        }
        let rows = rows.min(bottom - top + 1);
        let blank = Cell::blank_with_background(self.pen.bg);
        for _ in 0..rows {
            let (mut removed, removed_meta) =
                remove_line_with_meta(&mut self.lines, &mut self.line_meta, top)
                    .unwrap_or_else(|| (vec![blank.clone(); self.cols], LineMeta::default()));
            if top == 0 && bottom == self.visible_rows.saturating_sub(1) {
                if !self.is_alt {
                    self.push_scrollback_row(removed, removed_meta);
                } else if self.pool.len() < 128 {
                    removed.clear();
                    self.pool.push(removed);
                }
            } else if self.pool.len() < 128 {
                removed.clear();
                self.pool.push(removed);
            }
            let mut new_line = self
                .pool
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(self.cols));
            new_line.resize(self.cols, blank.clone());
            new_line.fill(blank.clone());
            insert_line_with_meta(
                &mut self.lines,
                &mut self.line_meta,
                bottom,
                new_line,
                LineMeta::default(),
            );
        }
        self.dirty = true;
    }

    pub(crate) fn scroll_region_down(&mut self, rows: usize) {
        let (top, bottom) = self.scroll_region;
        if bottom >= self.lines.len() || top >= bottom {
            return;
        }
        let rows = rows.min(bottom - top + 1);
        let blank = Cell::blank_with_background(self.pen.bg);
        for _ in 0..rows {
            if let Some((mut removed, _)) =
                remove_line_with_meta(&mut self.lines, &mut self.line_meta, bottom)
                && self.pool.len() < 128
            {
                removed.clear();
                self.pool.push(removed);
            }
            let mut new_line = self
                .pool
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(self.cols));
            new_line.resize(self.cols, blank.clone());
            new_line.fill(blank.clone());
            insert_line_with_meta(
                &mut self.lines,
                &mut self.line_meta,
                top,
                new_line,
                LineMeta::default(),
            );
        }
        self.dirty = true;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn push_scrollback_row_for_test(
        &mut self,
        row: Vec<Cell>,
        used_cols: usize,
        soft_wrapped: bool,
    ) {
        self.push_scrollback_row(
            row,
            LineMeta {
                soft_wrapped,
                used_cols: used_cols.min(self.cols),
            },
        );
    }

    pub fn selection_text(&self) -> String {
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

