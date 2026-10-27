//! BEL (0x07) reaches the host through `TermSink::bell`. The expected counts
//! follow the byte stream (one call per BEL byte), not the performer code.

use super::support::{RecordingSink, feed, test_config};
use crate::term::Term;

#[test]
fn bel_byte_calls_sink_bell_once() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x07");

    assert_eq!(sink.bells, 1);
    assert!(sink.bytes.is_empty());
}

#[test]
fn bel_inside_text_and_escape_sequences_is_one_call_each() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"a\x07b\x07\x07");

    assert_eq!(sink.bells, 3);
    assert_eq!(
        term.visible_row(0).map(|row| [row[0].ch(), row[1].ch()]),
        Some(['a', 'b'])
    );
}
