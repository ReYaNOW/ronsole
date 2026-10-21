//! Synchronized output (mode 2026): output between `CSI ? 2026 h` and
//! `CSI ? 2026 l` is held back and replayed through the parser at once.

use crate::term::{Performer, Term, TermSink};
use std::time::{Duration, Instant};
use vte::Parser;

/// Set sequence; the parser stops right after it and holds the rest.
const SYNC_SET: &[u8] = b"\x1b[?2026h";
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
                // A reset may straddle the previous batch: search from the
                // last bytes already held.
                let from = self.sync_buf.len().saturating_sub(SYNC_RESET.len() - 1);
                self.sync_buf.extend_from_slice(&bytes[..take]);
                let reset_end = self.sync_buf[from..]
                    .windows(SYNC_RESET.len())
                    .position(|window| window == SYNC_RESET)
                    .map(|pos| from + pos + SYNC_RESET.len());
                if let Some(end) = reset_end {
                    // Bytes after the reset came from `bytes`; parse them normally.
                    let held_after = self.sync_buf.len() - end;
                    bytes = &bytes[take - held_after..];
                    grid_changed |= self.replay_sync(parser, end, sink);
                } else if self.sync_buf.len() >= SYNC_BUF_LIMIT {
                    bytes = &bytes[take..];
                    let end = self.sync_buf.len();
                    grid_changed |= self.replay_sync(parser, end, sink);
                } else {
                    return grid_changed;
                }
            } else {
                let (consumed, sync_set) = {
                    let mut performer = Performer {
                        term: self,
                        sink: &mut *sink,
                        sync_set: false,
                    };
                    let consumed = parser.advance_until_terminated(&mut performer, bytes);
                    (consumed, performer.sync_set)
                };
                if sync_set {
                    self.sync_until = Some(now + SYNC_TIMEOUT);
                }
                // The set sequence that starts holding is not a grid change.
                let set_len = if sync_set && bytes[..consumed].ends_with(SYNC_SET) {
                    SYNC_SET.len()
                } else {
                    0
                };
                grid_changed |= consumed > set_len;
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
