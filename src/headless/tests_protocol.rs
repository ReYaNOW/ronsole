//! Protocol-level tests of the headless session: each runs commands through
//! `run_script` or `run_protocol` and checks the responses and the effects in the shell.

use super::keys::{key_spec, modifier_key_input};
use super::session::{FrameBudget, HeadlessSession, HzSource, ProfileChoice, SessionOptions, run_protocol};
use super::tests::{TEMP_PROFILE_LOCK, run_script, session_for_test};
use crate::input_types::{KeyState, Modifiers};
use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::MutexGuard;
use std::time::{Duration, Instant};

/// Takes the lock that serializes tests using `Temp` profiles, whose names share the pid.
fn temp_profile_lock() -> MutexGuard<'static, ()> {
    TEMP_PROFILE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Options of a protocol run with a `Temp` profile on `/bin/sh`, as `--headless` builds them.
fn temp_profile_options() -> SessionOptions {
    SessionOptions {
        size: (640, 400),
        scale: 1.0,
        profile: ProfileChoice::Temp { keep: false },
        shell: Some(PathBuf::from("/bin/sh")),
        budget: FrameBudget {
            budget_ms: 1000.0 / 240.0,
            hz: 240.0,
            source: HzSource::Default,
        },
    }
}

/// Runs `input` through `run_protocol` with a temporary profile; returns the exit code and lines.
fn run_temp(input: &[u8]) -> (u8, Vec<String>) {
    let mut output = Vec::new();
    let code = run_protocol(temp_profile_options(), Cursor::new(input), &mut output);
    let lines = String::from_utf8(output)
        .expect("protocol responses are UTF-8")
        .lines()
        .map(str::to_string)
        .collect();
    (code, lines)
}

/// Names of the `Temp` profile directories of this process in the base directory they use.
fn temp_profile_names() -> Vec<String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let prefix = format!("ronsole-headless-{}", std::process::id());
    let mut names: Vec<String> = fs::read_dir(&base)
        .expect("profile base directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name == &prefix || name.starts_with(&format!("{prefix}-")))
        .collect();
    names.sort();
    names
}

/// A fresh scratch directory for this test process, named after `tag`.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ronsole-t3-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// Steps the session with `wait 50` until `done` holds; false when `timeout` passes first.
fn wait_for(session: &mut HeadlessSession, timeout: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        run_script(session, b"wait 50\n");
    }
    true
}

/// Decodes the PNG at `path` and returns its width and height.
fn png_size(path: &Path) -> (u32, u32) {
    let file = fs::File::open(path).expect("open screenshot");
    let reader = png::Decoder::new(BufReader::new(file))
        .read_info()
        .expect("decode PNG header");
    let info = reader.info();
    (info.width, info.height)
}

#[test]
fn info_reports_budget() {
    let mut session = session_for_test(640, 400);
    let lines = run_script(&mut session, b"info\n");
    let payload = lines[0].strip_prefix("ok ").expect("info answers ok");
    let info: serde_json::Value = serde_json::from_str(payload).expect("info payload is JSON");
    assert_eq!(info["hz"].as_f64(), Some(240.0));
    let budget_ms = info["budget_ms"].as_f64().expect("budget_ms is a number");
    assert!(
        (budget_ms - 1000.0 / 240.0).abs() < 0.001,
        "budget_ms {budget_ms}"
    );
    assert!(info["gl_renderer"].as_str().is_some_and(|name| !name.is_empty()));
}

#[test]
fn type_and_key_reach_shell() {
    let dir = scratch_dir("keys");
    let out = dir.join("out");
    let mut session = session_for_test(640, 400);
    let script = format!(
        "type cat > {}\\n\nkey shift+a\nkey --char с a\nkey enter\nkey ctrl+d\n",
        out.display()
    );
    let lines = run_script(&mut session, script.as_bytes());
    assert_eq!(lines, vec!["ok"; 5]);
    let arrived = wait_for(&mut session, Duration::from_secs(5), || {
        fs::read(&out).is_ok_and(|bytes| bytes == "Aс\n".as_bytes())
    });
    assert!(arrived, "shell file {}: {:?}", out.display(), fs::read(&out).ok());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn err_does_not_stop_session() {
    let _lock = temp_profile_lock();

    let (code, lines) = run_temp(b"resize 100x100\ninfo\n");
    assert_eq!(code, 1);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("err "), "{lines:?}");
    assert!(lines[1].starts_with("ok {"), "{lines:?}");

    let (code, lines) = run_temp(b"info\n\xff\xfe\ninfo\n");
    assert_eq!(code, 1);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].starts_with("ok"), "{lines:?}");
    assert!(lines[1].starts_with("err "), "{lines:?}");
    assert!(lines[2].starts_with("ok"), "{lines:?}");

    let (code, lines) = run_temp(b"info\nquit\ninfo\n");
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 2, "{lines:?}");
}

#[test]
fn bad_arguments_answer_err() {
    let mut session = session_for_test(640, 400);
    let lines = run_script(
        &mut session,
        b"resize 0x0\nscale 10\nwait 70000\nkey bogus\ninfo\n",
    );
    assert_eq!(lines.len(), 5, "{lines:?}");
    for line in &lines[..4] {
        assert!(line.starts_with("err "), "{line}");
    }
    assert!(lines[4].starts_with("ok {"), "{lines:?}");
}

#[test]
fn app_exit_ends_script() {
    let _lock = temp_profile_lock();
    let before = temp_profile_names();
    let (code, lines) = run_temp(b"type exit\\n\nwait 5000\ninfo\n");
    assert_eq!(code, 0);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|line| line.starts_with("ok")), "{lines:?}");
    assert_eq!(temp_profile_names(), before);
}

#[test]
fn resize_and_screenshot() {
    let dir = scratch_dir("shot");
    let first = dir.join("a").join("b.png");
    let second = dir.join("a").join("c.png");
    let mut session = session_for_test(640, 400);
    let script = format!(
        "resize 800x600\nscreenshot {}\nscale 1.333\nscreenshot {}\n",
        first.display(),
        second.display()
    );
    let lines = run_script(&mut session, script.as_bytes());
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "ok 800x600");
    let first_abs = fs::canonicalize(&first).expect("first screenshot exists");
    assert_eq!(lines[1], format!("ok {} 800x600", first_abs.display()));
    assert_eq!(lines[2], "ok");
    let second_abs = fs::canonicalize(&second).expect("second screenshot exists");
    assert_eq!(lines[3], format!("ok {} 800x600", second_abs.display()));
    assert_eq!(png_size(&first_abs), (800, 600));
    assert_eq!(png_size(&second_abs), (800, 600));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn idle_session_waits_without_frames() {
    let dir = scratch_dir("idle");
    let marker = dir.join("r");
    let mut session = session_for_test(640, 400);
    let script = format!("type : > {}\\n\n", marker.display());
    let lines = run_script(&mut session, script.as_bytes());
    assert_eq!(lines, vec!["ok"]);
    assert!(wait_for(&mut session, Duration::from_secs(5), || marker.exists()));
    // The shell prints its prompt only after the command, so wait for that before judging idleness.
    run_script(&mut session, b"wait 200\n");
    let settle = run_script(&mut session, b"settle\n");
    assert!(settle[0].starts_with("ok frames="), "{settle:?}");
    let wake = run_script(&mut session, b"wake 50\n");
    assert_eq!(wake, vec!["ok cause=timeout frame=false flow=wait deadline_ms=none"]);
    let idle = run_script(&mut session, b"idle 300\n");
    assert!(idle[0].starts_with("ok frames=0 "), "{idle:?}");
    let settled = run_script(&mut session, b"settle\n");
    assert!(settled[0].ends_with("settled=true"), "{settled:?}");
    let _ = fs::remove_dir_all(&dir);
}

// Repro of the open bug: `wake 5000` after the marker file appears returns `ui_waker`
// about 150 ms later, before the `printf x` the script waits for (observed
// `ok cause=ui_waker frame=true`, `took 148ms`).
#[test]
#[ignore = "bug: wake returns ui_waker ~150 ms after settle, before the printf output"]
fn shell_output_wakes_by_ui_waker() {
    let dir = scratch_dir("waker");
    let marker = dir.join("r");
    let mut session = session_for_test(640, 400);
    let script = format!("type : > {}; sleep 2; printf x\\n\n", marker.display());
    let lines = run_script(&mut session, script.as_bytes());
    assert_eq!(lines, vec!["ok"]);
    assert!(wait_for(&mut session, Duration::from_secs(5), || marker.exists()));
    run_script(&mut session, b"settle\n");
    let started = Instant::now();
    let wake = run_script(&mut session, b"wake 5000\n");
    let elapsed = started.elapsed();
    assert!(wake[0].starts_with("ok cause=ui_waker frame=true"), "{wake:?}");
    assert!(
        elapsed >= Duration::from_millis(1000),
        "wake returned after {elapsed:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bad_args_exit_2() {
    let cases: [Vec<&str>; 3] = [
        vec!["ronsole", "--headless", "--hz", "60", "--budget-ms", "5"],
        vec!["ronsole", "--headless", "--pgo-train", "--script", "x"],
        vec!["ronsole", "--headless", "--bogus"],
    ];
    for case in cases {
        let args: Vec<OsString> = case.iter().map(OsString::from).collect();
        assert!(super::parse_args(&args).is_err(), "{case:?}");
        assert_eq!(super::main(&args), ExitCode::from(2), "{case:?}");
    }
}

#[test]
fn modifier_key_input_uses_each_modifier_keycode() {
    // evdev keycodes from linux/input-event-codes.h.
    let cases = [
        (Modifiers::CONTROL, 29),
        (Modifiers::SHIFT, 42),
        (Modifiers::ALT, 56),
        (Modifiers::SUPER, 125),
    ];
    for (modifier, keycode) in cases {
        let input = modifier_key_input(modifier, KeyState::Pressed);
        assert_eq!(input.keycode, keycode);
        assert_eq!(input.state, KeyState::Pressed);
    }
}

#[test]
fn key_spec_maps_names_to_evdev_keycodes() {
    // evdev keycodes from linux/input-event-codes.h.
    let table = [
        ("a", 30),
        ("z", 44),
        ("1", 2),
        ("0", 11),
        ("enter", 28),
        ("tab", 15),
        ("esc", 1),
        ("space", 57),
        ("f1", 59),
        ("f12", 88),
        ("up", 103),
        ("left", 105),
    ];
    for (name, keycode) in table {
        let spec = key_spec(name).unwrap_or_else(|| panic!("key '{name}' is known"));
        assert_eq!(spec.keycode, keycode, "{name}");
    }
}
