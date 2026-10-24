//! Session scenarios L3, L6 and L7 of the headless spec (§9.2) and the wake-up test (Q18).
//! Expected values come from the spec and the terminal parity references, never from the
//! output of the implementation.

use super::session::HeadlessSession;
use super::tests::{dump, run_script, session_for_test, wait_until};
use serde_json::{Value, json};
use std::fs;
use std::time::{Duration, Instant};

/// Synchronized-output timeout without new bytes (spec §9.2, L3).
const SYNC_TIMEOUT_MS: u64 = 150;

/// Allowance on top of `SYNC_TIMEOUT_MS` for the marker to show in a `dump` (spec §9.2, L3).
const SYNC_SLACK_MS: u64 = 100;

/// Runs `script` and asserts that no response is an `err`; returns the responses.
fn run_ok(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let lines = run_script(session, script);
    assert!(
        lines.iter().all(|line| !line.starts_with("err")),
        "unexpected err in {lines:?}"
    );
    lines
}

/// Visible rows of `state`, trimmed of surrounding spaces.
fn rows(state: &Value) -> Vec<String> {
    state["terminal"]["text"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_str().unwrap_or("").trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Value of the `name=<n>` counter in an `idle` response `ok frames=… wakes=… deadlines=…`.
fn counter(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("`{name}=` missing in {line}"))
}

#[test]
fn synchronized_output_is_drawn_within_its_timeout() {
    let mut session = session_for_test(800, 600);
    // The marker is split in the typed command, so only its printed row contains `visible`.
    // The `\\n` is unescaped by `type` into `\n`, which `printf` turns into a newline.
    run_ok(
        &mut session,
        br#"type printf '\033[?2026h'; printf 'vis''ible\\n'; sleep 1"#,
    );
    run_ok(&mut session, b"key enter\n");

    // The first dump with `sync_pending` is the reference point; the marker must not be drawn yet.
    // The instant is taken before the dump, which makes the later check stricter.
    let deadline = Instant::now() + Duration::from_secs(5);
    let started = loop {
        let before = Instant::now();
        let state = dump(&mut session);
        if state["terminal"]["modes"]["sync_pending"] == true {
            assert!(
                !rows(&state).iter().any(|row| row.contains("visible")),
                "marker is drawn while sync is pending: {state}"
            );
            break before;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for sync_pending: {state}"
        );
        run_script(&mut session, b"wait 50\n");
    };

    // The buffer must be flushed by the 150 ms timeout, so the marker is visible within 250 ms.
    let limit = Duration::from_millis(SYNC_TIMEOUT_MS + SYNC_SLACK_MS);
    loop {
        let state = dump(&mut session);
        let elapsed = started.elapsed();
        if rows(&state).iter().any(|row| row.contains("visible")) {
            assert!(elapsed <= limit, "marker drawn after {elapsed:?}, limit {limit:?}");
            break;
        }
        assert!(
            elapsed <= limit,
            "marker not drawn after {elapsed:?}, limit {limit:?}: {state}"
        );
        run_script(&mut session, b"wait 50\n");
    }
}

#[test]
fn synchronized_output_deadline_wakes_the_loop() {
    let mut session = session_for_test(800, 600);
    run_ok(
        &mut session,
        br#"type printf '\033[?2026h'; printf 'vis''ible\\n'; sleep 1"#,
    );
    run_ok(&mut session, b"key enter\n");
    wait_until(&mut session, 5000, "a pending synchronized update", |state| {
        state["terminal"]["modes"]["sync_pending"] == true
    });

    // The 2026 deadline ends the buffer inside this window, so the loop wakes on it.
    let idle = run_ok(&mut session, b"idle 400\n");
    assert!(counter(&idle[0], "deadlines") >= 1, "{idle:?}");
}

#[test]
fn osc52_write_reaches_the_memory_clipboard() {
    let mut session = session_for_test(800, 600);
    // Payload is base64 of `RONSOLE-OSC52`, produced by the `base64` tool outside the test.
    run_ok(
        &mut session,
        br#"type printf '\033]52;c;Uk9OU09MRS1PU0M1Mg==\007'"#,
    );
    run_ok(&mut session, b"key enter\n");
    wait_until(&mut session, 5000, "the OSC 52 clipboard write", |state| {
        state["clipboard"] == json!({"mode": "memory", "text": "RONSOLE-OSC52"})
    });
}

#[test]
fn new_tab_opens_in_the_directory_reported_by_osc7() {
    let mut session = session_for_test(800, 600);
    let dir = std::env::temp_dir().join(format!("ronsole-t11-cwd-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("test directory");
    let path = dir.display().to_string();

    // OSC 7 is followed by a marker, so the marker row proves the sequence was parsed first.
    run_ok(
        &mut session,
        format!("type printf '\\033]7;file://{path}\\007'; printf 'RE''ADY\\\\n'\n").as_bytes(),
    );
    run_ok(&mut session, b"key enter\n");
    wait_until(&mut session, 5000, "the marker after OSC 7", |state| {
        rows(state).iter().any(|row| row.contains("READY"))
    });

    run_ok(&mut session, b"key ctrl+shift+t\n");
    wait_until(&mut session, 5000, "the second tab to be active", |state| {
        state["tabs"][1]["active"] == true
    });
    run_ok(&mut session, b"type pwd\n");
    run_ok(&mut session, b"key enter\n");
    wait_until(&mut session, 5000, "pwd of the new tab", |state| {
        rows(state).iter().any(|row| row.contains(&path))
    });
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn idle_delivers_the_shell_output_wakeup() {
    let mut session = session_for_test(800, 600);
    // The foreground changes to `sleep` at once; `wait 600` lets the title worker's wake pass,
    // so the idle window counts the output wake-up only. The marker is printed about 1 s later.
    run_ok(&mut session, br#"type sleep 1; printf 'RE''ADY\\n'"#);
    run_ok(&mut session, b"key enter\n");
    run_ok(&mut session, b"wait 600\n");
    let idle = run_ok(&mut session, b"idle 2000\n");
    assert!(counter(&idle[0], "wakes") >= 1, "{idle:?}");
    // The row must already be on screen when `idle` returns, without a further wait.
    let state = dump(&mut session);
    assert!(
        rows(&state).iter().any(|row| row.contains("READY")),
        "marker missing right after idle: {state}"
    );
}
