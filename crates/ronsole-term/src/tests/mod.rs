mod csi;
mod edges;
mod osc;
mod reflow;
mod support;

use crate::term::Term;
use support::{RecordingSink, feed, test_config};

#[test]
fn advance_prints_text_and_replies_to_da1() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"ab");
    assert_eq!(
        term.visible_row(0).map(|row| [row[0].ch(), row[1].ch()]),
        Some(['a', 'b'])
    );

    // DA1 reply, as written by `Performer` for `CSI c` (perform.rs DA1 arm).
    feed(&mut term, &mut sink, b"\x1b[c");
    assert_eq!(sink.bytes, b"\x1b[?62c");
}
