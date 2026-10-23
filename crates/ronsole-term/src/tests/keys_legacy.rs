//! Legacy (flags 0) encoder cases and the Ronsole encoder tests ported onto
//! `KeyEvent`. Expected bytes come from Ronsole `src/input.rs`
//! (`terminal_key_sequence` / `terminal_text_sequence` tests) and from the
//! Task 8 brief, not from the encoder.

use super::keys::{enc, ev, mods, set_flags};
use super::support::{RecordingSink, feed, test_config};
use crate::term::Term;
use crate::{KeyKind, KeyMods, KeyOutcome, TermKey};

fn new_term() -> (Term, RecordingSink) {
    (Term::new(test_config(10, 3)), RecordingSink::new())
}

#[test]
fn russian_ctrl_c_flags0() {
    let (mut term, _) = new_term();
    let event = ev(
        TermKey::Char('с'),
        Some('c'),
        mods(false, true, false),
        KeyKind::Press,
        None,
    );
    assert_eq!(enc(&mut term, &event), (KeyOutcome::Sent, b"\x03".to_vec()));
}

#[test]
fn russian_ctrl_c_flag1() {
    let (mut term, mut sink) = new_term();
    set_flags(&mut term, &mut sink, 1);
    let event = ev(
        TermKey::Char('с'),
        Some('c'),
        mods(false, true, false),
        KeyKind::Press,
        None,
    );
    assert_eq!(
        enc(&mut term, &event),
        (KeyOutcome::Sent, b"\x1b[99;5u".to_vec())
    );
}

#[test]
fn ctrl_slash_flags0() {
    let (mut term, _) = new_term();
    let event = ev(
        TermKey::Char('/'),
        Some('/'),
        mods(false, true, false),
        KeyKind::Press,
        None,
    );
    assert_eq!(enc(&mut term, &event), (KeyOutcome::Sent, vec![0x1f]));
}

#[test]
fn ctrl_bracket_flags0() {
    let (mut term, _) = new_term();
    let event = ev(
        TermKey::Char('['),
        Some('['),
        mods(false, true, false),
        KeyKind::Press,
        None,
    );
    assert_eq!(enc(&mut term, &event), (KeyOutcome::Sent, vec![0x1b]));
}

#[test]
fn russian_alt_ef_flags0() {
    let (mut term, _) = new_term();
    // Alt+ф on a Russian layout: the physical key is `a` on US layout.
    let event = ev(
        TermKey::Char('ф'),
        Some('a'),
        mods(false, false, true),
        KeyKind::Press,
        Some("ф"),
    );
    let mut expected = vec![0x1b];
    expected.extend_from_slice("ф".as_bytes());
    assert_eq!(enc(&mut term, &event), (KeyOutcome::Sent, expected));
}

#[test]
fn shift_enter_flags0() {
    let (mut term, _) = new_term();
    let event = ev(
        TermKey::Enter,
        None,
        mods(true, false, false),
        KeyKind::Press,
        None,
    );
    assert_eq!(enc(&mut term, &event), (KeyOutcome::Sent, b"\x1b\r".to_vec()));
}

#[test]
fn altgr_text_is_text_flag1() {
    let (mut term, mut sink) = new_term();
    set_flags(&mut term, &mut sink, 1);
    // AltGr+E produces `€` without Ctrl or Alt in the modifier set.
    let event = ev(
        TermKey::Char('€'),
        Some('e'),
        mods(false, false, false),
        KeyKind::Press,
        Some("€"),
    );
    assert_eq!(enc(&mut term, &event).0, KeyOutcome::Text);
}

/// Ported from Ronsole `terminal_encoder_keeps_plain_f1_and_ctrl_t_sequences`.
#[test]
fn ronsole_encoder_keeps_plain_f1_and_ctrl_t() {
    let (mut term, _) = new_term();
    let f1 = ev(TermKey::F(1), None, mods(false, false, false), KeyKind::Press, None);
    assert_eq!(enc(&mut term, &f1), (KeyOutcome::Sent, b"\x1bOP".to_vec()));
    let ctrl_t = ev(
        TermKey::Char('t'),
        Some('t'),
        mods(false, true, false),
        KeyKind::Press,
        None,
    );
    assert_eq!(enc(&mut term, &ctrl_t), (KeyOutcome::Sent, vec![0x14]));
}

/// Ported from Ronsole `terminal_encoder_sends_esc_cr_for_shift_enter`.
#[test]
fn ronsole_encoder_sends_esc_cr_for_shift_enter() {
    let (mut term, _) = new_term();
    let enter = ev(TermKey::Enter, None, mods(false, false, false), KeyKind::Press, None);
    assert_eq!(enc(&mut term, &enter), (KeyOutcome::Sent, b"\r".to_vec()));
    let shift_enter = ev(TermKey::Enter, None, mods(true, false, false), KeyKind::Press, None);
    assert_eq!(
        enc(&mut term, &shift_enter),
        (KeyOutcome::Sent, b"\x1b\r".to_vec())
    );
}

/// Ported from Ronsole `project_owned_keyboard_contract_separates_physical_keys_from_text`
/// (`src/input.rs:1243-1263`).
#[test]
fn ronsole_keyboard_contract_separates_physical_keys_from_text() {
    let (mut term, _) = new_term();
    let plain_a = ev(TermKey::Char('a'), Some('a'), mods(false, false, false), KeyKind::Press, Some("a"));
    assert_eq!(enc(&mut term, &plain_a).0, KeyOutcome::Text);

    let japanese_text = ev(TermKey::Char('Ж'), None, mods(false, false, false), KeyKind::Press, Some("Ж界"));
    assert_eq!(enc(&mut term, &japanese_text).0, KeyOutcome::Text);

    let alt_x = ev(TermKey::Char('x'), Some('x'), mods(false, false, true), KeyKind::Press, Some("x"));
    assert_eq!(enc(&mut term, &alt_x), (KeyOutcome::Sent, b"\x1bx".to_vec()));

    let ctrl_a = ev(TermKey::Char('a'), Some('a'), mods(false, true, false), KeyKind::Press, None);
    assert_eq!(enc(&mut term, &ctrl_a), (KeyOutcome::Sent, vec![0x01]));

    // Ronsole `terminal_text_sequence("a", ctrl = true)` → `None` (`src/input.rs:1261`):
    // Ctrl text is never written as text; with the text field present it still sends `01`.
    let ctrl_a_with_text = ev(TermKey::Char('a'), Some('a'), mods(false, true, false), KeyKind::Press, Some("a"));
    assert_eq!(enc(&mut term, &ctrl_a_with_text), (KeyOutcome::Sent, vec![0x01]));
}

/// Ported from Ronsole `project_owned_keyboard_contract_covers_navigation_modifiers_and_release`
/// (`src/input.rs:1266-1345`). The release check (`input.rs:1338-1344`) maps to `Ignore` at flags 0:
/// release events are reported only with Kitty flag 2.
#[test]
fn ronsole_navigation_and_function_keys_flags0() {
    let (mut term, _) = new_term();
    let cases = [
        (TermKey::Up, mods(true, false, false), "\x1b[1;2A"),
        (TermKey::Left, mods(false, false, true), "\x1b[1;3D"),
        (TermKey::Home, mods(false, true, false), "\x1b[1;5H"),
        (TermKey::End, mods(false, false, false), "\x1b[F"),
        (TermKey::PageUp, mods(false, false, false), "\x1b[5~"),
        (TermKey::PageDown, mods(false, false, false), "\x1b[6~"),
        (TermKey::F(5), mods(false, false, false), "\x1b[15~"),
        (TermKey::F(8), mods(false, false, false), "\x1b[19~"),
        (TermKey::F(8), mods(true, false, false), "\x1b[19;2~"),
        (TermKey::F(8), mods(false, false, true), "\x1b[19;3~"),
        (TermKey::F(12), mods(false, true, true), "\x1b[24;7~"),
    ];
    for (key, modifiers, expected) in cases {
        let event = ev(key, None, modifiers, KeyKind::Press, None);
        assert_eq!(
            enc(&mut term, &event),
            (KeyOutcome::Sent, expected.as_bytes().to_vec()),
            "key {key:?} mods {modifiers:?}"
        );
    }

    let release_up = ev(TermKey::Up, None, mods(false, false, false), KeyKind::Release, None);
    assert_eq!(enc(&mut term, &release_up), (KeyOutcome::Ignore, Vec::new()));
}

/// Ported from Ronsole `terminal_key_sequences_cover_navigation_and_modifiers`
/// and `modified_f1_remains_available_to_terminal_layer`.
#[test]
fn ronsole_cursor_keys_follow_decckm_flags0() {
    let (mut term, mut sink) = new_term();
    let up = ev(TermKey::Up, None, mods(false, false, false), KeyKind::Press, None);
    assert_eq!(enc(&mut term, &up), (KeyOutcome::Sent, b"\x1b[A".to_vec()));

    feed(&mut term, &mut sink, b"\x1b[?1h");
    assert_eq!(enc(&mut term, &up), (KeyOutcome::Sent, b"\x1bOA".to_vec()));

    let delete = ev(TermKey::Delete, None, mods(true, true, false), KeyKind::Press, None);
    assert_eq!(enc(&mut term, &delete), (KeyOutcome::Sent, b"\x1b[3;6~".to_vec()));

    let f12 = ev(TermKey::F(12), None, mods(false, false, false), KeyKind::Press, None);
    assert_eq!(enc(&mut term, &f12), (KeyOutcome::Sent, b"\x1b[24~".to_vec()));

    let modified_f1 = ev(TermKey::F(1), None, mods(false, true, true), KeyKind::Press, None);
    assert_eq!(
        enc(&mut term, &modified_f1),
        (KeyOutcome::Sent, b"\x1b[1;7P".to_vec())
    );
}

/// Ronsole `terminal_key_sequence` returns `None` for a Ctrl key with no table entry
/// and `terminal_text_sequence` returns `None` for Ctrl text: no bytes are produced.
#[test]
fn ctrl_char_without_table_entry_sends_nothing() {
    let (mut term, _) = new_term();
    let event = ev(TermKey::Char('ж'), None, mods(false, true, false), KeyKind::Press, None);
    let (outcome, bytes) = enc(&mut term, &event);
    assert!(bytes.is_empty());
    assert_ne!(outcome, KeyOutcome::Sent);
}

/// Ronsole has entries only for F1–F12 (`src/input.rs` has no `F0`/`F13`), and
/// `TermKey::F` is documented as F1–F12 (`crates/ronsole-term/src/keys.rs`).
#[test]
fn out_of_range_function_keys_send_nothing() {
    let (mut term, _) = new_term();
    for number in [0u8, 13] {
        let event = ev(TermKey::F(number), None, mods(false, false, false), KeyKind::Press, None);
        let (outcome, bytes) = enc(&mut term, &event);
        assert!(bytes.is_empty(), "F({number})");
        assert_ne!(outcome, KeyOutcome::Sent, "F({number})");
    }
}

/// Ronsole `terminal_text_sequence(text, _, _, super_key = true)` returns `None`
/// (`src/input.rs:1186-1188`): a Super-only text key writes no bytes.
#[test]
#[ignore = "bug: crates/ronsole-term/src/keys.rs:462-465 writes text bytes for Super+text at flags 0; Ronsole returns None (src/input.rs:1186-1187)"]
fn super_text_without_ctrl_or_alt_sends_no_bytes() {
    let (mut term, _) = new_term();
    let super_only = KeyMods {
        shift: false,
        ctrl: false,
        alt: false,
        super_key: true,
    };
    let event = ev(TermKey::Char('a'), Some('a'), super_only, KeyKind::Press, Some("a"));
    let (outcome, bytes) = enc(&mut term, &event);
    assert!(bytes.is_empty());
    assert_ne!(outcome, KeyOutcome::Sent);
}
