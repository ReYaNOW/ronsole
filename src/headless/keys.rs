//! Synthesizes host key events for the headless protocol.
//!
//! Text and named-key decisions reuse the Wayland helpers, so a synthetic key takes the
//! same path as a compositor key: no copies of the text or keypad rules live here.

use crate::input_types::{KeyInput, KeyState, Modifiers};
use crate::wayland_input::{named_term_key, physical_key_from_evdev, should_emit_xkb_text};

/// A key on the US layout: its evdev keycode and the characters it produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeySpec {
    pub(crate) keycode: u32,
    /// Character at XKB level 0 (no modifiers); `None` for keys without one.
    pub(crate) unshifted: Option<char>,
    /// Character with Shift held; `None` for keys without one.
    pub(crate) shifted: Option<char>,
}

/// evdev keycodes of `a`..`z`, in alphabetical order.
const LETTER_KEYCODES: [u32; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
    45, 21, 44,
];

/// Shifted US-layout characters of the digits `0`..`9`.
const DIGIT_SHIFTED: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

const fn plain(keycode: u32) -> KeySpec {
    KeySpec { keycode, unshifted: None, shifted: None }
}

const fn symbol(keycode: u32, unshifted: char, shifted: char) -> KeySpec {
    KeySpec { keycode, unshifted: Some(unshifted), shifted: Some(shifted) }
}

/// Maps a lowercase protocol key name to its US-layout key; `None` for unknown names.
pub(crate) fn key_spec(name: &str) -> Option<KeySpec> {
    let spec = match name {
        "enter" => plain(28),
        "numpadenter" => plain(96),
        "tab" => plain(15),
        "escape" | "esc" => plain(1),
        "backspace" => plain(14),
        "delete" => plain(111),
        "space" => symbol(57, ' ', ' '),
        "up" => plain(103),
        "down" => plain(108),
        "left" => plain(105),
        "right" => plain(106),
        "home" => plain(102),
        "end" => plain(107),
        "pageup" => plain(104),
        "pagedown" => plain(109),
        "insert" => plain(110),
        "pause" => plain(119),
        "f1" => plain(59),
        "f2" => plain(60),
        "f3" => plain(61),
        "f4" => plain(62),
        "f5" => plain(63),
        "f6" => plain(64),
        "f7" => plain(65),
        "f8" => plain(66),
        "f9" => plain(67),
        "f10" => plain(68),
        "f11" => plain(87),
        "f12" => plain(88),
        "." => symbol(52, '.', '>'),
        "," => symbol(51, ',', '<'),
        "/" => symbol(53, '/', '?'),
        "-" => symbol(12, '-', '_'),
        "=" => symbol(13, '=', '+'),
        ";" => symbol(39, ';', ':'),
        "'" => symbol(40, '\'', '"'),
        "[" => symbol(26, '[', '{'),
        "]" => symbol(27, ']', '}'),
        "\\" => symbol(43, '\\', '|'),
        "`" => symbol(41, '`', '~'),
        _ => return letter_or_digit(name),
    };
    Some(spec)
}

/// Single-character names: `a`..`z` and `0`..`9`.
fn letter_or_digit(name: &str) -> Option<KeySpec> {
    let &[byte] = name.as_bytes() else {
        return None;
    };
    match byte {
        b'a'..=b'z' => {
            let c = char::from(byte);
            let keycode = LETTER_KEYCODES[usize::from(byte - b'a')];
            Some(symbol(keycode, c, c.to_ascii_uppercase()))
        }
        b'0'..=b'9' => {
            let digit = byte - b'0';
            let keycode = if digit == 0 { 11 } else { u32::from(digit) + 1 };
            Some(symbol(keycode, char::from(byte), DIGIT_SHIFTED[usize::from(digit)]))
        }
        _ => None,
    }
}

/// Replaces the layout characters of `spec` with `c` (`--char`): `c` unshifted and its
/// uppercase form shifted. Characters whose uppercase is not one char keep `c` shifted.
pub(crate) fn with_layout_char(spec: KeySpec, c: char) -> KeySpec {
    let mut upper = c.to_uppercase();
    let shifted = match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => c,
    };
    KeySpec { keycode: spec.keycode, unshifted: Some(c), shifted: Some(shifted) }
}

/// Keysym of a layout character as `named_term_key` expects it: printable ASCII keeps its
/// code point, every other character and keyless key maps to `0`.
fn keysym_of_char(c: Option<char>) -> u32 {
    match c {
        Some(c @ ' '..='~') => u32::from(c),
        _ => 0,
    }
}

/// Builds the host key event for `spec` with the given state and modifiers.
pub(crate) fn key_input(spec: &KeySpec, state: KeyState, mods: Modifiers) -> KeyInput {
    let physical_key = physical_key_from_evdev(spec.keycode);
    let text = if should_emit_xkb_text(
        state,
        physical_key,
        mods,
        false,
        state == KeyState::Repeat,
        false,
    ) {
        let character = if mods.shift_key() { spec.shifted } else { spec.unshifted };
        character.map(String::from)
    } else {
        None
    };
    let keysym = keysym_of_char(spec.unshifted);
    KeyInput {
        state,
        physical_key,
        keycode: spec.keycode,
        text,
        unshifted: spec.unshifted,
        named: named_term_key(spec.keycode, keysym, keysym),
        ime_preedit: false,
    }
}

/// Builds the host key event for one left-hand modifier key.
///
/// `modifier` should hold a single flag; the first set flag in ctrl, shift, alt, super
/// order is used. An empty set yields keycode `0` (`KEY_RESERVED`), which no key produces.
pub(crate) fn modifier_key_input(modifier: Modifiers, state: KeyState) -> KeyInput {
    let keycode = if modifier.control_key() {
        29
    } else if modifier.shift_key() {
        42
    } else if modifier.alt_key() {
        56
    } else if modifier.super_key() {
        125
    } else {
        0
    };
    KeyInput {
        state,
        physical_key: physical_key_from_evdev(keycode),
        keycode,
        text: None,
        unshifted: None,
        named: named_term_key(keycode, 0, 0),
        ime_preedit: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::terminal_key_event;
    use crate::input_types::{KeyCode, PhysicalKey};
    use ronsole_term::{ModKey, TermKey};

    fn spec(name: &str) -> KeySpec {
        key_spec(name).unwrap_or_else(|| panic!("known key '{name}'"))
    }

    #[test]
    fn ctrl_c_has_no_text_and_keeps_the_letter() {
        let input = key_input(&spec("c"), KeyState::Pressed, Modifiers::CONTROL);
        assert_eq!(input.keycode, 46);
        assert_eq!(input.text, None);
        assert_eq!(input.unshifted, Some('c'));
    }

    #[test]
    fn shift_letter_emits_uppercase_text() {
        let input = key_input(&spec("a"), KeyState::Pressed, Modifiers::SHIFT);
        assert_eq!(input.text.as_deref(), Some("A"));
        assert_eq!(input.unshifted, Some('a'));
    }

    #[test]
    fn repeat_of_a_letter_emits_text() {
        let input = key_input(&spec("a"), KeyState::Repeat, Modifiers::empty());
        assert_eq!(input.text.as_deref(), Some("a"));
    }

    #[test]
    fn released_key_emits_no_text() {
        let input = key_input(&spec("a"), KeyState::Released, Modifiers::empty());
        assert_eq!(input.text, None);
    }

    #[test]
    fn enter_without_modifiers_has_no_text() {
        let input = key_input(&spec("enter"), KeyState::Pressed, Modifiers::empty());
        assert_eq!(input.text, None);
        assert_eq!(input.physical_key, PhysicalKey::Code(KeyCode::Enter));
    }

    #[test]
    fn pause_is_unidentified_physical_key() {
        let input = key_input(&spec("pause"), KeyState::Pressed, Modifiers::empty());
        assert_eq!(input.physical_key, PhysicalKey::Unidentified);
        assert_eq!(input.keycode, 119);
    }

    #[test]
    fn layout_char_with_ctrl_has_no_text() {
        let c = with_layout_char(spec("c"), 'с');
        let input = key_input(&c, KeyState::Pressed, Modifiers::CONTROL);
        assert_eq!(input.unshifted, Some('с'));
        assert_eq!(input.text, None);
    }

    #[test]
    fn layout_char_text_follows_shift() {
        let c = with_layout_char(spec("c"), 'с');
        let plain = key_input(&c, KeyState::Pressed, Modifiers::empty());
        assert_eq!(plain.text.as_deref(), Some("с"));
        let shifted = key_input(&c, KeyState::Pressed, Modifiers::SHIFT);
        assert_eq!(shifted.text.as_deref(), Some("С"));
    }

    #[test]
    fn control_modifier_is_left_ctrl() {
        let input = modifier_key_input(Modifiers::CONTROL, KeyState::Pressed);
        assert_eq!(input.keycode, 29);
        assert_eq!(input.named, Some(TermKey::Modifier(ModKey::LeftCtrl)));
        assert_eq!(input.text, None);
        assert_eq!(input.unshifted, None);
    }

    /// Expected values come from `key_event_maps_russian_ctrl_c` in `src/input.rs`.
    #[test]
    fn synthetic_ctrl_c_matches_wayland_russian_layout_event() {
        let c = with_layout_char(spec("c"), 'с');
        let key = key_input(&c, KeyState::Pressed, Modifiers::CONTROL);
        let event = terminal_key_event(&key, Modifiers::CONTROL).expect("identified key");
        assert_eq!(event.physical, 46);
        assert_eq!(event.key, TermKey::Char('с'));
        assert_eq!(event.base, Some('c'));
        assert!(event.mods.ctrl);
    }
}
