//! Platform-neutral terminal model shared by Ronsole and RRiter.
//!
//! The crate holds cell storage, colour mapping and the protocol encoders
//! (mouse, bracketed paste, focus). It performs no I/O, reads no environment
//! and spawns no threads; the current time is always passed in by the host.

pub use vte;

mod cell;
mod color;
mod input;

pub use cell::{
    CELL_PRESENTATION_AUTO, CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT,
    TERMINAL_CELL_EXTRA_MAX_CHARS, apply_ansi_sgr, is_terminal_zero_width_format,
    terminal_char_width, terminal_effective_foreground, terminal_force_text_presentation,
    terminal_is_emoji_modifier, terminal_presentation_selector,
    terminal_should_render_zero_width, Cell, CellFlags, Hyperlink, UnderlineStyle,
};
pub use color::{CellColors, ColorKind, Rgb, TermPalette, TerminalColor, xterm_256_rgb};
pub use input::{
    MouseButton, MouseEventKind, MouseTrackingMode, bracketed_paste_bytes,
    focus_report_sequence, mouse_report,
};
