//! Shared helpers for the crate's unit tests.

use crate::cell::{Cell, terminal_char_width};
use crate::color::{Rgb, TermPalette};
use crate::term::{Term, TermConfig, TermIdentity, TermSink};
use std::time::Instant;

/// Records everything a [`Term`] sends to its host during a test.
pub(crate) struct RecordingSink {
    /// One entry per `reply` call.
    pub(crate) calls: Vec<Vec<u8>>,
    /// All `reply` bytes concatenated in call order.
    pub(crate) bytes: Vec<u8>,
    /// Every `title` call, in order.
    pub(crate) titles: Vec<String>,
    /// Number of `title_reset` calls.
    pub(crate) title_resets: usize,
    /// Number of `bell` calls.
    pub(crate) bells: usize,
    /// Every `clipboard_set` text, in order.
    pub(crate) clipboard: Vec<String>,
    /// Every `working_directory` call as (host, percent-decoded path).
    pub(crate) cwd: Vec<(String, Vec<u8>)>,
    /// Fixed time passed to `advance` by [`feed`].
    pub(crate) now: Instant,
}

impl RecordingSink {
    pub(crate) fn new() -> Self {
        Self {
            calls: Vec::new(),
            bytes: Vec::new(),
            titles: Vec::new(),
            title_resets: 0,
            bells: 0,
            clipboard: Vec::new(),
            cwd: Vec::new(),
            now: Instant::now(),
        }
    }
}

impl TermSink for RecordingSink {
    fn reply(&mut self, bytes: &[u8]) {
        self.calls.push(bytes.to_vec());
        self.bytes.extend_from_slice(bytes);
    }

    fn title(&mut self, title: &str) {
        self.titles.push(title.to_owned());
    }

    fn title_reset(&mut self) {
        self.title_resets += 1;
    }

    fn bell(&mut self) {
        self.bells += 1;
    }

    fn clipboard_set(&mut self, text: String) {
        self.clipboard.push(text);
    }

    fn working_directory(&mut self, host: &str, path: &[u8]) {
        self.cwd.push((host.to_owned(), path.to_vec()));
    }
}

/// Feeds `bytes` into `term` with the sink's fixed time.
pub(crate) fn feed(term: &mut Term, sink: &mut RecordingSink, bytes: &[u8]) {
    term.advance(bytes, sink.now, sink);
}

/// Configuration with 10 000 scrollback lines and a fixed dark palette.
pub(crate) fn test_config(cols: usize, rows: usize) -> TermConfig {
    let light = Rgb {
        r: 0xe5,
        g: 0xe5,
        b: 0xe5,
    };
    let dark = Rgb { r: 0, g: 0, b: 0 };
    TermConfig {
        cols,
        rows,
        scrollback_lines: 10_000,
        identity: TermIdentity {
            name: "ronsole-test",
            version: "0",
        },
        palette: TermPalette {
            foreground: light,
            background: dark,
            cursor: light,
            ansi: [dark; 16],
        },
    }
}

/// Writes `text` into the first cells of visible row `row` and marks the row
/// as used up to that width, not soft-wrapped.
pub(crate) fn set_line(grid: &mut Term, row: usize, text: &str) {
    let mut used_cols = 0usize;
    for (x, ch) in text.chars().enumerate() {
        grid.lines[row][x].set_ch(ch);
        used_cols = x + 1;
    }
    if let Some(state) = grid.line_meta.get_mut(row) {
        state.used_cols = used_cols.min(grid.cols);
        state.soft_wrapped = false;
    }
}

/// Checks that scrollback and visible rows keep their metadata in step.
pub(crate) fn assert_line_metadata_invariant(grid: &Term) {
    assert_eq!(grid.scrollback.len(), grid.scrollback_meta.len());
    assert_eq!(grid.lines.len(), grid.line_meta.len());
    if let Some(alt) = &grid.alt_lines {
        assert_eq!(
            alt.len(),
            grid.alt_line_meta.as_ref().map_or(0, |meta| meta.len())
        );
    } else {
        assert!(grid.alt_line_meta.is_none());
    }
    assert!(
        grid.scrollback_meta
            .iter()
            .chain(grid.line_meta.iter())
            .all(|state| state.used_cols <= grid.cols)
    );
}

/// Joins soft-wrapped physical rows back into logical lines of cells.
pub(crate) fn logical_cell_lines(grid: &Term) -> Vec<Vec<Cell>> {
    let mut logical = Vec::new();
    let mut current = Vec::new();
    for (row, state) in grid
        .scrollback
        .iter()
        .zip(grid.scrollback_meta.iter())
        .chain(grid.lines.iter().zip(grid.line_meta.iter()))
    {
        current.extend(row.iter().take(state.used_cols).cloned());
        if !state.soft_wrapped {
            logical.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        logical.push(current);
    }
    logical
}

/// Text of each logical line returned by [`logical_cell_lines`].
pub(crate) fn logical_texts(grid: &Term) -> Vec<String> {
    logical_cell_lines(grid)
        .into_iter()
        .map(|cells| {
            let mut text = String::new();
            for cell in cells {
                cell.append_text_to(&mut text);
            }
            text
        })
        .collect()
}

/// Checks that wide glyphs and their spacer cells pair up within `line`.
pub(crate) fn assert_wide_line_invariant(line: &[Cell]) {
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
        if terminal_char_width(cell.ch()) == 2 && !cell.is_wide_spacer() {
            assert!(
                cell.is_wide(),
                "width-2 glyph {:?} survived as a one-cell glyph at column {column}",
                cell.ch()
            );
        }
    }
}
