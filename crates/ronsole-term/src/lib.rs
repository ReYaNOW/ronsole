//! Platform-neutral terminal model shared by Ronsole and RRiter.
//!
//! The crate holds cell storage, colour mapping and the protocol encoders
//! (mouse, bracketed paste, focus). It performs no I/O, reads no environment
//! and spawns no threads; the current time is always passed in by the host.

pub use vte;

mod cell;
mod color;
mod grid;
mod input;
mod osc;
mod perform;
mod sgr;
mod term;
#[cfg(test)]
mod tests;

pub use grid::normalized_selection_bounds;
pub use osc::TERMINAL_TITLE_MAX_BYTES;
pub use term::{AdvanceOutcome, Term, TermConfig, TermIdentity, TermSink};

pub use cell::{
    CELL_PRESENTATION_AUTO, CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT,
    TERMINAL_CELL_EXTRA_MAX_CHARS, terminal_force_text_presentation,
    terminal_should_render_zero_width, Cell, CellFlags, Hyperlink, UnderlineStyle,
};
pub use color::{CellColors, ColorKind, Rgb, TermPalette, TerminalColor, xterm_256_rgb};
pub use input::{
    MouseButton, MouseEventKind, MouseTrackingMode, bracketed_paste_bytes,
    focus_report_sequence, mouse_report,
};
