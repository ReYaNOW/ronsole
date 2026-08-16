use super::{App, AppLoopControl};
use crate::platform::single_instance::{ExternalLaunchRequest, PrimaryInstance};
use crate::runtime::{WaylandRuntimeEvents, WindowRuntime, poll_timeout_millis};
use crate::wayland_input::WaylandInputEvent;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Instant;

const EXTERNAL_LAUNCH_QUEUE_CAPACITY: usize = 32;

impl App {
    pub(crate) fn run_direct_wayland(
        &mut self,
        primary_instance: &mut PrimaryInstance,
    ) -> Result<(), String> {
        let result = self.run_direct_wayland_inner(primary_instance);
        self.on_exiting();
        result
    }

    fn run_direct_wayland_inner(
        &mut self,
        primary_instance: &mut PrimaryInstance,
    ) -> Result<(), String> {
        let runtime = WindowRuntime::bootstrap_wayland(
            self.config.window_width,
            self.config.window_height,
            self.config.terminal_font_size,
            self.config.terminal_background,
        )?;
        let metrics = runtime.wayland_metrics();
        let wake = runtime.wake_handle();
        self.on_runtime_ready(runtime, metrics.physical_width, metrics.physical_height);
        self.on_resize_with_logical_size(
            metrics.physical_width,
            metrics.physical_height,
            Some((
                f64::from(metrics.logical_width),
                f64::from(metrics.logical_height),
            )),
        );

        let (launch_tx, launch_rx) = sync_channel(EXTERNAL_LAUNCH_QUEUE_CAPACITY);
        primary_instance
            .start_listener(move |request| {
                if launch_tx.send(request).is_err() {
                    return false;
                }
                wake.wake();
                true
            })
            .map_err(|error| format!("failed to start single-instance listener: {error}"))?;

        self.direct_wayland_loop(&launch_rx)
    }

    fn drain_external_launches(&mut self, receiver: &Receiver<ExternalLaunchRequest>) -> bool {
        let mut handled = false;
        while let Ok(request) = receiver.try_recv() {
            self.handle_external_launch(request);
            handled = true;
        }
        handled
    }

    fn handle_wayland_runtime_events(&mut self, events: WaylandRuntimeEvents) -> bool {
        if events.close_requested {
            self.on_close_requested();
            return true;
        }

        if let Some(metrics) = events.scale_changed {
            self.on_scale_changed(
                metrics.scale_factor,
                metrics.physical_width,
                metrics.physical_height,
            );
        }

        if let Some(metrics) = events.configured {
            self.on_resize_with_logical_size(
                metrics.physical_width,
                metrics.physical_height,
                Some((
                    f64::from(metrics.logical_width),
                    f64::from(metrics.logical_height),
                )),
            );
        }

        for event in events.input {
            match event {
                WaylandInputEvent::Focus(focused) => self.on_focus_changed(focused),
                WaylandInputEvent::Modifiers(modifiers) => self.on_modifiers(modifiers),
                WaylandInputEvent::Key(input) => {
                    if self.on_key(input) {
                        self.prepare_exit();
                        return true;
                    }
                }
                WaylandInputEvent::Text(text) => self.on_text(&text),
                WaylandInputEvent::ImeCommit(text) => self.on_ime_commit(&text),
                WaylandInputEvent::PointerMotion(position) => self.on_pointer_motion(position),
                WaylandInputEvent::PointerLeave => {
                    if let Some(metrics) = self.runtime.as_ref().map(WindowRuntime::wayland_metrics)
                    {
                        self.on_pointer_leave(metrics.physical_width, metrics.physical_height);
                    }
                }
                WaylandInputEvent::PointerButton(state, button) => {
                    if self.on_pointer_button(state, button) {
                        self.prepare_exit();
                        return true;
                    }
                }
                WaylandInputEvent::Scroll(delta) => self.on_scroll(delta),
            }
        }
        false
    }

    fn direct_wayland_loop(
        &mut self,
        external_launches: &Receiver<ExternalLaunchRequest>,
    ) -> Result<(), String> {
        loop {
            let _ = self.drain_external_launches(external_launches);

            let now = Instant::now();
            let pending_events = {
                let runtime = self.runtime.as_mut().ok_or_else(|| {
                    "Wayland runtime disappeared from application state".to_string()
                })?;
                runtime.process_wayland_input_deadline(now);
                runtime.take_wayland_events()
            };
            if self.handle_wayland_runtime_events(pending_events) {
                return Ok(());
            }

            let control = self.on_about_to_wait();
            if control == AppLoopControl::Exit {
                return Ok(());
            }

            let frame_ready = self
                .runtime
                .as_ref()
                .is_some_and(WindowRuntime::wayland_frame_ready_requested);
            if frame_ready {
                let take_frame = self
                    .runtime
                    .as_ref()
                    .is_some_and(WindowRuntime::take_wayland_frame_ready);
                if take_frame {
                    self.on_frame()?;
                }
                continue;
            }

            let app_deadline = match control {
                AppLoopControl::WaitUntil(deadline) => Some(deadline),
                AppLoopControl::Exit | AppLoopControl::Wait | AppLoopControl::Poll => None,
            };
            let input_deadline = self
                .runtime
                .as_ref()
                .and_then(WindowRuntime::wayland_input_deadline);
            let deadline = match (app_deadline, input_deadline) {
                (Some(app), Some(input)) => Some(app.min(input)),
                (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
                (None, None) => None,
            };
            let timeout_ms = poll_timeout_millis(deadline, Instant::now());
            let outcome = self
                .runtime
                .as_mut()
                .ok_or_else(|| "Wayland runtime disappeared from application state".to_string())?
                .poll_wayland(timeout_ms)?;
            let events = self
                .runtime
                .as_mut()
                .ok_or_else(|| "Wayland runtime disappeared from application state".to_string())?
                .take_wayland_events();

            if self.handle_wayland_runtime_events(events) {
                return Ok(());
            }

            if outcome.woke {
                let _ = self.drain_external_launches(external_launches);
                self.request_frame();
            }

            if outcome.timed_out {
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_external_launch_burst_preserves_requests_within_bound() {
        let (sender, receiver) =
            sync_channel::<ExternalLaunchRequest>(EXTERNAL_LAUNCH_QUEUE_CAPACITY);
        for index in 0..EXTERNAL_LAUNCH_QUEUE_CAPACITY {
            sender
                .try_send(ExternalLaunchRequest {
                    activation_token: Some(format!("token-{index}")),
                })
                .unwrap();
        }
        for index in 0..EXTERNAL_LAUNCH_QUEUE_CAPACITY {
            let request = receiver.try_recv().unwrap();
            let expected = format!("token-{index}");
            assert_eq!(request.activation_token.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn external_launch_handoff_is_explicitly_bounded() {
        use std::sync::mpsc::TrySendError;

        let (sender, _receiver) = sync_channel::<ExternalLaunchRequest>(2);
        assert!(sender.try_send(ExternalLaunchRequest::default()).is_ok());
        assert!(sender.try_send(ExternalLaunchRequest::default()).is_ok());
        assert!(matches!(
            sender.try_send(ExternalLaunchRequest::default()),
            Err(TrySendError::Full(_))
        ));
    }
}
