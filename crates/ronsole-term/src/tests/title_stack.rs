//! `CSI 22 ; Ps t` / `CSI 23 ; Ps t` window-title stack. Expected titles follow
//! the task rules (push saves the programmed OSC 0/2 state, pop restores it
//! through the OSC path, empty stack and private forms do nothing), not the
//! implementation output.

use super::support::{RecordingSink, feed, test_config};
use crate::term::Term;

const PUSH: &[u8] = b"\x1b[22;0t";
const POP: &[u8] = b"\x1b[23;0t";

#[test]
fn push_then_pop_restores_title_and_empty_ps_means_window_title() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x1b]2;vim\x07");
    feed(&mut term, &mut sink, b"\x1b[22;t");
    feed(&mut term, &mut sink, b"\x1b]2;shell\x07");
    feed(&mut term, &mut sink, b"\x1b[23t");

    assert_eq!(sink.titles, ["vim", "shell", "vim"]);
    assert_eq!(sink.title_resets, 0);
}

#[test]
fn nested_push_pop_is_lifo() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x1b]2;a\x07");
    feed(&mut term, &mut sink, PUSH);
    feed(&mut term, &mut sink, b"\x1b]2;b\x07");
    feed(&mut term, &mut sink, PUSH);
    feed(&mut term, &mut sink, b"\x1b]2;c\x07");
    feed(&mut term, &mut sink, POP);
    feed(&mut term, &mut sink, POP);
    feed(&mut term, &mut sink, POP);

    // The third pop finds the stack empty and changes nothing.
    assert_eq!(sink.titles, ["a", "b", "c", "b", "a"]);
}

#[test]
fn pop_on_empty_stack_does_nothing() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, POP);

    assert!(sink.titles.is_empty());
    assert_eq!(sink.title_resets, 0);
    assert!(sink.calls.is_empty());
}

#[test]
fn unset_title_is_saved_and_restored_as_reset() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, PUSH);
    feed(&mut term, &mut sink, b"\x1b]2;x\x07");
    assert_eq!(term.window_title.as_deref(), Some("x"));
    feed(&mut term, &mut sink, POP);

    assert_eq!(sink.titles, ["x"]);
    assert_eq!(sink.title_resets, 1);
    assert_eq!(term.window_title, None);
}

#[test]
fn overflow_drops_oldest_entry() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    // 4097 pushes into a 4096-entry stack: "t0" is the entry that gets dropped.
    for i in 0..=4096 {
        feed(&mut term, &mut sink, format!("\x1b]2;t{i}\x07").as_bytes());
        feed(&mut term, &mut sink, PUSH);
    }
    assert_eq!(sink.titles.len(), 4097);

    for _ in 0..4096 {
        feed(&mut term, &mut sink, POP);
    }
    assert_eq!(sink.titles.len(), 4097 + 4096);
    assert_eq!(sink.titles.last().map(String::as_str), Some("t1"));

    // Empty now: a further pop must not produce the dropped "t0".
    feed(&mut term, &mut sink, POP);
    assert_eq!(sink.titles.len(), 4097 + 4096);
    assert_eq!(sink.titles.last().map(String::as_str), Some("t1"));
}

#[test]
fn private_marker_forms_are_ignored() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x1b]2;a\x07");
    feed(&mut term, &mut sink, b"\x1b[?22;0t");
    feed(&mut term, &mut sink, b"\x1b]2;b\x07");
    feed(&mut term, &mut sink, b"\x1b[?23;0t");
    // Nothing was pushed by the private form, so a plain pop finds it empty.
    feed(&mut term, &mut sink, POP);

    assert_eq!(sink.titles, ["a", "b"]);
    assert_eq!(sink.title_resets, 0);
}

#[test]
fn other_selectors_and_operations_are_unchanged() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x1b]2;a\x07");
    // Ps 8 is not a window-title selector, so this push saves nothing.
    feed(&mut term, &mut sink, b"\x1b[22;8t");
    feed(&mut term, &mut sink, b"\x1b]2;b\x07");
    feed(&mut term, &mut sink, POP);
    // Other CSI t operations keep producing no output here.
    feed(&mut term, &mut sink, b"\x1b[18t");
    feed(&mut term, &mut sink, b"\x1b[8;24;80t");

    assert_eq!(sink.titles, ["a", "b"]);
    assert_eq!(sink.title_resets, 0);
    assert!(sink.calls.is_empty());
}

#[test]
fn ris_clears_the_stack() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    feed(&mut term, &mut sink, b"\x1b]2;a\x07");
    feed(&mut term, &mut sink, PUSH);
    feed(&mut term, &mut sink, b"\x1bc");
    feed(&mut term, &mut sink, POP);

    assert_eq!(sink.titles, ["a"]);
    assert_eq!(sink.title_resets, 0);
}
