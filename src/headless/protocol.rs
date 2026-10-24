//! Headless protocol: one command per input line, one `ok` or `err` line per command.
//!
//! Grammar and response shapes follow RRiter's headless protocol; the keyboard and
//! mouse vocabulary is Ronsole's own.

use super::keys::{KeySpec, key_spec, with_layout_char};
use crate::input_types::Modifiers;
use std::path::PathBuf;

const SIZE_MIN: (u32, u32) = (320, 200);
const SIZE_MAX: (u32, u32) = (8192, 8192);
const SCALE_MIN: f64 = 0.5;
const SCALE_MAX: f64 = 4.0;
const MAX_WAIT_MS: u64 = 60_000;
const DEFAULT_SETTLE_MS: u64 = 500;
const MAX_BENCH_FRAMES: u32 = 100_000;
const MAX_KEY_REPEATS: u32 = 1000;
/// Longest piece of user input echoed back inside an error reason.
const MAX_ECHO_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseButtonArg {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickPhase {
    Both,
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WheelUnit {
    Lines,
    Pixels,
}

/// A key press: the key on the US layout plus the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyCombo {
    pub(crate) key: KeySpec,
    pub(crate) mods: Modifiers,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BenchAction {
    None,
    Wheel { dx: f64, dy: f64 },
    Key(KeyCombo),
    Type(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    Type(String),
    Key { combo: KeyCombo, repeats: u32 },
    Mods(Modifiers),
    MouseMove { x: f64, y: f64 },
    MouseCell { col: usize, row: usize },
    Click { button: MouseButtonArg, phase: ClickPhase, alt: bool },
    Wheel { dx: f64, dy: f64, unit: WheelUnit },
    Resize { w: u32, h: u32 },
    Scale(f32),
    Screenshot(PathBuf),
    Dump(Option<PathBuf>),
    DumpRow(usize),
    Settle { ms: u64 },
    Wait { ms: u64 },
    /// Strict loop model: one native `about_to_wait` wake-up within `ms`.
    Wake { ms: u64 },
    /// Strict loop model: `wake` repeated for `ms` of input-free time.
    Idle { ms: u64 },
    Bench { frames: u32, csv: Option<PathBuf>, action: BenchAction },
    Info,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Response {
    Ok(Option<String>),
    Err(String),
}

impl Response {
    /// One protocol output line without the trailing newline. Line breaks in the payload
    /// and in the reason become spaces; the reason is clipped to `MAX_ECHO_CHARS` characters.
    pub(crate) fn line(&self) -> String {
        match self {
            Response::Ok(None) => "ok".to_string(),
            Response::Ok(Some(payload)) => format!("ok {}", single_line(payload)),
            Response::Err(reason) => format!("err {}", clip(&single_line(reason))),
        }
    }
}

/// Parses one protocol line; `Ok(None)` for blank and `#` comment lines.
pub(crate) fn parse_line(line: &[u8]) -> Result<Option<Command>, String> {
    let line = std::str::from_utf8(line).map_err(|_| "invalid utf-8".to_string())?;
    let mut args = Args { rest: line.trim_end_matches(['\r', '\n']) };
    let Some(name) = args.token() else {
        return Ok(None);
    };
    if name.starts_with('#') {
        return Ok(None);
    }
    let command = match name {
        "screenshot" => Command::Screenshot(path_rest(&mut args)?),
        "dump" => {
            let rest = args.remainder().trim_end();
            match dump_row(rest) {
                Some(row) => Command::DumpRow(row),
                None => Command::Dump((!rest.is_empty()).then(|| PathBuf::from(rest))),
            }
        }
        "type" => Command::Type(unescape_text(args.remainder())),
        "mods" => {
            let mut mods = Modifiers::empty();
            while let Some(token) = args.token() {
                add_modifier(&mut mods, token)?;
            }
            Command::Mods(mods)
        }
        "key" => parse_key(&mut args)?,
        "mouse_move" => {
            let x = parse_f64(args.required("x")?)?;
            let y = parse_f64(args.required("y")?)?;
            args.finish()?;
            Command::MouseMove { x, y }
        }
        "mouse_cell" => {
            let col = parse_uint::<usize>(args.required("column")?)?;
            let row = parse_uint::<usize>(args.required("row")?)?;
            args.finish()?;
            Command::MouseCell { col, row }
        }
        "click" => parse_click(&mut args)?,
        "wheel" => {
            let dx = parse_f64(args.required("dx")?)?;
            let dy = parse_f64(args.required("dy")?)?;
            let unit = match args.token() {
                None | Some("lines") => WheelUnit::Lines,
                Some("px") => WheelUnit::Pixels,
                Some(t) => return Err(format!("invalid wheel unit '{}'", clip(t))),
            };
            args.finish()?;
            Command::Wheel { dx, dy, unit }
        }
        "resize" => {
            let (w, h) = parse_size(args.required("size")?)?;
            args.finish()?;
            Command::Resize { w, h }
        }
        "scale" => {
            let scale = parse_scale(args.required("scale")?)?;
            args.finish()?;
            Command::Scale(scale)
        }
        "settle" => {
            let ms = match args.token() {
                None => DEFAULT_SETTLE_MS,
                Some(t) => parse_ms(t)?,
            };
            args.finish()?;
            Command::Settle { ms }
        }
        "wait" => {
            let ms = parse_ms(args.required("milliseconds")?)?;
            args.finish()?;
            Command::Wait { ms }
        }
        "wake" | "idle" => {
            let ms = parse_ms(args.required("milliseconds")?)?;
            args.finish()?;
            if name == "wake" { Command::Wake { ms } } else { Command::Idle { ms } }
        }
        "bench" => parse_bench(&mut args)?,
        "info" => {
            args.finish()?;
            Command::Info
        }
        "quit" => {
            args.finish()?;
            Command::Quit
        }
        _ => return Err(format!("unknown command '{}'", clip(name))),
    };
    Ok(Some(command))
}

/// Whitespace-separated token cursor over the argument part of a command line.
#[derive(Clone, Copy)]
struct Args<'a> {
    rest: &'a str,
}

impl<'a> Args<'a> {
    fn token(&mut self) -> Option<&'a str> {
        let s = self.rest.trim_start();
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        let (token, rest) = s.split_at(end);
        self.rest = rest;
        (!token.is_empty()).then_some(token)
    }

    fn required(&mut self, what: &str) -> Result<&'a str, String> {
        self.token().ok_or_else(|| format!("missing {what}"))
    }

    /// Rest of the line from its first non-whitespace char; trailing whitespace is kept.
    fn remainder(&mut self) -> &'a str {
        let s = self.rest.trim_start();
        self.rest = "";
        s
    }

    fn finish(&mut self) -> Result<(), String> {
        match self.token() {
            None => Ok(()),
            Some(extra) => Err(format!("unexpected argument '{}'", clip(extra))),
        }
    }
}

/// `dump row <uint>` addresses a row; any other rest of the line is a path.
fn dump_row(rest: &str) -> Option<usize> {
    let mut parts = rest.split_whitespace();
    if parts.next()? != "row" {
        return None;
    }
    let row = parts.next()?;
    if parts.next().is_some() || !row.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    row.parse().ok()
}

/// Applies `\n`, `\t`, `\\` left to right; any other `\x` and a trailing `\` stay verbatim.
fn unescape_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Adds one modifier name to `mods`; `mod` is the primary modifier, which is Ctrl on Linux.
fn add_modifier(mods: &mut Modifiers, token: &str) -> Result<(), String> {
    let flag = match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "mod" => Modifiers::CONTROL,
        "alt" => Modifiers::ALT,
        "shift" => Modifiers::SHIFT,
        "super" | "cmd" | "meta" => Modifiers::SUPER,
        _ => return Err(format!("unknown modifier '{}'", clip(token))),
    };
    if *mods | flag == *mods {
        return Err(format!("duplicate modifier '{}'", clip(token)));
    }
    *mods |= flag;
    Ok(())
}

/// Parses `mod+...+key`; the last `+`-separated token is the key.
fn parse_combo(text: &str) -> Result<KeyCombo, String> {
    let parts: Vec<&str> = text.split('+').collect();
    let Some((key_token, modifier_tokens)) = parts.split_last() else {
        return Err("missing key token".to_string());
    };
    let mut mods = Modifiers::empty();
    for token in modifier_tokens {
        add_modifier(&mut mods, token)?;
    }
    let key = key_spec(&key_token.to_ascii_lowercase())
        .ok_or_else(|| format!("unknown key token '{}'", clip(key_token)))?;
    Ok(KeyCombo { key, mods })
}

/// `key [--repeat N] [--char C] <combo>`; options may come in either order.
fn parse_key(args: &mut Args) -> Result<Command, String> {
    let mut repeats = None;
    let mut layout_char = None;
    let mut token = args.required("key combo")?;
    loop {
        match token {
            "--repeat" => {
                if repeats.is_some() {
                    return Err("duplicate --repeat".to_string());
                }
                let count = parse_uint::<u32>(args.required("repeat count")?)?;
                if count > MAX_KEY_REPEATS {
                    return Err(format!("repeat count exceeds {MAX_KEY_REPEATS}"));
                }
                repeats = Some(count);
            }
            "--char" => {
                if layout_char.is_some() {
                    return Err("duplicate --char".to_string());
                }
                let value = args.required("character")?;
                let mut chars = value.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return Err(format!("--char expects one character, got '{}'", clip(value)));
                };
                layout_char = Some(c);
            }
            _ => break,
        }
        token = args.required("key combo")?;
    }
    let mut combo = parse_combo(token)?;
    if let Some(c) = layout_char {
        combo.key = with_layout_char(combo.key, c);
    }
    args.finish()?;
    Ok(Command::Key { combo, repeats: repeats.unwrap_or(0) })
}

/// `bench <frames> [csv=<path>] [none | wheel <dx> <dy> | key <combo> | type <text>]`.
fn parse_bench(args: &mut Args) -> Result<Command, String> {
    let frames = parse_frames(args.required("frame count")?, MAX_BENCH_FRAMES)?;
    let mut csv = None;
    let mut probe = *args;
    if let Some(path) = probe.token().and_then(|t| t.strip_prefix("csv=")) {
        if path.is_empty() {
            return Err("missing csv path".to_string());
        }
        csv = Some(PathBuf::from(path));
        *args = probe;
    }
    let action = parse_action(args)?;
    Ok(Command::Bench { frames, csv, action })
}

fn parse_action(args: &mut Args) -> Result<BenchAction, String> {
    let action = match args.token() {
        None | Some("none") => BenchAction::None,
        Some("wheel") => {
            let dx = parse_f64(args.required("dx")?)?;
            let dy = parse_f64(args.required("dy")?)?;
            BenchAction::Wheel { dx, dy }
        }
        Some("key") => BenchAction::Key(parse_combo(args.required("key combo")?)?),
        Some("type") => return Ok(BenchAction::Type(unescape_text(args.remainder()))),
        Some(t) => return Err(format!("unknown action '{}'", clip(t))),
    };
    args.finish()?;
    Ok(action)
}

fn parse_click(args: &mut Args) -> Result<Command, String> {
    let mut button = None;
    let mut phase = None;
    let mut alt = false;
    while let Some(t) = args.token() {
        let duplicate = if t == "alt" {
            std::mem::replace(&mut alt, true)
        } else if let Some(b) = button_arg(t) {
            button.replace(b).is_some()
        } else {
            let p = match t {
                "down" => ClickPhase::Down,
                "up" => ClickPhase::Up,
                _ => return Err(format!("invalid click argument '{}'", clip(t))),
            };
            phase.replace(p).is_some()
        };
        if duplicate {
            return Err(format!("duplicate click argument '{t}'"));
        }
    }
    Ok(Command::Click {
        button: button.unwrap_or(MouseButtonArg::Left),
        phase: phase.unwrap_or(ClickPhase::Both),
        alt,
    })
}

fn button_arg(t: &str) -> Option<MouseButtonArg> {
    match t {
        "left" => Some(MouseButtonArg::Left),
        "right" => Some(MouseButtonArg::Right),
        "middle" => Some(MouseButtonArg::Middle),
        _ => None,
    }
}

/// Parses `WxH` within `SIZE_MIN..=SIZE_MAX`.
fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let invalid = || format!("invalid size '{}', expected WxH", clip(s));
    let (w, h) = s.split_once('x').ok_or_else(invalid)?;
    let w: u32 = w.parse().map_err(|_| invalid())?;
    let h: u32 = h.parse().map_err(|_| invalid())?;
    if !(SIZE_MIN.0..=SIZE_MAX.0).contains(&w) || !(SIZE_MIN.1..=SIZE_MAX.1).contains(&h) {
        return Err(format!(
            "size {w}x{h} out of range {}x{}..{}x{}",
            SIZE_MIN.0, SIZE_MIN.1, SIZE_MAX.0, SIZE_MAX.1
        ));
    }
    Ok((w, h))
}

/// Parses a finite UI scale within `SCALE_MIN..=SCALE_MAX`.
fn parse_scale(t: &str) -> Result<f32, String> {
    let scale = parse_f64(t)?;
    if !(SCALE_MIN..=SCALE_MAX).contains(&scale) {
        return Err(format!("scale must be between {SCALE_MIN} and {SCALE_MAX}"));
    }
    Ok(scale as f32)
}

fn parse_f64(t: &str) -> Result<f64, String> {
    let value: f64 = t.parse().map_err(|_| format!("invalid number '{}'", clip(t)))?;
    if !value.is_finite() {
        return Err("non-finite number".to_string());
    }
    Ok(value)
}

fn parse_uint<T: std::str::FromStr>(t: &str) -> Result<T, String> {
    t.parse().map_err(|_| format!("invalid integer '{}'", clip(t)))
}

fn parse_ms(t: &str) -> Result<u64, String> {
    let ms: u64 = parse_uint(t)?;
    if ms > MAX_WAIT_MS {
        return Err(format!("milliseconds must be at most {MAX_WAIT_MS}"));
    }
    Ok(ms)
}

fn parse_frames(t: &str, max: u32) -> Result<u32, String> {
    let frames: u32 = parse_uint(t)?;
    if !(1..=max).contains(&frames) {
        return Err(format!("frame count must be between 1 and {max}"));
    }
    Ok(frames)
}

fn path_rest(args: &mut Args) -> Result<PathBuf, String> {
    let rest = args.remainder().trim_end();
    if rest.is_empty() {
        return Err("missing path".to_string());
    }
    Ok(PathBuf::from(rest))
}

/// Replaces line breaks with spaces so one response always fits on one line.
fn single_line(s: &str) -> String {
    s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

/// Longest prefix of `s` holding at most `MAX_ECHO_CHARS` characters.
fn clip(s: &str) -> &str {
    match s.char_indices().nth(MAX_ECHO_CHARS) {
        Some((end, _)) => &s[..end],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_types::Modifiers;

    fn parse(line: &str) -> Result<Option<Command>, String> {
        parse_line(line.as_bytes())
    }

    fn cmd(line: &str) -> Command {
        match parse(line) {
            Ok(Some(command)) => command,
            other => panic!("expected a command for {line:?}, got {other:?}"),
        }
    }

    fn err(line: &str) -> String {
        match parse(line) {
            Err(reason) => reason,
            other => panic!("expected an error for {line:?}, got {other:?}"),
        }
    }

    fn spec(keycode: u32, unshifted: char, shifted: char) -> KeySpec {
        KeySpec { keycode, unshifted: Some(unshifted), shifted: Some(shifted) }
    }

    fn ctrl_c() -> KeyCombo {
        KeyCombo { key: spec(46, 'c', 'C'), mods: Modifiers::CONTROL }
    }

    #[test]
    fn blank_and_comment_lines_are_skipped() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("   "), Ok(None));
        assert_eq!(parse("# c"), Ok(None));
        assert_eq!(parse("\r\n"), Ok(None));
    }

    #[test]
    fn type_unescapes_only_n_t_and_backslash() {
        assert_eq!(cmd(r"type a\tb\\n"), Command::Type("a\tb\\n".to_string()));
        assert_eq!(cmd(r"type x\qy\"), Command::Type("x\\qy\\".to_string()));
    }

    #[test]
    fn key_parses_repeat_and_combo() {
        assert_eq!(
            cmd("key --repeat 3 ctrl+shift+a"),
            Command::Key {
                combo: KeyCombo {
                    key: spec(30, 'a', 'A'),
                    mods: Modifiers::new(true, true, false, false),
                },
                repeats: 3,
            }
        );
        assert_eq!(cmd("key ctrl+c"), Command::Key { combo: ctrl_c(), repeats: 0 });
    }

    #[test]
    fn key_char_replaces_layout_character() {
        assert_eq!(
            cmd("key --char с ctrl+c"),
            Command::Key {
                combo: KeyCombo {
                    key: KeySpec { keycode: 46, unshifted: Some('с'), shifted: Some('С') },
                    mods: Modifiers::CONTROL,
                },
                repeats: 0,
            }
        );
    }

    #[test]
    fn mods_lists_held_modifiers() {
        assert_eq!(cmd("mods ctrl shift"), Command::Mods(Modifiers::new(true, true, false, false)));
        assert_eq!(cmd("mods"), Command::Mods(Modifiers::empty()));
    }

    #[test]
    fn mouse_and_click_commands() {
        assert_eq!(cmd("mouse_cell 3 4"), Command::MouseCell { col: 3, row: 4 });
        assert_eq!(
            cmd("click right down alt"),
            Command::Click { button: MouseButtonArg::Right, phase: ClickPhase::Down, alt: true }
        );
        assert_eq!(
            cmd("click"),
            Command::Click { button: MouseButtonArg::Left, phase: ClickPhase::Both, alt: false }
        );
    }

    #[test]
    fn wheel_units() {
        assert_eq!(
            cmd("wheel 0 -3"),
            Command::Wheel { dx: 0.0, dy: -3.0, unit: WheelUnit::Lines }
        );
        assert_eq!(
            cmd("wheel 0 -40 px"),
            Command::Wheel { dx: 0.0, dy: -40.0, unit: WheelUnit::Pixels }
        );
    }

    #[test]
    fn resize_and_scale() {
        assert_eq!(cmd("resize 1280x800"), Command::Resize { w: 1280, h: 800 });
        assert_eq!(cmd("scale 1.333"), Command::Scale(1.333));
    }

    #[test]
    fn dump_forms() {
        assert_eq!(cmd("dump"), Command::Dump(None));
        assert_eq!(cmd("dump /tmp/a.json"), Command::Dump(Some(PathBuf::from("/tmp/a.json"))));
        assert_eq!(cmd("dump row 5"), Command::DumpRow(5));
        assert_eq!(cmd("dump row 5 x"), Command::Dump(Some(PathBuf::from("row 5 x"))));
        assert_eq!(cmd("dump row -1"), Command::Dump(Some(PathBuf::from("row -1"))));
    }

    #[test]
    fn timing_commands_and_default_settle() {
        assert_eq!(cmd("settle"), Command::Settle { ms: 500 });
        assert_eq!(cmd("settle 20"), Command::Settle { ms: 20 });
        assert_eq!(cmd("wait 60000"), Command::Wait { ms: 60_000 });
        assert_eq!(cmd("wake 8"), Command::Wake { ms: 8 });
        assert_eq!(cmd("idle 100"), Command::Idle { ms: 100 });
    }

    #[test]
    fn bench_with_csv_and_key_action() {
        assert_eq!(
            cmd("bench 100 csv=/tmp/b.csv key ctrl+c"),
            Command::Bench {
                frames: 100,
                csv: Some(PathBuf::from("/tmp/b.csv")),
                action: BenchAction::Key(ctrl_c()),
            }
        );
        assert_eq!(
            cmd("bench 5 wheel 0 2"),
            Command::Bench { frames: 5, csv: None, action: BenchAction::Wheel { dx: 0.0, dy: 2.0 } }
        );
    }

    #[test]
    fn info_and_quit() {
        assert_eq!(cmd("info"), Command::Info);
        assert_eq!(cmd("quit"), Command::Quit);
    }

    #[test]
    fn key_rejects_bad_repeat_and_unknown_token() {
        assert_eq!(err("key --repeat 1001 a"), "repeat count exceeds 1000");
        assert_eq!(err("key foo"), "unknown key token 'foo'");
        assert_eq!(err("key ctrl+"), "unknown key token ''");
        assert_eq!(err("key --char ab a"), "--char expects one character, got 'ab'");
        assert_eq!(err("key hyper+a"), "unknown modifier 'hyper'");
        assert_eq!(err("mods ctrl ctrl"), "duplicate modifier 'ctrl'");
    }

    #[test]
    fn resize_rejects_out_of_range_sizes() {
        assert!(parse("resize 0x0").is_err());
        assert!(parse("resize 9000x10").is_err());
        assert!(parse("resize 1280x800 extra").is_err());
    }

    #[test]
    fn scale_and_wait_reject_out_of_range_values() {
        assert!(parse("scale 10").is_err());
        assert!(parse("wait 60001").is_err());
    }

    #[test]
    fn unsupported_rriter_commands_are_unknown() {
        for name in ["open", "workspace", "dialog", "picker_answer", "record", "dblclick"] {
            assert_eq!(err(&format!("{name} x")), format!("unknown command '{name}'"));
        }
    }

    #[test]
    fn non_utf8_line_is_an_error() {
        assert_eq!(parse_line(&[b'i', b'n', b'f', 0xff]), Err("invalid utf-8".to_string()));
    }

    #[test]
    fn response_line_shapes() {
        assert_eq!(Response::Ok(None).line(), "ok");
        assert_eq!(Response::Ok(Some("a\nb".to_string())).line(), "ok a b");
        assert_eq!(Response::Ok(Some("x\r\ny".to_string())).line(), "ok x  y");
        assert_eq!(Response::Err("bad\rthing".to_string()).line(), "err bad thing");
    }

    #[test]
    fn response_line_clips_reason_to_64_chars() {
        let reason = "é".repeat(100);
        let line = Response::Err(reason).line();
        assert_eq!(line, format!("err {}", "é".repeat(64)));
    }
}
