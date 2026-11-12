//! Keeps a host viewport that is scrolled into the scrollback on the same lines
//! while output keeps pushing new lines into the scrollback.

use crate::sync::SYNC_TIMEOUT;
use crate::term::Term;
use std::time::{Duration, Instant};

/// Longest redraw window after a scrollback clear. A redraw that outlasts a
/// synchronized-output hold reaches the screen in parts anyway.
const REDRAW_WINDOW: Duration = SYNC_TIMEOUT;

/// Output pause that ends a redraw: lines after it are new output, not the
/// rest of the reprint.
pub(crate) const REDRAW_QUIET: Duration = Duration::from_millis(50);

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
/// the host keeps its offset unclamped while [`Self::redrawing`] holds,
/// because the content is still short of the reprint. The window closes once
/// output has paused for `REDRAW_QUIET` (timed from the last `hold` that saw
/// the clear or reprinted lines), at `REDRAW_WINDOW` after the `hold` that saw
/// the clear, or on `release`. The window is timed by the `now` the host
/// passes, so it closes even when no `hold` runs; the first `hold` after it
/// closed counts its lines as new output, and the host clamps the offset to
/// the reprinted content as usual.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbackAnchor {
    /// `Term::output_scrolled_lines` at the last `hold`; `None` while released.
    seen_scrolled: Option<u64>,
    /// `Term::history_clears` at the last `hold`.
    seen_clears: u64,
    /// The redraw window of the latest clear, until a `hold` closes it.
    redraw: Option<RedrawWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RedrawWindow {
    /// When the `hold` that saw the clear ran.
    opened: Instant,
    /// When the last `hold` that saw the clear or reprinted lines ran.
    active: Instant,
}

impl RedrawWindow {
    fn open_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.active) < REDRAW_QUIET
            && now.saturating_duration_since(self.opened) < REDRAW_WINDOW
    }
}

impl ScrollbackAnchor {
    pub const fn new() -> Self {
        Self {
            seen_scrolled: None,
            seen_clears: 0,
            redraw: None,
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
            Some(_) if term.history_clears != self.seen_clears => {
                // Lines pushed before the clear in this frame are erased by it.
                self.redraw = Some(RedrawWindow { opened: now, active: now });
                0
            }
            Some(seen) => {
                let pushed = scrolled.wrapping_sub(seen);
                match self.redraw.as_mut() {
                    Some(window) if window.open_at(now) => {
                        // Lines in the window are most likely the reprint.
                        if pushed != 0 {
                            window.active = now;
                        }
                        0
                    }
                    _ => {
                        self.redraw = None;
                        usize::try_from(pushed).unwrap_or(usize::MAX)
                    }
                }
            }
        };
        self.seen_scrolled = Some(scrolled);
        self.seen_clears = term.history_clears;
        lines
    }

    /// The lines that the next `hold` at `now` would return, without
    /// consuming them. Renders use it to draw the offset that the next `hold`
    /// will apply. 0 while released or redrawing.
    pub fn pending(&self, term: &Term, now: Instant) -> usize {
        let Some(seen) = self.seen_scrolled else {
            return 0;
        };
        if self.redrawing(term, now) {
            return 0;
        }
        usize::try_from(term.output_scrolled_lines().wrapping_sub(seen)).unwrap_or(usize::MAX)
    }

    /// Whether a scrollback clear's redraw window is open at `now`, or a
    /// clear since the last `hold` is about to open one. The content is short
    /// of the reprint then, so the host must not clamp its offset to it.
    pub fn redrawing(&self, term: &Term, now: Instant) -> bool {
        self.seen_scrolled.is_some()
            && (term.history_clears != self.seen_clears
                || self.redraw.is_some_and(|window| window.open_at(now)))
    }

    /// The viewport is at the bottom and follows output again.
    pub fn release(&mut self) {
        self.seen_scrolled = None;
        self.redraw = None;
    }
}
