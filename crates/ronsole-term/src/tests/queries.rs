use super::support::{RecordingSink, feed, test_config};
use crate::term::{Term, TermIdentity};

/// Sends `bytes` and returns exactly the reply bytes they produced.
fn reply_to(term: &mut Term, sink: &mut RecordingSink, bytes: &[u8]) -> Vec<u8> {
    sink.bytes.clear();
    feed(term, sink, bytes);
    std::mem::take(&mut sink.bytes)
}

fn term_with_identity(version: &'static str) -> Term {
    let mut config = test_config(10, 3);
    config.identity = TermIdentity {
        name: "ronsole",
        version,
    };
    Term::new(config)
}

#[test]
fn da1_only_without_intermediates() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[c"), b"\x1b[?62c");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[0c"), b"\x1b[?62c");
    // `CSI > c` is DA2, so it must not produce the DA1 reply.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>c"), b"\x1b[>1;0;0c");
    // DA3 (`CSI = c`) is not answered.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[=c"), b"");
}

#[test]
fn da2_reports_identity_version() {
    let cases: [(&'static str, &[u8]); 3] = [
        ("0.1.0", b"\x1b[>1;100;0c"),
        ("1.2.3", b"\x1b[>1;10203;0c"),
        ("x", b"\x1b[>1;0;0c"),
    ];
    for (version, expected) in cases {
        let mut term = term_with_identity(version);
        let mut sink = RecordingSink::new();
        assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>c"), expected);
        assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>0c"), expected);
    }
}

#[test]
fn xtversion_reports_name_and_version() {
    let mut term = term_with_identity("0.1.0");
    let mut sink = RecordingSink::new();
    assert_eq!(
        reply_to(&mut term, &mut sink, b"\x1b[>q"),
        b"\x1bP>|ronsole(0.1.0)\x1b\\"
    );
    assert_eq!(
        reply_to(&mut term, &mut sink, b"\x1b[>0q"),
        b"\x1bP>|ronsole(0.1.0)\x1b\\"
    );
}

#[test]
fn decrqm_private_modes() {
    // Modes listed in the spec's DECRQM section (5.2).
    const MODES: [u16; 13] = [1, 6, 7, 25, 47, 1047, 1049, 1000, 1002, 1003, 1004, 1006, 2004];
    for mode in MODES {
        let mut term = Term::new(test_config(10, 3));
        let mut sink = RecordingSink::new();
        let query = format!("\x1b[?{mode}$p");
        // Autowrap and the visible cursor are on by default; everything else starts reset.
        let initial = if mode == 7 || mode == 25 { 1 } else { 2 };
        assert_eq!(
            reply_to(&mut term, &mut sink, query.as_bytes()),
            format!("\x1b[?{mode};{initial}$y").as_bytes()
        );

        feed(&mut term, &mut sink, format!("\x1b[?{mode}l").as_bytes());
        assert_eq!(
            reply_to(&mut term, &mut sink, query.as_bytes()),
            format!("\x1b[?{mode};2$y").as_bytes()
        );

        feed(&mut term, &mut sink, format!("\x1b[?{mode}h").as_bytes());
        assert_eq!(
            reply_to(&mut term, &mut sink, query.as_bytes()),
            format!("\x1b[?{mode};1$y").as_bytes()
        );
    }

    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    // Outside a synchronized-output hold, 2026 is reset.
    assert_eq!(
        reply_to(&mut term, &mut sink, b"\x1b[?2026$p"),
        b"\x1b[?2026;2$y"
    );
    // Unknown modes report 0.
    assert_eq!(
        reply_to(&mut term, &mut sink, b"\x1b[?9999$p"),
        b"\x1b[?9999;0$y"
    );
}

#[test]
fn queries_keep_pending_wrap() {
    // Each query must leave the pending wrap alone, so the next glyph starts row 1.
    let queries: [&[u8]; 3] = [b"\x1b[>c", b"\x1b[>q", b"\x1b[?7$p"];
    for query in queries {
        let mut term = Term::new(test_config(10, 3));
        let mut sink = RecordingSink::new();
        feed(&mut term, &mut sink, b"xxxxxxxxxx");
        feed(&mut term, &mut sink, query);
        feed(&mut term, &mut sink, b"y");
        assert_eq!(
            term.visible_row(1).map(|row| row[0].ch()),
            Some('y'),
            "query {query:?} moved the cursor"
        );
        assert_eq!(term.cursor(), (1, 1));
    }
}

#[test]
fn decrqm_ansi_insert_mode() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[4$p"), b"\x1b[4;2$y");
    feed(&mut term, &mut sink, b"\x1b[4h");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[4$p"), b"\x1b[4;1$y");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[20$p"), b"\x1b[20;0$y");
}

#[test]
fn reply_buffer_capped_at_64kib() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let input = b"\x1b[c".repeat(20_000);
    feed(&mut term, &mut sink, &input);
    // Each DA1 reply is 6 bytes: 10 922 fit under 65 536 and the rest are dropped whole.
    assert_eq!(sink.calls.len(), 1);
    assert_eq!(sink.calls[0].len(), 10_922 * 6);
}

#[test]
fn malformed_queries_are_bounded() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    // Not specified by the spec: a DECRQM without a mode number gets no reply.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?$p"), b"");
    // XTVERSION is answered only for the parameter-less or `0` form.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[>999q"), b"");
    // Oversized mode number: the reply echoes the parsed value and reports 0.
    let huge = reply_to(&mut term, &mut sink, b"\x1b[?99999999999999999999$p");
    assert!(huge.starts_with(b"\x1b[?") && huge.ends_with(b";0$y"));
    // A query split across batches is answered once it completes.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?20"), b"");
    assert_eq!(reply_to(&mut term, &mut sink, b"26$p"), b"\x1b[?2026;2$y");
}
