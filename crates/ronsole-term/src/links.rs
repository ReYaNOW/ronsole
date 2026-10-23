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
    /// and the column range of that row. Sorted by row, then by start column;
    /// the renderer relies on this order.
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
        // Walks the logical line only as far as the search window reaches on each side
        // of `col`. Offsets below are cells from the start of `row`; `before` is the
        // number of cells in the rows above `row` that are walked.
        let mut first = row;
        let mut before = 0;
        while first > 0 && self.row_soft_wrapped(first - 1) && before + col < URL_SEARCH_RADIUS {
            first -= 1;
            before += self.row(first)?.len();
        }
        let window_end = col.saturating_add(URL_SEARCH_RADIUS).saturating_add(1);
        let mut last = row;
        let mut after = self.row(row)?.len();
        while self.row_soft_wrapped(last) && after <= window_end {
            let Some(next) = self.row(last + 1) else {
                break;
            };
            last += 1;
            after += next.len();
        }

        // Copied rows: index, offset in `line`, length.
        let mut line: Vec<Cell> = Vec::new();
        let mut copied_rows = Vec::with_capacity(last - first + 1);
        for index in first..=last {
            let cells = self.row(index)?;
            copied_rows.push((index, line.len(), cells.len()));
            line.extend_from_slice(cells);
        }
        let range = find_url(&line, before + col)?;

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
