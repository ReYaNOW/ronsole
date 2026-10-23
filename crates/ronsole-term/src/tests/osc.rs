use crate::osc::{TERMINAL_TITLE_MAX_BYTES, terminal_programmed_title};
use crate::color::{Rgb, TermPalette};
use crate::term::Term;
use super::support::{RecordingSink, feed, test_config};

#[test]
fn osc_titles_are_control_safe_and_bounded() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x1b]0;~ : htop\x07");
    feed(&mut term, &mut sink, b"\x1b]2;bin : sleep\x1b\\");
    assert_eq!(sink.titles, ["~ : htop", "bin : sleep"]);

    assert_eq!(terminal_programmed_title(&[b"\xff"]), None);
    assert_eq!(
        terminal_programmed_title(&[b"  safe\n\0title  "]).as_deref(),
        Some("safetitle")
    );

    let huge = "x".repeat(TERMINAL_TITLE_MAX_BYTES * 8);
    let sequence = format!("\x1b]2;{huge}\x07");
    feed(&mut term, &mut sink, sequence.as_bytes());
    assert_eq!(
        sink.titles.last().map(String::len),
        Some(TERMINAL_TITLE_MAX_BYTES)
    );
}

/// Term with the palette of the colour tests: foreground, background and
/// cursor from the brief, and a distinct `ansi[1]` (`#cc1122`).
fn osc_term() -> Term {
    let mut config = test_config(10, 3);
    config.palette = TermPalette {
        foreground: Rgb {
            r: 248,
            g: 248,
            b: 242,
        },
        background: Rgb {
            r: 40,
            g: 42,
            b: 54,
        },
        cursor: Rgb { r: 1, g: 2, b: 3 },
        ansi: [Rgb { r: 0, g: 0, b: 0 }; 16],
    };
    config.palette.ansi[1] = Rgb {
        r: 0xcc,
        g: 0x11,
        b: 0x22,
    };
    Term::new(config)
}

#[test]
fn osc_color_queries_mirror_terminator() {
    let mut term = osc_term();
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b]10;?\x07");
    feed(&mut term, &mut sink, b"\x1b]11;?\x1b\\");
    feed(&mut term, &mut sink, b"\x1b]12;?\x07");
    assert_eq!(
        sink.bytes,
        b"\x1b]10;rgb:f8f8/f8f8/f2f2\x07\x1b]11;rgb:2828/2a2a/3636\x1b\\\x1b]12;rgb:0101/0202/0303\x07"
    );
}

#[test]
fn osc4_queries_multiple_indices() {
    let mut term = osc_term();
    let mut sink = RecordingSink::new();
    // Index 200 is in the 6x6x6 cube: (255, 0, 215).
    feed(&mut term, &mut sink, b"\x1b]4;1;?;200;?\x07");
    assert_eq!(sink.calls.len(), 1);
    assert_eq!(
        sink.bytes,
        b"\x1b]4;1;rgb:cccc/1111/2222\x07\x1b]4;200;rgb:ffff/0000/d7d7\x07"
    );
    feed(&mut term, &mut sink, b"\x1b]4;300;?\x07");
    assert_eq!(sink.calls.len(), 1);
}

#[test]
fn osc_color_set_and_reset_are_noops() {
    let mut term = osc_term();
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b]10;#ffffff\x07");
    feed(&mut term, &mut sink, b"\x1b]104\x07");
    assert!(sink.calls.is_empty());
    feed(&mut term, &mut sink, b"\x1b]10;?\x07");
    assert_eq!(sink.bytes, b"\x1b]10;rgb:f8f8/f8f8/f2f2\x07");
}

#[test]
fn osc52_sets_clipboard() {
    let mut term = osc_term();
    let mut sink = RecordingSink::new();
    feed(&mut term, &mut sink, b"\x1b]52;c;aGk=\x07");
    feed(&mut term, &mut sink, b"\x1b]52;;aGk=\x07");
    feed(&mut term, &mut sink, b"\x1b]52;s;aGk=\x07");
    feed(&mut term, &mut sink, b"\x1b]52;cs;aGk=\x07");
    feed(&mut term, &mut sink, b"\x1b]52;c;aGk\x1b\\");
    assert_eq!(sink.clipboard, ["hi", "hi", "hi", "hi", "hi"]);
}

#[test]
fn osc52_rejects() {
    let mut term = osc_term();
    let mut sink = RecordingSink::new();
    let inputs: [&[u8]; 9] = [
        b"\x1b]52;p;aGk=\x07",
        b"\x1b]52;0;aGk=\x07",
        b"\x1b]52;c;?\x07",
        b"\x1b]52;c;\x07",
        b"\x1b]52;c;a$b=\x07",
        b"\x1b]52;c;/w==\x07",
        b"\x1b]52;c;aG=k\x07",
        b"\x1b]52;c;aGk==\x07",
        b"\x1b]52;c;a\x07",
    ];
    for input in inputs {
        feed(&mut term, &mut sink, input);
    }
    let over_limit = format!("\x1b]52;c;{}\x07", "A".repeat(5_592_412));
    feed(&mut term, &mut sink, over_limit.as_bytes());
    // Within the length limit, but decodes to 4 194 306 bytes.
    let decoded_over = format!("\x1b]52;c;{}\x07", "A".repeat(5_592_408));
    feed(&mut term, &mut sink, decoded_over.as_bytes());
    assert!(sink.clipboard.is_empty());
    assert!(sink.calls.is_empty());
}

#[test]
fn osc52_size_boundaries() {
    let mut term = osc_term();
    let mut sink = RecordingSink::new();
    // 5 592 404 `A` decode to 4 194 303 zero bytes.
    let below = format!("\x1b]52;c;{}\x07", "A".repeat(5_592_404));
    feed(&mut term, &mut sink, below.as_bytes());
    // Adding `AA==` decodes one more zero byte: exactly 4 MiB.
    let at = format!("\x1b]52;c;{}AA==\x07", "A".repeat(5_592_404));
    feed(&mut term, &mut sink, at.as_bytes());
    assert_eq!(sink.clipboard.len(), 2);
    assert_eq!(sink.clipboard[0].len(), 4_194_303);
    assert_eq!(sink.clipboard[1].len(), 4_194_304);
    assert!(sink.clipboard.iter().all(|text| text.bytes().all(|b| b == 0)));
}

#[test]
fn osc7_reports_host_and_decoded_path() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    feed(
        &mut term,
        &mut sink,
        b"\x1b]7;file://myhost/home/me/a%20b%3bc\x1b\\",
    );
    feed(&mut term, &mut sink, b"\x1b]7;file:///tmp\x07");
    assert_eq!(
        sink.cwd,
        [
            ("myhost".to_owned(), b"/home/me/a b;c".to_vec()),
            ("".to_owned(), b"/tmp".to_vec()),
        ]
    );
}

#[test]
fn osc7_rejects() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    // Path of 4097 bytes after decoding: one byte over the limit.
    let long_path = format!("\x1b]7;file://h/{}\x07", "a".repeat(4096));
    let inputs: [&[u8]; 6] = [
        b"\x1b]7;http://h/x\x07",
        b"\x1b]7;file://host\x07",
        b"\x1b]7;file://h/a%zz\x07",
        b"\x1b]7;file://h/a%00\x07",
        b"\x1b]7;file://\xff/tmp\x07",
        long_path.as_bytes(),
    ];
    for input in inputs {
        feed(&mut term, &mut sink, input);
    }
    assert!(sink.cwd.is_empty());
    // Exactly 4096 bytes after decoding is accepted.
    let at_limit = format!("\x1b]7;file://h/{}\x07", "a".repeat(4095));
    feed(&mut term, &mut sink, at_limit.as_bytes());
    assert_eq!(sink.cwd.len(), 1);
    assert_eq!(sink.cwd[0].1.len(), 4096);
}

#[test]
fn osc7_rejects_oversized_joined_payload() {
    // Two parts of 6000 and 6600 bytes: the joined payload (12610 bytes) is over
    // 3 * 4096 + 255, and the decoded path (12602 bytes) is over 4096.
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let text = format!(
        "\x1b]7;file://h/{};{}\x07",
        "a".repeat(6000),
        "a".repeat(6600)
    );
    feed(&mut term, &mut sink, text.as_bytes());
    assert!(sink.cwd.is_empty());
}
