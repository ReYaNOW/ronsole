//! Test scaffolding for the headless module: unit tests of its helpers and the
//! session helpers used by the protocol tests.

use super::*;
use super::session::{FrameBudget, HeadlessSession, HzSource, ProfileChoice, SessionOptions, serve};
use std::io::Cursor;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

/// Terminal background written into every session profile of the tests.
pub(crate) const TEST_BACKGROUND: [u8; 3] = [0x12, 0x34, 0x56];

/// Gives each test session its own profile directory name within the process.
static NEXT_PROFILE: AtomicU32 = AtomicU32::new(0);

/// Serializes tests that use `Temp` profiles, whose names share the process id.
pub(crate) static TEMP_PROFILE_LOCK: Mutex<()> = Mutex::new(());

/// Starts a session of `width` x `height` physical pixels on `/bin/sh` with
/// `TEST_BACKGROUND` as its terminal background.
pub(crate) fn session_for_test(width: u32, height: u32) -> HeadlessSession {
    crate::prefer_egl_vendor();
    let index = NEXT_PROFILE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ronsole-t3-{}-{index}", std::process::id()));
    fs::create_dir_all(&dir).expect("test profile directory");
    let [red, green, blue] = TEST_BACKGROUND;
    fs::write(
        dir.join("config"),
        format!("terminal_background=#{red:02X}{green:02X}{blue:02X}\n"),
    )
    .expect("test profile config");
    let options = SessionOptions {
        size: (width, height),
        scale: 1.0,
        profile: ProfileChoice::Dir(dir.clone()),
        shell: Some(PathBuf::from("/bin/sh")),
        budget: FrameBudget {
            budget_ms: 1000.0 / 240.0,
            hz: 240.0,
            source: HzSource::Default,
        },
    };
    let session = HeadlessSession::start(options).expect("test session starts");
    // The app reads its config while loading, so the directory is not needed after start.
    let _ = fs::remove_dir_all(&dir);
    session
}

/// Runs `script` through the protocol serializer and returns each response line.
pub(crate) fn run_script(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let mut output = Vec::new();
    serve(session, Cursor::new(script), &mut output);
    String::from_utf8(output)
        .expect("protocol responses are UTF-8")
        .lines()
        .map(str::to_string)
        .collect()
}

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
fn headless_args_without_pgo_train_select_protocol() {
    let Mode::Protocol(protocol) =
        parse_args(&args(&["ronsole", "--headless"])).expect("bare --headless is valid")
    else {
        panic!("bare --headless must select protocol mode");
    };
    assert_eq!(protocol.script, None);
    assert_eq!(protocol.options.size, (1280, 800));
    assert_eq!(protocol.options.scale, 1.0);
    assert_eq!(protocol.options.profile, ProfileChoice::Temp { keep: false });
    assert_eq!(protocol.options.shell, None);
    assert_eq!(protocol.options.budget.hz, 240.0);
    assert_eq!(protocol.options.budget.source, HzSource::Default);
}

#[test]
fn headless_args_pass_pgo_options() {
    let Mode::Pgo(options) = parse_args(&args(&[
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
    .expect("valid PGO arguments must parse")
    else {
        panic!("--pgo-train must select PGO mode");
    };
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
    .err()
    .expect("unknown flag must be rejected");
    assert_eq!(error, "unknown PGO automation argument: --script");
}

#[test]
fn headless_profile_dir_fallback_and_suffix() {
    let _lock = TEMP_PROFILE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
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
