//! OSC 8 hyperlinks, `Term::link_at` and plain URL detection.
//! Expected values are written by hand from the spec (5.5, 5.6) and the brief.

use super::support::{RecordingSink, feed, test_config};
use crate::url::find_url;
use crate::{Cell, LinkTarget, Term};

/// Term of `cols` x 3 with `text` fed through the parser.
fn term_with(cols: usize, text: &[u8]) -> Term {
    let mut term = Term::new(test_config(cols, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, text);
    term
}

/// URI of the OSC 8 hyperlink on cell (`row`, `col`), if any.
fn uri_at(term: &Term, row: usize, col: usize) -> Option<String> {
    term.visible_row(row)?
        .get(col)?
        .hyperlink()
        .map(|link| link.uri.to_string())
}

/// URI that `link_at` reports for (`row`, `col`) over all visible rows.
fn link_uri(term: &Term, row: usize, col: usize) -> Option<String> {
    term.link_at(row, col, 0..3)
        .map(|target| target.uri.to_string())
}

#[test]
fn osc8_marks_cells_and_closes() {
    let term = term_with(10, b"\x1b]8;;https://a\x1b\\ab\x1b]8;;\x1b\\c");
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://a"));
    assert_eq!(uri_at(&term, 0, 1).as_deref(), Some("https://a"));
    assert_eq!(uri_at(&term, 0, 2), None);
    assert_eq!(
        term.link_at(0, 0, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 0..2)],
        })
    );
}

#[test]
fn osc8_id_joins_equal_uri() {
    // Two separate OSC 8 openings with the same id and URI share one link.
    let term = term_with(
        10,
        b"\x1b]8;id=x;https://a\x1b\\a\x1b]8;;\x1b\\c\x1b]8;id=x;https://a\x1b\\b\x1b]8;;\x1b\\",
    );
    assert_eq!(
        term.link_at(0, 0, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 0..1), (0, 2..3)],
        })
    );
}

#[test]
fn osc8_without_id_equal_uri_distinct() {
    let term = term_with(
        10,
        b"\x1b]8;;https://a\x1b\\a\x1b]8;;\x1b\\\x1b]8;;https://a\x1b\\b\x1b]8;;\x1b\\",
    );
    assert_eq!(
        term.link_at(0, 0, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 0..1)],
        })
    );
}

#[test]
fn osc8_same_id_different_uri_distinct() {
    let term = term_with(
        10,
        b"\x1b]8;id=x;https://a\x1b\\a\x1b]8;;\x1b\\\x1b]8;id=x;https://b\x1b\\b\x1b]8;;\x1b\\",
    );
    assert_eq!(
        term.link_at(0, 0, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 0..1)],
        })
    );
}

#[test]
fn osc8_too_long_uri_ignored_and_closes() {
    // 2083 bytes is the largest accepted URI.
    let max = format!("https://{}", "a".repeat(2075));
    let at_max = term_with(
        10,
        format!("\x1b]8;;{max}\x1b\\a\x1b]8;;\x1b\\").as_bytes(),
    );
    assert_eq!(uri_at(&at_max, 0, 0), Some(max.clone()));

    // 2084 bytes: ignored, and the link that was open is closed.
    let too_long = format!("https://{}", "a".repeat(2076));
    let term = term_with(
        10,
        format!("\x1b]8;;https://ok\x1b\\a\x1b]8;;{too_long}\x1b\\b").as_bytes(),
    );
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://ok"));
    assert_eq!(uri_at(&term, 0, 1), None);
}

#[test]
fn osc8_control_char_ignored() {
    // DEL reaches the performer inside the OSC string, so the URI check rejects it.
    let term = term_with(10, b"\x1b]8;;https://ok\x1b\\a\x1b]8;;https://a\x7fb\x1b\\b");
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://ok"));
    assert_eq!(uri_at(&term, 0, 1), None);
}

#[test]
#[ignore = "bug: vte drops C0 bytes inside OSC strings before Perform sees them, so the URI becomes https://ab"]
fn osc8_c0_control_char_ignored() {
    let term = term_with(10, b"\x1b]8;;https://ok\x1b\\a\x1b]8;;https://a\x01b\x1b\\b");
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://ok"));
    assert_eq!(uri_at(&term, 0, 1), None);
}

#[test]
fn osc8_uri_with_semicolons() {
    let term = term_with(10, b"\x1b]8;;https://a/?x=1;y=2\x1b\\a\x1b]8;;\x1b\\");
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://a/?x=1;y=2"));
}

#[test]
fn osc8_survives_reflow() {
    let mut term = term_with(10, b"\x1b]8;;https://a\x1b\\abcdefghi\x1b]8;;\x1b\\");
    term.resize(5, 3);
    assert_eq!(
        term.link_at(1, 0, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 0..5), (1, 0..4)],
        })
    );
}

#[test]
fn find_url_trims_trailing_punctuation() {
    let term = term_with(40, b"see https://a.b/c).");
    assert_eq!(
        term.link_at(0, 5, 0..1),
        Some(LinkTarget {
            uri: "https://a.b/c".into(),
            cells: vec![(0, 4..17)],
        })
    );
}

#[test]
fn find_url_keeps_balanced_parens() {
    let term = term_with(40, b"https://w/x_(y)");
    assert_eq!(find_url(term.visible_row(0).unwrap(), 0), Some(0..15));
}

#[test]
fn find_url_counts_wide_chars() {
    // 日 and 本 take columns 0-3, the space is column 4, the URL starts at column 5.
    let term = term_with(20, "日本 https://a".as_bytes());
    let row = term.visible_row(0).unwrap();
    assert_eq!(find_url(row, 5), Some(5..14));
    // Column 1 is the spacer of 日, which is not part of a URL.
    assert_eq!(find_url(row, 1), None);
}

#[test]
fn find_url_none_without_scheme() {
    let term = term_with(40, b"a.b/c d");
    assert_eq!(find_url(term.visible_row(0).unwrap(), 0), None);
}

#[test]
fn find_url_col_outside_is_none() {
    let term = term_with(40, b"https://a");
    let row = term.visible_row(0).unwrap();
    assert_eq!(find_url(row, 39), None);
    assert_eq!(find_url(&row[..9], 9), None);
}

#[test]
fn find_url_rejects_degenerate_input() {
    let empty: &[Cell] = &[];
    assert_eq!(find_url(empty, 0), None);

    let dots = term_with(10, b"....");
    assert_eq!(find_url(dots.visible_row(0).unwrap(), 1), None);

    // A bare scheme with nothing after it is not a URL.
    let scheme_only = term_with(10, b"https://");
    assert_eq!(find_url(scheme_only.visible_row(0).unwrap(), 0), None);
}

#[test]
fn link_at_joins_wrapped_url() {
    // 19 chars in 10 columns: row 0 is soft-wrapped into row 1.
    let term = term_with(10, b"https://ab.c/defghi");
    assert_eq!(
        term.link_at(1, 2, 0..3),
        Some(LinkTarget {
            uri: "https://ab.c/defghi".into(),
            cells: vec![(0, 0..10), (1, 0..9)],
        })
    );
}

#[test]
fn link_at_prefers_osc8_over_url_text() {
    let term = term_with(
        40,
        b"\x1b]8;;https://osc.example\x1b\\https://text.example\x1b]8;;\x1b\\",
    );
    assert_eq!(link_uri(&term, 0, 2).as_deref(), Some("https://osc.example"));
    assert_eq!(
        term.link_at(0, 2, 0..1),
        Some(LinkTarget {
            uri: "https://osc.example".into(),
            cells: vec![(0, 0..20)],
        })
    );
}
