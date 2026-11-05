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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbackAnchor {
    /// `Term::scrollback_pushed` at the last `hold`; `None` while released.
    seen_pushed: Option<u64>,
}

impl ScrollbackAnchor {
    pub const fn new() -> Self {
        Self { seen_pushed: None }
    }

    /// Lines pushed into `term`'s scrollback since the previous `hold`, by
    /// which a scrolled-up offset (measured from the bottom) must grow to keep
    /// the same lines in view. The first call after `new` or `release` only
    /// records the count and returns 0.
    pub fn hold(&mut self, term: &Term) -> usize {
        let pushed = term.scrollback_pushed();
        let lines = self.seen_pushed.map_or(0, |seen| {
            usize::try_from(pushed.wrapping_sub(seen)).unwrap_or(usize::MAX)
        });
        self.seen_pushed = Some(pushed);
        lines
    }

    /// The viewport is at the bottom and follows output again.
    pub fn release(&mut self) {
        self.seen_pushed = None;
    }
}
