//! Replies to terminal queries: DA2, XTVERSION and DECRQM mode reports.

use crate::input::MouseTrackingMode;
use crate::term::Term;

impl Term {
    /// DA2 reply to `CSI > c`; the version is packed by [`pack_version`].
    pub(crate) fn queue_da2(&mut self) {
        let version = pack_version(self.identity.version);
        let msg = format!("\x1B[>1;{version};0c");
        self.queue_reply(msg.as_bytes());
    }

    /// XTVERSION reply to `CSI > q`: a DCS string naming the terminal.
    pub(crate) fn queue_xtversion(&mut self) {
        let msg = format!("\x1BP>|{}({})\x1B\\", self.identity.name, self.identity.version);
        self.queue_reply(msg.as_bytes());
    }

    /// DECRQM reply for a private mode, `CSI ? Ps ; Pm $ y`.
    pub(crate) fn queue_decrqm_private(&mut self, mode: u16) {
        let state = self.private_mode_state(mode);
        let msg = format!("\x1B[?{mode};{state}$y");
        self.queue_reply(msg.as_bytes());
    }

    /// DECRQM reply for an ANSI mode, `CSI Ps ; Pm $ y`.
    pub(crate) fn queue_decrqm_ansi(&mut self, mode: u16) {
        let state = match mode {
            4 => mode_state(self.insert_mode),
            _ => 0,
        };
        let msg = format!("\x1B[{mode};{state}$y");
        self.queue_reply(msg.as_bytes());
    }

    /// DECRQM state of a private mode: 1 set, 2 reset, 0 not recognized.
    fn private_mode_state(&self, mode: u16) -> u8 {
        let set = match mode {
            1 => self.app_cursor_keys,
            6 => self.origin_mode,
            7 => self.autowrap,
            25 => self.cursor_visible,
            47 | 1047 | 1049 => self.is_alt,
            1000 => matches!(self.mouse_tracking(), MouseTrackingMode::Press),
            1002 => matches!(self.mouse_tracking(), MouseTrackingMode::ButtonMotion),
            1003 => matches!(self.mouse_tracking(), MouseTrackingMode::AnyMotion),
            1004 => self.focus_reporting,
            1006 => self.mouse_sgr,
            2004 => self.bracketed_paste,
            // While the parser runs, 2026 is never on: a set stops the parse and
            // holds the rest, and replay runs with the mode off.
            2026 => false,
            _ => return 0,
        };
        mode_state(set)
    }
}

/// DECRQM state code for a recognized mode: 1 when set, 2 when reset.
fn mode_state(set: bool) -> u8 {
    if set { 1 } else { 2 }
}

/// Packs `major.minor.patch` as `major * 10000 + minor * 100 + patch`.
/// Any other shape, or an overflow, packs to 0.
fn pack_version(version: &str) -> u64 {
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return 0;
    };
    let (Ok(major), Ok(minor), Ok(patch)) = (
        major.parse::<u64>(),
        minor.parse::<u64>(),
        patch.parse::<u64>(),
    ) else {
        return 0;
    };
    major
        .checked_mul(10_000)
        .and_then(|value| value.checked_add(minor.checked_mul(100)?))
        .and_then(|value| value.checked_add(patch))
        .unwrap_or(0)
}
