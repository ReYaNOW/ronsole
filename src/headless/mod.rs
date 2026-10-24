//! Headless rendering: the runtime drives frames into an offscreen EGL pbuffer.

pub(crate) mod backend;
mod keys;
mod protocol;

use crate::app::{
    App, AutomationOptions, HeadlessRunError, automation_options_from_args,
    write_automation_startup_failure,
};
use crate::launch::TerminalLaunchSpec;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;

/// Physical size of the `--pgo-train` headless surface.
const PGO_TRAIN_WIDTH: u32 = 1280;
const PGO_TRAIN_HEIGHT: u32 = 800;
/// Number of `-<n>` suffixes tried before a profile directory is reported as busy.
const PROFILE_SUFFIX_LIMIT: u32 = 1000;

/// Entry point for `ronsole --headless ...`. Never opens the single-instance socket
/// or connects to Wayland.
pub(crate) fn main(args: &[OsString]) -> ExitCode {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("headless: {error}");
            return ExitCode::from(2);
        }
    };
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let profile = match Profile::create(runtime_dir, std::process::id()) {
        Ok(profile) => profile,
        Err(error) => {
            let message = error.to_string();
            eprintln!("headless: profile: {message}");
            let _ = write_automation_startup_failure(&options, "headless-profile", &message);
            return ExitCode::FAILURE;
        }
    };
    crate::prefer_egl_vendor();
    crate::tune_glibc_allocator();
    let mut app = match App::load_headless(Some(options.clone()), &profile.dir) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("headless: {error}");
            return ExitCode::FAILURE;
        }
    };
    match app.run_headless(
        (PGO_TRAIN_WIDTH, PGO_TRAIN_HEIGHT),
        1.0,
        TerminalLaunchSpec::default(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(HeadlessRunError::Egl(error)) => {
            eprintln!("headless: {error}");
            let _ = write_automation_startup_failure(&options, "headless-egl", &error);
            ExitCode::from(3)
        }
        Err(HeadlessRunError::Run(error)) => {
            eprintln!("headless: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Parses `ronsole --headless ...` argv into PGO options. Protocol mode is not
/// implemented yet, so `--pgo-train` is required.
fn parse_args(args: &[OsString]) -> Result<AutomationOptions, String> {
    if args.get(1).map(OsString::as_os_str) != Some(OsStr::new("--headless")) {
        return Err("expected --headless as the first argument".to_string());
    }
    let automation_args: Vec<OsString> = args
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, arg)| arg.clone())
        .collect();
    if !automation_args
        .iter()
        .skip(1)
        .any(|arg| arg == OsStr::new("--pgo-train"))
    {
        return Err("protocol mode is not implemented yet".to_string());
    }
    automation_options_from_args(&automation_args)
}

/// Per-run directory holding the headless config; removed when dropped.
pub(crate) struct Profile {
    pub(crate) dir: PathBuf,
}

impl Profile {
    /// Creates `<runtime_dir or /tmp>/ronsole-headless-<pid>`, adding `-<n>` while
    /// the name is taken. The directory is created with mode 0700.
    pub(crate) fn create(runtime_dir: Option<PathBuf>, pid: u32) -> io::Result<Self> {
        let base = runtime_dir.unwrap_or_else(|| PathBuf::from("/tmp"));
        for suffix in 0..=PROFILE_SUFFIX_LIMIT {
            let name = if suffix == 0 {
                format!("ronsole-headless-{pid}")
            } else {
                format!("ronsole-headless-{pid}-{suffix}")
            };
            let dir = base.join(name);
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free headless profile directory name",
        ))
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn unique_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("ronsole-t4-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("test base directory");
        base
    }

    #[test]
    fn headless_args_without_pgo_train_rejected() {
        let error = parse_args(&args(&["ronsole", "--headless"]))
            .expect_err("protocol mode must be rejected");
        assert_eq!(error, "protocol mode is not implemented yet");
    }

    #[test]
    fn headless_args_pass_pgo_options() {
        let options = parse_args(&args(&[
            "ronsole",
            "--headless",
            "--pgo-train",
            "--pgo-workspace",
            "/w",
            "--pgo-report",
            "/r",
            "--pgo-timeout-seconds",
            "5",
        ]))
        .expect("valid PGO arguments must parse");
        assert_eq!(options.workspace, PathBuf::from("/w"));
        assert_eq!(options.timeout, Duration::from_secs(5));
    }

    #[test]
    fn headless_args_unknown_flag_rejected() {
        let error = parse_args(&args(&[
            "ronsole",
            "--headless",
            "--pgo-train",
            "--script",
            "x",
        ]))
        .expect_err("unknown flag must be rejected");
        assert_eq!(error, "unknown PGO automation argument: --script");
    }

    #[test]
    fn headless_profile_dir_fallback_and_suffix() {
        let fallback = Profile::create(None, std::process::id()).expect("fallback profile");
        assert_eq!(fallback.dir.parent(), Some(Path::new("/tmp")));
        let mode = fs::metadata(&fallback.dir)
            .expect("profile metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        let base = unique_base("suffix");
        let pid = std::process::id();
        let first = Profile::create(Some(base.clone()), pid).expect("first profile");
        let second = Profile::create(Some(base.clone()), pid).expect("second profile");
        assert_eq!(first.dir, base.join(format!("ronsole-headless-{pid}")));
        assert_eq!(second.dir, base.join(format!("ronsole-headless-{pid}-1")));
    }

    #[test]
    fn headless_profile_removed_on_drop() {
        let base = unique_base("drop");
        let profile = Profile::create(Some(base.clone()), std::process::id()).expect("profile");
        let dir = profile.dir.clone();
        fs::write(dir.join("config"), "x").expect("write profile file");
        drop(profile);
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&base);
    }
}
