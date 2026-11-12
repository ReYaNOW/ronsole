use super::support::{RecordingSink, feed, test_config};
use crate::scroll_anchor::ScrollbackAnchor;
use crate::term::{Term, TermConfig};

fn lines(count: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in 0..count {
        bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
    }
    bytes
}

#[test]
fn hold_counts_lines_pushed_since_previous_hold() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(5));
    let mut anchor = ScrollbackAnchor::new();

    // The first hold only records the count.
    assert_eq!(anchor.hold(&term), 0);
    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.hold(&term), 4);
    assert_eq!(anchor.hold(&term), 0);
    // Output that stays on the screen pushes nothing.
    feed(&mut term, &mut sink, b"\x1b[Hno scroll");
    assert_eq!(anchor.hold(&term), 0);
}

#[test]
fn hold_counts_lines_evicted_from_full_scrollback() {
    let config = TermConfig {
        scrollback_lines: 8,
        ..test_config(10, 3)
    };
    let mut term = Term::new(config);
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    assert_eq!(term.scrollback_len(), 8);
    let mut anchor = ScrollbackAnchor::new();
    assert_eq!(anchor.hold(&term), 0);

    feed(&mut term, &mut sink, &lines(5));
    assert_eq!(term.scrollback_len(), 8);
    assert_eq!(anchor.hold(&term), 5);
}

#[test]
fn release_forgets_lines_pushed_while_following() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);
    anchor.release();
    feed(&mut term, &mut sink, &lines(6));

    assert_eq!(anchor.hold(&term), 0);
    feed(&mut term, &mut sink, &lines(2));
    assert_eq!(anchor.hold(&term), 2);
}

#[test]
fn resize_round_trip_is_not_output() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(5));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);
    let pushed_before = term.scrollback_pushed();

    // Shrinking moves top rows into the scrollback; growing pulls them back.
    term.resize(10, 1);
    assert_ne!(term.scrollback_pushed(), pushed_before);
    term.resize(10, 3);

    assert_eq!(anchor.hold(&term), 0);
    assert_eq!(anchor.pending(&term), 0);
}

#[test]
fn pending_reports_lines_without_consuming_them() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(5));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);

    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.pending(&term), 4);
    assert_eq!(anchor.pending(&term), 4);
    assert_eq!(anchor.hold(&term), 4);
    assert_eq!(anchor.pending(&term), 0);
}

/// An Ink-style full redraw: clear the screen and the history, reprint all.
fn full_redraw(count: usize) -> Vec<u8> {
    let mut bytes = b"\x1b[2J\x1b[3J\x1b[H".to_vec();
    bytes.extend_from_slice(&lines(count));
    bytes
}

#[test]
fn history_clear_and_reprint_does_not_grow_the_offset() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);

    feed(&mut term, &mut sink, &full_redraw(20));
    assert_eq!(anchor.pending(&term), 0);
    assert_eq!(anchor.hold(&term), 0);

    // Output after the redraw moves the view again.
    feed(&mut term, &mut sink, &lines(3));
    assert_eq!(anchor.hold(&term), 3);
}

#[test]
fn reprint_split_across_holds_refills_the_cleared_history_first() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let cleared = term.scrollback_len();
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);

    feed(&mut term, &mut sink, &full_redraw(8));
    assert_eq!(anchor.hold(&term), 0);
    // The rest of the reprint refills the cleared history; the lines beyond
    // it are new output.
    let refilled_after_split = cleared - term.scrollback_len();
    feed(&mut term, &mut sink, &lines(refilled_after_split + 2));
    assert_eq!(anchor.pending(&term), 2);
    assert_eq!(anchor.hold(&term), 2);
}

#[test]
fn release_drops_the_refill_debt() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);
    feed(&mut term, &mut sink, b"\x1b[3J");
    anchor.hold(&term);

    anchor.release();
    anchor.hold(&term);
    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.hold(&term), 4);
}

#[test]
fn pending_is_zero_while_released() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term);
    feed(&mut term, &mut sink, &lines(3));

    anchor.release();
    assert_eq!(anchor.pending(&term), 0);
}
