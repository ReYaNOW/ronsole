//! Tests of the `dump`, `dump row` and `mouse_cell` protocol commands, read through the
//! test helpers of `tests.rs`. Expected values come from the spec (§5.1, §5.2, Q24) and
//! from the commands' inputs, never from the implementation's output.

use super::session::HeadlessSession;
use super::tests::{
    TEST_BACKGROUND, cell_rect, dump, pixel_rgb, run_script, screenshot, session_for_test,
    session_with_shell, wait_until,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Runs `script` and asserts that no response is an `err`; returns the responses.
fn run_ok(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let lines = run_script(session, script);
    assert!(
        lines.iter().all(|line| !line.starts_with("err")),
        "unexpected err in {lines:?}"
    );
    lines
}

/// Index of the visible row whose text contains `marker`.
fn row_containing(dump: &Value, marker: &str) -> Option<usize> {
    dump["terminal"]["text"].as_array()?.iter().position(|row| {
        row.as_str().is_some_and(|text| text.contains(marker))
    })
}

/// Index of the visible row whose trimmed text is exactly `text`.
fn row_equal_to(dump: &Value, text: &str) -> Option<usize> {
    dump["terminal"]["text"].as_array()?.iter().position(|row| {
        row.as_str().is_some_and(|row| row.trim() == text)
    })
}

/// Parses the `ok <json>` response of `dump row <row>`.
fn dump_row(session: &mut HeadlessSession, row: usize) -> Value {
    let lines = run_ok(session, format!("dump row {row}\n").as_bytes());
    let json = lines[0]
        .strip_prefix("ok ")
        .unwrap_or_else(|| panic!("dump row answers `ok <json>`, got {lines:?}"));
    serde_json::from_str(json).expect("dump row payload is JSON")
}

/// Parses the `ok <x> <y>` response of `mouse_cell`.
fn mouse_cell_point(session: &mut HeadlessSession, col: usize, row: usize) -> (i32, i32) {
    let lines = run_ok(session, format!("mouse_cell {col} {row}\n").as_bytes());
    let mut words = lines[0].split(' ');
    assert_eq!(words.next(), Some("ok"), "mouse_cell answers ok: {lines:?}");
    let x = words.next().and_then(|word| word.parse().ok()).expect("x");
    let y = words.next().and_then(|word| word.parse().ok()).expect("y");
    (x, y)
}

#[test]
fn dump_text_cursor_and_title() {
    let mut session = session_for_test(800, 600);
    // Without `sleep` the shell prints its prompt on the marker line and moves the cursor.
    run_ok(
        &mut session,
        br#"type printf 'RE''ADY'; sleep 30"#,
    );
    run_ok(&mut session, b"key enter\n");
    let dump = wait_until(&mut session, 5000, "READY in terminal.text", |dump| {
        row_containing(dump, "READY").is_some()
    });
    // The tab shows the foreground process name until a title is set, so only the
    // terminal title and the tab title must agree, not a particular string.
    wait_until(&mut session, 5000, "terminal title matches tab", |d| {
        let t = d["terminal"]["title"].as_str().unwrap_or("");
        !t.is_empty() && Some(t) == d["tabs"][0]["title"].as_str()
    });
    let row = row_containing(&dump, "READY").expect("marker row");
    assert_eq!(dump["terminal"]["cursor"]["row"], row);
    assert_eq!(dump["terminal"]["cursor"]["col"], 5);
}

#[test]
fn viewport_scrollback_and_wheel() {
    let mut session = session_for_test(800, 600);
    let initial = dump(&mut session);
    let rows = initial["terminal"]["rows"].as_u64().expect("rows") as usize;
    let last = 3 * rows;
    run_ok(&mut session, format!("type seq 1 {last}\n").as_bytes());
    run_ok(&mut session, b"key enter\n");
    let before = wait_until(&mut session, 10_000, "seq output at the bottom", |dump| {
        dump["terminal"]["viewport"]["scrollback"].as_u64().unwrap_or(0) > 0
            && row_equal_to(dump, &last.to_string()).is_some()
    });
    let total = before["terminal"]["viewport"]["total"].as_u64().expect("total");
    let top_before = before["terminal"]["viewport"]["top"].as_u64().expect("top");
    assert_eq!(top_before, total - rows as u64, "viewport sits at the bottom");
    let first_before: u64 = before["terminal"]["text"][0]
        .as_str()
        .expect("text row 0")
        .trim()
        .parse()
        .expect("seq number in row 0");

    run_ok(&mut session, b"mouse_cell 0 0\n");
    run_ok(&mut session, b"wheel 0 1\n");
    // One wheel line is four terminal rows, so the viewport moves exactly four rows up.
    let after = wait_until(&mut session, 5000, "viewport four rows up", |dump| {
        dump["terminal"]["viewport"]["top"].as_u64() == Some(top_before - 4)
    });
    let first_after: u64 = after["terminal"]["text"][0]
        .as_str()
        .expect("text row 0")
        .trim()
        .parse()
        .expect("seq number in row 0");
    assert_eq!(first_after, first_before - 4);

    let dir = std::env::temp_dir().join(format!("ronsole-t4-dump-{}", std::process::id()));
    let path = dir.join("d.json");
    let lines = run_ok(
        &mut session,
        format!("dump {}\n", path.display()).as_bytes(),
    );
    let payload = lines[0].strip_prefix("ok ").expect("dump path answer");
    assert_eq!(
        Path::new(payload),
        fs::canonicalize(&path).expect("dump file exists")
    );
    let file: Value = serde_json::from_str(&fs::read_to_string(&path).expect("read dump file"))
        .expect("dump file is JSON");
    let same_state = dump(&mut session);
    assert_eq!(file["terminal"], same_state["terminal"]);
    assert_eq!(file["size"], same_state["size"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn alt_screen_has_no_scrollback() {
    let mut session = session_for_test(800, 600);
    run_ok(&mut session, b"type seq 1 300\n");
    run_ok(&mut session, b"key enter\n");
    wait_until(&mut session, 10_000, "scrollback from seq", |dump| {
        dump["terminal"]["viewport"]["scrollback"].as_u64().unwrap_or(0) > 0
    });
    run_ok(
        &mut session,
        br#"type printf '\033[?1049h'; sleep 30"#,
    );
    run_ok(&mut session, b"key enter\n");
    let dump = wait_until(&mut session, 5000, "alt screen without scrollback", |dump| {
        dump["terminal"]["viewport"]["scrollback"] == 0
    });
    assert_eq!(dump["terminal"]["viewport"]["top"], 0);
    let rows = dump["terminal"]["rows"].as_u64().expect("rows");
    let cursor_row = dump["terminal"]["cursor"]["row"].as_u64().expect("cursor row");
    assert!(cursor_row < rows, "cursor stays on the visible rows");
    run_ok(&mut session, b"mouse_cell 0 0\n");
}

#[test]
fn dump_row_colors_and_wide() {
    let mut session = session_for_test(800, 600);
    run_ok(
        &mut session,
        "type printf '\\033[1;38;2;200;10;20mR\\033[0m\u{754c}'; sleep 30".as_bytes(),
    );
    run_ok(&mut session, b"key enter\n");
    // The echoed command also contains the wide character, but never `R` directly before it,
    // so the marker `R界` finds the output row and not the typed line.
    let marker = "R\u{754c}";
    let shown = wait_until(&mut session, 5000, "R and the wide character in terminal.text", |dump| {
        row_containing(dump, marker).is_some()
    });
    let row = row_containing(&shown, marker).expect("row with the output");

    let row_dump = dump_row(&mut session, row);
    assert_eq!(row_dump["row"], row);
    let cells = row_dump["cells"].as_array().expect("cells");
    let red = cells
        .iter()
        .find(|cell| cell["ch"] == "R")
        .expect("cell R is dumped");
    assert_eq!(red["fg"], "#c80a14");
    assert!(red["bg"].is_null());
    assert!(
        red["flags"].as_array().expect("flags").iter().any(|flag| flag == "bold"),
        "R is bold: {red}"
    );
    let wide_col = cells
        .iter()
        .find(|cell| cell["ch"] == "界")
        .and_then(|cell| cell["col"].as_u64())
        .expect("wide character is dumped") as usize;
    let wide = cells
        .iter()
        .find(|cell| cell["col"] == wide_col)
        .expect("wide cell");
    assert!(wide["flags"].as_array().expect("flags").iter().any(|flag| flag == "wide"));
    let spacer = cells
        .iter()
        .find(|cell| cell["col"] == wide_col + 1)
        .expect("spacer cell after the wide character");
    assert!(
        spacer["flags"]
            .as_array()
            .expect("flags")
            .iter()
            .any(|flag| flag == "wide_spacer")
    );

    let latest = dump(&mut session);
    let (x, y) = mouse_cell_point(&mut session, wide_col, row);
    // The wide cell's rectangle comes from the latest dump, which is taken after the pointer moved.
    let (x0, y0, x1, bottom) = cell_rect(&latest, wide_col, row);
    assert!(
        x0 <= x && x < x1 && y0 <= y && y < bottom,
        "pointer ({x}, {y}) is inside the wide cell {x0},{y0}..{x1},{bottom}"
    );

    // Negative case: a row beyond the visible area is an error, and the session goes on.
    let lines = run_script(&mut session, b"dump row 999\n");
    assert_eq!(lines, vec!["err row out of range".to_string()]);
    run_ok(&mut session, b"mouse_cell 0 0\n");
}

#[test]
fn mouse_cell_matches_cell_rect() {
    let mut session = session_for_test(800, 600);
    let latest = dump(&mut session);
    let (x, y) = mouse_cell_point(&mut session, 2, 1);
    let (x0, y0, x1, bottom) = cell_rect(&latest, 2, 1);
    assert!(
        x0 <= x && x < x1 && y0 <= y && y < bottom,
        "pointer ({x}, {y}) is inside cell (2, 1) {x0},{y0}..{x1},{bottom}"
    );
}

#[test]
fn mouse_cell_outside_the_visible_area_is_err() {
    let mut session = session_for_test(800, 600);
    let latest = dump(&mut session);
    let cols = latest["terminal"]["cols"].as_u64().expect("cols");
    let rows = latest["terminal"]["rows"].as_u64().expect("rows");
    for script in [
        format!("mouse_cell 9999 0\n"),
        format!("mouse_cell 0 {rows}\n"),
        format!("mouse_cell {cols} 0\n"),
    ] {
        let lines = run_script(&mut session, script.as_bytes());
        assert!(lines[0].starts_with("err "), "`{script}` must be err: {lines:?}");
    }
    // Empty and garbage arguments are protocol errors, not a crash.
    for script in ["mouse_cell\n", "mouse_cell x 1\n", "mouse_cell 2 -1\n"] {
        let lines = run_script(&mut session, script.as_bytes());
        assert!(lines[0].starts_with("err "), "`{script}` must be err: {lines:?}");
    }
    run_ok(&mut session, b"mouse_cell 0 0\n");
}

#[test]
fn mouse_cell_on_clipped_bottom_row_is_err() {
    let mut session = session_for_test(800, 600);
    run_ok(&mut session, b"type seq 1 300\n");
    run_ok(&mut session, b"key enter\n");
    let initial = wait_until(&mut session, 10_000, "scrollback from seq", |dump| {
        dump["terminal"]["viewport"]["scrollback"].as_u64().unwrap_or(0) > 0
    });
    let rows = initial["terminal"]["rows"].as_u64().expect("rows") as usize;
    let layout = &initial["terminal"]["layout"];
    let char_h = layout["char_h"].as_f64().expect("char_h");
    let body = &layout["body"];
    let body_bottom = body[1].as_f64().expect("body y") + body[3].as_f64().expect("body h");
    let cell0_y = layout["cell0"][1].as_f64().expect("cell0 y");
    // Scroll up by the distance that moves the centre of the bottom row to the bottom edge of
    // the body; the scroll stays below one row, so the bottom row is the same row as before.
    let bottom_centre = cell0_y + (rows - 1) as f64 * char_h + char_h / 2.0;
    let shift = (body_bottom - bottom_centre).ceil();
    assert!(
        shift > 0.0 && shift < char_h,
        "precondition: the scroll shift {shift} lies within one row ({char_h})"
    );
    run_ok(&mut session, b"mouse_cell 0 0\n");
    run_ok(&mut session, format!("wheel 0 {shift} px\n").as_bytes());
    run_ok(&mut session, b"settle 1000\n");
    let scrolled = dump(&mut session);
    let scrolled_layout = &scrolled["terminal"]["layout"];
    let scrolled_cell0_y = scrolled_layout["cell0"][1].as_f64().expect("cell0 y");
    let clipped_centre = scrolled_cell0_y + (rows - 1) as f64 * char_h + char_h / 2.0;
    assert!(
        clipped_centre >= body_bottom,
        "precondition: the bottom row is clipped ({clipped_centre} >= {body_bottom})"
    );
    let lines = run_script(&mut session, format!("mouse_cell 0 {}\n", rows - 1).as_bytes());
    assert!(lines[0].starts_with("err "), "clipped bottom row must be err: {lines:?}");
    run_ok(&mut session, format!("mouse_cell 0 {}\n", rows - 2).as_bytes());
}

#[test]
fn background_pixel_matches_config() {
    let mut session = session_for_test(800, 600);
    run_ok(
        &mut session,
        br#"type printf '\033[48;2;10;200;30m  \033[0m'; sleep 30"#,
    );
    run_ok(&mut session, b"key enter\n");
    let dump = wait_until(&mut session, 5000, "two coloured cells on the cursor row", |dump| {
        dump["terminal"]["cursor"]["col"] == 2
    });
    let row = dump["terminal"]["cursor"]["row"].as_u64().expect("cursor row") as usize;
    let (x0, y0, x1, bottom) = cell_rect(&dump, 0, row);
    let centre = ((x0 + x1) / 2, (y0 + bottom) / 2);
    let below = cell_rect(&dump, 0, row + 1);
    let below_centre = ((below.0 + below.2) / 2, (below.1 + below.3) / 2);

    let image = screenshot(&mut session);
    let coloured = pixel_rgb(&image, centre);
    for (channel, expected) in coloured.iter().zip([10u8, 200, 30]) {
        assert!(channel.abs_diff(expected) <= 2, "{coloured:?} vs {:?}", [10, 200, 30]);
    }
    let background = pixel_rgb(&image, below_centre);
    for (channel, expected) in background.iter().zip(TEST_BACKGROUND) {
        assert!(channel.abs_diff(expected) <= 2, "{background:?} vs {TEST_BACKGROUND:?}");
    }
}

#[test]
fn frame_requests_grow_on_output() {
    let mut session = session_for_test(800, 600);
    let before = dump(&mut session);
    run_ok(&mut session, b"type x\n");
    let after = dump(&mut session);
    assert!(
        after["event_loop"]["frame_requests"].as_u64() > before["event_loop"]["frame_requests"].as_u64(),
        "typing requests a frame: {before} -> {after}"
    );
    assert!(after["cursor_icon"].is_string());
    let tabs = after["tabs"].as_array().expect("tabs");
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0]["active"], true);
    assert!(!tabs[0]["rect"].is_null(), "the tab has a rect after a frame");
}

#[test]
fn missing_shell_reports_in_grid() {
    let mut session = session_with_shell(1280, 600, Some(PathBuf::from("/nonexistent/sh")));
    wait_until(&mut session, 5000, "the missing shell path in the grid", |dump| {
        row_containing(dump, "/nonexistent/sh").is_some()
    });
}
