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
fn find_url_trims_long_closing_bracket_run() {
    // 8000 unbalanced `)` after `https://a` are all cut; the URL is the 9 cells of
    // `https://a`. The token stays inside the search window, so the whole run is trimmed.
    let mut text = b"https://a".to_vec();
    text.resize(text.len() + 8_000, b')');
    let term = term_with(text.len(), &text);
    assert_eq!(find_url(term.visible_row(0).unwrap(), 0), Some(0..9));
}

#[test]
fn find_url_cuts_only_unbalanced_closers() {
    // `(b)` is balanced; of the three `)` after it, the last two are unmatched and cut.
    let term = term_with(40, b"https://a/(b)))");
    assert_eq!(find_url(term.visible_row(0).unwrap(), 0), Some(0..13));
}

#[test]
fn find_url_token_beyond_window_is_none() {
    // The token is 20008 cells with no separator. Column 10000 is more than
    // 8192 cells from the token end, so the URL is too long to report.
    let mut text = b"https://".to_vec();
    text.resize(20_008, b'a');
    let term = term_with(20_008, &text);
    assert_eq!(find_url(term.visible_row(0).unwrap(), 10_000), None);
}

#[test]
fn find_url_finds_url_in_long_line() {
    // 9000 cells of `x`, a space, then the URL at cells 9001-9009; column 9005 is inside the window.
    let mut text = vec![b'x'; 9_000];
    text.extend_from_slice(b" https://a");
    let term = term_with(9_010, &text);
    assert_eq!(find_url(term.visible_row(0).unwrap(), 9_005), Some(9_001..9_010));
}

#[test]
fn link_at_window_skips_rows_before_it() {
    // Row 0 ends at cell 9000, which is more than 8192 cells before the clicked cell
    // (logical cell 17495), so the wrapped URL is found from row 1 alone.
    let mut text = vec![b'x'; 17_490];
    text.extend_from_slice(b" https://a");
    let term = term_with(9_000, &text);
    assert_eq!(
        term.link_at(1, 8_495, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(1, 8_491..8_500)],
        })
    );
}

#[test]
fn osc8_adjacent_equal_uri_without_id_are_distinct() {
    // Two openings with equal URI and no id, not closed in between: two links (spec 5.5).
    let term = term_with(
        10,
        b"\x1b]8;;https://a\x1b\\a\x1b]8;;https://a\x1b\\b\x1b]8;;\x1b\\",
    );
    assert_eq!(
        term.link_at(0, 0, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 0..1)],
        })
    );
    assert_eq!(
        term.link_at(0, 1, 0..3),
        Some(LinkTarget {
            uri: "https://a".into(),
            cells: vec![(0, 1..2)],
        })
    );
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

#[test]
fn osc8_id_of_256_bytes_is_kept() {
    let id = "i".repeat(256);
    let term = term_with(
        10,
        format!("\x1b]8;id={id};https://a\x1b\\a\x1b]8;;\x1b\\").as_bytes(),
    );
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://a"));
    let link = term.visible_row(0).unwrap()[0].hyperlink().unwrap();
    assert_eq!(link.id.as_deref(), Some(id.as_str()));
}

#[test]
fn osc8_id_over_256_bytes_voids_sequence_and_closes_link() {
    // The oversized id voids the second OSC 8, so the open link `https://ok` closes
    // and the text after it is plain.
    let id = "i".repeat(257);
    let term = term_with(
        10,
        format!("\x1b]8;;https://ok\x1b\\a\x1b]8;id={id};https://a\x1b\\b\x1b]8;;\x1b\\")
            .as_bytes(),
    );
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://ok"));
    assert_eq!(uri_at(&term, 0, 1), None);
}

#[test]
fn osc8_c1_control_in_uri_ignored() {
    // U+0085 is two bytes in UTF-8, so only the decoded character shows it is a C1 control.
    let term = term_with(
        10,
        b"\x1b]8;;https://ok\x1b\\a\x1b]8;;https://a\xc2\x85b\x1b\\b\x1b]8;;\x1b\\",
    );
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://ok"));
    assert_eq!(uri_at(&term, 0, 1), None);
}

#[test]
fn osc8_budget_caps_live_links_and_frees_them_on_reset() {
    // Each link charges its 1000-byte URI against the 8 MiB budget (8 388 608 bytes),
    // so 8 388 links fit and the next one is printed without a link.
    const BUDGET_BYTES: usize = 8 * 1024 * 1024;
    const URI_BYTES: usize = 1000;
    let accepted = BUDGET_BYTES / URI_BYTES;
    let tail = "x".repeat(URI_BYTES - "https://000000/".len());
    let mut input = Vec::new();
    for index in 0..=accepted {
        let link = format!("\x1b]8;;https://{index:06}/{tail}\x1b\\");
        input.extend_from_slice(link.as_bytes());
        input.extend_from_slice(b"a\x1b]8;;\x1b\\");
    }
    let mut term = term_with(accepted + 16, &input);
    let row = term.visible_row(0).unwrap();
    assert!(row[accepted - 1].hyperlink().is_some());
    assert!(row[accepted].hyperlink().is_none());

    // Hard reset drops every cell, so the budget is free again.
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1bc\x1b]8;;https://ok\x1b\\a");
    assert_eq!(uri_at(&term, 0, 0).as_deref(), Some("https://ok"));
}

#[test]
fn link_at_long_logical_line_finds_url_in_window() {
    // 50 000 cells in one logical line of 100-column rows: 25 000 `x`, a space, the URL
    // at cells 25 001-25 011, a space, then 24 987 `x`. Row 250 holds the URL at columns 1-11.
    let mut text = vec![b'x'; 25_000];
    text.extend_from_slice(b" https://a/b ");
    text.extend(std::iter::repeat_n(b'x', 24_987));
    let term = term_with(100, &text);
    assert_eq!(
        term.link_at(250, 5, 0..600),
        Some(LinkTarget {
            uri: "https://a/b".into(),
            cells: vec![(250, 1..12)],
        })
    );
}
