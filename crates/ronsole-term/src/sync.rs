//! Synchronized output (mode 2026): output between `CSI ? 2026 h` and
//! `CSI ? 2026 l` is held back and replayed through the parser at once.

use crate::term::{Performer, Term, TermSink};
use std::time::{Duration, Instant};
use vte::Parser;

/// Reset sequence; its end is the replay boundary.
const SYNC_RESET: &[u8] = b"\x1b[?2026l";
/// How long a set may hold the grid before the buffer is replayed.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Held bytes that force a replay even without a reset.
const SYNC_BUF_LIMIT: usize = 2 * 1024 * 1024;
/// Buffer capacity kept between synchronized updates.
const SYNC_BUF_KEEP: usize = 64 * 1024;

impl Term {
    /// Runs `bytes` through the terminal, holding output while synchronized
    /// output is on. Returns whether any byte was applied to the grid.
    pub(crate) fn apply_bytes(
        &mut self,
        parser: &mut Parser,
        mut bytes: &[u8],
        now: Instant,
        sink: &mut dyn TermSink,
    ) -> bool {
        let mut grid_changed = false;
        while !bytes.is_empty() {
            if self.sync_until.is_some() {
                // Hold bytes until a reset arrives or the buffer is full.
                let take = bytes.len().min(SYNC_BUF_LIMIT - self.sync_buf.len());
                // Search before copying, so each held byte is copied once even
                // when a batch holds many set/reset pairs.
                if let Some(end) = reset_end_in(&self.sync_buf, &bytes[..take]) {
                    self.sync_buf.extend_from_slice(&bytes[..end]);
                    let sync_end = self.sync_buf.len();
                    // Bytes after the reset are parsed normally.
                    bytes = &bytes[end..];
                    grid_changed |= self.replay_sync(parser, sync_end, sink);
                } else {
                    self.sync_buf.extend_from_slice(&bytes[..take]);
                    if self.sync_buf.len() < SYNC_BUF_LIMIT {
                        return grid_changed;
                    }
                    bytes = &bytes[take..];
                    let end = self.sync_buf.len();
                    grid_changed |= self.replay_sync(parser, end, sink);
                }
            } else {
                let (consumed, applied, sync_set) = {
                    let mut performer = Performer {
                        term: self,
                        sink: &mut *sink,
                        sync_set: false,
                        applied: false,
                    };
                    let consumed = parser.advance_until_terminated(&mut performer, bytes);
                    (consumed, performer.applied, performer.sync_set)
                };
                if sync_set {
                    self.sync_until = Some(now + SYNC_TIMEOUT);
                }
                grid_changed |= applied;
                bytes = &bytes[consumed..];
            }
        }
        grid_changed
    }

    /// Replays the first `end` held bytes with synchronized output off and
    /// keeps the buffer for reuse. Returns whether any byte was replayed.
    pub(crate) fn replay_sync(
        &mut self,
        parser: &mut Parser,
        end: usize,
        sink: &mut dyn TermSink,
    ) -> bool {
        let mut held = std::mem::take(&mut self.sync_buf);
        self.sync_until = None;
        self.replaying = true;
        {
            let mut performer = Performer {
                term: self,
                sink,
                sync_set: false,
                applied: false,
            };
            parser.advance(&mut performer, &held[..end]);
        }
        self.replaying = false;
        held.clear();
        if held.capacity() > SYNC_BUF_KEEP {
            held.shrink_to(SYNC_BUF_KEEP);
        }
        self.sync_buf = held;
        end > 0
    }
}

/// Finds the first reset that completes in `bytes`, given the already held
/// bytes `held`, which contain no complete reset. Returns how many bytes of
/// `bytes` belong to that reset, counting from the first one.
fn reset_end_in(held: &[u8], bytes: &[u8]) -> Option<usize> {
    // A reset straddling the boundary starts in `held` and ends in `bytes`.
    // Only the first byte of the reset is ESC, so at most one split matches.
    for split in 1..SYNC_RESET.len() {
        if held.ends_with(&SYNC_RESET[..split]) && bytes.starts_with(&SYNC_RESET[split..]) {
            return Some(SYNC_RESET.len() - split);
        }
    }
    bytes
        .windows(SYNC_RESET.len())
        .position(|window| window == SYNC_RESET)
        .map(|pos| pos + SYNC_RESET.len())
}
