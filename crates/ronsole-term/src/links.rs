//! Hyperlink queries: the link under a cell, from OSC 8 or from a URL in the logical line.

use crate::cell::{Cell, Hyperlink};
use crate::term::Term;
use crate::url::{URL_SEARCH_RADIUS, find_url};
use std::ops::Range;
use std::sync::Arc;

/// Link under a cell, as returned by [`Term::link_at`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTarget {
    /// OSC 8 URI, or the URL text found in the logical line.
    pub uri: Box<str>,
    /// Cells of the link inside the requested rows: row index as in [`Term::row`]
    /// and the column range of that row.
    pub cells: Vec<(usize, Range<usize>)>,
}

/// Whether two hyperlinks are the same link: the same `Arc`, or equal URIs with
/// the same non-empty id. Links without an id stay distinct when their URIs match.
fn same_hyperlink(a: &Arc<Hyperlink>, b: &Arc<Hyperlink>) -> bool {
    Arc::ptr_eq(a, b) || (a.uri == b.uri && a.id.is_some() && a.id == b.id)
}

/// Appends the maximal column runs of `line` whose cells satisfy `matches`,
/// as (`index`, column range) pairs.
fn push_matching_runs(
    cells: &mut Vec<(usize, Range<usize>)>,
    index: usize,
    line: &[Cell],
    matches: impl Fn(&Cell) -> bool,
) {
    let mut run_start = None;
    for (col, cell) in line.iter().enumerate() {
        if matches(cell) {
            run_start.get_or_insert(col);
        } else if let Some(start) = run_start.take() {
            cells.push((index, start..col));
        }
    }
    if let Some(start) = run_start {
        cells.push((index, start..line.len()));
    }
}

impl Term {
    /// Link at (`row`, `col`) for rows in `rows`, with row indices as in [`Term::row`].
    /// A hyperlink set by OSC 8 on the cell wins; otherwise the plain URL around the
    /// cell is searched in its logical line, which joins soft-wrapped rows.
    pub fn link_at(&self, row: usize, col: usize, rows: Range<usize>) -> Option<LinkTarget> {
        if !rows.contains(&row) {
            return None;
        }
        let cell = self.row(row)?.get(col)?;
        if let Some(link) = cell.hyperlink() {
            let mut cells = Vec::new();
            for index in rows {
                let Some(line) = self.row(index) else {
                    break;
                };
                push_matching_runs(&mut cells, index, line, |other| {
                    other.hyperlink().is_some_and(|other| same_hyperlink(link, other))
                });
            }
            return Some(LinkTarget {
                uri: link.uri.clone(),
                cells,
            });
        }
        self.url_link_at(row, col, rows)
    }

    /// Plain URL around (`row`, `col`), searched in the logical line holding `row`.
    fn url_link_at(&self, row: usize, col: usize, rows: Range<usize>) -> Option<LinkTarget> {
        let mut first = row;
        while first > 0 && self.row_soft_wrapped(first - 1) {
            first -= 1;
        }
        let mut last = row;
        while self.row_soft_wrapped(last) && self.row(last + 1).is_some() {
            last += 1;
        }

        // Each physical row of the logical line: index, offset in the logical line, length.
        // Only lengths are read for all rows; cells are copied for the rows that the
        // URL search window touches.
        let mut segments = Vec::with_capacity(last - first + 1);
        let mut logical_len = 0;
        for index in first..=last {
            let len = self.row(index)?.len();
            segments.push((index, logical_len, len));
            logical_len += len;
        }
        let base = segments.iter().find(|segment| segment.0 == row)?.1;
        let target = base + col;
        let window_start = target.saturating_sub(URL_SEARCH_RADIUS);
        let window_end = target.saturating_add(URL_SEARCH_RADIUS).saturating_add(1);
        // The row starting after the window end is copied too, so the copied line
        // extends past the window unless it ends the logical line.
        let first_copied = segments
            .iter()
            .position(|&(_, offset, len)| offset + len > window_start)?;
        let last_copied = segments
            .iter()
            .rposition(|&(_, offset, _)| offset <= window_end)?;
        let copied = segments.get(first_copied..=last_copied)?;
        let origin = copied.first()?.1;

        // Copied rows: index, offset in `line`, length.
        let mut line: Vec<Cell> = Vec::new();
        let mut copied_rows = Vec::with_capacity(copied.len());
        for &(index, offset, _) in copied {
            let cells = self.row(index)?;
            copied_rows.push((index, offset - origin, cells.len()));
            line.extend_from_slice(cells);
        }
        let range = find_url(&line, target - origin)?;

        let mut uri = String::new();
        for cell in &line[range.clone()] {
            cell.append_text_to(&mut uri);
        }
        let mut cells = Vec::new();
        for &(index, offset, len) in &copied_rows {
            if !rows.contains(&index) {
                continue;
            }
            let start = range.start.max(offset);
            let end = range.end.min(offset + len);
            if start < end {
                cells.push((index, start - offset..end - offset));
            }
        }
        Some(LinkTarget {
            uri: uri.into_boxed_str(),
            cells,
        })
    }
}
