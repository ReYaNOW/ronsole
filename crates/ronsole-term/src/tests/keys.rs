use super::support::{RecordingSink, feed, test_config};
use crate::term::Term;

/// Sends `bytes` and returns exactly the reply bytes they produced.
fn reply_to(term: &mut Term, sink: &mut RecordingSink, bytes: &[u8]) -> Vec<u8> {
    sink.bytes.clear();
    feed(term, sink, bytes);
    std::mem::take(&mut sink.bytes)
}

/// Expected replies come from the Kitty protocol reference (`CSI ? flags u`),
/// not from the implementation.
#[test]
fn kitty_stack_push_pop_set_query() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");

    // Push 1 saves 0; query shows 1.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>1u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?1u");

    // Push 5 keeps only the supported bits: 5 & 0b11011 == 1 (flag 4 is masked).
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>5u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?1u");

    // Pop restores the flags saved by the push of 5, which are 1.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[<u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?1u");

    // Set with mode 1 (default replace), then OR (2), then AND-NOT (3).
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[=3;1u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?3u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[=8;2u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?11u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[=1;3u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?10u");

    // The stack still holds the 0 saved by the first push.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[<u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");

    // Pop from an empty stack gives flags 0.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[<u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");

    // Garbage flags are masked to the supported bits: 255 & 0b11011 == 27.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>255u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?27u");
}

#[test]
fn kitty_stacks_are_per_screen() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>1u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1049h"), b"");
    // The alternate screen has its own empty stack and flags 0.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>9u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?9u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1049l"), b"");
    // Back on the main screen, flags 1 pushed there are still active.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?1u");
}

#[test]
fn kitty_stack_overflow_drops_oldest() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    let mut bytes = b"\x1b[=1u".to_vec();
    for _ in 0..32 {
        bytes.extend_from_slice(b"\x1b[>2u");
    }
    // The 33rd push evicts the oldest saved entry, which is the 1 at the bottom.
    bytes.extend_from_slice(b"\x1b[>8u");
    for _ in 0..32 {
        bytes.extend_from_slice(b"\x1b[<u");
    }
    assert_eq!(reply_to(&mut term, &mut sink, &bytes), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?2u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[<u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");
}

#[test]
fn ris_and_decstr_reset_kitty() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    // RIS clears flags and the stack: a pop afterwards must give 0, not the stale 3.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>3u\x1b[>5u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1bc"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[<u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");

    // DECSTR does the same.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[=2u\x1b[>1u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[!p"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[<u"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");
}

#[test]
fn kitty_query_keeps_pending_wrap() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"xxxxxxxxxx");
    assert_eq!(term.cursor(), (9, 0));

    // Kitty queries and pushes do not move the cursor, so the wrap stays pending.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u\x1b[>1u"), b"\x1b[?0u");
    assert_eq!(term.cursor(), (9, 0));

    feed(&mut term, &mut sink, b"y");
    assert_eq!(term.cursor(), (1, 1));
    assert_eq!(term.visible_row(1).map(|row| row[0].ch()), Some('y'));
}

#[test]
fn csi_question_u_does_not_restore_cursor() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"ab\x1b7cd");
    assert_eq!(term.cursor(), (4, 0));

    // Kitty forms with intermediates leave the cursor where it is.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?0u");
    assert_eq!(term.cursor(), (4, 0));
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>1u\x1b[=2u\x1b[<u"), b"");
    assert_eq!(term.cursor(), (4, 0));

    // Plain `CSI u` is still DECRC and restores the saved cursor.
    feed(&mut term, &mut sink, b"\x1b[u");
    assert_eq!(term.cursor(), (2, 0));
}
