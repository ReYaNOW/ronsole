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
