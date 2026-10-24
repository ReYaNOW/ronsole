//! Test scaffolding for the headless module: unit tests of its helpers and the
//! session helpers used by the protocol tests.

use super::*;
use super::session::{FrameBudget, HeadlessSession, HzSource, ProfileChoice, SessionOptions, serve};
use std::io::Cursor;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Terminal background written into every session profile of the tests.
pub(crate) const TEST_BACKGROUND: [u8; 3] = [0x12, 0x34, 0x56];

/// Gives each test session its own profile directory name within the process.
static NEXT_PROFILE: AtomicU32 = AtomicU32::new(0);

/// Serializes tests that use `Temp` profiles, whose names share the process id.
pub(crate) static TEMP_PROFILE_LOCK: Mutex<()> = Mutex::new(());

/// Starts a session of `width` x `height` physical pixels on `/bin/sh` with
/// `TEST_BACKGROUND` as its terminal background.
pub(crate) fn session_for_test(width: u32, height: u32) -> HeadlessSession {
    session_with_shell(width, height, Some(PathBuf::from("/bin/sh")))
}

/// Starts a session like `session_for_test`, but running `shell` (`None` leaves the choice to the app).
pub(crate) fn session_with_shell(
    width: u32,
    height: u32,
    shell: Option<PathBuf>,
) -> HeadlessSession {
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
        shell,
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

/// Runs `dump` and returns its parsed JSON; panics unless the response is `ok <json>`.
pub(crate) fn dump(session: &mut HeadlessSession) -> serde_json::Value {
    let lines = run_script(session, b"dump\n");
    let json = lines
        .first()
        .and_then(|line| line.strip_prefix("ok "))
        .unwrap_or_else(|| panic!("dump must answer `ok <json>`, got {lines:?}"));
    serde_json::from_str(json).expect("dump payload is JSON")
}

/// Polls `dump` every 50 ms until `done` accepts it. Panics at `timeout_ms` with `what`
/// and the last dump.
pub(crate) fn wait_until(
    session: &mut HeadlessSession,
    timeout_ms: u64,
    what: &str,
    mut done: impl FnMut(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let value = dump(session);
        if done(&value) {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}: {value}");
        }
        run_script(session, b"wait 50\n");
    }
}

/// Runs `wake <ms>` and returns its response with the instant the step started.
// Consumed by the wake tests of later protocol tasks; no test in this task calls it yet.
#[allow(dead_code)]
pub(crate) fn native_wake(session: &mut HeadlessSession, ms: u64) -> (String, Instant) {
    let started = Instant::now();
    let lines = run_script(session, format!("wake {ms}\n").as_bytes());
    (
        lines.into_iter().next().expect("wake answers one line"),
        started,
    )
}

/// Pixel rectangle `(x0, y0, x1, row_bottom)` of cell `(col, row)` of the visible area, from
/// the `terminal.layout` of `dump`. Left and top edges round up and right and bottom edges
/// round down, so a point with `x0 <= x < x1` and `y0 <= y < row_bottom` lies inside the cell.
pub(crate) fn cell_rect(dump: &serde_json::Value, col: usize, row: usize) -> (i32, i32, i32, i32) {
    let layout = &dump["terminal"]["layout"];
    let cell0_x = layout["cell0"][0].as_f64().expect("layout cell0 x");
    let cell0_y = layout["cell0"][1].as_f64().expect("layout cell0 y");
    let char_w = layout["char_w"].as_f64().expect("layout char_w");
    let char_h = layout["char_h"].as_f64().expect("layout char_h");
    let left = cell0_x + col as f64 * char_w;
    let top = cell0_y + row as f64 * char_h;
    (
        left.ceil() as i32,
        top.ceil() as i32,
        (left + char_w).floor() as i32,
        (top + char_h).floor() as i32,
    )
}

/// Decoded RGBA pixels of a `screenshot`.
pub(crate) struct TestImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Takes a `screenshot` into a file unique to this process and decodes the PNG.
pub(crate) fn screenshot(session: &mut HeadlessSession) -> TestImage {
    let index = NEXT_PROFILE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "ronsole-t4-shot-{}-{index}.png",
        std::process::id()
    ));
    let lines = run_script(
        session,
        format!("screenshot {}\n", path.display()).as_bytes(),
    );
    assert!(
        lines.first().is_some_and(|line| line.starts_with("ok ")),
        "screenshot must answer ok, got {lines:?}"
    );
    let file = fs::File::open(&path).expect("screenshot file exists");
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("screenshot PNG header");
    let mut rgba = vec![0; reader.output_buffer_size().expect("PNG buffer size")];
    let info = reader.next_frame(&mut rgba).expect("screenshot PNG frame");
    rgba.truncate(info.buffer_size());
    let _ = fs::remove_file(&path);
    assert_eq!(info.color_type, png::ColorType::Rgba);
    TestImage {
        width: info.width,
        height: info.height,
        rgba,
    }
}

/// Colour of the pixel at `(x, y)` of `image`; panics outside the image.
pub(crate) fn pixel_rgb(image: &TestImage, (x, y): (i32, i32)) -> [u8; 3] {
    assert!(
        x >= 0 && y >= 0 && (x as u32) < image.width && (y as u32) < image.height,
        "pixel ({x}, {y}) is outside {}x{}",
        image.width,
        image.height
    );
    let offset = (y as usize * image.width as usize + x as usize) * 4;
    [image.rgba[offset], image.rgba[offset + 1], image.rgba[offset + 2]]
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
