//! Mouse, paste and focus protocol encoders.

/// Mouse reporting mode requested by the application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTrackingMode {
    #[default]
    None,
    Press,
    ButtonMotion,
    AnyMotion,
}

impl MouseTrackingMode {
    pub const fn enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Mouse button as reported to the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// A pointer event to encode. Coordinates are passed separately to [`mouse_report`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEventKind {
    Press(MouseButton),
    Release(MouseButton),
    Motion { held: Option<MouseButton> },
    WheelUp,
    WheelDown,
}

/// Largest 1-based coordinate the legacy X10 encoding can carry: the
/// coordinate is sent as `value + 32` in one byte.
const LEGACY_COORDINATE_LIMIT: usize = 223;

const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

const fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Encodes a mouse event for the given tracking mode. `col` and `row` are
/// zero-based; the report uses one-based coordinates. Returns `None` when the
/// mode does not report this event. Legacy (non-SGR) coordinates saturate at
/// `LEGACY_COORDINATE_LIMIT`; SGR coordinates are not limited.
pub fn mouse_report(
    mode: MouseTrackingMode,
    sgr: bool,
    kind: MouseEventKind,
    col: usize,
    row: usize,
) -> Option<Vec<u8>> {
    if !mode.enabled() {
        return None;
    }
    let (code, pressed) = match kind {
        MouseEventKind::Press(button) => (button_code(button), true),
        MouseEventKind::Release(button) => (button_code(button), false),
        MouseEventKind::Motion { held } => {
            let base = match (mode, held) {
                (MouseTrackingMode::AnyMotion, None) => 3,
                (MouseTrackingMode::AnyMotion | MouseTrackingMode::ButtonMotion, Some(button)) => {
                    button_code(button)
                }
                _ => return None,
            };
            (base + 32, true)
        }
        MouseEventKind::WheelUp => (64, true),
        MouseEventKind::WheelDown => (65, true),
    };
    let x = col + 1;
    let y = row + 1;
    if sgr {
        let final_byte = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{x};{y}{final_byte}").into_bytes());
    }
    // Legacy encoding has no separate release code per button; release is code 3.
    let legacy_code = if pressed { code } else { 3 };
    // Coordinates past the limit clamp to it, matching the original encoder.
    Some(vec![
        0x1b,
        b'[',
        b'M',
        legacy_code + 32,
        x.min(LEGACY_COORDINATE_LIMIT) as u8 + 32,
        y.min(LEGACY_COORDINATE_LIMIT) as u8 + 32,
    ])
}

/// Wraps a paste payload in bracketed-paste markers when the application enabled them.
pub fn bracketed_paste_bytes(payload: Vec<u8>, enabled: bool) -> Vec<u8> {
    if !enabled {
        return payload;
    }
    let mut out = Vec::with_capacity(BRACKETED_PASTE_START.len() + payload.len() + BRACKETED_PASTE_END.len());
    out.extend_from_slice(BRACKETED_PASTE_START);
    out.extend_from_slice(&payload);
    out.extend_from_slice(BRACKETED_PASTE_END);
    out
}

/// Focus-in or focus-out report (DECSET 1004), when focus reporting is enabled.
pub fn focus_report_sequence(enabled: bool, focused: bool) -> Option<&'static [u8]> {
    enabled.then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_report_sgr_press_and_release_use_final_m_and_m() {
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, true, MouseEventKind::Press(MouseButton::Left), 0, 0),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, true, MouseEventKind::Release(MouseButton::Left), 0, 0),
            Some(b"\x1b[<0;1;1m".to_vec())
        );
    }

    #[test]
    fn mouse_report_legacy_release_uses_code_three() {
        // Code 3 + 32 = '#', coordinates 1 + 32 = '!'.
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, false, MouseEventKind::Release(MouseButton::Left), 0, 0),
            Some(b"\x1b[M#!!".to_vec())
        );
    }

    #[test]
    fn mouse_report_press_mode_ignores_motion() {
        assert_eq!(
            mouse_report(
                MouseTrackingMode::Press,
                true,
                MouseEventKind::Motion { held: Some(MouseButton::Left) },
                3,
                4,
            ),
            None
        );
    }

    #[test]
    fn mouse_report_button_motion_without_held_button_is_none() {
        assert_eq!(
            mouse_report(
                MouseTrackingMode::ButtonMotion,
                true,
                MouseEventKind::Motion { held: None },
                0,
                0,
            ),
            None
        );
    }

    #[test]
    fn mouse_report_wheel_up_uses_code_64() {
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, true, MouseEventKind::WheelUp, 0, 0),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
    }

    #[test]
    fn mouse_report_legacy_coordinate_clamps_to_limit() {
        // 1-based 223 is the last unclamped column: 223 + 32 = 255.
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, false, MouseEventKind::Press(MouseButton::Left), 222, 0),
            Some(vec![0x1b, b'[', b'M', 32, 255, 33])
        );
        // 1-based 224 and beyond clamp to 223, so the byte is 255 as well.
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, false, MouseEventKind::Press(MouseButton::Left), 223, 299),
            Some(vec![0x1b, b'[', b'M', 32, 255, 255])
        );
        // SGR has no such limit.
        assert_eq!(
            mouse_report(MouseTrackingMode::Press, true, MouseEventKind::Press(MouseButton::Left), 223, 0),
            Some(b"\x1b[<0;224;1M".to_vec())
        );
    }

    #[test]
    fn bracketed_paste_wraps_only_when_enabled() {
        assert_eq!(bracketed_paste_bytes(b"plain".to_vec(), false), b"plain".to_vec());
        assert_eq!(
            bracketed_paste_bytes(b"plain".to_vec(), true),
            b"\x1b[200~plain\x1b[201~".to_vec()
        );
    }

    #[test]
    fn focus_report_follows_enabled_flag() {
        assert_eq!(focus_report_sequence(false, true), None);
        assert_eq!(focus_report_sequence(false, false), None);
        assert_eq!(focus_report_sequence(true, true), Some(&b"\x1b[I"[..]));
        assert_eq!(focus_report_sequence(true, false), Some(&b"\x1b[O"[..]));
    }
}
