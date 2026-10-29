use super::{App, AppLoopControl};
use crate::config::floating_logical_size;
use crate::launch::TerminalLaunchSpec;
use crate::platform::signals;
use crate::platform::single_instance::{InstanceMessage, PrimaryInstance};
use crate::runtime::{
    WaylandPollOutcome, WaylandRuntimeEvents, WaylandWindowMetrics, WindowRuntime,
    poll_timeout_millis,
};
use crate::wayland_input::WaylandInputEvent;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Instant;

const EXTERNAL_LAUNCH_QUEUE_CAPACITY: usize = 32;

/// Outcome of one event-loop iteration: keep looping with its report, or stop with the final result.
#[derive(Debug)]
pub(crate) enum LoopStep {
    Continue(StepReport),
    Exit(Result<(), String>),
}

/// What one event-loop iteration did. The window loop ignores it; the headless
/// protocol reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StepReport {
    /// The step drew a frame through `on_frame`.
    pub(crate) frame: bool,
    /// Flow the app asked for, taken from the step's `AppLoopControl`.
    pub(crate) flow: LoopFlow,
    /// Merged app and input deadline the step waited with, before any caller cap.
    pub(crate) deadline: Option<Instant>,
    /// Poll result; `None` when the step returned early with a ready frame.
    pub(crate) poll: Option<WaylandPollOutcome>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopFlow {
    Wait,
    Poll,
    WaitUntil,
}

/// Maps the app's control to a report flow. `Exit` returns before any report is built,
/// so it shares `Wait` only to keep the mapping total.
pub(super) fn loop_flow(control: AppLoopControl) -> LoopFlow {
    match control {
        AppLoopControl::Exit | AppLoopControl::Wait => LoopFlow::Wait,
        AppLoopControl::Poll => LoopFlow::Poll,
        AppLoopControl::WaitUntil(_) => LoopFlow::WaitUntil,
    }
}

/// Caps a poll timeout at `max_wait_ms`. `None` and negative caps leave it unchanged.
fn cap_timeout(timeout_ms: i32, max_wait_ms: Option<i32>) -> i32 {
    match max_wait_ms {
        Some(max) if max >= 0 => {
            if timeout_ms < 0 {
                max
            } else {
                timeout_ms.min(max)
            }
        }
        _ => timeout_ms,
    }
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
        // Installed before the first tab exists. Failure leaves the loop without
        // signal handling, reported once here.
        if let Err(error) = signals::install() {
            eprintln!("Ronsole: signal shutdown unavailable: {error}");
        }
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
        // Applied after the rest of the batch: a close that only opens the
        // confirmation dialog must not drop the resize or input queued with it.
        let close_requested = events.close_requested;

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
                persisted_logical_size(metrics),
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

        // Only a close that actually proceeds interrupts automation; a close
        // that opens the dialog leaves the run alone.
        if close_requested && self.on_close_requested() {
            self.interrupt_automation("Wayland compositor requested window close");
            return true;
        }
        false
    }

    /// Installs a freshly bootstrapped runtime and sizes the app to its physical
    /// target. Shared by the Wayland and headless entry points.
    pub(super) fn prepare_bootstrapped_runtime(
        &mut self,
        mut runtime: WindowRuntime,
        initial_launch: TerminalLaunchSpec,
    ) {
        // The renderer starts with default colours; apply the configured ones before its first frame.
        let _ = runtime.set_terminal_colors(&self.config);
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
            persisted_logical_size(metrics),
        );
    }

    /// Earliest of the app's own deadline and the input deadline; `None` waits for an event.
    pub(super) fn wait_deadline(&self, control: AppLoopControl) -> Option<Instant> {
        let app_deadline = match control {
            AppLoopControl::WaitUntil(deadline) => Some(deadline),
            AppLoopControl::Exit | AppLoopControl::Wait | AppLoopControl::Poll => None,
        };
        let input_deadline = self
            .runtime
            .as_ref()
            .and_then(WindowRuntime::wayland_input_deadline);
        match (app_deadline, input_deadline) {
            (Some(app), Some(input)) => Some(app.min(input)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    /// Runs one event-loop iteration. `external_launches` is `None` for headless
    /// runs, which have no single-instance listener. `max_wait_ms` caps the wait
    /// (`None` or `-1` leaves it to the app and input deadlines).
    pub(super) fn loop_step(
        &mut self,
        external_launches: Option<&Receiver<InstanceMessage>>,
        max_wait_ms: Option<i32>,
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
            let frame = take_frame && self.on_frame()?;
            return Ok(LoopStep::Continue(StepReport {
                frame,
                flow: loop_flow(control),
                deadline: None,
                poll: None,
            }));
        }

        let deadline = self.wait_deadline(control);
        let timeout_ms = cap_timeout(poll_timeout_millis(deadline, Instant::now()), max_wait_ms);
        let outcome = self
            .runtime
            .as_mut()
            .ok_or_else(|| "Wayland runtime disappeared from application state".to_string())?
            .poll_wayland(timeout_ms)?;
        // Signal shutdown skips the close-request path on purpose: it runs the
        // shared teardown directly and never asks for confirmation.
        if signals::take_shutdown_request() {
            self.interrupt_automation("terminated by signal");
            self.finish_terminal_session();
            return Ok(LoopStep::Exit(self.take_automation_exit_result()));
        }
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

        Ok(LoopStep::Continue(StepReport {
            frame: false,
            flow: loop_flow(control),
            deadline,
            poll: Some(outcome),
        }))
    }

    fn direct_wayland_loop(
        &mut self,
        external_launches: &Receiver<InstanceMessage>,
    ) -> Result<(), String> {
        loop {
            if let LoopStep::Exit(result) = self.loop_step(Some(external_launches), None)? {
                return result;
            }
        }
    }
}

/// Logical size that a runtime metrics report persists; only a floating toplevel yields one.
fn persisted_logical_size(metrics: WaylandWindowMetrics) -> Option<(f64, f64)> {
    floating_logical_size(
        metrics.floating,
        metrics.logical_width,
        metrics.logical_height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::single_instance::ExternalLaunchRequest;

    fn configured_metrics(logical: u32) -> WaylandWindowMetrics {
        WaylandWindowMetrics {
            logical_width: logical,
            logical_height: logical,
            physical_width: logical * 2,
            physical_height: logical * 2,
            scale_factor: 2.0,
            floating: true,
        }
    }

    fn toplevel_metrics(width: u32, height: u32, floating: bool) -> WaylandWindowMetrics {
        WaylandWindowMetrics {
            logical_width: width,
            logical_height: height,
            physical_width: width * 2,
            physical_height: height * 2,
            scale_factor: 2.0,
            floating,
        }
    }

    #[test]
    fn maximized_configure_does_not_persist_window_size() {
        assert_eq!(
            persisted_logical_size(toplevel_metrics(1000, 700, true)),
            Some((1000.0, 700.0))
        );
        assert_eq!(
            persisted_logical_size(toplevel_metrics(1920, 1080, false)),
            None
        );

        let mut app = crate::app::tests::busy_shell_app("maximized-size");
        let events = WaylandRuntimeEvents {
            configured: Some(toplevel_metrics(1000, 700, true)),
            ..Default::default()
        };
        app.handle_wayland_runtime_events(events);
        assert_eq!(app.config.window_width, 1000.0);
        assert_eq!(app.config.window_height, 700.0);

        // Maximized: the configured size is applied but not persisted.
        let events = WaylandRuntimeEvents {
            configured: Some(toplevel_metrics(1920, 1080, false)),
            ..Default::default()
        };
        app.handle_wayland_runtime_events(events);
        assert_eq!(app.config.window_width, 1000.0);
        assert_eq!(app.config.window_height, 700.0);

        // Un-maximize: the runtime reports the restored floating size (see runtime tests).
        let events = WaylandRuntimeEvents {
            configured: Some(toplevel_metrics(1000, 700, true)),
            ..Default::default()
        };
        app.handle_wayland_runtime_events(events);
        assert_eq!(app.config.window_width, 1000.0);
        assert_eq!(app.config.window_height, 700.0);
        app.shutdown_all();
    }

    #[test]
    fn close_in_batch_still_applies_resize_and_opens_the_dialog() {
        // A busy tab makes the close open the dialog instead of exiting.
        let mut app = crate::app::tests::busy_shell_app("batch-close");
        let events = WaylandRuntimeEvents {
            close_requested: true,
            configured: Some(configured_metrics(900)),
            ..Default::default()
        };
        assert!(!app.handle_wayland_runtime_events(events));
        assert!(app.close_confirm.is_some());
        assert_eq!(app.config.window_width, 900.0);

        // With the dialog open, a batch without a close still applies its resize.
        let events = WaylandRuntimeEvents {
            configured: Some(configured_metrics(1000)),
            ..Default::default()
        };
        assert!(!app.handle_wayland_runtime_events(events));
        assert!(app.close_confirm.is_some());
        assert_eq!(app.config.window_width, 1000.0);
        app.shutdown_all();
    }

    #[test]
    fn rapid_external_launch_burst_preserves_requests_within_bound() {
        let (sender, receiver) =
            sync_channel::<InstanceMessage>(EXTERNAL_LAUNCH_QUEUE_CAPACITY);
        for index in 0..EXTERNAL_LAUNCH_QUEUE_CAPACITY {
            sender
                .try_send(InstanceMessage::ExternalLaunch(ExternalLaunchRequest {
                    activation_token: Some(format!("token-{index}")),
                    launch: TerminalLaunchSpec {
                        shell: None,
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

    #[test]
    fn cap_timeout_follows_documented_contract() {
        // No cap, and a negative cap, both leave the timeout unchanged.
        assert_eq!(cap_timeout(100, None), 100);
        assert_eq!(cap_timeout(-1, None), -1);
        assert_eq!(cap_timeout(100, Some(-1)), 100);
        assert_eq!(cap_timeout(-1, Some(-1)), -1);
        // A non-negative cap replaces "no deadline" and limits longer waits only.
        assert_eq!(cap_timeout(-1, Some(50)), 50);
        assert_eq!(cap_timeout(10, Some(50)), 10);
        assert_eq!(cap_timeout(100, Some(50)), 50);
    }
}
