//! Keyboard event model and Kitty keyboard protocol state.
//!
//! Hosts describe key presses with [`KeyEvent`]; the encoder that turns them
//! into bytes reads the mode flags kept in [`KeyProtocol`]. The sent-key set
//! and the flag stacks change only through the methods of [`KeyProtocol`], so
//! every reset point clears them the same way.

/// Kitty flags Ronsole supports. Other requested bits, including flag 4
/// (alternate keys), are masked off on push, set and query.
pub(crate) const KITTY_SUPPORTED_FLAGS: u8 = 0b1_1011;

/// Entries kept per screen by the flag stack; a push beyond this drops the oldest.
const KITTY_STACK_DEPTH: usize = 32;

/// Pairs kept by the sent-key set; inserting into a full set drops the oldest.
const SENT_KEYS_CAPACITY: usize = 16;

/// Keypad keys, named after the physical keypad legends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KpKey {
    Kp0,
    Kp1,
    Kp2,
    Kp3,
    Kp4,
    Kp5,
    Kp6,
    Kp7,
    Kp8,
    Kp9,
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Enter,
    Equal,
    Separator,
    Begin,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
}

/// Modifier keys, reported as keys of their own when the protocol asks for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModKey {
    LeftShift,
    LeftCtrl,
    LeftAlt,
    LeftSuper,
    RightShift,
    RightCtrl,
    RightAlt,
    RightSuper,
    CapsLock,
    NumLock,
    IsoLevel3Shift,
}

/// Named key, independent of the windowing system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermKey {
    /// Unshifted character of the key in the active layout.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key F1 to F12.
    F(u8),
    Kp(KpKey),
    Modifier(ModKey),
}

/// Modifiers held with a key. AltGr and ISO Level 3 are already removed by
/// the host when they produced text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyMods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Kind of key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    Press,
    Repeat,
    Release,
}

/// One key event from the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent<'a> {
    /// Opaque physical key identifier, the same for press, repeat and release.
    pub physical: u64,
    pub key: TermKey,
    /// Unshifted character of the same physical key on a US layout.
    pub base: Option<char>,
    pub mods: KeyMods,
    pub kind: KeyKind,
    /// Text the key produces with the current modifiers.
    pub text: Option<&'a str>,
}

/// What the host should do with a key event after encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Bytes were written to the output buffer; the host must not also send `text`.
    Sent,
    /// The host sends `text` through its text path.
    Text,
    /// Nothing is sent. This includes Super-only text without flag 1: it is
    /// neither bytes nor text for the host, and the press is not remembered.
    Ignore,
}

/// Form in which a pressed key was sent, needed to repeat the same form on release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SentForm {
    /// `CSI code ; mods u`.
    Unicode,
    /// `CSI 1 ; mods <final>`, with the final byte given here.
    Letter(u8),
    /// `CSI code ; mods ~`.
    Tilde,
}

/// A key press that was sent to the PTY, kept until its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SentKey {
    pub(crate) physical: u64,
    pub(crate) code: u32,
    pub(crate) form: SentForm,
}

/// Fixed-size set of sent presses keyed by physical key, with no allocation.
#[derive(Clone, Debug)]
pub(crate) struct SentKeys {
    entries: [Option<SentKey>; SENT_KEYS_CAPACITY],
    len: usize,
}

impl Default for SentKeys {
    fn default() -> Self {
        Self {
            entries: [None; SENT_KEYS_CAPACITY],
            len: 0,
        }
    }
}

impl SentKeys {
    /// Stores `key`, replacing an entry with the same physical key. A full set
    /// drops its oldest entry.
    pub(crate) fn insert(&mut self, key: SentKey) {
        if let Some(index) = self.position(key.physical) {
            self.entries[index] = Some(key);
            return;
        }
        if self.len == SENT_KEYS_CAPACITY {
            self.entries.copy_within(1.., 0);
            self.len -= 1;
        }
        self.entries[self.len] = Some(key);
        self.len += 1;
    }

    /// Returns the entry for `physical` without removing it.
    pub(crate) fn get(&self, physical: u64) -> Option<SentKey> {
        self.position(physical).and_then(|index| self.entries[index])
    }

    /// Removes and returns the entry for `physical`.
    pub(crate) fn remove(&mut self, physical: u64) -> Option<SentKey> {
        let index = self.position(physical)?;
        let removed = self.entries[index];
        self.entries.copy_within(index + 1..self.len, index);
        self.len -= 1;
        self.entries[self.len] = None;
        removed
    }

    /// Empties the set.
    pub(crate) fn clear(&mut self) {
        self.entries[..self.len].fill(None);
        self.len = 0;
    }

    fn position(&self, physical: u64) -> Option<usize> {
        self.entries[..self.len]
            .iter()
            .position(|entry| matches!(entry, Some(key) if key.physical == physical))
    }
}

/// Flags of one screen: the active value plus the saved values below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KittyFlagStack {
    current: u8,
    saved: [u8; KITTY_STACK_DEPTH],
    len: usize,
}

impl KittyFlagStack {
    fn current(&self) -> u8 {
        self.current
    }

    /// Saves the current flags and makes `flags` current. A full stack drops
    /// its oldest saved value.
    fn push(&mut self, flags: u8) {
        if self.len == KITTY_STACK_DEPTH {
            self.saved.copy_within(1.., 0);
            self.len -= 1;
        }
        self.saved[self.len] = self.current;
        self.len += 1;
        self.current = flags;
    }

    /// Restores `count` saved values. Popping an empty stack makes the flags 0.
    fn pop(&mut self, count: usize) {
        for _ in 0..count {
            match self.len.checked_sub(1) {
                Some(top) => {
                    self.len = top;
                    self.current = self.saved[top];
                }
                None => {
                    self.current = 0;
                    break;
                }
            }
        }
    }
}

/// Kitty keyboard state of a terminal: flag stacks for both screens and the
/// set of presses sent to the PTY and still awaiting their release.
#[derive(Clone, Debug, Default)]
pub(crate) struct KeyProtocol {
    /// Index 0 is the main screen, index 1 the alternate screen.
    stacks: [KittyFlagStack; 2],
    sent: SentKeys,
    /// Whether the alternate screen is active; changed only through [`KeyProtocol::set_alt`].
    alt: bool,
}

/// Masks requested Kitty flags down to the supported bits.
fn masked_flags(raw: u16) -> u8 {
    (raw & u16::from(KITTY_SUPPORTED_FLAGS)) as u8
}

impl KeyProtocol {
    /// Flags of the active screen, limited to the supported bits.
    pub(crate) fn flags(&self) -> u8 {
        self.stacks[usize::from(self.alt)].current()
    }

    /// `CSI > flags u`. Clears the sent-key set.
    pub(crate) fn push(&mut self, raw_flags: u16) {
        self.stacks[usize::from(self.alt)].push(masked_flags(raw_flags));
        self.sent.clear();
    }

    /// `CSI < count u`; a count of 0 pops one entry. Clears the sent-key set.
    pub(crate) fn pop(&mut self, count: u16) {
        self.stacks[usize::from(self.alt)].pop(usize::from(count.max(1)));
        self.sent.clear();
    }

    /// `CSI = flags ; mode u`: mode 1 (and 0) replaces, 2 adds bits, 3 removes
    /// bits. Other modes change nothing. Clears the sent-key set only when a
    /// known mode is applied.
    pub(crate) fn set(&mut self, raw_flags: u16, mode: u16) {
        let flags = masked_flags(raw_flags);
        let stack = &mut self.stacks[usize::from(self.alt)];
        let next = match mode {
            0 | 1 => flags,
            2 => stack.current() | flags,
            3 => stack.current() & !flags,
            _ => return,
        };
        stack.current = next;
        self.sent.clear();
    }

    /// Selects the active screen. Switching clears the sent-key set, so a
    /// release that crosses the switch is ignored.
    pub(crate) fn set_alt(&mut self, alt: bool) {
        self.alt = alt;
        self.sent.clear();
    }

    /// Clears the sent-key set. Called on focus loss.
    pub(crate) fn clear_sent(&mut self) {
        self.sent.clear();
    }

    /// Clears both stacks, all flags and the sent-key set (RIS and DECSTR).
    /// The active screen is kept; RIS moves it through [`KeyProtocol::set_alt`].
    pub(crate) fn reset(&mut self) {
        let alt = self.alt;
        *self = Self { alt, ..Self::default() };
    }

    fn sent(&self) -> &SentKeys {
        &self.sent
    }

    fn sent_mut(&mut self) -> &mut SentKeys {
        &mut self.sent
    }
}

/// Kitty flag bits read by the encoder.
const FLAG_DISAMBIGUATE: u8 = 0b1;
const FLAG_REPORT_EVENTS: u8 = 0b10;
const FLAG_REPORT_ALL: u8 = 0b1000;
const FLAG_REPORT_TEXT: u8 = 0b1_0000;

/// Modifier bits of the Kitty modifier field, before the `+ 1` offset.
const BIT_SHIFT: u8 = 1;
const BIT_ALT: u8 = 2;
const BIT_CTRL: u8 = 4;
const BIT_SUPER: u8 = 8;

/// CSI form of a key that is not sent as text.
#[derive(Clone, Copy)]
enum CsiForm {
    /// `CSI A` style: a final byte only; code 1 is written when a field follows.
    Letter(u8),
    /// `CSI code u` or `CSI code ~`.
    Number(u32, u8),
}

/// Encodes one key event into `out` and reports what the host must do with it.
/// `flags` are the active Kitty flags and `app_cursor` is the DECCKM mode.
pub(crate) fn encode_key(
    protocol: &mut KeyProtocol,
    flags: u8,
    app_cursor: bool,
    event: &KeyEvent<'_>,
    out: &mut Vec<u8>,
) -> KeyOutcome {
    let all = flags & FLAG_REPORT_ALL != 0;
    let events = flags & FLAG_REPORT_EVENTS != 0;
    let disambiguate = all || flags & FLAG_DISAMBIGUATE != 0;
    let release = event.kind == KeyKind::Release;
    let bits = modifier_bits(event.mods);
    // Associated text may not hold control codes, and empty text adds no field (reference, flag 16).
    let text = event.text.filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
    let associated = if all && flags & FLAG_REPORT_TEXT != 0 { text } else { None };
    // The base character stands in for a non-ASCII key only with Ctrl, Alt or Super (decision 18).
    let base = event.base.filter(|_| event.mods.ctrl || event.mods.alt || event.mods.super_key);

    if let TermKey::Modifier(modifier) = event.key {
        if !all || (release && !events) {
            return KeyOutcome::Ignore;
        }
        let own = own_bit(modifier);
        let bits = if release { bits & !own } else { bits | own };
        write_csi(out, modifier_code(modifier), 1 + bits, event_field(event.kind, events), None, b'u');
        return KeyOutcome::Sent;
    }

    let mod_field = 1 + bits;
    // Press, repeat and release all read the same key: keypad keys become their
    // legacy equivalents only while flag 1 is off.
    let key = if disambiguate { event.key } else { legacy_equivalent(event.key) };
    if release {
        if !events {
            return KeyOutcome::Ignore;
        }
        let pair = protocol.sent_mut().remove(event.physical);
        if !all && matches!(key, TermKey::Enter | TermKey::Tab | TermKey::Backspace) {
            return KeyOutcome::Ignore;
        }
        let Some(pair) = pair else {
            return KeyOutcome::Ignore;
        };
        write_sent(out, pair, mod_field, 3, None);
        return KeyOutcome::Sent;
    }

    let repeat = event.kind == KeyKind::Repeat && events;
    // KP_BEGIN has no legacy form (reference legacy column `-`), so without flag 1 it sends nothing.
    if !disambiguate && matches!(key, TermKey::Kp(KpKey::Begin)) {
        return KeyOutcome::Ignore;
    }
    // Super alone adds no legacy bytes: only disambiguation (flag 1) reports it.
    let plain = !event.mods.ctrl && !event.mods.alt && (!event.mods.super_key || !disambiguate);
    if !all && plain && is_text_key(key) {
        protocol.sent_mut().remove(event.physical);
        // Super-only text without flag 1 is ignored, not sent as text (KeyOutcome::Ignore).
        return if event.mods.super_key { KeyOutcome::Ignore } else { KeyOutcome::Text };
    }

    // Unmodified Enter, Tab and Backspace keep their legacy bytes under flag 1 and
    // on repeat; every other legacy key needs flags 0 or 2 only.
    let legacy_control = mod_field == 1 && matches!(key, TermKey::Enter | TermKey::Tab | TermKey::Backspace);
    if !all && ((!disambiguate && !repeat) || legacy_control) {
        if let Some(outcome) = write_legacy(out, key, event, text, flags, app_cursor) {
            if outcome == KeyOutcome::Sent && events && event.kind == KeyKind::Press {
                if let Some((code, form)) = legacy_pair(key, base) {
                    protocol.sent_mut().insert(SentKey { physical: event.physical, code, form });
                }
            }
            return outcome;
        }
    }

    if repeat {
        if let Some(pair) = protocol.sent().get(event.physical) {
            write_sent(out, pair, mod_field, 2, associated);
            return KeyOutcome::Sent;
        }
    }

    let Some(form) = csi_form(key, base) else {
        return KeyOutcome::Ignore;
    };
    let event_code = event_field(event.kind, events);
    match form {
        CsiForm::Letter(final_byte) => {
            let code = if mod_field == 1 && event_code == 1 { 0 } else { 1 };
            write_csi(out, code, mod_field, event_code, None, final_byte);
        }
        CsiForm::Number(code, final_byte) => write_csi(out, code, mod_field, event_code, associated, final_byte),
    }
    if events {
        let (code, form) = match form {
            CsiForm::Letter(final_byte) => (1, SentForm::Letter(final_byte)),
            CsiForm::Number(code, b'~') => (code, SentForm::Tilde),
            CsiForm::Number(code, _) => (code, SentForm::Unicode),
        };
        protocol.sent_mut().insert(SentKey { physical: event.physical, code, form });
    }
    KeyOutcome::Sent
}

/// Legacy bytes for keys without Kitty flags 1 or 8. Returns `None` when the
/// key must take the CSI path instead.
fn write_legacy(
    out: &mut Vec<u8>,
    key: TermKey,
    event: &KeyEvent<'_>,
    text: Option<&str>,
    flags: u8,
    app_cursor: bool,
) -> Option<KeyOutcome> {
    let mods = event.mods;
    let alt = mods.alt;
    match key {
        TermKey::Char(c) => {
            if mods.ctrl {
                let Some(byte) = control_byte(event.base.unwrap_or(c)) else {
                    return Some(KeyOutcome::Ignore);
                };
                alt_prefixed(out, alt, &[byte]);
            } else {
                let mut buf = [0u8; 4];
                let typed = text.unwrap_or_else(|| &*c.encode_utf8(&mut buf));
                alt_prefixed(out, alt, typed.as_bytes());
            }
        }
        TermKey::Enter => alt_prefixed(out, alt || mods.shift, b"\r"),
        TermKey::Tab if mods.shift => out.extend_from_slice(b"\x1b[Z"),
        TermKey::Tab => alt_prefixed(out, alt, b"\t"),
        TermKey::Backspace => {
            let byte = if mods.ctrl {
                0x17
            } else if mods.shift {
                0x08
            } else {
                0x7f
            };
            alt_prefixed(out, alt, &[byte]);
        }
        TermKey::Escape => out.push(0x1b),
        _ => {
            // Unmodified cursor keys in cursor-key mode and F1-F4 use SS3.
            let final_byte = ss3_final(key)?;
            if flags & (FLAG_DISAMBIGUATE | FLAG_REPORT_EVENTS | FLAG_REPORT_ALL) != 0 || modifier_bits(mods) != 0 {
                return None;
            }
            let cursor = matches!(key, TermKey::Up | TermKey::Down | TermKey::Left | TermKey::Right | TermKey::Home | TermKey::End);
            if cursor && !app_cursor {
                return None;
            }
            out.extend_from_slice(&[0x1b, b'O', final_byte]);
        }
    }
    Some(KeyOutcome::Sent)
}

/// Writes `CSI code ; mods:event ; text final`. Default fields are omitted;
/// a code of 0 is omitted.
fn write_csi(out: &mut Vec<u8>, code: u32, mods: u8, event: u8, text: Option<&str>, final_byte: u8) {
    out.extend_from_slice(b"\x1b[");
    if code != 0 {
        push_decimal(out, code);
    }
    if mods != 1 || event != 1 || text.is_some() {
        out.push(b';');
        if mods != 1 || event != 1 {
            push_decimal(out, u32::from(mods));
        }
        if event != 1 {
            out.push(b':');
            push_decimal(out, u32::from(event));
        }
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            out.push(b';');
            for (index, ch) in text.chars().enumerate() {
                if index > 0 {
                    out.push(b':');
                }
                push_decimal(out, u32::from(ch));
            }
        }
    }
    out.push(final_byte);
}

/// Writes a sent key again with the form it was pressed in.
/// Only the `Unicode` form carries associated text, as on press.
fn write_sent(out: &mut Vec<u8>, pair: SentKey, mod_field: u8, event: u8, text: Option<&str>) {
    match pair.form {
        SentForm::Unicode => write_csi(out, pair.code, mod_field, event, text, b'u'),
        SentForm::Letter(final_byte) => write_csi(out, 1, mod_field, event, None, final_byte),
        SentForm::Tilde => write_csi(out, pair.code, mod_field, event, None, b'~'),
    }
}

/// Code and form under which a legacy press is remembered for its release.
fn legacy_pair(key: TermKey, base: Option<char>) -> Option<(u32, SentForm)> {
    match key {
        TermKey::Char(c) => Some((unicode_code(c, base), SentForm::Unicode)),
        TermKey::Enter => Some((13, SentForm::Unicode)),
        TermKey::Tab => Some((9, SentForm::Unicode)),
        TermKey::Backspace => Some((127, SentForm::Unicode)),
        TermKey::Escape => Some((27, SentForm::Unicode)),
        _ => None,
    }
}

fn csi_form(key: TermKey, base: Option<char>) -> Option<CsiForm> {
    Some(match key {
        TermKey::Char(c) => CsiForm::Number(unicode_code(c, base), b'u'),
        TermKey::Enter => CsiForm::Number(13, b'u'),
        TermKey::Tab => CsiForm::Number(9, b'u'),
        TermKey::Backspace => CsiForm::Number(127, b'u'),
        TermKey::Escape => CsiForm::Number(27, b'u'),
        TermKey::Up => CsiForm::Letter(b'A'),
        TermKey::Down => CsiForm::Letter(b'B'),
        TermKey::Right => CsiForm::Letter(b'C'),
        TermKey::Left => CsiForm::Letter(b'D'),
        TermKey::Home => CsiForm::Letter(b'H'),
        TermKey::End => CsiForm::Letter(b'F'),
        TermKey::PageUp => CsiForm::Number(5, b'~'),
        TermKey::PageDown => CsiForm::Number(6, b'~'),
        TermKey::Insert => CsiForm::Number(2, b'~'),
        TermKey::Delete => CsiForm::Number(3, b'~'),
        TermKey::F(1) => CsiForm::Letter(b'P'),
        TermKey::F(2) => CsiForm::Letter(b'Q'),
        TermKey::F(4) => CsiForm::Letter(b'S'),
        TermKey::F(n @ (3 | 5..=12)) => CsiForm::Number(tilde_code(n), b'~'),
        TermKey::F(_) => return None,
        // KP_BEGIN is written in the functional `1 E` form of its reference row, like F1's `1 P`.
        TermKey::Kp(KpKey::Begin) => CsiForm::Letter(b'E'),
        TermKey::Kp(key) => CsiForm::Number(kp_code(key), b'u'),
        TermKey::Modifier(_) => return None,
    })
}

/// Final byte of the SS3 form of a key, if it has one.
fn ss3_final(key: TermKey) -> Option<u8> {
    Some(match key {
        TermKey::Up => b'A',
        TermKey::Down => b'B',
        TermKey::Right => b'C',
        TermKey::Left => b'D',
        TermKey::Home => b'H',
        TermKey::End => b'F',
        TermKey::F(1) => b'P',
        TermKey::F(2) => b'Q',
        TermKey::F(3) => b'R',
        TermKey::F(4) => b'S',
        _ => return None,
    })
}

/// Keypad keys that produce text, as opposed to keys like KP_Enter.
fn is_text_key(key: TermKey) -> bool {
    match key {
        TermKey::Char(_) => true,
        TermKey::Kp(key) => matches!(
            key,
            KpKey::Kp0
                | KpKey::Kp1
                | KpKey::Kp2
                | KpKey::Kp3
                | KpKey::Kp4
                | KpKey::Kp5
                | KpKey::Kp6
                | KpKey::Kp7
                | KpKey::Kp8
                | KpKey::Kp9
                | KpKey::Decimal
                | KpKey::Divide
                | KpKey::Multiply
                | KpKey::Subtract
                | KpKey::Add
                | KpKey::Equal
                | KpKey::Separator
        ),
        _ => false,
    }
}

/// Character that a text-producing keypad key stands for without flag 1.
fn keypad_char(key: KpKey) -> Option<char> {
    Some(match key {
        KpKey::Kp0 => '0',
        KpKey::Kp1 => '1',
        KpKey::Kp2 => '2',
        KpKey::Kp3 => '3',
        KpKey::Kp4 => '4',
        KpKey::Kp5 => '5',
        KpKey::Kp6 => '6',
        KpKey::Kp7 => '7',
        KpKey::Kp8 => '8',
        KpKey::Kp9 => '9',
        KpKey::Decimal => '.',
        KpKey::Divide => '/',
        KpKey::Multiply => '*',
        KpKey::Subtract => '-',
        KpKey::Add => '+',
        KpKey::Equal => '=',
        KpKey::Separator => ',',
        _ => return None,
    })
}

/// Non-keypad key that a keypad key stands for when Kitty flag 1 and flag 8 are off.
fn legacy_equivalent(key: TermKey) -> TermKey {
    if let TermKey::Kp(key) = key {
        if let Some(c) = keypad_char(key) {
            return TermKey::Char(c);
        }
    }
    match key {
        TermKey::Kp(KpKey::Enter) => TermKey::Enter,
        TermKey::Kp(KpKey::Left) => TermKey::Left,
        TermKey::Kp(KpKey::Right) => TermKey::Right,
        TermKey::Kp(KpKey::Up) => TermKey::Up,
        TermKey::Kp(KpKey::Down) => TermKey::Down,
        TermKey::Kp(KpKey::PageUp) => TermKey::PageUp,
        TermKey::Kp(KpKey::PageDown) => TermKey::PageDown,
        TermKey::Kp(KpKey::Home) => TermKey::Home,
        TermKey::Kp(KpKey::End) => TermKey::End,
        TermKey::Kp(KpKey::Insert) => TermKey::Insert,
        TermKey::Kp(KpKey::Delete) => TermKey::Delete,
        other => other,
    }
}

/// Code point of a text key: the lowercase character for ASCII, otherwise the
/// US base character when there is one.
fn unicode_code(c: char, base: Option<char>) -> u32 {
    if c.is_ascii() {
        u32::from(c.to_ascii_lowercase())
    } else {
        u32::from(base.unwrap_or(c))
    }
}

/// C0 byte for Ctrl plus the unshifted character `c`.
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        ' ' | '2' | '@' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' | '6' => 0x1e,
        '/' | '-' | '_' => 0x1f,
        _ => return None,
    })
}

fn kp_code(key: KpKey) -> u32 {
    match key {
        KpKey::Kp0 => 57399,
        KpKey::Kp1 => 57400,
        KpKey::Kp2 => 57401,
        KpKey::Kp3 => 57402,
        KpKey::Kp4 => 57403,
        KpKey::Kp5 => 57404,
        KpKey::Kp6 => 57405,
        KpKey::Kp7 => 57406,
        KpKey::Kp8 => 57407,
        KpKey::Kp9 => 57408,
        KpKey::Decimal => 57409,
        KpKey::Divide => 57410,
        KpKey::Multiply => 57411,
        KpKey::Subtract => 57412,
        KpKey::Add => 57413,
        KpKey::Enter => 57414,
        KpKey::Equal => 57415,
        KpKey::Separator => 57416,
        KpKey::Left => 57417,
        KpKey::Right => 57418,
        KpKey::Up => 57419,
        KpKey::Down => 57420,
        KpKey::PageUp => 57421,
        KpKey::PageDown => 57422,
        KpKey::Home => 57423,
        KpKey::End => 57424,
        KpKey::Insert => 57425,
        KpKey::Delete => 57426,
        KpKey::Begin => 57427,
    }
}

/// Tilde code of F3 and F5 to F12.
fn tilde_code(n: u8) -> u32 {
    match n {
        3 => 13,
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        _ => 24,
    }
}

/// PUA code point of a modifier key, from the Kitty functional key table.
fn modifier_code(key: ModKey) -> u32 {
    match key {
        ModKey::LeftShift => 57441,
        ModKey::LeftCtrl => 57442,
        ModKey::LeftAlt => 57443,
        ModKey::LeftSuper => 57444,
        ModKey::RightShift => 57447,
        ModKey::RightCtrl => 57448,
        ModKey::RightAlt => 57449,
        ModKey::RightSuper => 57450,
        ModKey::CapsLock => 57358,
        ModKey::NumLock => 57360,
        ModKey::IsoLevel3Shift => 57453,
    }
}

/// Modifier bit that a modifier key sets on its own press and clears on release.
fn own_bit(key: ModKey) -> u8 {
    match key {
        ModKey::LeftShift | ModKey::RightShift => BIT_SHIFT,
        ModKey::LeftAlt | ModKey::RightAlt => BIT_ALT,
        ModKey::LeftCtrl | ModKey::RightCtrl => BIT_CTRL,
        ModKey::LeftSuper | ModKey::RightSuper => BIT_SUPER,
        ModKey::CapsLock | ModKey::NumLock | ModKey::IsoLevel3Shift => 0,
    }
}

fn modifier_bits(mods: KeyMods) -> u8 {
    u8::from(mods.shift) * BIT_SHIFT
        | u8::from(mods.alt) * BIT_ALT
        | u8::from(mods.ctrl) * BIT_CTRL
        | u8::from(mods.super_key) * BIT_SUPER
}

/// Event-type field: 1 when events are not reported, otherwise press 1, repeat 2, release 3.
fn event_field(kind: KeyKind, events: bool) -> u8 {
    match (events, kind) {
        (false, _) | (true, KeyKind::Press) => 1,
        (true, KeyKind::Repeat) => 2,
        (true, KeyKind::Release) => 3,
    }
}

/// Writes `ESC` before `bytes` when `alt` is set.
fn alt_prefixed(out: &mut Vec<u8>, alt: bool, bytes: &[u8]) {
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
}

/// Writes a decimal number without allocating.
fn push_decimal(out: &mut Vec<u8>, mut value: u32) {
    let mut digits = [0u8; 10];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend(digits[..len].iter().rev());
}
