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
