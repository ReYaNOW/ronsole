use crate::osc::{TERMINAL_TITLE_MAX_BYTES, terminal_programmed_title};
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
