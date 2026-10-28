//! Frame pacing, wake polling and size bookkeeping for the headless runtime target.
//!
//! This backend has no Wayland surface: frames are requested by the app, paced to
//! `HEADLESS_FRAME_PACE`, and the poll loop only waits on the wake eventfd.

use std::cell::Cell;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::input_types::CursorKind;
use crate::runtime::{WaylandPollOutcome, WaylandWindowMetrics};
use crate::wake::WakeHandle;

/// Minimum interval between two headless frames.
pub(crate) const HEADLESS_FRAME_PACE: Duration = Duration::from_millis(8);

/// Counters the headless session reads back to see what the runtime did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeadlessCounters {
    pub(crate) frames: u64,
    pub(crate) frame_requests: u64,
    pub(crate) redraw_requested: bool,
    pub(crate) cursor_kind: CursorKind,
}

pub(crate) struct HeadlessBackend {
    metrics: WaylandWindowMetrics,
    frame_requested: Cell<bool>,
    last_frame: Cell<Option<Instant>>,
    frames: u64,
    frame_requests: Cell<u64>,
    cursor_kind: Cell<CursorKind>,
    session_error: Option<String>,
}

impl HeadlessBackend {
    pub(crate) fn new(physical_width: u32, physical_height: u32, scale_factor: f32) -> Self {
        let mut backend = Self {
            metrics: WaylandWindowMetrics {
                logical_width: 0,
                logical_height: 0,
                physical_width,
                physical_height,
                scale_factor,
                floating: true,
            },
            frame_requested: Cell::new(false),
            last_frame: Cell::new(None),
            frames: 0,
            frame_requests: Cell::new(0),
            cursor_kind: Cell::new(CursorKind::default()),
            session_error: None,
        };
        backend.recompute_logical_size();
        backend
    }

    pub(crate) fn metrics(&self) -> WaylandWindowMetrics {
        self.metrics
    }

    pub(crate) fn set_physical_size(&mut self, physical_width: u32, physical_height: u32) {
        self.metrics.physical_width = physical_width;
        self.metrics.physical_height = physical_height;
        self.recompute_logical_size();
    }

    pub(crate) fn set_scale_factor(&mut self, scale_factor: f32) {
        self.metrics.scale_factor = scale_factor;
        self.recompute_logical_size();
    }

    fn recompute_logical_size(&mut self) {
        let scale = self.metrics.scale_factor;
        self.metrics.logical_width = logical_size(self.metrics.physical_width, scale);
        self.metrics.logical_height = logical_size(self.metrics.physical_height, scale);
    }

    pub(crate) fn request_frame(&self) {
        self.frame_requested.set(true);
        self.frame_requests
            .set(self.frame_requests.get().saturating_add(1));
    }

    /// Drops a pending frame request without drawing, used by the forced step.
    pub(crate) fn clear_frame_request(&self) {
        self.frame_requested.set(false);
    }

    /// True when a frame is requested and the pace interval since the last frame has elapsed.
    pub(crate) fn frame_ready(&self, now: Instant) -> bool {
        self.frame_requested.get() && self.pace_elapsed(now)
    }

    fn pace_elapsed(&self, now: Instant) -> bool {
        match self.last_frame.get() {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= HEADLESS_FRAME_PACE,
        }
    }

    /// Consumes the request when a frame is due; a request that is not yet due stays pending.
    pub(crate) fn take_frame_ready(&self, now: Instant) -> bool {
        if !self.frame_ready(now) {
            return false;
        }
        self.frame_requested.set(false);
        true
    }

    /// Caps the poll timeout so a requested frame wakes the loop when its pace expires.
    pub(crate) fn poll_timeout(&self, timeout_ms: i32, now: Instant) -> i32 {
        if !self.frame_requested.get() {
            return timeout_ms;
        }
        if self.frame_ready(now) {
            return 0;
        }
        let Some(last) = self.last_frame.get() else {
            return 0;
        };
        let remaining = HEADLESS_FRAME_PACE.saturating_sub(now.saturating_duration_since(last));
        let remaining_ms = i32::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
        if timeout_ms < 0 {
            remaining_ms
        } else {
            timeout_ms.min(remaining_ms)
        }
    }

    /// Records a presented frame; the next frame is paced from `now`.
    pub(crate) fn finish_frame(&mut self, now: Instant) {
        self.last_frame.set(Some(now));
        self.frames += 1;
    }

    /// Waits on the wake eventfd only, for at most `timeout_ms` (`-1` waits without a deadline).
    pub(crate) fn poll(
        &self,
        wake: &WakeHandle,
        timeout_ms: i32,
    ) -> Result<WaylandPollOutcome, String> {
        let mut fd = libc::pollfd {
            fd: wake.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
        if poll_result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(WaylandPollOutcome::default());
            }
            return Err(format!("wake poll failed: {error}"));
        }
        if poll_result == 0 {
            return Ok(WaylandPollOutcome {
                woke: false,
                timed_out: true,
            });
        }
        if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err("wake eventfd became unavailable".to_string());
        }
        wake.drain()
            .map_err(|error| format!("draining wake eventfd failed: {error}"))?;
        Ok(WaylandPollOutcome {
            woke: true,
            timed_out: false,
        })
    }

    /// Keeps the first error: later failures do not hide the one that was reported first.
    pub(crate) fn set_session_error(&mut self, error: String) {
        if self.session_error.is_none() {
            self.session_error = Some(error);
        }
    }

    pub(crate) fn take_session_error(&mut self) -> Option<String> {
        self.session_error.take()
    }

    pub(crate) fn set_cursor_kind(&self, kind: CursorKind) {
        self.cursor_kind.set(kind);
    }

    pub(crate) fn counters(&self) -> HeadlessCounters {
        HeadlessCounters {
            frames: self.frames,
            frame_requests: self.frame_requests.get(),
            redraw_requested: self.frame_requested.get(),
            cursor_kind: self.cursor_kind.get(),
        }
    }
}

/// Converts a physical pbuffer dimension to logical units, never below one.
fn logical_size(physical: u32, scale_factor: f32) -> u32 {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        f64::from(scale_factor)
    } else {
        1.0
    };
    (f64::from(physical) / scale)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ready_respects_pace() {
        let mut backend = HeadlessBackend::new(320, 200, 1.0);
        let start = Instant::now();
        backend.request_frame();
        assert!(backend.frame_ready(start), "no previous frame means no pace limit");
        assert!(backend.take_frame_ready(start));
        backend.finish_frame(start);

        backend.request_frame();
        assert!(!backend.frame_ready(start), "request right after a frame must wait");
        assert!(!backend.frame_ready(start + Duration::from_millis(7)));
        assert!(backend.frame_ready(start + HEADLESS_FRAME_PACE));
    }

    #[test]
    fn poll_timeout_caps_infinite_wait_at_pace() {
        let mut backend = HeadlessBackend::new(320, 200, 1.0);
        let start = Instant::now();
        backend.finish_frame(start);
        let now = start + Duration::from_millis(3);

        assert_eq!(backend.poll_timeout(-1, now), -1, "no request keeps the caller's wait");
        backend.request_frame();
        assert_eq!(backend.poll_timeout(-1, now), 5);
        assert_eq!(backend.poll_timeout(2, now), 2);
    }

    #[test]
    fn logical_size_divides_physical_by_scale() {
        assert_eq!(logical_size(640, 2.0), 320);
        assert_eq!(logical_size(0, 1.0), 1);
        assert_eq!(logical_size(100, f32::NAN), 100);
    }
}
