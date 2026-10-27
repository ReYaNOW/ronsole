//! Copying a selection joins soft-wrapped rows without a line break.
//!
//! Expected texts follow the copy rules, not the implementation: a soft-wrapped
//! row continues the logical line (no `\n`, its trailing blanks are text), and
//! a logical line ends with `\n` (its trailing blanks are padding).

use super::support::{RecordingSink, feed, test_config};
use crate::term::Term;

#[test]
fn long_line_wrapped_over_three_rows_copies_without_newlines() {
    // 26 characters in 10 columns: rows 0 and 1 are soft-wrapped, row 2 is the end.
    let mut term = Term::new(test_config(10, 5));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"abcdefghijklmnopqrstuvwxyz");
    term.set_selection(Some((0, 0, 5, 2)));
    assert_eq!(term.selection_text(), "abcdefghijklmnopqrstuvwxyz");
}

#[test]
fn two_logical_lines_are_joined_with_newline() {
    let mut term = Term::new(test_config(10, 5));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"abc\r\nxyz");
    term.set_selection(Some((0, 0, 2, 1)));
    assert_eq!(term.selection_text(), "abc\nxyz");
}

#[test]
fn trailing_space_of_wrapped_row_is_kept() {
    // "abc " fills the 4-column row, so the row wraps after its space.
    let mut term = Term::new(test_config(4, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"abc d");
    term.set_selection(Some((0, 0, 0, 1)));
    assert_eq!(term.selection_text(), "abc d");
}

#[test]
fn wide_character_moved_by_wrap_adds_no_space() {
    // "日" does not fit in the last column, so column 3 becomes padding and
    // the wide glyph starts the next row; the copy must read "abc日".
    let mut term = Term::new(test_config(4, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, "abc日".as_bytes());
    term.set_selection(Some((0, 0, 1, 1)));
    assert_eq!(term.selection_text(), "abc日");
}

#[test]
fn wrapped_row_in_scrollback_is_joined_with_visible_row() {
    let mut term = Term::new(test_config(10, 2));
    let mut sink = RecordingSink::new();
    // Row 0 wraps into row 1; the next line feed pushes row 0 into scrollback.
    feed(&mut term, &mut sink, b"abcdefghijklm\r\nxyz");
    assert_eq!(term.scrollback_len(), 1);
    // Scrollback row 0, then visible rows "klm" (end of logical line) and "xyz".
    term.set_selection(Some((0, 0, 2, 2)));
    assert_eq!(term.selection_text(), "abcdefghijklm\nxyz");
}

#[test]
fn blanks_of_wrapped_row_before_line_break_are_padding() {
    // "a" plus five spaces: the first row "a   " wraps, the second row "  " ends the logical line.
    let mut term = Term::new(test_config(4, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"a     \r\nb");
    term.set_selection(Some((0, 0, 0, 2)));
    assert_eq!(term.selection_text(), "a\nb");
}

#[test]
fn non_space_whitespace_before_line_break_is_kept() {
    // U+00A0 is text, not an empty cell, so it stays before the line break.
    let mut term = Term::new(test_config(10, 5));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, "a\u{a0}\r\nb".as_bytes());
    term.set_selection(Some((0, 0, 0, 1)));
    assert_eq!(term.selection_text(), "a\u{a0}\nb");
}

#[test]
fn trailing_blank_rows_add_no_line_breaks() {
    // Rows 1..=3 are blank padding after the line "abc" and must not produce newlines.
    let mut term = Term::new(test_config(10, 5));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"abc\r\n");
    term.set_selection(Some((0, 0, 9, 3)));
    assert_eq!(term.selection_text(), "abc");
}

#[test]
fn alt_screen_selection_reads_visible_rows_not_scrollback() {
    let mut term = Term::new(test_config(10, 2));
    let mut sink = RecordingSink::new();
    // Fill the scrollback on the main screen first.
    feed(&mut term, &mut sink, b"1\r\n2\r\n3\r\n4");
    assert!(term.scrollback_len() > 0);
    // Enter the alternate screen (hides scrollback), home the cursor, print on row 0.
    feed(&mut term, &mut sink, b"\x1b[?1049h\x1b[HALT");
    assert_eq!(term.scrollback_len(), 0);
    term.set_selection(Some((0, 0, 2, 0)));
    assert_eq!(term.selection_text(), "ALT");
}
