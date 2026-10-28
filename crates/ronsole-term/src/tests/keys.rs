use super::support::{RecordingSink, feed, test_config};
use crate::term::Term;
use crate::{KeyEvent, KeyKind, KeyMods, KeyOutcome, KpKey, ModKey, TermKey};

/// Sends `bytes` and returns exactly the reply bytes they produced.
pub(super) fn reply_to(term: &mut Term, sink: &mut RecordingSink, bytes: &[u8]) -> Vec<u8> {
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

/// Builds a key event; `physical` is fixed so press, repeat and release match.
pub(super) fn ev<'a>(key: TermKey, base: Option<char>, mods: KeyMods, kind: KeyKind, text: Option<&'a str>) -> KeyEvent<'a> {
    KeyEvent { physical: 1, key, base, mods, kind, text }
}

pub(super) fn mods(shift: bool, ctrl: bool, alt: bool) -> KeyMods {
    KeyMods { shift, ctrl, alt, super_key: false }
}

/// Encodes one event and returns the outcome and the bytes written.
pub(super) fn enc(term: &mut Term, event: &KeyEvent<'_>) -> (KeyOutcome, Vec<u8>) {
    let mut out = Vec::new();
    let outcome = term.encode_key(event, &mut out);
    (outcome, out)
}

/// Sets Kitty flags with `CSI = flags u` (replace mode).
pub(super) fn set_flags(term: &mut Term, sink: &mut RecordingSink, flags: u8) {
    let bytes = format!("\x1b[={flags}u");
    assert_eq!(reply_to(term, sink, bytes.as_bytes()), b"");
}

/// Expected bytes come from the Kitty reference, "Worked byte examples"
/// (`docs/superpowers/specs/2026-10-21-terminal-protocol-ref.md`), not from the implementation.
#[test]
fn kitty_reference_examples() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let press = KeyKind::Press;
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, none, press, None)), (KeyOutcome::Sent, b"\r".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, mods(true, false, false), press, None)).1, b"\x1b[13;2u");
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, mods(false, true, false), press, None)).1, b"\x1b[13;5u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), mods(false, false, true), press, None)).1, b"\x1b[97;3u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), mods(true, true, false), press, Some("A"))).1, b"\x1b[97;6u");
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, press, None)).1, b"\x1b[27u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), mods(false, true, false), press, None)).1, b"\x1b[99;5u");
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, mods(true, false, false), press, None)).1, b"\x1b[1;2A");

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, press, None)).1, b"\x1b[27u");
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, KeyKind::Release, None)).1, b"\x1b[27;1:3u");
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));

    set_flags(&mut term, &mut sink, 8);
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, mods(true, false, false), press, None)).1, b"\x1b[13;2u");
}

/// Word-editing keys in the two modes fish 4 uses: legacy (flags 0) and `CSI = 5 u`,
/// which masks to flag 1 here. Legacy bytes match the pre-kitty encoder
/// (`terminal_key_sequence` at 0944987); flag-1 bytes follow the protocol reference
/// (Ctrl+Backspace → `CSI 127;5u`). fish binds `ctrl-w` (legacy 0x17) and
/// `ctrl-backspace` (`CSI 127;5u`) to different commands, so the distinction is intended.
#[test]
fn word_editing_keys_legacy_and_fish_flags() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let press = KeyKind::Press;
    let ctrl = mods(false, true, false);
    let alt = mods(false, false, true);
    let ctrl_alt = mods(false, true, true);
    let cases: [(TermKey, KeyMods, &[u8], &[u8]); 7] = [
        (TermKey::Backspace, ctrl, b"\x17", b"\x1b[127;5u"),
        (TermKey::Backspace, alt, b"\x1b\x7f", b"\x1b[127;3u"),
        (TermKey::Backspace, ctrl_alt, b"\x1b\x17", b"\x1b[127;7u"),
        (TermKey::Backspace, mods(false, false, false), b"\x7f", b"\x7f"),
        (TermKey::Delete, ctrl, b"\x1b[3;5~", b"\x1b[3;5~"),
        (TermKey::Left, ctrl, b"\x1b[1;5D", b"\x1b[1;5D"),
        (TermKey::Right, ctrl, b"\x1b[1;5C", b"\x1b[1;5C"),
    ];
    for (key, key_mods, legacy, _) in cases {
        assert_eq!(enc(&mut term, &ev(key, None, key_mods, press, None)), (KeyOutcome::Sent, legacy.to_vec()), "flags 0: {key:?}");
    }
    // fish 4 pushes `CSI = 5 u`; flag 4 is unsupported and masked, leaving flag 1.
    set_flags(&mut term, &mut sink, 5);
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?1u");
    for (key, key_mods, _, kitty) in cases {
        assert_eq!(enc(&mut term, &ev(key, None, key_mods, press, None)), (KeyOutcome::Sent, kitty.to_vec()), "flags 1: {key:?}");
    }
}

/// Flag 2 alone: the press is the legacy byte, repeat and release are CSI u with the event field.
#[test]
fn escape_flag2_only() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 2);
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, KeyKind::Press, None)), (KeyOutcome::Sent, b"\x1b".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, KeyKind::Repeat, None)).1, b"\x1b[27;1:2u");
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, KeyKind::Release, None)).1, b"\x1b[27;1:3u");
}

/// Flag 2 alone: text, Ctrl letters, Enter, Backspace and Escape by their press/repeat/release rules.
#[test]
fn flag2_only_text_and_escape() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 2);
    // Ctrl+c: legacy byte on press, CSI u with the event field on repeat and release.
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Press, None)), (KeyOutcome::Sent, b"\x03".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Repeat, None)).1, b"\x1b[99;5:2u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Release, None)).1, b"\x1b[99;5:3u");
    // Plain text stays text on every kind; its release is ignored.
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Press, Some("a"))), (KeyOutcome::Text, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Repeat, Some("a"))), (KeyOutcome::Text, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Backspace, None, none, KeyKind::Repeat, None)), (KeyOutcome::Sent, b"\x7f".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, none, KeyKind::Press, None)), (KeyOutcome::Sent, b"\r".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, KeyKind::Press, None)).1, b"\x1b");
    assert_eq!(enc(&mut term, &ev(TermKey::Escape, None, none, KeyKind::Release, None)).1, b"\x1b[27;1:3u");
}

/// Functional keys lose SS3 as soon as flag 1 is set, whatever DECCKM says.
#[test]
fn functional_keys_flag1_no_ss3() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let press = KeyKind::Press;

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1h"), b"");
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, none, press, None)).1, b"\x1b[A");
    assert_eq!(enc(&mut term, &ev(TermKey::Home, None, none, press, None)).1, b"\x1b[H");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1l"), b"");
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, none, press, None)).1, b"\x1b[A");
    assert_eq!(enc(&mut term, &ev(TermKey::Home, None, none, press, None)).1, b"\x1b[H");
    assert_eq!(enc(&mut term, &ev(TermKey::F(1), None, none, press, None)).1, b"\x1b[P");
    assert_eq!(enc(&mut term, &ev(TermKey::F(2), None, none, press, None)).1, b"\x1b[Q");
    assert_eq!(enc(&mut term, &ev(TermKey::F(3), None, none, press, None)).1, b"\x1b[13~");
    assert_eq!(enc(&mut term, &ev(TermKey::F(4), None, none, press, None)).1, b"\x1b[S");
    assert_eq!(enc(&mut term, &ev(TermKey::F(5), None, none, press, None)).1, b"\x1b[15~");
    assert_eq!(enc(&mut term, &ev(TermKey::Insert, None, none, press, None)).1, b"\x1b[2~");

    // Flag 0 with DECCKM: cursor keys and F1 keep the legacy SS3 bytes (F1 is `ESC OP` in src/input.rs terminal_key_sequence).
    set_flags(&mut term, &mut sink, 0);
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1h"), b"");
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, none, press, None)).1, b"\x1bOA");
    assert_eq!(enc(&mut term, &ev(TermKey::F(1), None, none, press, None)).1, b"\x1bOP");

    // Flag 2 alone does not switch cursor keys to SS3 even with DECCKM off: CSI form.
    set_flags(&mut term, &mut sink, 2);
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1l"), b"");
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, none, press, None)).1, b"\x1b[A");
}

/// A release with no matching press in the sent set is ignored, with or without flag 8.
#[test]
fn release_without_sent_press_ignored() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));

    set_flags(&mut term, &mut sink, 2);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));

    set_flags(&mut term, &mut sink, 8);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));

    set_flags(&mut term, &mut sink, 10);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// The release field is built from the press code even after Ctrl is let go.
#[test]
fn release_uses_press_code_after_ctrl_up() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 3);
    // Cyrillic Ctrl+С: the code comes from the base key `c`.
    assert_eq!(enc(&mut term, &ev(TermKey::Char('С'), Some('c'), ctrl, KeyKind::Press, None)).1, b"\x1b[99;5u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('С'), Some('c'), none, KeyKind::Release, None)).1, b"\x1b[99;1:3u");
}

/// A repeated letter is text under flags 0 and 1.
#[test]
fn repeat_letter_is_text_flags0_and_1() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 0);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Repeat, Some("a"))), (KeyOutcome::Text, Vec::new()));

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Repeat, Some("a"))), (KeyOutcome::Text, Vec::new()));
}

/// A repeat of a key that was sent as CSI u is sent again with event type 2.
#[test]
fn repeat_sent_key_flags3() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('С'), Some('c'), ctrl, KeyKind::Press, None)).1, b"\x1b[99;5u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('С'), Some('c'), ctrl, KeyKind::Repeat, None)).1, b"\x1b[99;5:2u");
}

/// Ctrl released during a repeat: the repeat is text, and the pair is gone so the release is ignored.
#[test]
fn ctrl_released_during_repeat_goes_text() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Press, None)).1, b"\x1b[99;5u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), none, KeyKind::Repeat, Some("c"))), (KeyOutcome::Text, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// The sent set holds 16 pairs: the 17th press evicts the oldest one, whose release is then ignored.
#[test]
fn seventeenth_key_evicts_oldest() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 3);
    for (index, letter) in (b'a'..=b'q').enumerate() {
        let ch = char::from(letter);
        let press = KeyEvent { physical: index as u64 + 1, ..ev(TermKey::Char(ch), Some(ch), mods(false, true, false), KeyKind::Press, None) };
        // Expected code is the ASCII code of the letter, i.e. the Unicode codepoint of the key.
        assert_eq!(enc(&mut term, &press).1, format!("\x1b[{};5u", letter).into_bytes());
    }
    // Physical key 1 ('a') was evicted by the 17th press.
    assert_eq!(enc(&mut term, &KeyEvent { physical: 1, ..ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None) }), (KeyOutcome::Ignore, Vec::new()));
    // Physical key 17 ('q') is still in the set.
    assert_eq!(enc(&mut term, &KeyEvent { physical: 17, ..ev(TermKey::Char('q'), Some('q'), none, KeyKind::Release, None) }).1, b"\x1b[113;1:3u");
}

/// Losing focus clears the sent set, so a later release of a pressed key is ignored.
#[test]
fn focus_loss_clears_sent_set() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Press, None)).1, b"\x1b[99;5u");
    // Mode 1004 is off, so no focus report is produced.
    assert_eq!(term.focus_changed(false), None);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// Switching to the alternate screen and back clears the sent set; flags are per screen.
#[test]
fn screen_switch_clears_sent_set() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1049h"), b"");
    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), ctrl, KeyKind::Press, None)).1, b"\x1b[99;5u");
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1049l"), b"");
    // Main screen flags 3 are still active after leaving the alternate screen.
    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?u"), b"\x1b[?3u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('c'), Some('c'), none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// Shift+Enter is CSI u under flag 3, and its release is ignored by the flag 2 Enter rule.
#[test]
fn shift_enter_release_ignored_flags3() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, mods(true, false, false), KeyKind::Press, None)).1, b"\x1b[13;2u");
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, none, KeyKind::Release, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// Modifier field is 1 + bits and can reach two digits with Super (8).
#[test]
fn super_mods_two_digits() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let super_ctrl = KeyMods { shift: false, ctrl: true, alt: false, super_key: true };

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), super_ctrl, KeyKind::Press, None)).1, b"\x1b[97;13u");
}

/// Modifier keys are reported only with flag 8.
#[test]
fn modifier_key_ignored_without_flag8() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::LeftShift), None, none, KeyKind::Press, None)), (KeyOutcome::Ignore, Vec::new()));
    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::LeftShift), None, none, KeyKind::Press, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// Left Shift under flags 10: press includes its own bit, release does not.
#[test]
fn left_shift_press_and_release_flag8() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 10);
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::LeftShift), None, mods(true, false, false), KeyKind::Press, None)).1, b"\x1b[57441;2u");
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::LeftShift), None, none, KeyKind::Release, None)).1, b"\x1b[57441;1:3u");
}

/// Flag 16 adds the text as the third field; flags 26 add event types on top.
#[test]
fn associated_text_flag24() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let shift = mods(true, false, false);

    set_flags(&mut term, &mut sink, 24);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Press, Some("a"))).1, b"\x1b[97;;97u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), shift, KeyKind::Press, Some("A"))).1, b"\x1b[97;2;65u");

    set_flags(&mut term, &mut sink, 26);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Press, Some("a"))).1, b"\x1b[97;;97u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)).1, b"\x1b[97;1:3u");
}

/// Keypad Enter is a non-text key: PUA code under flag 1.
#[test]
fn kp_enter_flag1_is_pua() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Enter), None, none, KeyKind::Press, None)).1, b"\x1b[57414u");
}

/// Keypad Enter without flags is the same CR as the main Enter key.
#[test]
fn kp_enter_flags0_is_cr() {
    let mut term = Term::new(test_config(10, 3));
    let none = mods(false, false, false);

    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Enter), None, none, KeyKind::Press, None)), (KeyOutcome::Sent, b"\r".to_vec()));
}

/// A non-ASCII character with no base and no Ctrl/Alt is text under flag 1.
#[test]
fn non_ascii_char_without_base_is_text_flag1() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('ф'), None, none, KeyKind::Press, Some("ф"))), (KeyOutcome::Text, Vec::new()));
}

/// Focus reports follow DECSET 1004: `ESC[I`/`ESC[O` when on, `None` when off.
#[test]
fn focus_changed_follows_mode_1004() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();

    assert_eq!(term.focus_changed(true), None);
    assert_eq!(term.focus_changed(false), None);

    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1004h"), b"");
    assert_eq!(term.focus_changed(true), Some(&b"\x1b[I"[..]));
    assert_eq!(term.focus_changed(false), Some(&b"\x1b[O"[..]));

    assert_eq!(reply_to(&mut term, &mut sink, b"\x1b[?1004l"), b"");
    assert_eq!(term.focus_changed(true), None);
}

/// Flag 16 carries the text on repeat as well as on press; the mods field is
/// written as `1` once an event type follows (brief, flag 16; kitty `write_csi`).
#[test]
fn associated_text_on_repeat_flags26() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 26);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Press, Some("a"))).1, b"\x1b[97;;97u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Repeat, Some("a"))).1, b"\x1b[97;1:2;97u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Release, None)).1, b"\x1b[97;1:3u");
}

/// Decision 18 (spec 5.3): the base code point is used only with Ctrl, Alt or
/// Super; a non-ASCII key without them keeps its own code point.
#[test]
fn non_ascii_key_code_uses_base_only_with_mods_flag8() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 8);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('ф'), Some('a'), none, KeyKind::Press, Some("ф"))).1, b"\x1b[1092u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('с'), Some('c'), ctrl, KeyKind::Press, None)).1, b"\x1b[99;5u");
}

/// Keypad digits without flag 1 are their non-keypad equivalents: Alt sends
/// ESC plus the digit, Ctrl has no C0 byte for it. KP_BEGIN has no legacy
/// form in the reference (legacy column `-`), so flag 0 sends nothing.
#[test]
fn keypad_keys_flags0_use_non_keypad_equivalent() {
    let mut term = Term::new(test_config(10, 3));
    let none = mods(false, false, false);
    let alt = mods(false, false, true);
    let ctrl = mods(false, true, false);

    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Kp5), None, alt, KeyKind::Press, Some("5"))), (KeyOutcome::Sent, b"\x1b5".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Kp5), None, ctrl, KeyKind::Press, None)), (KeyOutcome::Ignore, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Kp5), None, none, KeyKind::Press, Some("5"))), (KeyOutcome::Text, Vec::new()));
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Begin), None, none, KeyKind::Press, None)), (KeyOutcome::Ignore, Vec::new()));
}

/// KP_BEGIN under flag 1 takes the first form of its reference row, `1 E`,
/// written like F1's `1 P` (code 1 omitted without modifiers).
#[test]
fn kp_begin_flag1_is_csi_e() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 1);
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Begin), None, none, KeyKind::Press, None)).1, b"\x1b[E");
}

/// Associated text may not contain control codes, and empty text adds no field
/// (reference, Flag 16: "associated text must not contain control codes").
#[test]
fn associated_text_skips_control_and_empty_flag24() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 24);
    assert_eq!(enc(&mut term, &ev(TermKey::Enter, None, none, KeyKind::Press, Some("\r"))).1, b"\x1b[13u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), ctrl, KeyKind::Press, Some("\x01"))).1, b"\x1b[97;5u");
    assert_eq!(enc(&mut term, &ev(TermKey::Char('a'), Some('a'), none, KeyKind::Press, Some(""))).1, b"\x1b[97u");

    set_flags(&mut term, &mut sink, 0);
    let alt = mods(false, false, true);
    assert_eq!(enc(&mut term, &ev(TermKey::Char('x'), Some('x'), alt, KeyKind::Press, Some(""))), (KeyOutcome::Sent, b"\x1bx".to_vec()));
}

/// Keypad Enter is a non-text key: its press and its release use the same
/// PUA code under flag 3, so the release is not dropped as a legacy Enter.
#[test]
fn kp_enter_press_and_release_flags3() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 3);
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Enter), None, none, KeyKind::Press, None)), (KeyOutcome::Sent, b"\x1b[57414u".to_vec()));
    assert_eq!(enc(&mut term, &ev(TermKey::Kp(KpKey::Enter), None, none, KeyKind::Release, None)), (KeyOutcome::Sent, b"\x1b[57414;1:3u".to_vec()));
}

/// Reference, "Modifiers" (ref line 121): a modifier key's own bit is the modifier
/// state after the event, so the press sets `ctrl` and the release clears it.
/// Flags 8+2, because modifier events are reported only with flag 8.
#[test]
fn modifier_own_bit_follows_host_state_flags10() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 10);
    let press = KeyEvent { physical: 1, ..ev(TermKey::Modifier(ModKey::LeftCtrl), None, ctrl, KeyKind::Press, None) };
    assert_eq!(enc(&mut term, &press), (KeyOutcome::Sent, b"\x1b[57442;5u".to_vec()));
    let release = KeyEvent { physical: 1, ..ev(TermKey::Modifier(ModKey::LeftCtrl), None, none, KeyKind::Release, None) };
    assert_eq!(enc(&mut term, &release), (KeyOutcome::Sent, b"\x1b[57442;1:3u".to_vec()));
}

/// Reference, "Modifiers" (ref line 121): with both Ctrl keys held, releasing one
/// keeps the `ctrl` bit set, because the host still reports the other as held.
#[test]
fn modifier_release_keeps_ctrl_bit_while_other_ctrl_held_flags10() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 10);
    let left_press = KeyEvent { physical: 1, ..ev(TermKey::Modifier(ModKey::LeftCtrl), None, ctrl, KeyKind::Press, None) };
    let right_press = KeyEvent { physical: 2, ..ev(TermKey::Modifier(ModKey::RightCtrl), None, ctrl, KeyKind::Press, None) };
    let left_release = KeyEvent { physical: 1, ..ev(TermKey::Modifier(ModKey::LeftCtrl), None, ctrl, KeyKind::Release, None) };
    let right_release = KeyEvent { physical: 2, ..ev(TermKey::Modifier(ModKey::RightCtrl), None, none, KeyKind::Release, None) };
    assert_eq!(enc(&mut term, &left_press).1, b"\x1b[57442;5u");
    assert_eq!(enc(&mut term, &right_press).1, b"\x1b[57448;5u");
    assert_eq!(enc(&mut term, &left_release), (KeyOutcome::Sent, b"\x1b[57442;5:3u".to_vec()));
    assert_eq!(enc(&mut term, &right_release), (KeyOutcome::Sent, b"\x1b[57448;1:3u".to_vec()));
}

/// A modifier release is reported only for a press still in the sent set: a
/// release with no press is ignored, and so is one whose press a focus change cleared.
#[test]
fn modifier_release_without_sent_press_is_ignored_flags10() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let ctrl = mods(false, true, false);

    set_flags(&mut term, &mut sink, 10);
    let release = KeyEvent { physical: 1, ..ev(TermKey::Modifier(ModKey::LeftCtrl), None, none, KeyKind::Release, None) };
    assert_eq!(enc(&mut term, &release), (KeyOutcome::Ignore, Vec::new()));

    let press = KeyEvent { physical: 1, ..ev(TermKey::Modifier(ModKey::LeftCtrl), None, ctrl, KeyKind::Press, None) };
    assert_eq!(enc(&mut term, &press).1, b"\x1b[57442;5u");
    // Every focus change clears the sent set, so the release of the held Ctrl is ignored.
    assert_eq!(term.focus_changed(true), None);
    assert_eq!(enc(&mut term, &release), (KeyOutcome::Ignore, Vec::new()));
}

/// Flags 0 keep the bytes of commit 93ad29c (`terminal_key_sequence` and
/// `terminal_function_key` in `src/input.rs`): modified F1-F4 use
/// `CSI 1 ; mods <P|Q|R|S>`, and Super is not part of the modifier field.
#[test]
fn legacy_flags0_modified_function_keys_match_93ad29c() {
    let mut term = Term::new(test_config(10, 3));
    let shift = mods(true, false, false);
    let alt = mods(false, false, true);
    let ctrl = mods(false, true, false);
    let super_only = KeyMods { shift: false, ctrl: false, alt: false, super_key: true };
    let none = mods(false, false, false);
    let cases: &[(TermKey, KeyMods, &[u8])] = &[
        (TermKey::F(1), none, b"\x1bOP"),
        (TermKey::F(1), ctrl, b"\x1b[1;5P"),
        (TermKey::F(2), shift, b"\x1b[1;2Q"),
        (TermKey::F(3), alt, b"\x1b[1;3R"),
        (TermKey::F(3), ctrl, b"\x1b[1;5R"),
        (TermKey::F(4), ctrl, b"\x1b[1;5S"),
        (TermKey::F(3), super_only, b"\x1bOR"),
        (TermKey::Up, super_only, b"\x1b[A"),
        (TermKey::Home, ctrl, b"\x1b[1;5H"),
        (TermKey::End, shift, b"\x1b[1;2F"),
        (TermKey::End, alt, b"\x1b[1;3F"),
        (TermKey::Home, super_only, b"\x1b[H"),
        (TermKey::Insert, shift, b"\x1b[2;2~"),
        (TermKey::Delete, ctrl, b"\x1b[3;5~"),
        (TermKey::PageUp, alt, b"\x1b[5;3~"),
        (TermKey::PageDown, super_only, b"\x1b[6~"),
        (TermKey::F(5), ctrl, b"\x1b[15;5~"),
        (TermKey::F(5), shift, b"\x1b[15;2~"),
        (TermKey::F(6), alt, b"\x1b[17;3~"),
        (TermKey::F(7), ctrl, b"\x1b[18;5~"),
        (TermKey::F(8), super_only, b"\x1b[19~"),
        (TermKey::F(9), none, b"\x1b[20~"),
        (TermKey::F(10), shift, b"\x1b[21;2~"),
        (TermKey::F(11), ctrl, b"\x1b[23;5~"),
        (TermKey::F(12), alt, b"\x1b[24;3~"),
        // Super alone under flags 0 is not in the modifier field: unmodified legacy bytes.
        (TermKey::F(2), super_only, b"\x1bOQ"),
        (TermKey::F(4), super_only, b"\x1bOS"),
        (TermKey::F(5), super_only, b"\x1b[15~"),
        (TermKey::F(6), super_only, b"\x1b[17~"),
        (TermKey::F(7), super_only, b"\x1b[18~"),
        (TermKey::F(9), super_only, b"\x1b[20~"),
        (TermKey::F(10), super_only, b"\x1b[21~"),
        (TermKey::F(11), super_only, b"\x1b[23~"),
        (TermKey::F(12), super_only, b"\x1b[24~"),
        (TermKey::Left, super_only, b"\x1b[D"),
        (TermKey::Right, super_only, b"\x1b[C"),
        (TermKey::Down, super_only, b"\x1b[B"),
        (TermKey::End, super_only, b"\x1b[F"),
        (TermKey::Insert, super_only, b"\x1b[2~"),
        (TermKey::Delete, super_only, b"\x1b[3~"),
        (TermKey::PageUp, super_only, b"\x1b[5~"),
    ];
    for (key, modifiers, want) in cases {
        assert_eq!(enc(&mut term, &ev(*key, None, *modifiers, KeyKind::Press, None)).1, *want, "key {key:?} mods {modifiers:?}");
    }
}

/// Super enters the modifier field (Super = 8) only when the flags are not 0.
/// Flag 16 alone is nonzero, so Super+Up becomes `CSI 1;9A` and Super+F3 `CSI 1;9R`.
/// Source: Kitty modifier bits in the protocol reference (Super = 8, field = bits + 1).
#[test]
fn super_reported_when_flags_nonzero_flag16() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let super_only = KeyMods { shift: false, ctrl: false, alt: false, super_key: true };

    set_flags(&mut term, &mut sink, 16);
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, super_only, KeyKind::Press, None)).1, b"\x1b[1;9A");
    assert_eq!(enc(&mut term, &ev(TermKey::F(3), None, super_only, KeyKind::Press, None)).1, b"\x1b[1;9R");
    set_flags(&mut term, &mut sink, 0);
    assert_eq!(enc(&mut term, &ev(TermKey::Up, None, super_only, KeyKind::Press, None)).1, b"\x1b[A");
}

/// Flag 1 already reports Super: F1, F2, F4 use `CSI 1 ; 9 <P|Q|S>`, F3 uses `CSI 13 ; 9 ~`.
/// Source: protocol reference, functional-key table (F1-F4 row, F3 `13 ~`) and modifier bits (Super = 8, field = bits + 1).
#[test]
fn super_f1_to_f4_flags1_report_super_bit() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let super_only = KeyMods { shift: false, ctrl: false, alt: false, super_key: true };

    set_flags(&mut term, &mut sink, 1);
    let cases: &[(u8, &[u8])] = &[(1, b"\x1b[1;9P"), (2, b"\x1b[1;9Q"), (3, b"\x1b[13;9~"), (4, b"\x1b[1;9S")];
    for (n, want) in cases {
        assert_eq!(enc(&mut term, &ev(TermKey::F(*n), None, super_only, KeyKind::Press, None)).1, *want, "F{n}");
    }
}

/// Flag 1 keeps F5-F12 in the `CSI <n> ~` form, unmodified and with Shift.
/// Source: protocol reference, functional-key table (F5-F12 row) and legacy column (`CSI 17 ~` ... `CSI 24 ~`).
#[test]
fn f6_to_f12_flags1_csi_tilde_forms() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let shift = mods(true, false, false);

    set_flags(&mut term, &mut sink, 1);
    let cases: &[(u8, &[u8])] = &[
        (6, b"\x1b[17~"),
        (7, b"\x1b[18~"),
        (8, b"\x1b[19~"),
        (9, b"\x1b[20~"),
        (10, b"\x1b[21~"),
        (11, b"\x1b[23~"),
        (12, b"\x1b[24~"),
    ];
    for (n, want) in cases {
        assert_eq!(enc(&mut term, &ev(TermKey::F(*n), None, none, KeyKind::Press, None)).1, *want, "F{n}");
    }
    assert_eq!(enc(&mut term, &ev(TermKey::F(6), None, shift, KeyKind::Press, None)).1, b"\x1b[17;2~");
    assert_eq!(enc(&mut term, &ev(TermKey::F(12), None, shift, KeyKind::Press, None)).1, b"\x1b[24;2~");
}

/// Right modifiers have their own PUA codes (RIGHT_SHIFT 57447, RIGHT_ALT 57449,
/// RIGHT_SUPER 57450), not the left codes. Press includes its own bit, release does not.
/// Source: protocol reference, functional-key table (RIGHT_* rows) and modifier bits (Alt=2, Super=8).
#[test]
fn right_modifiers_report_own_codes_flags10() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);
    let shift = mods(true, false, false);
    let alt = mods(false, false, true);
    let super_only = KeyMods { shift: false, ctrl: false, alt: false, super_key: true };

    set_flags(&mut term, &mut sink, 10);
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::RightShift), None, shift, KeyKind::Press, None)).1, b"\x1b[57447;2u");
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::RightShift), None, none, KeyKind::Release, None)).1, b"\x1b[57447;1:3u");
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::RightAlt), None, alt, KeyKind::Press, None)).1, b"\x1b[57449;3u");
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::RightAlt), None, none, KeyKind::Release, None)).1, b"\x1b[57449;1:3u");
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::RightSuper), None, super_only, KeyKind::Press, None)).1, b"\x1b[57450;9u");
}

/// CapsLock and NumLock are reported under their own PUA codes (57358, 57360)
/// with flag 8. `KeyMods` carries no lock bits, so the lock state cannot change
/// these bytes in this model; only the key's own code is checked.
/// Source: protocol reference, functional-key table (CAPS_LOCK 57358 u, NUM_LOCK 57360 u).
#[test]
fn caps_and_num_lock_keys_report_own_codes_flags10() {
    let mut term = Term::new(test_config(10, 3));
    let mut sink = RecordingSink::new();
    let none = mods(false, false, false);

    set_flags(&mut term, &mut sink, 10);
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::CapsLock), None, none, KeyKind::Press, None)).1, b"\x1b[57358u");
    assert_eq!(enc(&mut term, &ev(TermKey::Modifier(ModKey::NumLock), None, none, KeyKind::Press, None)).1, b"\x1b[57360u");
}
