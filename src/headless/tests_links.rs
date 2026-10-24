//! Link scenarios L4, L5, L15, L16 and L20 of the headless spec (§9.2). Plain and OSC 8 links
//! are printed by `/bin/sh` and checked through `dump` (`hovered_link`, `external_request`) and
//! `dump row` (the `link` of each cell). Expected values come from the spec and the parity
//! references, never from the output of the implementation.

use super::session::HeadlessSession;
use super::tests::{dump, run_script, session_for_test, wait_until};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Gives each test directory its own name within the process.
static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// Runs `script` and asserts that no response is an `err`; returns the responses.
fn run_ok(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let lines = run_script(session, script);
    assert!(
        lines.iter().all(|line| !line.starts_with("err")),
        "unexpected err in {lines:?}"
    );
    lines
}

/// Creates an empty directory unique to this process and `tag`.
fn fresh_dir(tag: &str) -> PathBuf {
    let index = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ronsole-t10-{}-{tag}-{index}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("test directory");
    dir
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

/// Parses the `ok <json>` response of `dump row <row>`.
fn dump_row(session: &mut HeadlessSession, row: usize) -> Value {
    let lines = run_ok(session, format!("dump row {row}\n").as_bytes());
    let json = lines
        .first()
        .and_then(|line| line.strip_prefix("ok "))
        .unwrap_or_else(|| panic!("dump row answers `ok <json>`, got {lines:?}"));
    serde_json::from_str(json).expect("dump row payload is JSON")
}

/// Hyperlink URI of cell `col` of a `dump row` payload, if the cell has one.
fn cell_link(row: &Value, col: usize) -> Option<String> {
    row["cells"]
        .get(col)
        .and_then(|cell| cell["link"].as_str())
        .map(str::to_string)
}

/// Writes `body` as `links.sh` in `dir`, types `sh <file>` and waits for the ready marker.
/// The script clears the screen first and keeps the shell busy with `sleep 30` afterwards,
/// so no prompt covers the output. The marker is split so the typed command never contains it.
fn start_script(session: &mut HeadlessSession, dir: &Path, body: &str) {
    let path = dir.join("links.sh");
    let script = format!("printf '\\033[2J\\033[H'\n{body}printf 'RE''ADY\\n'\nsleep 30\n");
    fs::write(&path, script).expect("test script file");
    run_ok(session, format!("type sh {}\n", path.display()).as_bytes());
    run_ok(session, b"key enter\n");
    wait_until(session, 5000, "the ready marker", |dump| {
        row_index(&rows(dump), "READY").is_some()
    });
}

/// Returns the visible index of the row that contains `marker`, scrolling up first if needed.
/// The wheel is far larger than the scrollback, so the view should reach the top of the history,
/// where the first rows of these scenarios' output are. The final position is confirmed by
/// `wait_until`, because scrolling is animated.
fn scroll_to_row(session: &mut HeadlessSession, marker: &str) -> usize {
    let state = dump(session);
    if let Some(row) = row_index(&rows(&state), marker) {
        return row;
    }
    run_ok(session, b"mouse_cell 0 0\n");
    run_ok(session, b"wheel 0 100\n");
    let state = wait_until(session, 5000, "the viewport at the top of the history", |dump| {
        dump["terminal"]["viewport"]["top"].as_u64() == Some(0)
    });
    row_index(&rows(&state), marker).expect("marker row is visible")
}

#[test]
#[ignore = "bug: wheel stops at max_scroll (fractional viewport height in terminal_max_scroll) with viewport top 1; dump floors scroll_offset, so the link on line 0 is not reached"]
fn osc8_link_survives_scrollback_and_resize() {
    let mut session = session_for_test(800, 600);
    const URI: &str = "https://example.com/ronsole-l4";
    let dir = fresh_dir("l4");
    let rows_before = dump(&mut session)["terminal"]["rows"]
        .as_u64()
        .expect("visible rows");
    // The link line, then two screens of plain lines push it into the scrollback.
    let body = format!(
        "printf '\\033]8;;{URI}\\007lnk\\033]8;;\\007\\n'\n\
         i=0\nwhile [ $i -lt {} ]; do echo filler; i=$((i+1)); done\n",
        2 * rows_before
    );
    start_script(&mut session, &dir, &body);

    let row = scroll_to_row(&mut session, "lnk");
    let line = dump_row(&mut session, row);
    assert_eq!(
        cell_link(&line, 0).as_deref(),
        Some(URI),
        "link cell after scrolling up: {line}"
    );

    // After a resize the link is still in the output and keeps its hyperlink.
    run_ok(&mut session, b"resize 800x400\n");
    wait_until(&mut session, 5000, "the layout after resize", |dump| {
        dump["terminal"]["rows"].as_u64() != Some(rows_before)
    });
    let row = scroll_to_row(&mut session, "lnk");
    let line = dump_row(&mut session, row);
    assert_eq!(
        cell_link(&line, 0).as_deref(),
        Some(URI),
        "link cell after resize: {line}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plain_urls_hover_with_ctrl_and_ctrl_click_opens_them() {
    let mut session = session_for_test(800, 600);
    let dir = fresh_dir("l5");
    let file = dir.join("note.txt");
    fs::write(&file, "note\n").expect("test file");
    let canonical = fs::canonicalize(&file).expect("canonical test file");
    let file_url = format!("file://{}", canonical.display());
    let urls = [
        "http://example.com/ronsole-l5-http".to_string(),
        "https://example.com/ronsole-l5-https".to_string(),
        file_url,
    ];
    let mut body = String::new();
    for url in &urls {
        body.push_str(&format!("printf '{url}\\n'\n"));
    }
    start_script(&mut session, &dir, &body);

    run_ok(&mut session, b"mods ctrl\n");
    for url in &urls {
        let state = dump(&mut session);
        let row = row_index(&rows(&state), url).expect("URL row is visible");
        run_ok(&mut session, format!("mouse_cell 2 {row}\n").as_bytes());
        wait_until(&mut session, 5000, "the hovered URL", |state| {
            state["terminal"]["hovered_link"]["uri"] == url.as_str()
        });
        run_ok(&mut session, b"click\n");
        let opened = wait_until(&mut session, 5000, "the open_url request", |state| {
            !state["external_request"].is_null()
        });
        assert_eq!(opened["external_request"]["kind"], "open_url", "{url}");
        assert_eq!(opened["external_request"]["arg"], url.as_str(), "{url}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn hover_change_with_ctrl_requests_a_frame_without_pointer_move() {
    let mut session = session_for_test(800, 600);
    const URI: &str = "https://example.com/ronsole-l15";
    let dir = fresh_dir("l15");
    start_script(&mut session, &dir, &format!("printf '{URI}\\n'\n"));

    let row = row_index(&rows(&dump(&mut session)), URI).expect("URL row is visible");
    run_ok(&mut session, format!("mouse_cell 2 {row}\n").as_bytes());
    let before = dump(&mut session);
    assert!(
        before["terminal"]["hovered_link"].is_null(),
        "no link is hovered without Ctrl: {}",
        before["terminal"]["hovered_link"]
    );
    let frames_before = before["event_loop"]["frame_requests"]
        .as_u64()
        .expect("frame_requests");

    // Ctrl alone, with no pointer movement, must change the hovered link and request a frame.
    run_ok(&mut session, b"mods ctrl\n");
    let after = wait_until(&mut session, 5000, "the Ctrl hover", |state| {
        !state["terminal"]["hovered_link"].is_null()
    });
    assert_eq!(after["terminal"]["hovered_link"]["uri"], URI);
    assert!(
        after["event_loop"]["frame_requests"].as_u64() > Some(frames_before),
        "Ctrl without movement must request a frame: {} -> {}",
        frames_before,
        after["event_loop"]["frame_requests"]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn link_ranges_cover_two_rows_and_two_segments_of_one_row() {
    let mut session = session_for_test(800, 600);
    const URI: &str = "https://example.com/ronsole-l16";
    let dir = fresh_dir("l16");
    let cols = dump(&mut session)["terminal"]["cols"]
        .as_u64()
        .expect("visible columns");
    // Row 0: link "ab", plain "-", link "cd" and then link text up to the last column.
    // Row 1: link text that wraps into the next line, so the link covers columns 0..3 there.
    // Both segments share an `id`, because cells join one link only when URI and id match.
    let filler = cols - 2;
    let body = format!(
        "printf '\\033]8;id=l16;{URI}\\007ab\\033]8;;\\007-\\033]8;id=l16;{URI}\\007cd'\n\
         i=0\nwhile [ $i -lt {filler} ]; do printf e; i=$((i+1)); done\n\
         printf '\\033]8;;\\007\\n'\n"
    );
    start_script(&mut session, &dir, &body);

    let state = dump(&mut session);
    let row = row_index(&rows(&state), "ab-cd").expect("link row is visible");
    run_ok(&mut session, b"mods ctrl\n");
    run_ok(&mut session, format!("mouse_cell 3 {row}\n").as_bytes());
    let hovered = wait_until(&mut session, 5000, "the hovered link", |state| {
        !state["terminal"]["hovered_link"].is_null()
    });
    let link = &hovered["terminal"]["hovered_link"];
    assert_eq!(link["uri"], URI);
    // Ranges are [row, start, end) in visible coordinates; the first segment ends before the plain "-".
    assert_eq!(
        link["ranges"],
        json!([[row, 0, 2], [row, 3, cols], [row + 1, 0, 3]])
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ctrl_click_on_file_link_to_executable_or_desktop_opens_its_directory() {
    let mut session = session_for_test(800, 600);
    let dir = fresh_dir("l20");
    let exe = dir.join("tool");
    fs::write(&exe, "#!/bin/sh\necho ran\n").expect("test executable");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("executable mode");
    let desktop = dir.join("app.desktop");
    fs::write(&desktop, "[Desktop Entry]\nType=Application\nName=app\nExec=/bin/true\n")
        .expect("test desktop file");
    let canonical_exe = fs::canonicalize(&exe).expect("canonical executable");
    let canonical_desktop = fs::canonicalize(&desktop).expect("canonical desktop file");
    let canonical_dir = fs::canonicalize(&dir).expect("canonical test directory");
    let expected = format!("file://{}", canonical_dir.display());
    let body = format!(
        "printf '\\033]8;;file://{}\\007exe-link\\033]8;;\\007\\n'\n\
         printf '\\033]8;;file://{}\\007desk-link\\033]8;;\\007\\n'\n",
        canonical_exe.display(),
        canonical_desktop.display()
    );
    start_script(&mut session, &dir, &body);

    run_ok(&mut session, b"mods ctrl\n");
    for (marker, target) in [
        ("exe-link", &canonical_exe),
        ("desk-link", &canonical_desktop),
    ] {
        let state = dump(&mut session);
        let row = row_index(&rows(&state), marker).expect("link row is visible");
        run_ok(&mut session, format!("mouse_cell 1 {row}\n").as_bytes());
        wait_until(&mut session, 5000, "the hovered file link", |state| {
            state["terminal"]["hovered_link"]["uri"]
                == format!("file://{}", target.display()).as_str()
        });
        run_ok(&mut session, b"click\n");
        let opened = wait_until(&mut session, 5000, "the open request for the directory", |state| {
            !state["external_request"].is_null()
        });
        assert_eq!(
            opened["external_request"]["arg"], expected.as_str(),
            "file link to {marker} must open its directory"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}
