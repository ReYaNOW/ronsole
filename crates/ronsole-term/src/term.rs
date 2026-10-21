//! The terminal model: state, host-facing API and the parser driver.

use crate::cell::{Cell, UnderlineStyle};
use crate::color::{TermPalette, TerminalColor};
use crate::grid::LineMeta;
use crate::input::MouseTrackingMode;
use std::ops::Range;
use std::time::Instant;
use vte::Parser;

/// Events a [`Term`] hands to its host while parsing output.
///
/// The host passes one sink into every call that feeds bytes; the terminal
/// never stores it.
pub trait TermSink {
    /// Bytes to write back to the PTY (DA, DSR, and similar replies).
    fn reply(&mut self, bytes: &[u8]);
    /// Programmed window title from OSC 0 or OSC 2.
    fn title(&mut self, title: &str);
    /// Clipboard text from OSC 52, decoded as UTF-8.
    fn clipboard_set(&mut self, text: String);
    /// Working directory from OSC 7, with percent-decoded path bytes.
    fn working_directory(&mut self, host: &str, path: &[u8]);
}

/// Terminal identity reported in XTVERSION and DA2 replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermIdentity {
    pub name: &'static str,
    pub version: &'static str,
}

/// Construction-time configuration for [`Term`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermConfig {
    pub cols: usize,
    pub rows: usize,
    pub scrollback_lines: usize,
    pub identity: TermIdentity,
    pub palette: TermPalette,
}

/// Outcome of a [`Term::advance`] call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdvanceOutcome {
    pub grid_changed: bool,
    pub new_deadline: bool,
}

/// Terminal grid, parser state and mode flags.
///
/// Fields are crate-private; hosts read through the accessors below and
/// write through the methods that keep the grid invariants.
pub struct Term {
    pub(crate) parser: Option<Parser>,
    pub(crate) scrollback: std::collections::VecDeque<Vec<Cell>>,
    pub(crate) scrollback_meta: std::collections::VecDeque<LineMeta>,
    pub(crate) lines: std::collections::VecDeque<Vec<Cell>>,
    pub(crate) line_meta: std::collections::VecDeque<LineMeta>,
    pub(crate) alt_lines: Option<std::collections::VecDeque<Vec<Cell>>>,
    pub(crate) alt_line_meta: Option<std::collections::VecDeque<LineMeta>>,
    pub(crate) alt_saved_cursor: Option<(usize, usize)>,
    pub(crate) is_alt: bool,
    pub(crate) cols: usize,
    pub(crate) visible_rows: usize,
    pub(crate) cur_x: usize,
    pub(crate) cur_y: usize,
    pub(crate) cur_fg: TerminalColor,
    pub(crate) cur_bg: TerminalColor,
    pub(crate) cur_bold: bool,
    pub(crate) cur_dim: bool,
    pub(crate) cur_underline: UnderlineStyle,
    pub(crate) cur_inverse: bool,
    pub(crate) dirty: bool,
    pub(crate) content_generation: u64,
    pub(crate) presentation_ready: bool,
    pub(crate) presentation_layout_ready: bool,
    pub(crate) selection: Option<(usize, usize, usize, usize)>,
    pub(crate) saved_cursor: Option<(usize, usize)>,
    pub(crate) scroll_region: (usize, usize),
    pub(crate) cursor_visible: bool,
    pub(crate) app_cursor_keys: bool,
    pub(crate) mouse_tracking_mode: MouseTrackingMode,
    pub(crate) mouse_tracking_mask: u8,
    pub(crate) mouse_sgr: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) focus_reporting: bool,
    pub(crate) autowrap: bool,
    pub(crate) wrap_pending: bool,
    pub(crate) insert_mode: bool,
    pub(crate) origin_mode: bool,
    pub(crate) tab_stops: Vec<bool>,
    pub(crate) join_next: bool,
    pub(crate) scrollback_storage_cols: usize,
    /// Rows ever appended to `scrollback` (wrapping); lets a scan find the
    /// rows that entered it since an earlier scan.
    pub(crate) scrollback_pushed: u64,
    pub(crate) pool: Vec<Vec<Cell>>,
    pub(crate) scrollback_limit: usize,
    pub(crate) identity: TermIdentity,
    pub(crate) palette: TermPalette,
}

/// Drives the vte parser over a [`Term`] and forwards events to a sink.
pub(crate) struct Performer<'a> {
    pub(crate) term: &'a mut Term,
    pub(crate) sink: &'a mut dyn TermSink,
}

impl Term {
    pub fn new(config: TermConfig) -> Self {
        let TermConfig {
            cols,
            rows,
            scrollback_lines,
            identity,
            palette,
        } = config;
        let mut lines = std::collections::VecDeque::new();
        let mut line_meta = std::collections::VecDeque::new();
        for _ in 0..rows {
            lines.push_back(vec![Cell::default(); cols]);
            line_meta.push_back(LineMeta::default());
        }
        Self {
            parser: Some(Parser::new()),
            scrollback: std::collections::VecDeque::new(),
            scrollback_meta: std::collections::VecDeque::new(),
            lines,
            line_meta,
            alt_lines: None,
            alt_line_meta: None,
            alt_saved_cursor: None,
            is_alt: false,
            cols,
            visible_rows: rows,
            cur_x: 0,
            cur_y: 0,
            cur_fg: TerminalColor::default_foreground(),
            cur_bg: TerminalColor::default_background(),
            cur_bold: false,
            cur_dim: false,
            cur_underline: UnderlineStyle::None,
            cur_inverse: false,
            dirty: true,
            content_generation: 0,
            presentation_ready: false,
            presentation_layout_ready: false,
            selection: None,
            saved_cursor: None,
            scroll_region: (0, rows.saturating_sub(1)),
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
            tab_stops: crate::grid::default_tab_stops(cols),
            join_next: false,
            scrollback_storage_cols: cols,
            scrollback_pushed: 0,
            pool: Vec::with_capacity(128),
            scrollback_limit: scrollback_lines,
            identity,
            palette,
        }
    }

    /// Parses `bytes` and applies them to the grid.
    ///
    /// `_now` is reserved for the synchronized-output deadline (Task 6).
    pub fn advance(
        &mut self,
        bytes: &[u8],
        _now: Instant,
        sink: &mut dyn TermSink,
    ) -> AdvanceOutcome {
        // Take the parser out so the performer can borrow the rest of `self`;
        // it is put back below, so no parser is built per batch.
        let mut parser = self.parser.take().unwrap_or_default();
        {
            let mut performer = Performer { term: self, sink };
            parser.advance(&mut performer, bytes);
        }
        self.parser = Some(parser);
        AdvanceOutcome {
            grid_changed: !bytes.is_empty(),
            new_deadline: false,
        }
    }

    #[inline]
    pub(crate) fn try_queue_reply(&self, sink: &mut dyn TermSink, reply: Vec<u8>) {
        sink.reply(&reply);
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.visible_rows
    }

    /// Zero in the alternate screen, which has no scrollback of its own.
    pub fn scrollback_len(&self) -> usize {
        if self.is_alt { 0 } else { self.scrollback.len() }
    }

    pub fn total_rows(&self) -> usize {
        self.scrollback_len() + self.lines.len()
    }

    /// Row `index`: scrollback first, then visible rows.
    pub fn row(&self, index: usize) -> Option<&[Cell]> {
        let scrollback_len = self.scrollback_len();
        if index < scrollback_len {
            self.scrollback.get(index).map(Vec::as_slice)
        } else {
            self.lines.get(index - scrollback_len).map(Vec::as_slice)
        }
    }

    /// Visible row `y`, counted from the top of the screen.
    pub fn visible_row(&self, y: usize) -> Option<&[Cell]> {
        self.lines.get(y).map(Vec::as_slice)
    }

    /// Cursor position `(x, y)` within the visible area.
    pub fn cursor(&self) -> (usize, usize) {
        (self.cur_x, self.cur_y)
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn is_alt_screen(&self) -> bool {
        self.is_alt
    }

    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }

    pub fn mouse_tracking(&self) -> MouseTrackingMode {
        self.mouse_tracking_mode
    }

    pub fn mouse_sgr(&self) -> bool {
        self.mouse_sgr
    }

    pub fn focus_reporting(&self) -> bool {
        self.focus_reporting
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    pub fn presentation_ready(&self) -> bool {
        self.presentation_ready
    }

    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    pub fn scrollback_pushed(&self) -> u64 {
        self.scrollback_pushed
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Writes one cell at `row` (indexed as in [`Term::row`]) and `col`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_cell_for_test(&mut self, row: usize, col: usize, cell: Cell) {
        let scrollback_len = self.scrollback_len();
        let line = if row < scrollback_len {
            self.scrollback.get_mut(row)
        } else {
            self.lines.get_mut(row - scrollback_len)
        };
        if let Some(slot) = line.and_then(|line| line.get_mut(col)) {
            *slot = cell;
        }
    }

    pub fn selection(&self) -> Option<(usize, usize, usize, usize)> {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Option<(usize, usize, usize, usize)>) {
        self.selection = selection;
    }

    /// Marks cells in row `row` (indexed as in [`Term::row`]) within `cols`
    /// as hidden from the host's own rendering, or clears the mark.
    pub fn set_host_hidden(&mut self, row: usize, cols: Range<usize>, hidden: bool) {
        let scrollback_len = self.scrollback_len();
        let line = if row < scrollback_len {
            self.scrollback.get_mut(row)
        } else {
            self.lines.get_mut(row - scrollback_len)
        };
        if let Some(line) = line {
            let end = cols.end.min(line.len());
            if cols.start < end {
                for cell in &mut line[cols.start..end] {
                    cell.set_hidden(hidden);
                }
            }
        }
    }

    pub fn mark_changed(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
        self.dirty = true;
    }

    pub fn set_dirty(&mut self, dirty: bool) {
        self.dirty = dirty;
    }

    /// Stores the palette for future use; colour replies do not read it yet.
    pub fn set_palette(&mut self, palette: TermPalette) {
        self.palette = palette;
    }
}
