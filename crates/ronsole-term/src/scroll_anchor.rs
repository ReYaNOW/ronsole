//! Keeps a host viewport that is scrolled into the scrollback on the same lines
//! while output keeps pushing new lines into the scrollback.

use crate::sync::SYNC_TIMEOUT;
use crate::term::Term;
use std::time::{Duration, Instant};

/// Longest redraw window after a scrollback clear. A redraw that outlasts a
/// synchronized-output hold reaches the screen in parts anyway.
const REDRAW_WINDOW: Duration = SYNC_TIMEOUT;

/// Anchor of a viewport scrolled up into the scrollback.
///
/// Hosts measure their scroll offset from the bottom of the content, so every
/// line that output pushes into the scrollback moves the content under a fixed
/// offset. While the viewport is scrolled up the host calls [`Self::hold`] once
/// per frame (or output batch) and grows its offset by the returned number of
/// lines; at the bottom it calls [`Self::release`] and keeps following output.
///
/// When the scrollback is full, the pushed lines evict the oldest ones; the
/// host clamps the grown offset to its maximum, so a viewport on the oldest
/// retained line stays pinned to it while content is evicted under it.
///
/// A scrollback clear (`ED 3`, `RIS`) removes the anchored lines. Apps such as
/// Ink TUIs redraw by clearing the screen and the history and reprinting the
/// whole transcript (for example on a focus report); counting the reprint
/// would move the viewport by the whole history. A clear therefore opens a
/// redraw window: lines pushed while it is open do not grow the offset, and
/// the host keeps its offset unclamped ([`Self::redrawing`]) because the
/// content is still short of the reprint. The window closes at the first
/// `hold` that sees no new lines and no new clear after the reprint pushed
/// lines in an earlier `hold` of the window, at `REDRAW_WINDOW` after
/// the `hold` that saw the clear, or on `release`; the host then clamps the
/// offset to the reprinted content as usual.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbackAnchor {
    /// `Term::output_scrolled_lines` at the last `hold`; `None` while released.
    seen_scrolled: Option<u64>,
    /// `Term::history_clears` at the last `hold`.
    seen_clears: u64,
    /// When the `hold` that saw the latest clear ran, while its redraw
    /// window is open.
    redraw_since: Option<Instant>,
    /// The open redraw window has seen the reprint push lines; only then a
    /// quiet `hold` means the reprint is over.
    redraw_pushed: bool,
}

impl ScrollbackAnchor {
    pub const fn new() -> Self {
        Self {
            seen_scrolled: None,
            seen_clears: 0,
            redraw_since: None,
            redraw_pushed: false,
        }
    }

    /// Lines output scrolled into `term`'s history since the previous `hold`,
    /// by which a scrolled-up offset (measured from the bottom) must grow to
    /// keep the same lines in view; `now` times the redraw window of a
    /// scrollback clear. The first call after `new` or `release` only records
    /// the count and returns 0.
    pub fn hold(&mut self, term: &Term, now: Instant) -> usize {
        let scrolled = term.output_scrolled_lines();
        let lines = match self.seen_scrolled {
            None => 0,
            Some(seen) if term.history_clears != self.seen_clears => {
                self.redraw_since = Some(now);
                // Lines in the window of the clear are most likely its reprint.
                self.redraw_pushed = scrolled != seen;
                0
            }
            Some(seen) => {
                let pushed = scrolled.wrapping_sub(seen);
                match self.redraw_since {
                    Some(since) => {
                        // Lines up to the closing `hold` still belong to the redraw.
                        let quiet = pushed == 0 && self.redraw_pushed;
                        self.redraw_pushed |= pushed != 0;
                        if quiet || now.saturating_duration_since(since) >= REDRAW_WINDOW {
                            self.redraw_since = None;
                        }
                        0
                    }
                    None => usize::try_from(pushed).unwrap_or(usize::MAX),
                }
            }
        };
        self.seen_scrolled = Some(scrolled);
        self.seen_clears = term.history_clears;
        lines
    }

    /// The lines that the next `hold` would return, without consuming them.
    /// Renders use it to draw the offset that the next `hold` will apply.
    /// 0 while released or redrawing.
    pub fn pending(&self, term: &Term) -> usize {
        let Some(seen) = self.seen_scrolled else {
            return 0;
        };
        if self.redrawing(term) {
            return 0;
        }
        usize::try_from(term.output_scrolled_lines().wrapping_sub(seen)).unwrap_or(usize::MAX)
    }

    /// Whether a scrollback clear's redraw window is open, or a clear since
    /// the last `hold` is about to open one. The content is short of the
    /// reprint then, so the host must not clamp its offset to it.
    pub fn redrawing(&self, term: &Term) -> bool {
        self.seen_scrolled.is_some()
            && (self.redraw_since.is_some() || term.history_clears != self.seen_clears)
    }

    /// The viewport is at the bottom and follows output again.
    pub fn release(&mut self) {
        self.seen_scrolled = None;
        self.redraw_since = None;
        self.redraw_pushed = false;
    }
}
