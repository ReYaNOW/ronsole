use super::support::{RecordingSink, test_config};
use crate::term::{AdvanceOutcome, Term};
use std::time::{Duration, Instant};

const SET: &[u8] = b"\x1b[?2026h";
const RESET: &[u8] = b"\x1b[?2026l";
const TWO_MIB: usize = 2 * 1024 * 1024;

/// Text of visible row `row`, without trailing blanks.
fn row_chars(term: &Term, row: usize) -> String {
    term.visible_row(row)
        .map(|cells| cells.iter().map(|cell| cell.ch()).collect::<String>())
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

#[test]
fn sync_holds_grid_until_reset() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let held = term.advance(b"\x1b[?2026hAB", t0, &mut sink);
    assert_eq!(
        held,
        AdvanceOutcome {
            grid_changed: false,
            new_deadline: true
        }
    );
    assert_eq!(row_chars(&term, 0), "");

    let released = term.advance(RESET, t0 + Duration::from_millis(10), &mut sink);
    assert!(released.grid_changed);
    assert_eq!(row_chars(&term, 0), "AB");
    assert_eq!(term.sync_deadline(), None);
}

#[test]
fn sync_reset_split_between_batches() {
    let t0 = Instant::now();
    // The reset is 8 bytes; cut it after each of the first 7 bytes.
    for split in 1..RESET.len() {
        let mut term = Term::new(test_config(10, 3));
        let mut sink = RecordingSink::new();
        term.advance(b"\x1b[?2026hAB", t0, &mut sink);
        term.advance(&RESET[..split], t0, &mut sink);
        let released = term.advance(&RESET[split..], t0, &mut sink);
        assert!(released.grid_changed, "split at {split}");
        assert_eq!(row_chars(&term, 0), "AB", "split at {split}");
        assert_eq!(term.sync_deadline(), None, "split at {split}");
    }
}

#[test]
fn sync_repeated_set_keeps_deadline() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(SET, t0, &mut sink);
    let repeated = term.advance(SET, t0 + Duration::from_millis(100), &mut sink);
    assert_eq!(repeated, AdvanceOutcome::default());
    assert_eq!(
        term.sync_deadline(),
        Some(t0 + Duration::from_millis(150))
    );
}

#[test]
fn sync_hold_needs_lone_2026_param() {
    // A combined set applies its own modes and holds nothing: `?25h` shows the cursor
    // and the text is on the grid at once (only a lone `?2026h` starts a hold).
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(b"\x1b[?25l", t0, &mut sink);
    assert!(!term.cursor_visible);
    let outcome = term.advance(b"\x1b[?25;2026hAB", t0, &mut sink);
    assert!(outcome.grid_changed);
    assert_eq!(term.sync_deadline(), None);
    assert!(term.cursor_visible);
    assert_eq!(row_chars(&term, 0), "AB");
}

#[test]
fn sync_set_alone_reports_new_deadline() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let outcome = term.advance(SET, t0, &mut sink);
    assert_eq!(
        outcome,
        AdvanceOutcome {
            grid_changed: false,
            new_deadline: true
        }
    );
}

#[test]
fn sync_text_before_set_reports_change() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let outcome = term.advance(b"AB\x1b[?2026h", t0, &mut sink);
    assert_eq!(
        outcome,
        AdvanceOutcome {
            grid_changed: true,
            new_deadline: true
        }
    );
    assert_eq!(row_chars(&term, 0), "AB");
}

#[test]
fn sync_nested_set_in_replay_is_noop() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(b"\x1b[?2026hA\x1b[?2026hB\x1b[?2026l", t0, &mut sink);
    assert_eq!(row_chars(&term, 0), "AB");
    assert_eq!(term.sync_deadline(), None);
}

#[test]
fn sync_flush_expired_applies_buffer() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(b"\x1b[?2026hAB", t0, &mut sink);
    let early = term.flush_expired(t0 + Duration::from_millis(149), &mut sink);
    assert_eq!(early, AdvanceOutcome::default());
    assert_eq!(row_chars(&term, 0), "");
    assert_eq!(
        term.sync_deadline(),
        Some(t0 + Duration::from_millis(150))
    );

    let expired = term.flush_expired(t0 + Duration::from_millis(150), &mut sink);
    assert_eq!(
        expired,
        AdvanceOutcome {
            grid_changed: true,
            new_deadline: false
        }
    );
    assert_eq!(row_chars(&term, 0), "AB");
    assert_eq!(term.sync_deadline(), None);
}

#[test]
fn sync_buffer_limit_2mib_applies() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let mut input = SET.to_vec();
    input.resize(input.len() + TWO_MIB + 1, b'A');
    // The last byte marks the end of the input: 2 MiB + 1 printed cells on a
    // 10-column grid leave 3 cells on the bottom row, "AAB".
    *input.last_mut().unwrap() = b'B';
    let outcome = term.advance(&input, t0, &mut sink);
    assert_eq!(
        outcome,
        AdvanceOutcome {
            grid_changed: true,
            new_deadline: false
        }
    );
    assert_eq!(row_chars(&term, 0), "AAAAAAAAAA");
    assert_eq!(row_chars(&term, 1), "AAAAAAAAAA");
    assert_eq!(row_chars(&term, 2), "AAB");
    assert_eq!(term.sync_deadline(), None);
}

#[test]
fn sync_forty_da_requests_one_reply() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let mut input = SET.to_vec();
    for _ in 0..40 {
        input.extend_from_slice(b"\x1b[c");
    }
    input.extend_from_slice(RESET);
    term.advance(&input, t0, &mut sink);
    assert_eq!(sink.calls.len(), 1);
    assert_eq!(sink.bytes, b"\x1b[?62c".repeat(40));
}

#[test]
fn decrqm_2026_inside_buffer_answers_reset() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(SET, t0, &mut sink);
    term.advance(b"\x1b[?2026$p", t0, &mut sink);
    assert_eq!(sink.bytes, b"");
    term.advance(RESET, t0, &mut sink);
    assert_eq!(sink.bytes, b"\x1b[?2026;2$y");
}

#[test]
fn sync_advance_reports_no_change_while_active() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(SET, t0, &mut sink);
    let outcome = term.advance(b"ABC", t0, &mut sink);
    assert_eq!(outcome, AdvanceOutcome::default());
}

#[test]
fn sync_buffer_capacity_shrinks_after_flush() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(SET, t0, &mut sink);
    term.advance(&vec![b'A'; 1024 * 1024], t0, &mut sink);
    term.advance(RESET, t0, &mut sink);
    assert!(term.sync_buf.capacity() <= 65_536);
}

#[test]
fn sync_buffer_not_allocated_without_mode() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    term.advance(&vec![b'A'; 1024 * 1024], t0, &mut sink);
    assert_eq!(term.sync_buf.capacity(), 0);
}

#[test]
fn sync_set_split_between_batches_holds_without_change() {
    let t0 = Instant::now();
    // The set is 8 bytes; cut it after each of the first 7 bytes.
    for split in 1..SET.len() {
        let mut term = Term::new(test_config(10, 3));
        let mut sink = RecordingSink::new();
        let first = term.advance(&SET[..split], t0, &mut sink);
        assert_eq!(first, AdvanceOutcome::default(), "split at {split}");

        let mut input = SET[split..].to_vec();
        input.extend_from_slice(b"AB");
        let second = term.advance(&input, t0, &mut sink);
        assert_eq!(
            second,
            AdvanceOutcome {
                grid_changed: false,
                new_deadline: true
            },
            "split at {split}"
        );
        assert_eq!(row_chars(&term, 0), "", "split at {split}");

        term.advance(RESET, t0, &mut sink);
        assert_eq!(row_chars(&term, 0), "AB", "split at {split}");
    }
}

#[test]
fn sync_set_after_reset_in_same_batch_holds_again() {
    let t0 = Instant::now();
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let outcome = term.advance(b"\x1b[?2026hA\x1b[?2026l\x1b[?2026hB", t0, &mut sink);
    assert_eq!(
        outcome,
        AdvanceOutcome {
            grid_changed: true,
            new_deadline: true
        }
    );
    assert_eq!(row_chars(&term, 0), "A");
    assert_eq!(term.sync_deadline(), Some(t0 + Duration::from_millis(150)));
}

#[test]
fn sync_reset_split_then_text_applies_after_reset() {
    let t0 = Instant::now();
    // The reset is 8 bytes; cut it after each of the first 7 bytes.
    for split in 1..RESET.len() {
        let mut term = Term::new(test_config(10, 3));
        let mut sink = RecordingSink::new();
        let mut first = SET.to_vec();
        first.extend_from_slice(b"AB");
        first.extend_from_slice(&RESET[..split]);
        term.advance(&first, t0, &mut sink);

        let mut second = RESET[split..].to_vec();
        second.extend_from_slice(b"CD");
        let released = term.advance(&second, t0, &mut sink);
        assert!(released.grid_changed, "split at {split}");
        assert_eq!(row_chars(&term, 0), "ABCD", "split at {split}");
        assert_eq!(term.sync_deadline(), None, "split at {split}");
    }
}

#[test]
fn sync_many_pairs_in_one_batch_match_one_by_one() {
    let t0 = Instant::now();
    let pair = b"\x1b[?2026hA\x1b[?2026l";
    let mut single = Term::new(test_config(10, 3));
    let mut batch = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let mut input = Vec::new();
    for _ in 0..500 {
        single.advance(pair, t0, &mut sink);
        input.extend_from_slice(pair);
    }
    batch.advance(&input, t0, &mut sink);

    // Feeding the pairs one by one is the reference; the batch must match it.
    for row in 0..3 {
        assert_eq!(row_chars(&batch, row), row_chars(&single, row), "row {row}");
    }
    assert_eq!(single.sync_deadline(), None);
    assert_eq!(batch.sync_deadline(), None);
}
