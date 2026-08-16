use std::ops::{BitOr, BitOrAssign};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyState {
    Pressed,
    Released,
}

impl KeyState {
    #[inline]
    pub(crate) fn is_pressed(self) -> bool {
        self == Self::Pressed
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifiers(u8);

impl Modifiers {
    pub(crate) const SHIFT: Self = Self(1 << 0);
    pub(crate) const CONTROL: Self = Self(1 << 1);
    pub(crate) const ALT: Self = Self(1 << 2);
    pub(crate) const SUPER: Self = Self(1 << 3);

    #[inline]
    pub(crate) const fn empty() -> Self {
        Self(0)
    }

    #[inline]
    pub(crate) const fn new(shift: bool, control: bool, alt: bool, super_key: bool) -> Self {
        let mut bits = 0;
        if shift {
            bits |= Self::SHIFT.0;
        }
        if control {
            bits |= Self::CONTROL.0;
        }
        if alt {
            bits |= Self::ALT.0;
        }
        if super_key {
            bits |= Self::SUPER.0;
        }
        Self(bits)
    }

    #[inline]
    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub(crate) const fn shift_key(self) -> bool {
        self.0 & Self::SHIFT.0 != 0
    }

    #[inline]
    pub(crate) const fn control_key(self) -> bool {
        self.0 & Self::CONTROL.0 != 0
    }

    #[inline]
    pub(crate) const fn alt_key(self) -> bool {
        self.0 & Self::ALT.0 != 0
    }

    #[inline]
    pub(crate) const fn super_key(self) -> bool {
        self.0 & Self::SUPER.0 != 0
    }
}

impl BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyCode {
    Enter,
    NumpadEnter,
    Backspace,
    Tab,
    Escape,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Home,
    End,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Space,
    Digit2,
    Digit4,
    Digit6,
    Minus,
    Slash,
    BracketLeft,
    Backslash,
    BracketRight,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PhysicalKey {
    Code(KeyCode),
    Unidentified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyInput {
    pub state: KeyState,
    pub physical_key: PhysicalKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
    Other(u16),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollDelta {
    Line { x: f32, y: f32 },
    Pixel { x: f32, y: f32 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PointerPosition {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CursorKind {
    #[default]
    Default,
    Pointer,
    Text,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_keep_backend_independent_bit_semantics() {
        let modifiers = Modifiers::CONTROL | Modifiers::SHIFT;
        assert!(modifiers.control_key());
        assert!(modifiers.shift_key());
        assert!(!modifiers.alt_key());
        assert!(!modifiers.super_key());
        assert!(!modifiers.is_empty());
        assert!(Modifiers::empty().is_empty());
    }
}
