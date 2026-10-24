//! Input and response scenarios L2, L8, L9 and L10 of the headless spec (§9.2). Bytes that the
//! terminal sends to the program are read by the §9.1 shell reader and compared as one hex
//! string. Expected bytes come from the spec and the protocol reference, never from the output
//! of the implementation.

use super::session::HeadlessSession;
use super::tests::{
    TEST_BACKGROUND, cell_rect, dump, pixel_rgb, run_script, screenshot, session_for_test,
    wait_until,
};
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

/// Clears the visible rows, so that markers left by an earlier reader in the same session cannot
/// satisfy the waits of this one.
const SCREEN_CLEAR: &str = "printf '\\033[2J\\033[H'\n";

/// Starts the §9.1 reader in `/bin/sh`: `prelude` runs right after the ready marker, then the
/// reader waits for input. Returns once the ready marker is on screen. The screen is cleared first.
fn start_reader(session: &mut HeadlessSession, prelude: &str) {
    start_reader_with_setup(session, SCREEN_CLEAR, prelude);
}

/// Starts the §9.1 reader like `start_reader`, but runs `setup` before the ready marker instead
/// of clearing the screen, so output of `setup` (such as a hyperlink) stays on screen. That output
/// lies above the marker, so `finish_reader` does not include it.
fn start_reader_with_setup(session: &mut HeadlessSession, setup: &str, prelude: &str) {
    let path = write_script(&format!("{setup}{READER_HEAD}{prelude}{READER_TAIL}"));
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

/// Waits until the active screen reports Kitty keyboard `flags` in the dump.
fn wait_for_kitty_flags(session: &mut HeadlessSession, flags: u64) {
    wait_until(session, 5000, "the kitty keyboard flags", |dump| {
        dump["terminal"]["modes"]["kitty_flags"] == flags
    });
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

#[test]
fn kitty_flag_stacks_and_queries_are_answered() {
    let mut session = session_for_test(800, 600);
    // Each `?u` query is answered with the current flags. Push `CSI > f u` stores the current
    // flags and sets f; pop `CSI < n u` restores them; set `CSI = f ; mode u` replaces (mode 1),
    // ORs (mode 2) or clears (mode 3) bits; flag 4 is not supported and is masked out.
    let prelude = "printf '\\033[?u\\033[>1u\\033[?u\\033[>3u\\033[?u\\033[<1u\\033[?u'\n\
                   printf '\\033[=6;2u\\033[?u\\033[=2;3u\\033[?u\\033[<9u\\033[?u'\n\
                   printf '\\033[=2u\\033[?u\\033[=4;1u\\033[?u'\n";
    start_reader(&mut session, prelude);
    let answer = finish_reader(&mut session);

    // Expected flags after each query, in order: 0; push 1; push 3; pop to 1; set 6 OR into 1 with
    // flag 4 masked out (6 becomes 2, 1|2 = 3); clear 2 from 3 = 1; pop past the stack resets to 0;
    // replace with 2; flag 4 masked to 0.
    let expected = "\x1b[?0u\x1b[?1u\x1b[?3u\x1b[?1u\x1b[?3u\x1b[?1u\x1b[?0u\x1b[?2u\x1b[?0u";
    assert_eq!(answer, hex(expected.as_bytes()));
}

#[test]
fn kitty_key_encoding_follows_flags() {
    let mut session = session_for_test(800, 600);

    // Flags 0 keep the legacy bytes: Enter is CR, Shift+Enter is ESC CR (parity spec 5.3),
    // and Ctrl+С on a Cyrillic layout is ETX because the key is taken from its physical base.
    start_reader(&mut session, "");
    run_ok(&mut session, b"key enter\n");
    run_ok(&mut session, b"key shift+enter\n");
    run_ok(&mut session, "key --char с ctrl+c\n".as_bytes());
    let legacy = finish_reader(&mut session);
    assert_eq!(legacy, hex(b"\r\x1b\r\x03"));

    // Flag 1: Enter stays CR, Shift+Enter is CSI 13;2u and Ctrl+С is CSI 99;5u (parity spec 5.3).
    start_reader(&mut session, "printf '\\033[=1;1u'\n");
    wait_for_kitty_flags(&mut session, 1);
    run_ok(&mut session, b"key enter\n");
    run_ok(&mut session, b"key shift+enter\n");
    run_ok(&mut session, "key --char с ctrl+c\n".as_bytes());
    let disambiguated = finish_reader(&mut session);
    assert_eq!(disambiguated, hex(b"\r\x1b[13;2u\x1b[99;5u"));

    // Flags 3 (disambiguate and event types): a held Ctrl+C sends press CSI 99;5u, repeats
    // CSI 99;5:2u and release CSI 99;5:3u, the release carrying the press code. Enter release
    // sends nothing. IME commit text goes out as UTF-8 at any flags.
    start_reader(&mut session, "printf '\\033[=3;1u'\n");
    wait_for_kitty_flags(&mut session, 3);
    run_ok(&mut session, b"key --repeat 2 ctrl+c\n");
    run_ok(&mut session, b"key enter\n");
    run_ok(&mut session, "type привет\n".as_bytes());
    let events = finish_reader(&mut session);
    let expected = "\x1b[99;5u\x1b[99;5:2u\x1b[99;5:2u\x1b[99;5:3u\rпривет";
    assert_eq!(events, hex(expected.as_bytes()));
}

#[test]
fn ctrl_hover_shows_pointer_and_underline_and_ctrl_click_stays_out_of_the_app() {
    let mut session = session_for_test(800, 600);
    const URI: &str = "https://example.com/ronsole-l9";
    // The hyperlink is printed before the ready marker: the reader's screen clear would erase it.
    let link = format!("printf '\\033]8;;{URI}\\007link\\033]8;;\\007'\n");
    // Press reporting (`?1000`) is set after the marker; `dump` shows it once it is active.
    start_reader_with_setup(&mut session, &link, "printf '\\033[?1000h'\n");
    let state = wait_until(&mut session, 5000, "mouse tracking", |state| {
        state["terminal"]["modes"]["mouse_tracking"] == "press"
    });
    let row = row_index(&rows(&state), "link").expect("link row");

    // Without Ctrl the link cell has no underline; this screenshot is the reference.
    run_ok(&mut session, format!("mouse_cell 1 {row}\n").as_bytes());
    let before = dump(&mut session);
    let (x0, top, x1, bottom) = cell_rect(&before, 1, row);
    let plain = screenshot(&mut session);

    // Holding Ctrl over the link: the pointer becomes a hand and the underline is drawn.
    run_ok(&mut session, b"mods ctrl\n");
    wait_until(&mut session, 5000, "the Ctrl hover", |state| {
        state["terminal"]["hovered_link"].is_object() && state["cursor_icon"] == "Pointer"
    });
    let ctrl = screenshot(&mut session);
    // Threshold: any changed pixel in the lower quarter of the link cell's rect. The spec gives
    // no colour or exact row for the underline, so only that some pixel there changed is checked.
    let band_top = bottom - (bottom - top) / 4;
    let underlined = (x0..x1).any(|x| {
        (band_top..bottom).any(|y| pixel_rgb(&plain, (x, y)) != pixel_rgb(&ctrl, (x, y)))
    });
    assert!(underlined, "Ctrl hover must change the lower quarter of the link cell");

    // Ctrl+click opens the link through the open handler and is not sent to the mouse-tracking app.
    run_ok(&mut session, b"click\n");
    let opened = wait_until(&mut session, 5000, "the open_url request", |state| {
        !state["external_request"].is_null()
    });
    assert_eq!(opened["external_request"]["kind"], "open_url");
    assert_eq!(opened["external_request"]["arg"], URI);

    // Negative check: after a fixed pause the reader has received nothing but the final `x`.
    run_ok(&mut session, b"wait 300\n");
    run_ok(&mut session, b"mods\n");
    run_ok(&mut session, b"key x\n");
    assert_eq!(finish_reader(&mut session), hex(b"x"));
}

#[test]
fn mouse_report_uses_sgr_encoding_of_the_clicked_cell() {
    let mut session = session_for_test(800, 600);
    // `?1000` reports presses and releases and `?1006` encodes them as SGR; both are set after
    // the marker and `dump` shows them once they are active.
    start_reader(&mut session, "printf '\\033[?1000h\\033[?1006h'\n");
    wait_until(&mut session, 5000, "mouse tracking with SGR", |state| {
        state["terminal"]["modes"]["mouse_tracking"] == "press"
            && state["terminal"]["modes"]["mouse_sgr"] == true
    });

    // Cell (col 2, row 3) is reported 1-based as column 3, row 4: the press ends in `M`, the
    // release in `m` (xterm SGR mouse encoding).
    run_ok(&mut session, b"mouse_cell 2 3\n");
    run_ok(&mut session, b"click\n");
    let events = finish_reader(&mut session);
    assert_eq!(events, hex(b"\x1b[<0;3;4M\x1b[<0;3;4m"));
}

#[test]
fn focus_reports_follow_tab_switches() {
    let mut session = session_for_test(800, 600);
    // Focus reporting (`?1004`) is set on the first tab after the marker; `dump` shows it.
    start_reader(&mut session, "printf '\\033[?1004h'\n");
    wait_until(&mut session, 5000, "focus reporting", |state| {
        state["terminal"]["modes"]["focus_reporting"] == true
    });

    // A new tab takes focus, so the reader's tab gets focus-out `ESC [ O`.
    run_ok(&mut session, b"key ctrl+shift+t\n");
    wait_until(&mut session, 5000, "the second tab to be active", |state| {
        state["tabs"][1]["active"] == true
    });

    // Clicking the first tab's rect returns focus to the reader's tab: focus-in `ESC [ I`.
    let state = dump(&mut session);
    let [x, y, w, h] = serde_json::from_value::<[f64; 4]>(state["tabs"][0]["rect"].clone())
        .expect("first tab rect");
    run_ok(
        &mut session,
        format!("mouse_move {} {}\n", (x + w / 2.0) as i64, (y + h / 2.0) as i64).as_bytes(),
    );
    run_ok(&mut session, b"click\n");
    wait_until(&mut session, 5000, "the first tab to be active", |state| {
        state["tabs"][0]["active"] == true
    });
    let events = finish_reader(&mut session);
    assert_eq!(events, hex(b"\x1b[O\x1b[I"));
}

#[test]
fn drag_selection_copies_and_bracketed_paste_wraps_the_clipboard() {
    let mut session = session_for_test(800, 600);
    // The word is split in the typed command, so only its output row contains it.
    run_ok(&mut session, b"type echo RO''NSOLE\n");
    run_ok(&mut session, b"key enter\n");
    let state = wait_until(&mut session, 5000, "the printed word", |state| {
        row_index(&rows(state), "RONSOLE").is_some()
    });
    let row = row_index(&rows(&state), "RONSOLE").expect("word row");

    // Drag from the first to the last letter (the last cell is included), then copy with Ctrl+C.
    run_ok(&mut session, format!("mouse_cell 0 {row}\n").as_bytes());
    run_ok(&mut session, b"click left down\n");
    run_ok(&mut session, format!("mouse_cell 6 {row}\n").as_bytes());
    run_ok(&mut session, b"click left up\n");
    run_ok(&mut session, b"key ctrl+c\n");
    wait_until(&mut session, 5000, "the copied selection", |state| {
        state["clipboard"]["text"] == "RONSOLE"
    });

    // Paste is wrapped in `ESC [ 200 ~` and `ESC [ 201 ~` only once the app enables `?2004`.
    start_reader(&mut session, "printf '\\033[?2004h'\n");
    wait_until(&mut session, 5000, "bracketed paste", |state| {
        state["terminal"]["modes"]["bracketed_paste"] == true
    });
    run_ok(&mut session, b"key ctrl+v\n");
    let events = finish_reader(&mut session);
    assert_eq!(events, hex(b"\x1b[200~RONSOLE\x1b[201~"));
}
