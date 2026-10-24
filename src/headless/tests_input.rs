//! Input and response scenarios L2, L8, L9 and L10 of the headless spec (§9.2). Bytes that the
//! terminal sends to the program are read by the §9.1 shell reader and compared as one hex
//! string. Expected bytes come from the spec and the protocol reference, never from the output
//! of the implementation.

use super::session::HeadlessSession;
use super::tests::{TEST_BACKGROUND, run_script, session_for_test, wait_until};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// Gives each reader script its own directory within the process.
static NEXT_SCRIPT: AtomicU32 = AtomicU32::new(0);

/// Opening lines of the §9.1 reader: raw input, then the ready marker. The marker is split so
/// the echoed command never contains it, and it ends with `\r\n` because raw mode turns OPOST off.
const READER_HEAD: &str = "stty raw -echo min 1 time 0\nprintf 'RE''ADY\\r\\n'\n";

/// Closing lines of the §9.1 reader: the first byte waits without a deadline, the rest is read
/// until one second of silence, then the hex dump and the end marker are printed.
const READER_TAIL: &str = "{ dd bs=1 count=1; stty min 0 time 10; dd bs=1; } 2>/dev/null | od -An -tx1 -v | tr -d ' \\n'\nprintf '\\r\\nEN''D\\r\\n'\nstty sane\n";

/// Runs `script` and asserts that no response is an `err`; returns the responses.
fn run_ok(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let lines = run_script(session, script);
    assert!(
        lines.iter().all(|line| !line.starts_with("err")),
        "unexpected err in {lines:?}"
    );
    lines
}

/// Visible rows of `dump`, trimmed of surrounding spaces.
fn rows(dump: &Value) -> Vec<String> {
    dump["terminal"]["text"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_str().unwrap_or("").trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Index of the first visible row that contains `marker`.
fn row_index(rows: &[String], marker: &str) -> Option<usize> {
    rows.iter().position(|row| row.contains(marker))
}

/// Lowercase hex of `bytes`, without separators, as the reader prints it.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// DA2 version number `major·10000 + minor·100 + patch` of the package version (spec §5.2).
fn da2_version() -> u32 {
    let mut parts = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse::<u32>().expect("numeric package version part"));
    let major = parts.next().expect("major version");
    let minor = parts.next().expect("minor version");
    let patch = parts.next().expect("patch version");
    major * 10_000 + minor * 100 + patch
}

/// Writes `body` to a fresh script file of this process and returns its path.
fn write_script(body: &str) -> PathBuf {
    let index = NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ronsole-t9-{}-{index}", std::process::id()));
    fs::create_dir_all(&dir).expect("script directory");
    let path = dir.join("reader.sh");
    fs::write(&path, body).expect("script file");
    path
}

/// Starts the §9.1 reader in `/bin/sh`: `prelude` runs right after the ready marker, then the
/// reader waits for input. Returns once the ready marker is on screen.
fn start_reader(session: &mut HeadlessSession, prelude: &str) {
    let path = write_script(&format!("{READER_HEAD}{prelude}{READER_TAIL}"));
    run_ok(session, format!("type sh {}\n", path.display()).as_bytes());
    run_ok(session, b"key enter\n");
    wait_until(session, 5000, "the reader ready marker", |dump| {
        row_index(&rows(dump), "READY").is_some()
    });
}

/// Waits for the reader's end marker and returns the hex of every byte read between the markers,
/// joining the rows without a separator because long output wraps.
fn finish_reader(session: &mut HeadlessSession) -> String {
    let dump = wait_until(session, 10_000, "the reader end marker", |dump| {
        let rows = rows(dump);
        match (row_index(&rows, "READY"), row_index(&rows, "END")) {
            (Some(ready), Some(end)) => end > ready,
            _ => false,
        }
    });
    let rows = rows(&dump);
    let ready = row_index(&rows, "READY").expect("ready row");
    let end = row_index(&rows, "END").expect("end row");
    rows[ready + 1..end].concat()
}

#[test]
fn terminal_identity_and_mode_queries_are_answered() {
    let mut session = session_for_test(800, 600);
    let version = env!("CARGO_PKG_VERSION");
    // The shell's printf emits each query; the terminal answers into the reader's input.
    let prelude = "printf '\\033[>c\\033[>q\\033[?2026$p\\033[?9999$p\\033[20$p\\033]11;?\\007'\n\
                   printf '\\033]10;?\\007\\033]12;?\\007\\033]4;1;?\\007'\n";
    start_reader(&mut session, prelude);
    let answer = finish_reader(&mut session);

    // Expected answers: DA2 with the version formula, XTVERSION, DECRQM (2026 is reset, 9999 and
    // ANSI 20 are unimplemented), and OSC 11 with the configured background, each channel's byte
    // repeated as 16 bits and the BEL terminator of the request.
    let [red, green, blue] = TEST_BACKGROUND;
    let known = format!(
        "\x1b[>1;{};0c\x1bP>|ronsole({version})\x1b\\\x1b[?2026;2$y\x1b[?9999;0$y\x1b[20;0$y\x1b]11;rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}\x07",
        da2_version()
    );
    let known_hex = hex(known.as_bytes());
    assert!(
        answer.starts_with(&known_hex),
        "answer {answer} must start with {known_hex}"
    );
    // OSC 10, 12 and 4 are only checked by shape: their colours are render constants that the spec
    // does not fix (parity spec §5.2 and the TermPalette note in its section 4), so no value is asserted.
    let rest = &answer[known_hex.len()..];
    assert!(rest.starts_with(&hex(b"\x1b]10;rgb:")), "OSC 10 answer in {rest}");
    assert!(rest.contains(&hex(b"\x1b]12;rgb:")), "OSC 12 answer in {rest}");
    assert!(rest.contains(&hex(b"\x1b]4;1;rgb:")), "OSC 4 answer in {rest}");
    assert!(rest.ends_with(&hex(b"\x07")), "last answer ends with BEL in {rest}");
}
