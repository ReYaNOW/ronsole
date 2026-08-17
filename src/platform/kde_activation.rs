use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const KWIN_SERVICE: &str = "org.kde.KWin";
const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const COMMAND_TIMEOUT: Duration = Duration::from_millis(750);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

static SCRIPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KdeActivationRequest {
    Enqueued,
    Coalesced,
    Unavailable,
}

pub(crate) struct KdeActivationWorker {
    sender: Option<SyncSender<u32>>,
    worker: Option<JoinHandle<()>>,
    queued_or_running: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    diagnostic_emitted: Arc<AtomicBool>,
}

impl KdeActivationWorker {
    pub(crate) fn new() -> Self {
        Self::with_runner(activate_primary_window)
    }

    fn with_runner(mut runner: impl FnMut(u32) -> bool + Send + 'static) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<u32>(1);
        let queued_or_running = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let diagnostic_emitted = Arc::new(AtomicBool::new(false));
        let worker_queued = Arc::clone(&queued_or_running);
        let worker_stop = Arc::clone(&stop);
        let worker_diagnostic = Arc::clone(&diagnostic_emitted);
        let worker = super::spawn_named("ronsole-kde-activation", move || {
            while let Ok(pid) = receiver.recv() {
                if worker_stop.load(Ordering::Acquire) {
                    worker_queued.store(false, Ordering::Release);
                    break;
                }
                let activated = runner(pid);
                worker_queued.store(false, Ordering::Release);
                if !activated {
                    report_activation_failure_once(
                        &worker_diagnostic,
                        "Ronsole: KDE activation fallback failed",
                    );
                }
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
            }
        })
        .ok();

        Self {
            sender: worker.as_ref().map(|_| sender),
            worker,
            queued_or_running,
            stop,
            diagnostic_emitted,
        }
    }

    pub(crate) fn try_activate(&self, pid: u32) {
        let _ = self.try_activate_result(pid);
    }

    fn try_activate_result(&self, pid: u32) -> KdeActivationRequest {
        let Some(sender) = self.sender.as_ref() else {
            report_activation_failure_once(
                &self.diagnostic_emitted,
                "Ronsole: KDE activation fallback worker is unavailable",
            );
            return KdeActivationRequest::Unavailable;
        };
        if self.queued_or_running.swap(true, Ordering::AcqRel) {
            return KdeActivationRequest::Coalesced;
        }
        match sender.try_send(pid) {
            Ok(()) => KdeActivationRequest::Enqueued,
            Err(TrySendError::Full(_)) => KdeActivationRequest::Coalesced,
            Err(TrySendError::Disconnected(_)) => {
                self.queued_or_running.store(false, Ordering::Release);
                report_activation_failure_once(
                    &self.diagnostic_emitted,
                    "Ronsole: KDE activation fallback worker disconnected",
                );
                KdeActivationRequest::Unavailable
            }
        }
    }

    pub(crate) fn shutdown_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.queued_or_running.store(false, Ordering::Release);
    }
}

impl Drop for KdeActivationWorker {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

fn report_activation_failure_once(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::AcqRel) {
        eprintln!("{message}");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandSpec {
    program: PathBuf,
    args: Vec<OsString>,
    timeout: Duration,
}

impl CommandSpec {
    fn qdbus(program: &Path, args: impl IntoIterator<Item = OsString>) -> Self {
        Self {
            program: program.to_path_buf(),
            args: args.into_iter().collect(),
            timeout: COMMAND_TIMEOUT,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CommandOutput {
    success: bool,
    stdout: Vec<u8>,
}

struct ActivationScript {
    path: PathBuf,
}

impl ActivationScript {
    fn create(runtime_dir: &Path, pid: u32, sequence: u64) -> io::Result<Self> {
        let path = runtime_dir.join(format!("ronsole-kwin-activate-{pid}-{sequence}.js"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(activation_script(pid).as_bytes())?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { path })
    }
}

impl Drop for ActivationScript {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn activate_primary_window(pid: u32) -> bool {
    if !kde_session_active() {
        return false;
    }
    let Some(qdbus) = find_qdbus_with(|name| std::env::var_os(name)) else {
        return false;
    };
    let Some(runtime_dir) = private_xdg_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR")) else {
        return false;
    };

    activate_primary_window_with(pid, &qdbus, &runtime_dir, run_command)
}

fn activate_primary_window_with(
    pid: u32,
    qdbus: &Path,
    runtime_dir: &Path,
    mut runner: impl FnMut(&CommandSpec) -> io::Result<CommandOutput>,
) -> bool {
    let sequence = SCRIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let script = match ActivationScript::create(runtime_dir, pid, sequence) {
        Ok(script) => script,
        Err(_) => return false,
    };
    let plugin_name = format!("ronsole-activate-{pid}-{sequence}");

    let load = qdbus_load_script(qdbus, &script.path, &plugin_name);
    let load_output = match runner(&load) {
        Ok(output) if output.success => output,
        _ => return false,
    };
    let script_id = match parse_script_id(&load_output.stdout) {
        Some(id) => id,
        None => {
            let _ = runner(&qdbus_unload_script(qdbus, &plugin_name));
            return false;
        }
    };

    let run_ok = runner(&qdbus_run_script(qdbus, script_id))
        .map(|output| output.success)
        .unwrap_or(false);
    let _ = runner(&qdbus_unload_script(qdbus, &plugin_name));
    run_ok
}

fn activation_script(pid: u32) -> String {
    format!(
        r#"(function() {{
    const targetPid = {pid};
    const windows = workspace.stackingOrder;
    let target = null;

    for (let i = windows.length - 1; i >= 0; --i) {{
        const candidate = windows[i];
        if (candidate && !candidate.deleted && candidate.pid === targetPid) {{
            target = candidate;
            break;
        }}
    }}

    if (!target) {{
        return;
    }}

    if (target.minimized) {{
        target.minimized = false;
    }}

    if (target.desktops && target.desktops.length > 0) {{
        const targetDesktop = target.desktops[0];
        if (target.output && typeof workspace.setCurrentDesktopForScreen === "function") {{
            workspace.setCurrentDesktopForScreen(targetDesktop, target.output);
        }} else {{
            workspace.currentDesktop = targetDesktop;
        }}
    }}

    target.demandsAttention = false;
    workspace.activeWindow = target;
    workspace.raiseWindow(target);
}})();
"#
    )
}

fn qdbus_load_script(qdbus: &Path, script_path: &Path, plugin_name: &str) -> CommandSpec {
    CommandSpec::qdbus(
        qdbus,
        [
            OsString::from(KWIN_SERVICE),
            OsString::from(KWIN_SCRIPTING_PATH),
            OsString::from("loadScript"),
            script_path.as_os_str().to_os_string(),
            OsString::from(plugin_name),
        ],
    )
}

fn qdbus_run_script(qdbus: &Path, script_id: i32) -> CommandSpec {
    CommandSpec::qdbus(
        qdbus,
        [
            OsString::from(KWIN_SERVICE),
            OsString::from(format!("/Scripting/Script{script_id}")),
            OsString::from("run"),
        ],
    )
}

fn qdbus_unload_script(qdbus: &Path, plugin_name: &str) -> CommandSpec {
    CommandSpec::qdbus(
        qdbus,
        [
            OsString::from(KWIN_SERVICE),
            OsString::from(KWIN_SCRIPTING_PATH),
            OsString::from("unloadScript"),
            OsString::from(plugin_name),
        ],
    )
}

fn parse_script_id(stdout: &[u8]) -> Option<i32> {
    let value = std::str::from_utf8(stdout)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    (value >= 0).then_some(value)
}

pub(crate) fn kde_session_active() -> bool {
    is_kde_session_with(|name| std::env::var_os(name))
}

fn is_kde_session_with(mut env_value: impl FnMut(&str) -> Option<OsString>) -> bool {
    if env_value("KDE_FULL_SESSION").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    env_value("XDG_CURRENT_DESKTOP")
        .and_then(|value| value.into_string().ok())
        .is_some_and(|value| {
            value
                .split(':')
                .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
        })
}

fn find_qdbus_with(mut env_value: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    let path = env_value("PATH")?;
    for name in ["qdbus6", "qdbus"] {
        if let Some(executable) = find_executable_in_path(name, &path) {
            return Some(executable);
        }
    }
    None
}

fn find_executable_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(name))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn private_xdg_runtime_dir(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    if path.as_os_str().is_empty() {
        return None;
    }
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return None;
    }
    Some(path)
}

fn run_command(spec: &CommandSpec) -> io::Result<CommandOutput> {
    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_end(&mut stdout)?;
            }
            return Ok(CommandOutput {
                success: status.success(),
                stdout,
            });
        }
        if started.elapsed() >= spec.timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "KWin activation command timed out",
            ));
        }
        thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs::File;
    use std::sync::atomic::AtomicU64;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ronsole-kde-activation-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn output(success: bool, stdout: &str) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success,
            stdout: stdout.as_bytes().to_vec(),
        })
    }

    #[test]
    fn kde_session_detection_accepts_plasma_markers_only() {
        assert!(is_kde_session_with(|name| match name {
            "XDG_CURRENT_DESKTOP" => Some(OsString::from("KDE")),
            _ => None,
        }));
        assert!(is_kde_session_with(|name| match name {
            "XDG_CURRENT_DESKTOP" => Some(OsString::from("GNOME:KDE")),
            _ => None,
        }));
        assert!(is_kde_session_with(|name| match name {
            "KDE_FULL_SESSION" => Some(OsString::from("true")),
            _ => None,
        }));
        assert!(!is_kde_session_with(|name| match name {
            "XDG_CURRENT_DESKTOP" => Some(OsString::from("GNOME")),
            _ => None,
        }));
    }

    #[test]
    fn activation_script_targets_primary_pid_and_restores_focus_stack_and_desktop() {
        let script = activation_script(4242);
        assert!(script.contains("const targetPid = 4242;"));
        assert!(script.contains("candidate.pid === targetPid"));
        assert!(script.contains("target.minimized = false;"));
        assert!(script.contains("target.desktops.length > 0"));
        assert!(
            script.contains("workspace.setCurrentDesktopForScreen(targetDesktop, target.output);")
        );
        assert!(script.contains("workspace.currentDesktop = targetDesktop;"));
        assert!(script.contains("target.demandsAttention = false;"));
        assert!(script.contains("workspace.activeWindow = target;"));
        assert!(script.contains("workspace.raiseWindow(target);"));
        assert!(!script.contains("caption"));
        assert!(!script.contains("title"));
    }

    #[test]
    fn qdbus_plan_loads_runs_and_unloads_one_private_script() {
        let temp = TestDir::new();
        let qdbus = Path::new("/usr/bin/qdbus6");
        let mut calls = Vec::new();
        let mut replies =
            VecDeque::from([output(true, "17\n"), output(true, ""), output(true, "")]);

        let activated = activate_primary_window_with(31337, qdbus, &temp.path, |spec| {
            calls.push(spec.clone());
            replies.pop_front().unwrap()
        });

        assert!(activated);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].program, qdbus);
        assert_eq!(calls[0].args[0], KWIN_SERVICE);
        assert_eq!(calls[0].args[1], KWIN_SCRIPTING_PATH);
        assert_eq!(calls[0].args[2], "loadScript");
        assert!(
            calls[0].args[3]
                .to_string_lossy()
                .contains("ronsole-kwin-activate-31337-")
        );
        assert!(
            calls[0].args[4]
                .to_string_lossy()
                .starts_with("ronsole-activate-31337-")
        );
        assert_eq!(calls[1].args, [KWIN_SERVICE, "/Scripting/Script17", "run"]);
        assert!(calls.iter().all(|call| call.timeout == COMMAND_TIMEOUT));
        assert_eq!(calls[2].args[2], "unloadScript");
        assert_eq!(calls[2].args[3], calls[0].args[4]);
        assert!(fs::read_dir(&temp.path).unwrap().next().is_none());
    }

    #[test]
    fn failed_or_malformed_kwin_calls_fall_back_without_leaking_script() {
        for first_reply in [output(false, ""), output(true, "not-an-id\n")] {
            let temp = TestDir::new();
            let mut replies = VecDeque::from([first_reply, output(true, "")]);
            let activated =
                activate_primary_window_with(7, Path::new("/usr/bin/qdbus6"), &temp.path, |_| {
                    replies.pop_front().unwrap()
                });
            assert!(!activated);
            assert!(fs::read_dir(&temp.path).unwrap().next().is_none());
        }
    }

    #[test]
    fn run_failure_still_unloads_and_cleans_up_script() {
        let temp = TestDir::new();
        let mut calls = Vec::new();
        let mut replies =
            VecDeque::from([output(true, "3\n"), output(false, ""), output(true, "")]);
        let activated =
            activate_primary_window_with(8, Path::new("/usr/bin/qdbus6"), &temp.path, |spec| {
                calls.push(spec.clone());
                replies.pop_front().unwrap()
            });
        assert!(!activated);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].args[2], "unloadScript");
        assert!(fs::read_dir(&temp.path).unwrap().next().is_none());
    }

    #[test]
    fn runner_timeout_is_bounded_unloads_loaded_script_and_cleans_up() {
        let temp = TestDir::new();
        let mut calls = Vec::new();
        let mut replies = VecDeque::from([
            output(true, "9\n"),
            Err(io::Error::new(io::ErrorKind::TimedOut, "simulated timeout")),
            output(true, ""),
        ]);
        let activated =
            activate_primary_window_with(9, Path::new("/usr/bin/qdbus6"), &temp.path, |spec| {
                calls.push(spec.clone());
                replies.pop_front().unwrap()
            });
        assert!(!activated);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].timeout, COMMAND_TIMEOUT);
        assert_eq!(calls[2].args[2], "unloadScript");
        assert!(fs::read_dir(&temp.path).unwrap().next().is_none());
    }

    #[test]
    fn qdbus_lookup_prefers_qt6_and_ignores_non_executable_files() {
        let temp = TestDir::new();
        let qdbus = temp.path.join("qdbus");
        let qdbus6 = temp.path.join("qdbus6");
        File::create(&qdbus).unwrap();
        fs::set_permissions(&qdbus, fs::Permissions::from_mode(0o755)).unwrap();
        File::create(&qdbus6).unwrap();
        fs::set_permissions(&qdbus6, fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths([&temp.path]).unwrap();

        assert_eq!(
            find_qdbus_with(|name| (name == "PATH").then(|| path.clone())),
            Some(qdbus)
        );

        fs::set_permissions(&qdbus6, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            find_qdbus_with(|name| (name == "PATH").then(|| path.clone())),
            Some(qdbus6)
        );
    }

    #[test]
    fn private_runtime_dir_rejects_missing_non_directory_and_group_accessible_paths() {
        assert!(private_xdg_runtime_dir(None).is_none());

        let temp = TestDir::new();
        let file = temp.path.join("file");
        File::create(&file).unwrap();
        assert!(private_xdg_runtime_dir(Some(file.into_os_string())).is_none());

        fs::set_permissions(&temp.path, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(private_xdg_runtime_dir(Some(temp.path.clone().into_os_string())).is_none());

        fs::set_permissions(&temp.path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            private_xdg_runtime_dir(Some(temp.path.clone().into_os_string())),
            Some(temp.path.clone())
        );
    }

    #[test]
    fn script_id_parser_rejects_negative_and_non_numeric_replies() {
        assert_eq!(parse_script_id(b"0\n"), Some(0));
        assert_eq!(parse_script_id(b"42"), Some(42));
        assert_eq!(parse_script_id(b"-1\n"), None);
        assert_eq!(parse_script_id(b"i 4\n"), None);
        assert_eq!(parse_script_id(b""), None);
    }

    #[test]
    fn worker_runs_activation_off_caller_thread_and_coalesces_rapid_requests() {
        use std::sync::{Condvar, Mutex};

        let caller = thread::current().id();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = Arc::clone(&release);
        let mut worker = KdeActivationWorker::with_runner(move |pid| {
            let _ = started_tx.send((pid, thread::current().id()));
            let (lock, wake) = &*worker_release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            true
        });

        assert_eq!(
            worker.try_activate_result(4242),
            KdeActivationRequest::Enqueued
        );
        let (pid, worker_thread) = started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(pid, 4242);
        assert_ne!(worker_thread, caller);
        for _ in 0..16 {
            assert_eq!(
                worker.try_activate_result(4242),
                KdeActivationRequest::Coalesced
            );
        }

        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        worker.shutdown_and_join();
        assert!(worker.worker.is_none());
        assert!(worker.sender.is_none());
        assert!(!worker.queued_or_running.load(Ordering::Acquire));
    }

    #[test]
    fn worker_shutdown_is_bounded_to_the_single_coalesced_job() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let mut worker = KdeActivationWorker::with_runner(move |pid| {
            let _ = started_tx.send(pid);
            true
        });
        assert_eq!(
            worker.try_activate_result(7),
            KdeActivationRequest::Enqueued
        );
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 7);
        worker.shutdown_and_join();
        assert!(worker.worker.is_none());
        assert!(worker.sender.is_none());
    }

    #[test]
    fn kde_worker_source_does_not_own_wayland_or_runtime_objects() {
        let source = include_str!("kde_activation.rs");
        for forbidden in [
            concat!("way", "land_client"),
            concat!("wl_", "surface"),
            concat!("Queue", "Handle"),
            concat!("crate::", "runtime"),
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden ownership token: {forbidden}"
            );
        }
    }
}
