//! Acceptance scenarios of split panes through the headless protocol: the
//! `key` shortcuts that split, focus and close panes, the `tabs`/`active_pane`/
//! `panels` sections of `dump`, and screenshots of the 2- and 3-pane layouts.
//! Expected geometry comes from the split rules of the design (§8.2): whole
//! pixels separated by `GRID_TILE_GAP` scaled by the dump `scale`.

use super::session::{FrameBudget, HeadlessSession, HzSource, ProfileChoice, SessionOptions};
use super::tests::{TEST_BACKGROUND, run_script, wait_until};
use crate::panels::GRID_TILE_GAP;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Physical framebuffer of the acceptance screenshots (plan Task 10).
const ACCEPTANCE_SIZE: (u32, u32) = (2560, 1440);
/// UI scale of the acceptance screenshots (plan Task 10).
const ACCEPTANCE_SCALE: f32 = 1.333;

/// Gives each test its own profile and screenshot directories within the process.
static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// Fresh directory unique to this process and `tag`.
fn fresh_dir(tag: &str) -> PathBuf {
    let index = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ronsole-split-{}-{tag}-{index}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("test directory");
    dir
}

/// Session at the acceptance size and scale, running `/bin/sh`.
fn acceptance_session() -> HeadlessSession {
    crate::prefer_egl_vendor();
    let dir = fresh_dir("profile");
    let [red, green, blue] = TEST_BACKGROUND;
    fs::write(
        dir.join("config"),
        format!("terminal_background=#{red:02X}{green:02X}{blue:02X}\n"),
    )
    .expect("test profile config");
    let options = SessionOptions {
        size: ACCEPTANCE_SIZE,
        scale: ACCEPTANCE_SCALE,
        profile: ProfileChoice::Dir(dir),
        shell: Some(PathBuf::from("/bin/sh")),
        budget: FrameBudget {
            budget_ms: 1000.0 / 240.0,
            hz: 240.0,
            source: HzSource::Default,
        },
    };
    let mut session = HeadlessSession::start(options).expect("acceptance session starts");
    session.remove_profile_on_drop();
    session
}

/// Runs `script` and panics if any response is an `err`; returns the responses.
fn run_ok(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let lines = run_script(session, script);
    assert!(
        lines.iter().all(|line| !line.starts_with("err")),
        "unexpected err in {lines:?}"
    );
    lines
}

/// `(x, y, w, h)` of one `panels[]` entry.
fn panel_rect(panel: &Value) -> (f64, f64, f64, f64) {
    let number = |key: &str| {
        panel[key]
            .as_f64()
            .unwrap_or_else(|| panic!("panel {key} is a number: {panel}"))
    };
    (number("x"), number("y"), number("w"), number("h"))
}

/// Pane id of one `panels[]` entry.
fn pane_id(panel: &Value) -> u64 {
    panel["pane_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("panel pane_id is a number: {panel}"))
}

/// Asserts that no two panel bodies share a pixel.
fn assert_no_overlap(panels: &[Value]) {
    for (index, first) in panels.iter().enumerate() {
        for second in &panels[index + 1..] {
            let (ax, ay, aw, ah) = panel_rect(first);
            let (bx, by, bw, bh) = panel_rect(second);
            let horizontal = ax < bx + bw && bx < ax + aw;
            let vertical = ay < by + bh && by < ay + ah;
            assert!(
                !(horizontal && vertical),
                "panels overlap: {first} and {second}"
            );
        }
    }
}

/// Gap between siblings in pixels for the dump's scale, snapped to whole
/// pixels exactly like the split layout.
fn expected_gap(dump: &Value) -> f64 {
    let scale = dump["scale"].as_f64().expect("dump scale is a number");
    (f64::from(GRID_TILE_GAP) * scale).round()
}

/// Polls `dump` until `count` panels with a non-zero body are laid out.
fn wait_for_panels(session: &mut HeadlessSession, count: usize, what: &str) -> Value {
    wait_until(session, 5000, what, |dump| {
        dump["panels"].as_array().is_some_and(|panels| {
            panels.len() == count
                && panels.iter().all(|panel| {
                    panel["w"].as_f64().unwrap_or(0.0) > 0.0
                        && panel["h"].as_f64().unwrap_or(0.0) > 0.0
                })
        })
    })
}

/// Asserts that a split did not change the tab strip: one tab, numbered 1.
fn assert_single_tab(dump: &Value) {
    let tabs = dump["tabs"].as_array().expect("dump tabs is an array");
    assert_eq!(tabs.len(), 1, "a split stays in one tab: {tabs:?}");
    assert_eq!(tabs[0]["index"], 0);
    assert_eq!(tabs[0]["active"], true);
    assert_eq!(tabs[0]["title_number"], 1);
    assert!(tabs[0]["title"].is_string(), "tab title is a string");
}

/// Runs `settle 500`, takes `screenshot` of `name` into `shots`, and
/// asserts the reply and the PNG file describe the whole framebuffer.
fn take_screenshot(
    session: &mut HeadlessSession,
    shots: &Path,
    name: &str,
    size: (u32, u32),
) -> PathBuf {
    run_ok(session, b"settle 500\n");
    let path = shots.join(name);
    let _ = fs::remove_file(&path);
    let lines = run_ok(
        session,
        format!("screenshot {}\n", path.display()).as_bytes(),
    );
    let payload = lines[0]
        .strip_prefix("ok ")
        .unwrap_or_else(|| panic!("screenshot answers `ok <path> <w>x<h>`, got {lines:?}"));
    let (reported, dimensions) = payload
        .rsplit_once(' ')
        .unwrap_or_else(|| panic!("screenshot payload has a path and a size: {payload}"));
    assert_eq!(
        Path::new(reported),
        fs::canonicalize(&path).expect("screenshot file exists")
    );
    assert_eq!(dimensions, format!("{}x{}", size.0, size.1));
    let bytes = fs::read(&path).expect("screenshot bytes");
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "{} is a PNG",
        path.display()
    );
    path
}

#[test]
fn headless_split_panes_screenshot_and_dump() {
    let shots = fresh_dir("shots");
    let mut session = acceptance_session();

    // Start from one pane: the dump reports the tab, its single number and the
    // active pane of the tab's tree. The acceptance framebuffer and scale come
    // from the plan, not from the implementation.
    let single = wait_for_panels(&mut session, 1, "one pane before any split");
    assert_single_tab(&single);
    assert_eq!(
        single["size"][0].as_u64(),
        Some(u64::from(ACCEPTANCE_SIZE.0))
    );
    assert_eq!(
        single["size"][1].as_u64(),
        Some(u64::from(ACCEPTANCE_SIZE.1))
    );
    let scale = single["scale"].as_f64().expect("dump scale is a number");
    assert!(
        (scale - f64::from(ACCEPTANCE_SCALE)).abs() < 1e-6,
        "acceptance scale is {scale}"
    );
    let first_pane = pane_id(&single["panels"][0]);
    assert_eq!(single["active_pane"].as_u64(), Some(first_pane));
    assert_eq!(single["panels"][0]["content_hidden"], false);

    // Ctrl+Shift+D splits the active pane side by side (design §3 T1).
    run_ok(&mut session, b"key ctrl+shift+d\n");
    let side_by_side = wait_for_panels(&mut session, 2, "two panes after ctrl+shift+d");
    assert_single_tab(&side_by_side);
    let panels = side_by_side["panels"].as_array().expect("panels");
    assert_no_overlap(panels);
    let gap = expected_gap(&side_by_side);
    assert_eq!(
        gap,
        (f64::from(GRID_TILE_GAP) * f64::from(ACCEPTANCE_SCALE)).round(),
        "the divider gap follows the acceptance scale"
    );
    let (left_x, left_y, left_w, left_h) = panel_rect(&panels[0]);
    let (right_x, right_y, _right_w, right_h) = panel_rect(&panels[1]);
    assert_eq!(left_y, right_y, "side-by-side panes share a top edge");
    assert_eq!(left_h, right_h, "side-by-side panes share a height");
    assert_eq!(
        right_x,
        left_x + left_w + gap,
        "the horizontal divider is exactly the gap"
    );
    assert_eq!(
        pane_id(&panels[1]),
        side_by_side["active_pane"].as_u64().expect("active pane"),
        "the new pane takes the focus"
    );
    take_screenshot(&mut session, &shots, "split-2-side-by-side.png", ACCEPTANCE_SIZE);

    // Ctrl+Shift+E splits the focused pane into a stacked column (3 panes).
    run_ok(&mut session, b"key ctrl+shift+e\n");
    let nested = wait_for_panels(&mut session, 3, "three panes after ctrl+shift+e");
    assert_single_tab(&nested);
    let panels = nested["panels"].as_array().expect("panels");
    assert_no_overlap(panels);
    let gap = expected_gap(&nested);
    let (column_x, column_y, column_w, column_h) = panel_rect(&panels[0]);
    let (top_x, top_y, top_w, top_h) = panel_rect(&panels[1]);
    let (bottom_x, bottom_y, bottom_w, bottom_h) = panel_rect(&panels[2]);
    assert_eq!(column_y, top_y, "the left column starts at the body top");
    assert_eq!(
        column_h,
        top_h + gap + bottom_h,
        "the right column covers the body height and its gap"
    );
    assert_eq!(
        top_x,
        column_x + column_w + gap,
        "the vertical divider is exactly the gap"
    );
    assert_eq!(top_x, bottom_x, "stacked panes share the column x");
    assert_eq!(top_w, bottom_w, "stacked panes share the column width");
    assert_eq!(
        bottom_y,
        top_y + top_h + gap,
        "the horizontal divider is exactly the gap"
    );
    assert_eq!(
        pane_id(&panels[2]),
        nested["active_pane"].as_u64().expect("active pane"),
        "the newest pane takes the focus"
    );
    take_screenshot(&mut session, &shots, "split-3-nested.png", ACCEPTANCE_SIZE);

    // Ctrl+Shift+W closes the focused pane and gives focus to its neighbour.
    run_ok(&mut session, b"key ctrl+shift+w\n");
    let after_close = wait_for_panels(&mut session, 2, "two panes after ctrl+shift+w");
    assert_single_tab(&after_close);
    let panels = after_close["panels"].as_array().expect("panels");
    assert_no_overlap(panels);
    let remaining: Vec<u64> = panels.iter().map(pane_id).collect();
    assert_eq!(
        remaining,
        vec![pane_id(&nested["panels"][0]), pane_id(&nested["panels"][1])],
        "closing the stacked pane keeps the left pane and the right top"
    );
    let focused = after_close["active_pane"]
        .as_u64()
        .expect("focus moved to the closed pane's neighbour");
    assert_eq!(focused, remaining[1], "focus goes to the tree successor");

    // Alt+Left moves the focus to the pane on the left.
    run_ok(&mut session, b"key alt+left\n");
    let focused_left = wait_until(&mut session, 5000, "alt+left focuses the left pane", |dump| {
        dump["active_pane"].as_u64() == Some(remaining[0])
    });
    assert_eq!(focused_left["panels"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        focused_left["panels"][0]["pane_id"].as_u64(),
        Some(remaining[0])
    );
}
