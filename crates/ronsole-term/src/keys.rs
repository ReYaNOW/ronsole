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
    /// Nothing is sent.
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
}

/// Masks requested Kitty flags down to the supported bits.
fn masked_flags(raw: u16) -> u8 {
    (raw & u16::from(KITTY_SUPPORTED_FLAGS)) as u8
}

impl KeyProtocol {
    /// Flags of the given screen (`alt` is true for the alternate screen).
    pub(crate) fn flags(&self, alt: bool) -> u8 {
        self.stacks[usize::from(alt)].current()
    }

    /// `CSI > flags u`. Clears the sent-key set.
    pub(crate) fn push(&mut self, alt: bool, raw_flags: u16) {
        self.stacks[usize::from(alt)].push(masked_flags(raw_flags));
        self.sent.clear();
    }

    /// `CSI < count u`; a count of 0 pops one entry. Clears the sent-key set.
    pub(crate) fn pop(&mut self, alt: bool, count: u16) {
        self.stacks[usize::from(alt)].pop(usize::from(count.max(1)));
        self.sent.clear();
    }

    /// `CSI = flags ; mode u`: mode 1 (and 0) replaces, 2 adds bits, 3 removes
    /// bits. Other modes change nothing. Clears the sent-key set only when a
    /// known mode is applied.
    pub(crate) fn set(&mut self, alt: bool, raw_flags: u16, mode: u16) {
        let flags = masked_flags(raw_flags);
        let stack = &mut self.stacks[usize::from(alt)];
        let next = match mode {
            0 | 1 => flags,
            2 => stack.current() | flags,
            3 => stack.current() & !flags,
            _ => return,
        };
        stack.current = next;
        self.sent.clear();
    }

    /// Clears the sent-key set. Called on focus loss.
    pub(crate) fn clear_sent(&mut self) {
        self.sent.clear();
    }

    /// Clears the sent-key set when the active screen changes.
    pub(crate) fn screen_switched(&mut self) {
        self.sent.clear();
    }

    /// Clears both stacks, all flags and the sent-key set (RIS and DECSTR).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn sent(&self) -> &SentKeys {
        &self.sent
    }

    pub(crate) fn sent_mut(&mut self) -> &mut SentKeys {
        &mut self.sent
    }
}
