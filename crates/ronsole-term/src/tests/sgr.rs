//! SGR parsing into the pen and the style stored on written cells.
//!
//! Expected values follow the SGR table in the terminal parity spec, section 5.1,
//! and the SGR reference, section 9. They are not taken from the parser output.

use std::sync::Arc;

use super::support::{RecordingSink, feed, test_config};
use crate::cell::{Cell, CellFlags, UnderlineStyle};
use crate::color::TerminalColor;
use crate::term::Term;

/// Feeds `bytes` into a fresh one-row terminal and returns the cell at `col`.
fn cell_at(bytes: &[u8], col: usize) -> Cell {
    let mut term = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, bytes);
    term.visible_row(0).expect("row 0")[col].clone()
}

type CellCheck = fn(&Cell) -> bool;

/// Each row is `(on code, off code, attribute predicate)`. The on code must set
/// the attribute on the printed cell; the off code must clear it again.
fn check_table(rows: &[(&str, &str, CellCheck)]) {
    for &(on, off, is_set) in rows {
        let set = cell_at(format!("\x1b[{on}mx").as_bytes(), 0);
        assert!(is_set(&set), "SGR {on} must set the attribute");
        let cleared = cell_at(format!("\x1b[{on}m\x1b[{off}mx").as_bytes(), 0);
        assert!(!is_set(&cleared), "SGR {off} must clear the attribute set by {on}");
    }
}

#[test]
fn sgr_table_semicolon_forms() {
    check_table(&[
        ("1", "22", |c: &Cell| c.flags().contains(CellFlags::BOLD)),
        ("2", "22", |c: &Cell| c.is_dim()),
        ("3", "23", |c: &Cell| c.flags().contains(CellFlags::ITALIC)),
        ("4", "24", |c: &Cell| c.underline() == UnderlineStyle::Single),
        ("21", "24", |c: &Cell| c.underline() == UnderlineStyle::Double),
        ("7", "27", |c: &Cell| c.is_inverse()),
        ("8", "28", |c: &Cell| c.flags().contains(CellFlags::CONCEAL)),
        ("9", "29", |c: &Cell| c.flags().contains(CellFlags::STRIKE)),
        ("58;5;196", "59", |c: &Cell| {
            c.underline_color() == Some(TerminalColor::indexed(196))
        }),
        ("58;2;1;2;3", "59", |c: &Cell| {
            c.underline_color() == Some(TerminalColor::rgb(1, 2, 3))
        }),
    ]);
}

#[test]
fn sgr_table_colon_forms() {
    check_table(&[
        ("4:1", "4:0", |c: &Cell| c.underline() == UnderlineStyle::Single),
        ("4:2", "4:0", |c: &Cell| c.underline() == UnderlineStyle::Double),
        ("4:3", "4:0", |c: &Cell| c.underline() == UnderlineStyle::Curly),
        ("4:4", "4:0", |c: &Cell| c.underline() == UnderlineStyle::Dotted),
        ("4:5", "4:0", |c: &Cell| c.underline() == UnderlineStyle::Dashed),
        ("58:5:196", "59", |c: &Cell| {
            c.underline_color() == Some(TerminalColor::indexed(196))
        }),
        ("58:2::1:2:3", "59", |c: &Cell| {
            c.underline_color() == Some(TerminalColor::rgb(1, 2, 3))
        }),
        ("58:2:1:2:3", "59", |c: &Cell| {
            c.underline_color() == Some(TerminalColor::rgb(1, 2, 3))
        }),
    ]);
}

#[test]
fn sgr_unknown_underline_subparam_is_single() {
    assert_eq!(cell_at(b"\x1b[4:9mx", 0).underline(), UnderlineStyle::Single);
}

#[test]
fn sgr_21_is_double_and_keeps_bold() {
    let cell = cell_at(b"\x1b[1;21mx", 0);
    assert!(cell.flags().contains(CellFlags::BOLD));
    assert_eq!(cell.underline(), UnderlineStyle::Double);
}

#[test]
fn sgr_22_clears_bold_and_dim() {
    let cell = cell_at(b"\x1b[1;2m\x1b[22mx", 0);
    assert!(!cell.flags().contains(CellFlags::BOLD));
    assert!(!cell.is_dim());
}

#[test]
fn sgr_0_resets_underline_style_and_color() {
    let set = cell_at(b"\x1b[4:3;58;5;1mx", 0);
    assert_eq!(set.underline(), UnderlineStyle::Curly);
    assert_eq!(set.underline_color(), Some(TerminalColor::indexed(1)));

    let reset = cell_at(b"\x1b[4:3;58;5;1m\x1b[0mx", 0);
    assert_eq!(reset.underline(), UnderlineStyle::None);
    assert_eq!(reset.underline_color(), None);
}

#[test]
fn underline_color_forms() {
    assert_eq!(
        cell_at(b"\x1b[58;5;196mx", 0).underline_color(),
        Some(TerminalColor::indexed(196))
    );
    for bytes in [
        &b"\x1b[58:2::1:2:3mx"[..],
        &b"\x1b[58:2:1:2:3mx"[..],
        &b"\x1b[58;2;1;2;3mx"[..],
    ] {
        assert_eq!(
            cell_at(bytes, 0).underline_color(),
            Some(TerminalColor::rgb(1, 2, 3))
        );
    }
    assert_eq!(cell_at(b"\x1b[58;5;1m\x1b[59mx", 0).underline_color(), None);
}

#[test]
fn sgr_ignores_blink_and_overline() {
    let cell = cell_at(b"\x1b[5;6;53;55mx", 0);
    assert_eq!(cell.flags(), CellFlags::empty());
    assert_eq!(cell.underline(), UnderlineStyle::None);
    assert_eq!(cell.underline_color(), None);
    assert!(cell.extra().is_none());
}

#[test]
fn sgr_malformed_color_forms_are_ignored() {
    // Truncated colon forms: the 58 parameter is dropped, nothing is consumed.
    assert_eq!(cell_at(b"\x1b[58:2:1:2mx", 0).underline_color(), None);
    assert_eq!(cell_at(b"\x1b[58:5mx", 0).underline_color(), None);
    // Over-long colon forms do not match a known layout.
    assert_eq!(cell_at(b"\x1b[58:5:1:2:3mx", 0).underline_color(), None);
    assert_eq!(cell_at(b"\x1b[58:2:1:2:3:4:5mx", 0).underline_color(), None);
    // Semicolon forms missing their components.
    assert_eq!(cell_at(b"\x1b[58;5mx", 0).underline_color(), None);
    assert_eq!(cell_at(b"\x1b[58;2;1;2mx", 0).underline_color(), None);
}

#[test]
fn sgr_extra_params_after_legacy_color_are_plain_attributes() {
    // `58;5;n` consumes two parameters; the following `2` and `3` are plain SGR.
    let cell = cell_at(b"\x1b[58;5;1;2;3mx", 0);
    assert_eq!(cell.underline_color(), Some(TerminalColor::indexed(1)));
    assert!(cell.is_dim());
    assert!(cell.flags().contains(CellFlags::ITALIC));
}

#[test]
fn plain_text_allocates_no_extra() {
    assert!(cell_at(b"abc", 0).extra().is_none());
}

#[test]
fn cells_of_one_pen_share_extra() {
    let mut term = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b[58;5;1mab");
    let row = term.visible_row(0).expect("row 0");
    let first = row[0].extra().expect("first cell extra");
    let second = row[1].extra().expect("second cell extra");
    assert!(Arc::ptr_eq(first, second));
}

#[test]
fn zero_width_on_shared_extra_copies() {
    let mut term = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, "\x1b[58;5;1mab\u{0301}".as_bytes());
    let row = term.visible_row(0).expect("row 0");
    assert!(row[0].zero_width().is_empty());
    assert_eq!(row[1].zero_width(), &['\u{0301}']);
}

#[test]
fn sgr_extended_colors_keep_legacy_and_colon_forms() {
    let cell = cell_at(b"\x1b[38;2;1;2;3;48;5;200m\x1b[38:2::4:5:6mx", 0);
    assert_eq!(cell.fg().rgb_value(), Some((4, 5, 6)));
    assert_eq!(cell.bg().indexed_value(), Some(200));
    assert_eq!(std::mem::size_of::<TerminalColor>(), 4);
}

#[test]
fn sgr_common_state_toggles_and_reset_are_independent() {
    let set = cell_at(b"\x1b[1;2;4;7mx", 0);
    assert!(set.flags().contains(CellFlags::BOLD));
    assert!(set.is_dim());
    assert_eq!(set.underline(), UnderlineStyle::Single);
    assert!(set.is_inverse());

    let cleared = cell_at(b"\x1b[1;2;4;7m\x1b[22;24;27mx", 0);
    assert!(!cleared.flags().contains(CellFlags::BOLD));
    assert!(!cleared.is_dim());
    assert_eq!(cleared.underline(), UnderlineStyle::None);
    assert!(!cleared.is_inverse());

    let reset = cell_at(b"\x1b[1;2;4;7;31;44m\x1b[0mx", 0);
    assert!(reset.fg().is_default_foreground());
    assert!(reset.bg().is_default_background());
    assert_eq!(reset.flags(), CellFlags::empty());
    assert_eq!(reset.underline(), UnderlineStyle::None);
}

/// `CSI > 4 ; 2 m` and `CSI > 4 m` are xterm XTMODKEYS (set / reset
/// modifyOtherKeys), which Claude Code sends; `CSI ? 4 m` is XTQMODKEYS. None of
/// them is SGR, so they must not turn on underline (4) or faint (2).
#[test]
fn private_marker_m_sequences_are_not_sgr() {
    for seq in [
        &b"\x1b[>4;2m"[..],
        b"\x1b[>4m",
        b"\x1b[>4;1m",
        b"\x1b[>1m",
        b"\x1b[?4m",
        b"\x1b[=4m",
        b"\x1b[<4m",
    ] {
        let mut bytes = seq.to_vec();
        bytes.push(b'x');
        let cell = cell_at(&bytes, 0);
        let label = String::from_utf8_lossy(seq);
        assert_eq!(cell.underline(), UnderlineStyle::None, "{label:?} set underline");
        assert!(!cell.is_dim(), "{label:?} set faint");
        assert_eq!(cell.flags(), CellFlags::empty(), "{label:?} set flags");
    }
    // A private form between real SGR keeps the pen as the real SGR left it.
    let cell = cell_at(b"\x1b[1m\x1b[>4;2m\x1b[>4mx", 0);
    assert_eq!(cell.flags(), CellFlags::BOLD);
    assert_eq!(cell.underline(), UnderlineStyle::None);
    assert!(!cell.is_dim());
}

#[test]
fn conceal_survives_host_unhide() {
    let mut term = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b[8mab");
    term.set_host_hidden(0, 0..2, true);
    assert!(term.visible_row(0).expect("row 0")[0].is_hidden());
    term.set_host_hidden(0, 0..2, false);
    let row = term.visible_row(0).expect("row 0");
    for cell in &row[..2] {
        assert!(cell.flags().contains(CellFlags::CONCEAL));
        assert!(!cell.is_hidden());
    }
}

#[test]
fn write_clears_host_hidden_not_pen_conceal() {
    let mut term = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b[8ma");
    term.set_host_hidden(0, 0..1, true);
    feed(&mut term, &mut sink, b"\x1b[1Gc");
    let cell = &term.visible_row(0).expect("row 0")[0];
    assert_eq!(cell.ch(), 'c');
    assert!(!cell.is_hidden());
    assert!(cell.flags().contains(CellFlags::CONCEAL));
}

#[test]
fn selection_copies_concealed_chars() {
    let mut term = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b[8mab");
    term.set_selection(Some((0, 0, 0, 1)));
    assert_eq!(term.selection_text(), "ab");
}
