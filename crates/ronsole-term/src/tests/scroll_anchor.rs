use super::support::{RecordingSink, feed, test_config};
use crate::scroll_anchor::{REDRAW_QUIET, ScrollbackAnchor};
use crate::term::{Term, TermConfig};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// One fixed instant: holds at it never reach the redraw deadline, so the
/// tests do not depend on how fast they run.
fn now() -> Instant {
    static NOW: OnceLock<Instant> = OnceLock::new();
    *NOW.get_or_init(Instant::now)
}

/// The first instant at which a redraw window opened or active at `now()`
/// has gone quiet.
fn quiet() -> Instant {
    now() + REDRAW_QUIET
}

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
    assert_eq!(anchor.hold(&term, now()), 0);
    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.hold(&term, now()), 4);
    assert_eq!(anchor.hold(&term, now()), 0);
    // Output that stays on the screen pushes nothing.
    feed(&mut term, &mut sink, b"\x1b[Hno scroll");
    assert_eq!(anchor.hold(&term, now()), 0);
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
    assert_eq!(anchor.hold(&term, now()), 0);

    feed(&mut term, &mut sink, &lines(5));
    assert_eq!(term.scrollback_len(), 8);
    assert_eq!(anchor.hold(&term, now()), 5);
}

#[test]
fn release_forgets_lines_pushed_while_following() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());
    anchor.release();
    feed(&mut term, &mut sink, &lines(6));

    assert_eq!(anchor.hold(&term, now()), 0);
    feed(&mut term, &mut sink, &lines(2));
    assert_eq!(anchor.hold(&term, now()), 2);
}

#[test]
fn resize_round_trip_is_not_output() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(5));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());
    let pushed_before = term.scrollback_pushed();

    // Shrinking moves top rows into the scrollback; growing pulls them back.
    term.resize(10, 1);
    assert_ne!(term.scrollback_pushed(), pushed_before);
    term.resize(10, 3);

    assert_eq!(anchor.hold(&term, now()), 0);
    assert_eq!(anchor.pending(&term, now()), 0);
}

#[test]
fn pending_reports_lines_without_consuming_them() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(5));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());

    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.pending(&term, now()), 4);
    assert_eq!(anchor.pending(&term, now()), 4);
    assert_eq!(anchor.hold(&term, now()), 4);
    assert_eq!(anchor.pending(&term, now()), 0);
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
    anchor.hold(&term, now());

    feed(&mut term, &mut sink, &full_redraw(20));
    assert_eq!(anchor.pending(&term, now()), 0);
    assert_eq!(anchor.hold(&term, now()), 0);
    // Output went quiet: the redraw window closes.
    assert_eq!(anchor.hold(&term, quiet()), 0);

    // Output after the redraw moves the view again.
    feed(&mut term, &mut sink, &lines(3));
    assert_eq!(anchor.hold(&term, quiet()), 3);
}

/// Offset growth over a redraw of `reprint` lines whose stream a frame cuts
/// after `split` lines, until output goes quiet.
fn redraw_growth(split: usize, reprint: usize) -> usize {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());

    feed(&mut term, &mut sink, &full_redraw(split));
    let mut grown = anchor.hold(&term, now());
    feed(&mut term, &mut sink, &lines(reprint - split));
    grown += anchor.hold(&term, now());
    grown + anchor.hold(&term, quiet())
}

#[test]
fn redraw_moves_the_offset_alike_wherever_a_frame_cuts_it() {
    // The reprint is longer than the cleared history.
    assert_eq!(redraw_growth(22, 22), 0);
    assert_eq!(redraw_growth(8, 22), 0);
    assert_eq!(redraw_growth(0, 22), 0);
}

#[test]
fn quiet_frame_before_the_reprint_keeps_the_redraw_window_open() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());

    feed(&mut term, &mut sink, &full_redraw(0));
    assert_eq!(anchor.hold(&term, now()), 0);
    // A frame lands before the reprint reaches the terminal.
    assert_eq!(anchor.hold(&term, now()), 0);
    assert!(anchor.redrawing(&term, now()));
    feed(&mut term, &mut sink, &lines(22));
    assert_eq!(anchor.hold(&term, now()), 0);
}

#[test]
fn shell_history_before_a_redraw_does_not_absorb_later_output() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    // Shell history, then an app that redraws only its own short transcript.
    feed(&mut term, &mut sink, &lines(30));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());
    feed(&mut term, &mut sink, &full_redraw(5));
    assert_eq!(anchor.hold(&term, now()), 0);
    assert_eq!(anchor.hold(&term, quiet()), 0);

    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.pending(&term, quiet()), 4);
    assert_eq!(anchor.hold(&term, quiet()), 4);
}

#[test]
fn split_reprint_beyond_the_scrollback_limit_does_not_grow_the_offset() {
    let config = TermConfig {
        scrollback_lines: 8,
        ..test_config(10, 3)
    };
    let mut term = Term::new(config);
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());

    feed(&mut term, &mut sink, &full_redraw(8));
    assert_eq!(anchor.hold(&term, now()), 0);
    // The rest of the reprint evicts lines from the full scrollback.
    feed(&mut term, &mut sink, &lines(12));
    assert_eq!(anchor.pending(&term, now()), 0);
    assert_eq!(anchor.hold(&term, now()), 0);
    assert_eq!(anchor.hold(&term, quiet()), 0);

    feed(&mut term, &mut sink, &lines(2));
    assert_eq!(anchor.hold(&term, quiet()), 2);
}

#[test]
fn release_closes_the_redraw_window() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());
    feed(&mut term, &mut sink, b"\x1b[3J");
    anchor.hold(&term, now());
    assert!(anchor.redrawing(&term, now()));

    anchor.release();
    assert!(!anchor.redrawing(&term, now()));
    anchor.hold(&term, now());
    feed(&mut term, &mut sink, &lines(4));
    assert_eq!(anchor.hold(&term, now()), 4);
}

#[test]
fn redraw_window_closes_at_its_deadline_under_steady_output() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    let start = now();
    let at = |ms| start + Duration::from_millis(ms);
    anchor.hold(&term, start);

    feed(&mut term, &mut sink, &full_redraw(5));
    assert_eq!(anchor.hold(&term, start), 0);
    // Output keeps coming without a pause long enough to end the redraw.
    for ms in [40, 80, 120] {
        feed(&mut term, &mut sink, &lines(5));
        assert_eq!(anchor.hold(&term, at(ms)), 0);
        assert!(anchor.redrawing(&term, at(ms)));
    }
    // Past the deadline the window closes and output counts again.
    assert!(!anchor.redrawing(&term, at(160)));
    feed(&mut term, &mut sink, &lines(5));
    assert_eq!(anchor.pending(&term, at(160)), 5);
    assert_eq!(anchor.hold(&term, at(160)), 5);
    feed(&mut term, &mut sink, &lines(3));
    assert_eq!(anchor.hold(&term, at(170)), 3);
}

#[test]
fn clear_since_the_last_hold_counts_as_redrawing() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    feed(&mut term, &mut sink, b"\x1b[3J");
    // A released anchor has no offset to keep.
    assert!(!anchor.redrawing(&term, now()));
    anchor.hold(&term, now());
    assert!(!anchor.redrawing(&term, now()));

    feed(&mut term, &mut sink, &full_redraw(8));
    assert!(anchor.redrawing(&term, now()));
    assert_eq!(anchor.pending(&term, now()), 0);
    assert_eq!(anchor.hold(&term, now()), 0);
    assert!(anchor.redrawing(&term, now()));
    // A quiet window stops redrawing without waiting for the next hold.
    assert!(!anchor.redrawing(&term, quiet()));
    assert_eq!(anchor.hold(&term, quiet()), 0);
    assert!(!anchor.redrawing(&term, quiet()));
}

#[test]
fn redraw_window_counts_output_after_an_idle_gap() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    let start = now();
    anchor.hold(&term, start);

    feed(&mut term, &mut sink, &full_redraw(20));
    assert_eq!(anchor.hold(&term, start), 0);
    // No frame comes after the reprint; genuine output arrives much later.
    feed(&mut term, &mut sink, &lines(6));
    let later = start + Duration::from_secs(10);
    assert!(!anchor.redrawing(&term, later));
    assert_eq!(anchor.pending(&term, later), 6);
    assert_eq!(anchor.hold(&term, later), 6);
}

#[test]
fn redraw_window_stays_open_over_a_pause_between_reprint_chunks() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(40));
    let mut anchor = ScrollbackAnchor::new();
    let start = now();
    anchor.hold(&term, start);

    feed(&mut term, &mut sink, &full_redraw(10));
    assert_eq!(anchor.hold(&term, start), 0);
    // A frame lands in a short pause of the reprint.
    assert_eq!(anchor.hold(&term, start + Duration::from_millis(10)), 0);
    assert!(anchor.redrawing(&term, start + Duration::from_millis(10)));
    feed(&mut term, &mut sink, &lines(30));
    assert_eq!(anchor.hold(&term, start + Duration::from_millis(20)), 0);
}

#[test]
fn redraw_window_output_before_the_clear_does_not_start_the_reprint() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, &lines(20));
    let mut anchor = ScrollbackAnchor::new();
    let start = now();
    anchor.hold(&term, start);

    // Lines that the clear erases arrive in the same frame as the clear.
    feed(&mut term, &mut sink, &lines(3));
    feed(&mut term, &mut sink, &full_redraw(0));
    assert_eq!(anchor.hold(&term, start), 0);
    // A frame lands before the reprint reaches the terminal.
    assert_eq!(anchor.hold(&term, start + Duration::from_millis(10)), 0);
    feed(&mut term, &mut sink, &lines(22));
    assert_eq!(anchor.hold(&term, start + Duration::from_millis(20)), 0);
}

#[test]
fn pending_is_zero_while_released() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let mut anchor = ScrollbackAnchor::new();
    anchor.hold(&term, now());
    feed(&mut term, &mut sink, &lines(3));

    anchor.release();
    assert_eq!(anchor.pending(&term, now()), 0);
}
