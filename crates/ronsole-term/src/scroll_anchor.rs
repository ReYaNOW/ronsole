//! Keeps a host viewport that is scrolled into the scrollback on the same lines
//! while output keeps pushing new lines into the scrollback.

use crate::term::Term;

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
/// whole transcript (for example on a focus report); the reprint pushes the
/// history again, and counting it would move the viewport by the whole
/// history. Lines pushed in the `hold` window of a clear and the lines that
/// later refill the cleared history do not grow the offset, so the viewport
/// keeps its distance from the bottom across the redraw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbackAnchor {
    /// `Term::output_scrolled_lines` at the last `hold`; `None` while released.
    seen_scrolled: Option<u64>,
    /// `Term::history_clears` at the last `hold`.
    seen_clears: u64,
    /// Cleared history lines that output has not refilled by the last `hold`.
    refill_debt: u64,
}

impl ScrollbackAnchor {
    pub const fn new() -> Self {
        Self {
            seen_scrolled: None,
            seen_clears: 0,
            refill_debt: 0,
        }
    }

    /// Lines output scrolled into `term`'s history since the previous `hold`,
    /// by which a scrolled-up offset (measured from the bottom) must grow to
    /// keep the same lines in view. The first call after `new` or `release`
    /// only records the count and returns 0.
    pub fn hold(&mut self, term: &Term) -> usize {
        let (lines, debt) = self.pending_since_seen(term);
        self.seen_scrolled = Some(term.output_scrolled_lines());
        self.seen_clears = term.history_clears;
        self.refill_debt = debt;
        lines
    }

    /// The lines that the next `hold` would return, without consuming them.
    /// Renders use it to draw the offset that the next `hold` will apply.
    /// 0 while released.
    pub fn pending(&self, term: &Term) -> usize {
        self.pending_since_seen(term).0
    }

    /// The lines the offset grows by and the refill debt left after them.
    fn pending_since_seen(&self, term: &Term) -> (usize, u64) {
        let Some(seen) = self.seen_scrolled else {
            return (0, 0);
        };
        let scrolled = term.output_scrolled_lines();
        let (lines, debt) = if term.history_clears != self.seen_clears {
            // The lines pushed before the latest clear are gone with the
            // history; the ones pushed after it start refilling it.
            let refilled = scrolled.wrapping_sub(term.scrolled_at_history_clear);
            (0, term.history_cleared_lines.saturating_sub(refilled))
        } else {
            let pushed = scrolled.wrapping_sub(seen);
            (pushed.saturating_sub(self.refill_debt), self.refill_debt.saturating_sub(pushed))
        };
        (usize::try_from(lines).unwrap_or(usize::MAX), debt)
    }

    /// The viewport is at the bottom and follows output again.
    pub fn release(&mut self) {
        self.seen_scrolled = None;
        self.refill_debt = 0;
    }
}
