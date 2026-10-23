//! Headless run: bootstraps the offscreen runtime and drives the shared event loop
//! without a window, a Wayland connection, or a single-instance listener.

use super::App;
use super::direct_wayland::LoopStep;
use crate::launch::TerminalLaunchSpec;
use crate::runtime::WindowRuntime;

/// Why a headless run stopped with an error. `Egl` means the runtime never started,
/// so the caller reports it as a startup failure.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeadlessRunError {
    Egl(String),
    Run(String),
}

impl App {
    pub(crate) fn run_headless(
        &mut self,
        physical: (u32, u32),
        scale: f32,
        initial_launch: TerminalLaunchSpec,
    ) -> Result<(), HeadlessRunError> {
        let result = self.run_headless_inner(physical, scale, initial_launch);
        if let Err(HeadlessRunError::Run(error)) = result.as_ref() {
            self.interrupt_automation(&format!("headless runtime failure: {error}"));
        }
        self.on_exiting();
        result
    }

    fn run_headless_inner(
        &mut self,
        physical: (u32, u32),
        scale: f32,
        initial_launch: TerminalLaunchSpec,
    ) -> Result<(), HeadlessRunError> {
        let runtime = WindowRuntime::bootstrap_headless(
            physical.0,
            physical.1,
            scale,
            self.config.terminal_font_size,
            self.config.terminal_background,
        )
        .map_err(HeadlessRunError::Egl)?;
        self.prepare_bootstrapped_runtime(runtime, initial_launch);
        loop {
            match self.loop_step(None).map_err(HeadlessRunError::Run)? {
                LoopStep::Continue => {}
                LoopStep::Exit(result) => return result.map_err(HeadlessRunError::Run),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AutomationOptions;
    use crate::app::automation::AutomationController;
    use crate::headless::Profile;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Creates `<tmp>/ronsole-t4-<tag>-<pid>/workspace` with an empty fixture file
    /// (the name matches `FIXTURE_NAME` in the automation module).
    fn workspace_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ronsole-t4-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("test workspace");
        std::fs::write(workspace.join("terminal_fixture.sh"), "").expect("test fixture");
        root
    }

    fn options_for(root: &std::path::Path) -> AutomationOptions {
        AutomationOptions {
            workspace: root.join("workspace"),
            report: root.join("report.json"),
            timeout: Duration::from_secs(30),
        }
    }

    #[test]
    fn headless_session_error_fails_automation() {
        crate::prefer_egl_vendor();
        let root = workspace_root("session");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let mut app = App::load_headless(Some(options_for(&root)), &profile.dir)
            .expect("headless app");
        let mut runtime = WindowRuntime::bootstrap_headless(
            320,
            200,
            1.0,
            14.0,
            crate::config::DEFAULT_TERMINAL_BACKGROUND,
        )
        .expect("headless runtime");
        runtime.inject_session_error("resize pbuffer: test".to_string());
        app.prepare_bootstrapped_runtime(
            runtime,
            TerminalLaunchSpec::hermetic_shell_for_test(),
        );

        let step = app.loop_step(None).expect("loop step");
        assert!(matches!(step, LoopStep::Exit(Err(_))));
        let report = std::fs::read_to_string(root.join("report.json"))
            .expect("automation must write its report");
        assert!(report.contains("\"status\": \"failed\""), "{report}");
        assert!(report.contains("resize pbuffer: test"), "{report}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn headless_short_automation_reaches_ok() {
        crate::prefer_egl_vendor();
        let root = workspace_root("short");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let controller = AutomationController::with_short_scenario(options_for(&root))
            .expect("short automation scenario");
        let mut app = App::load_headless_with(Some(controller), &profile.dir)
            .expect("headless app");

        app.run_headless(
            (320, 200),
            1.0,
            TerminalLaunchSpec::hermetic_shell_for_test(),
        )
        .expect("short automation must finish");
        let report = std::fs::read_to_string(root.join("report.json"))
            .expect("automation must write its report");
        assert!(report.contains("\"status\": \"ok\""), "{report}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn headless_zero_size_maps_to_egl_error() {
        crate::prefer_egl_vendor();
        let root = workspace_root("zero");
        let profile = Profile::create(Some(root.clone()), std::process::id()).expect("profile");
        let mut app = App::load_headless(None, &profile.dir).expect("headless app");
        let error = app
            .run_headless((0, 0), 1.0, TerminalLaunchSpec::default())
            .expect_err("zero-size surface must not start");
        assert_eq!(
            error,
            HeadlessRunError::Egl("EGL setup failed at CreatePbuffer: zero size".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
