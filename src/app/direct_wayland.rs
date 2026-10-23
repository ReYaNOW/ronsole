use super::{App, AppLoopControl};
use crate::launch::TerminalLaunchSpec;
use crate::platform::single_instance::{InstanceMessage, PrimaryInstance};
use crate::runtime::{WaylandRuntimeEvents, WindowRuntime, poll_timeout_millis};
use crate::wayland_input::WaylandInputEvent;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Instant;

const EXTERNAL_LAUNCH_QUEUE_CAPACITY: usize = 32;

/// Outcome of one event-loop iteration: keep looping, or stop with the final result.
pub(super) enum LoopStep {
    Continue,
    Exit(Result<(), String>),
}

impl App {
    pub(crate) fn run_direct_wayland(
        &mut self,
        primary_instance: &mut PrimaryInstance,
        initial_launch: TerminalLaunchSpec,
    ) -> Result<(), String> {
        let result = self.run_direct_wayland_inner(primary_instance, initial_launch);
        if let Err(error) = result.as_ref() {
            self.interrupt_automation(&format!("Wayland runtime failure: {error}"));
        }
        self.on_exiting();
        result
    }

    fn run_direct_wayland_inner(
        &mut self,
        primary_instance: &mut PrimaryInstance,
        initial_launch: TerminalLaunchSpec,
    ) -> Result<(), String> {
        let runtime = WindowRuntime::bootstrap_wayland(
            self.config.window_width,
            self.config.window_height,
            self.config.terminal_font_size,
            self.config.terminal_background,
        )?;
        let wake = runtime.wake_handle();
        self.prepare_bootstrapped_runtime(runtime, initial_launch);

        let (launch_tx, launch_rx) = sync_channel(EXTERNAL_LAUNCH_QUEUE_CAPACITY);
        primary_instance
            .start_listener(move |message| {
                if launch_tx.try_send(message).is_err() {
                    return false;
                }
                wake.wake();
                true
            })
            .map_err(|error| format!("failed to start single-instance listener: {error}"))?;

        self.direct_wayland_loop(&launch_rx)
    }

    fn drain_external_launches(&mut self, receiver: &Receiver<InstanceMessage>) -> bool {
        let mut handled = false;
        while let Ok(message) = receiver.try_recv() {
            match message {
                InstanceMessage::ExternalLaunch(request) => self.handle_external_launch(request),
                InstanceMessage::TabInput(envelope) => self.handle_tab_input(envelope),
                InstanceMessage::BridgeRequest(envelope) => self.handle_bridge_request(envelope),
                InstanceMessage::TabStatus(envelope) => self.handle_tab_status(envelope),
            }
            handled = true;
        }
        handled
    }

    fn handle_wayland_runtime_events(&mut self, events: WaylandRuntimeEvents) -> bool {
        if events.close_requested {
            self.interrupt_automation("Wayland compositor requested window close");
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
                        self.interrupt_automation("keyboard path closed the final terminal");
                        self.prepare_exit();
                        return true;
                    }
                }
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
                        self.interrupt_automation("pointer path closed the final terminal");
                        self.prepare_exit();
                        return true;
                    }
                }
                WaylandInputEvent::Scroll(delta) => self.on_scroll(delta),
            }
        }
        false
    }

    /// Installs a freshly bootstrapped runtime and sizes the app to its physical
    /// target. Shared by the Wayland and headless entry points.
    pub(super) fn prepare_bootstrapped_runtime(
        &mut self,
        runtime: WindowRuntime,
        initial_launch: TerminalLaunchSpec,
    ) {
        let metrics = runtime.wayland_metrics();
        self.on_runtime_ready(
            runtime,
            metrics.physical_width,
            metrics.physical_height,
            initial_launch,
        );
        self.on_resize_with_logical_size(
            metrics.physical_width,
            metrics.physical_height,
            Some((
                f64::from(metrics.logical_width),
                f64::from(metrics.logical_height),
            )),
        );
    }

    /// Runs one event-loop iteration. `external_launches` is `None` for headless
    /// runs, which have no single-instance listener.
    pub(super) fn loop_step(
        &mut self,
        external_launches: Option<&Receiver<InstanceMessage>>,
    ) -> Result<LoopStep, String> {
        // A session error (for example a failed pbuffer resize) ends the run before
        // any further work. Wayland runtimes never report one.
        if let Some(error) = self.runtime.as_mut().and_then(WindowRuntime::take_session_error) {
            self.interrupt_automation(&error);
            self.prepare_exit();
            let automation_result = self.take_automation_exit_result();
            return Ok(LoopStep::Exit(automation_result.and(Err(error))));
        }

        if let Some(receiver) = external_launches {
            let _ = self.drain_external_launches(receiver);
        }

        let now = Instant::now();
        let pending_events = {
            let runtime = self.runtime.as_mut().ok_or_else(|| {
                "Wayland runtime disappeared from application state".to_string()
            })?;
            runtime.process_wayland_input_deadline(now);
            runtime.take_wayland_events()
        };
        if self.handle_wayland_runtime_events(pending_events) {
            return Ok(LoopStep::Exit(self.take_automation_exit_result()));
        }

        let control = self.on_about_to_wait();
        if control == AppLoopControl::Exit {
            return Ok(LoopStep::Exit(self.take_automation_exit_result()));
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
            return Ok(LoopStep::Continue);
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
            return Ok(LoopStep::Exit(self.take_automation_exit_result()));
        }

        if outcome.woke {
            if let Some(receiver) = external_launches {
                let _ = self.drain_external_launches(receiver);
            }
            // Timer-only wakes (synchronized-output deadlines) must not
            // spend a frame; only frame-requesting wakes do.
            let frame_wanted = self
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.wake_handle().take_frame_wake());
            if frame_wanted {
                self.request_frame();
            }
        }

        Ok(LoopStep::Continue)
    }

    fn direct_wayland_loop(
        &mut self,
        external_launches: &Receiver<InstanceMessage>,
    ) -> Result<(), String> {
        loop {
            if let LoopStep::Exit(result) = self.loop_step(Some(external_launches))? {
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::single_instance::ExternalLaunchRequest;

    #[test]
    fn rapid_external_launch_burst_preserves_requests_within_bound() {
        let (sender, receiver) =
            sync_channel::<InstanceMessage>(EXTERNAL_LAUNCH_QUEUE_CAPACITY);
        for index in 0..EXTERNAL_LAUNCH_QUEUE_CAPACITY {
            sender
                .try_send(InstanceMessage::ExternalLaunch(ExternalLaunchRequest {
                    activation_token: Some(format!("token-{index}")),
                    launch: TerminalLaunchSpec {
                        working_directory: Some(format!("/tmp/session-{index}").into()),
                        inherited_directory: None,
                        command: vec![format!("command-{index}").into()],
                        hold: index % 2 == 0,
                        bridge_launch_id: None,
                    },
                }))
                .unwrap();
        }
        for index in 0..EXTERNAL_LAUNCH_QUEUE_CAPACITY {
            let request = match receiver.try_recv().unwrap() {
                InstanceMessage::ExternalLaunch(request) => request,
                InstanceMessage::TabInput(_) | InstanceMessage::BridgeRequest(_) | InstanceMessage::TabStatus(_) => {
                    panic!("unexpected message")
                }
            };
            let expected_token = format!("token-{index}");
            let expected_command = std::ffi::OsString::from(format!("command-{index}"));
            let expected_workdir = std::path::PathBuf::from(format!("/tmp/session-{index}"));
            assert_eq!(
                request.activation_token.as_deref(),
                Some(expected_token.as_str())
            );
            assert_eq!(
                request.launch.working_directory.as_deref(),
                Some(expected_workdir.as_path())
            );
            assert_eq!(request.launch.command, vec![expected_command]);
            assert_eq!(request.launch.hold, index % 2 == 0);
        }
    }

    #[test]
    fn external_launch_handoff_is_explicitly_bounded() {
        use std::sync::mpsc::TrySendError;

        let (sender, _receiver) = sync_channel::<InstanceMessage>(2);
        assert!(sender.try_send(InstanceMessage::ExternalLaunch(ExternalLaunchRequest::default())).is_ok());
        assert!(sender.try_send(InstanceMessage::ExternalLaunch(ExternalLaunchRequest::default())).is_ok());
        assert!(matches!(
            sender.try_send(InstanceMessage::ExternalLaunch(ExternalLaunchRequest::default())),
            Err(TrySendError::Full(_))
        ));
    }
}
