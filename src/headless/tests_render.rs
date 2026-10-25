//! Rendering scenarios L1, L11, L12, L13, L14 and L17 of the terminal parity spec (§9.2).
//! Expected values come from the parity spec (§5.1 SGR table, §6.2 decoration layer), the
//! headless protocol reference (`dump row`, `layout`) and the pixels of `screenshot`, never
//! from the renderer's own output.

use super::session::HeadlessSession;
use super::tests::{
    TEST_BACKGROUND, TestImage, cell_rect, dump, pixel_rgb, run_script, screenshot,
    session_for_test, wait_until,
};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Emoji font files the renderer loads, in its search order (`src/renderer.rs:366-368`).
const EMOJI_FONT_PATHS: [&str; 3] = [
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
    "/usr/share/fonts/noto/NotoColorEmoji.google.ttf",
];

/// One letter per SGR row of parity spec table 5.1 (L1); `Z` is printed after every reset.
const SGR_BODY: &str = r"\033[1mB\033[22m\033[3mI\033[23m\033[2mD\033[22m\033[7mV\033[27m\033[8mC\033[28m\033[9mS\033[29m\033[4:1mU\033[4:0m\033[4:2mW\033[4:0m\033[4:3mY\033[4:0m\033[4:4mG\033[4:0m\033[4:5mH\033[4:0m\033[21mJ\033[24m\033[4m\033[58;2;10;200;30mK\033[59m\033[24m\033[4m\033[58:2::10:200:30mR\033[59m\033[24mZ";

/// Seven cells: blanks underlined with default colour, underlined with the foreground colour,
/// underlined with SGR 58 (semicolon and colon forms), struck through in red, an inverse
/// underlined blank, and a dim underlined struck `x` (L12, L13).
const DECORATED_BODY: &str = r"\033[4m \033[0m\033[4;38;2;200;10;20m \033[0m\033[4m\033[58;2;10;200;30m \033[59m\033[0m\033[4m\033[58:2::10:200:30m \033[59m\033[0m\033[9;38;2;200;10;20m \033[0m\033[7;4m \033[0m\033[2;4;9mx\033[0m";

/// Number of cells `DECORATED_BODY` prints.
const DECORATED_CELLS: u64 = 7;

/// Default foreground under SGR 2: `ANSI_16_COLORS[7]` (0.90) times the renderer's dim factor
/// 0.66, in f32, times 255 gives 151.47, so every channel is 0x97.
const DIM_DEFAULT_FG: &str = "#979797";

/// Runs `script` and asserts that no response is an `err`; returns the responses.
fn run_ok(session: &mut HeadlessSession, script: &[u8]) -> Vec<String> {
    let lines = run_script(session, script);
    assert!(
        lines.iter().all(|line| !line.starts_with("err")),
        "unexpected err in {lines:?}"
    );
    lines
}

/// Parses the `ok <json>` response of `dump row <row>`.
fn dump_row(session: &mut HeadlessSession, row: usize) -> Value {
    let lines = run_ok(session, format!("dump row {row}\n").as_bytes());
    let json = lines[0]
        .strip_prefix("ok ")
        .unwrap_or_else(|| panic!("dump row answers `ok <json>`, got {lines:?}"));
    serde_json::from_str(json).expect("dump row payload is JSON")
}

/// Prints `body` with `printf` from a script file, so the typed command stays short, and
/// waits until the cursor sits after `cols` cells on a row below the typed command.
/// Returns the dump taken at that point and the index of the output row.
fn print_cells(session: &mut HeadlessSession, tag: &str, body: &str, cols: u64) -> (Value, usize) {
    let prompt_row = dump(session)["terminal"]["cursor"]["row"]
        .as_u64()
        .expect("cursor row before the command");
    let script = std::env::temp_dir().join(format!("ronsole-t8-{}-{tag}.sh", std::process::id()));
    fs::write(&script, format!("printf '{body}'\nsleep 30\n")).expect("test script file");
    run_ok(session, format!("type sh {}\n", script.display()).as_bytes());
    run_ok(session, b"key enter\n");
    let shown = wait_until(session, 5000, "cells printed up to the cursor", |dump| {
        dump["terminal"]["cursor"]["col"] == cols
            && dump["terminal"]["cursor"]["row"].as_u64() != Some(prompt_row)
    });
    // The shell has already read the script; removing the name does not affect it.
    let _ = fs::remove_file(&script);
    let row = shown["terminal"]["cursor"]["row"]
        .as_u64()
        .expect("cursor row after the output") as usize;
    (shown, row)
}

/// Cell of a `dump row` payload whose text is `ch`.
fn cell_by_char<'a>(line: &'a Value, ch: &str) -> &'a Value {
    line["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .find(|cell| cell["ch"] == ch)
        .unwrap_or_else(|| panic!("cell {ch} is dumped: {line}"))
}

/// Cell of a `dump row` payload at column `col`.
fn cell_by_col(line: &Value, col: u64) -> &Value {
    line["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .find(|cell| cell["col"] == col)
        .unwrap_or_else(|| panic!("cell at column {col} is dumped: {line}"))
}

fn has_flag(cell: &Value, flag: &str) -> bool {
    cell["flags"]
        .as_array()
        .expect("flags")
        .iter()
        .any(|name| name == flag)
}

/// Every pixel of `rect` `(x0, y0, x1, y1)`, `y1` and `x1` exclusive.
fn pixels(rect: (i32, i32, i32, i32)) -> impl Iterator<Item = (i32, i32)> {
    (rect.1..rect.3).flat_map(move |y| (rect.0..rect.2).map(move |x| (x, y)))
}

/// Pixel rectangle from the left edge of cell `first` to the right edge of cell `last`.
fn span_rect(dump: &Value, first: usize, last: usize, row: usize) -> (i32, i32, i32, i32) {
    let start = cell_rect(dump, first, row);
    let end = cell_rect(dump, last, row);
    (start.0, start.1, end.2, start.3)
}

/// Total channel distance of the pixels of `rect` from the terminal background.
fn ink(image: &TestImage, rect: (i32, i32, i32, i32)) -> u64 {
    pixels(rect)
        .map(|point| {
            pixel_rgb(image, point)
                .iter()
                .zip(TEST_BACKGROUND)
                .map(|(value, background)| u64::from(value.abs_diff(background)))
                .sum::<u64>()
        })
        .sum()
}

/// True when some channel of `px` differs from the terminal background by more than 60.
fn is_ink(px: [u8; 3]) -> bool {
    px.iter()
        .zip(TEST_BACKGROUND)
        .any(|(value, background)| value.abs_diff(background) > 60)
}

/// True when every channel of `px` is within 2 of `expected`.
fn close(px: [u8; 3], expected: [u8; 3]) -> bool {
    px.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 2)
}

/// Parses a `#rrggbb` colour from a dump.
fn rgb(hex: &str) -> [u8; 3] {
    let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).expect("#rrggbb colour");
    [(value >> 16) as u8, (value >> 8) as u8, value as u8]
}

/// Underline colour of a dumped cell: `underline_color` when SGR 58 set one, else `fg` (§6.2).
fn expected_underline(cell: &Value) -> [u8; 3] {
    rgb(cell["underline_color"]
        .as_str()
        .unwrap_or_else(|| cell["fg"].as_str().expect("fg")))
}

/// Integer baseline of `row` in pixels: the row's top edge plus `terminal.layout.baseline`.
fn baseline_y(dump: &Value, row: usize) -> i32 {
    let layout = &dump["terminal"]["layout"];
    let top = layout["cell0"][1].as_f64().expect("cell0 y")
        + row as f64 * layout["char_h"].as_f64().expect("char_h");
    (top + layout["baseline"].as_f64().expect("baseline")).round() as i32
}

#[test]
fn l1_sgr_attributes_reach_the_cell_model() {
    let mut session = session_for_test(800, 600);
    let (shown, row) = print_cells(&mut session, "l1", SGR_BODY, 15);
    let line = dump_row(&mut session, row);

    // (glyph, expected flag or "", expected underline style), from parity spec table 5.1.
    // 21 and 4:2 are both double underlines.
    for (ch, flag, underline) in [
        ("B", "bold", "none"),
        ("I", "italic", "none"),
        ("D", "dim", "none"),
        ("V", "inverse", "none"),
        ("C", "conceal", "none"),
        ("S", "strike", "none"),
        ("U", "", "single"),
        ("W", "", "double"),
        ("Y", "", "curly"),
        ("G", "", "dotted"),
        ("H", "", "dashed"),
        ("J", "", "double"),
        ("K", "", "single"),
        ("R", "", "single"),
        ("Z", "", "none"),
    ] {
        let cell = cell_by_char(&line, ch);
        assert_eq!(cell["underline"], underline, "underline of {ch}: {cell}");
        if !flag.is_empty() {
            assert!(has_flag(cell, flag), "{ch} carries flag {flag}: {cell}");
        }
    }
    // SGR 58 in both forms sets the underline colour; the colour is #0ac81e (10, 200, 30).
    for ch in ["K", "R"] {
        assert_eq!(
            cell_by_char(&line, ch)["underline_color"],
            "#0ac81e",
            "underline colour of {ch}"
        );
    }
    let plain = cell_by_char(&line, "Z");
    assert!(plain["flags"].as_array().expect("flags").is_empty(), "Z has no flags: {plain}");
    // SGR 59 before Z resets the underline colour.
    assert_eq!(plain["underline_color"], Value::Null, "Z has no underline colour: {plain}");
    // Dim scales the default foreground.
    let dim = cell_by_char(&line, "D");
    assert_eq!(dim["fg"], DIM_DEFAULT_FG, "dim foreground: {dim}");

    // Inverse swaps the default colours: the foreground becomes the terminal background
    // (#123456, the configured background) and the background becomes the default foreground.
    let inverse = cell_by_char(&line, "V");
    assert_eq!(inverse["fg"], "#123456", "inverse foreground: {inverse}");
    assert_eq!(inverse["bg"], plain["fg"], "inverse background: {inverse}");

    // Conceal draws no glyph and no decoration: every pixel of the cell is the background.
    let concealed = cell_by_char(&line, "C");
    let col = concealed["col"].as_u64().expect("column") as usize;
    let rect = cell_rect(&shown, col, row);
    let image = screenshot(&mut session);
    for point in pixels(rect) {
        let px = pixel_rgb(&image, point);
        assert!(
            close(px, TEST_BACKGROUND),
            "concealed cell pixel {point:?} is {px:?}, not the background"
        );
    }
}

#[test]
fn l11_bold_face_draws_more_ink_than_regular() {
    let mut session = session_for_test(800, 600);
    let (shown, row) = print_cells(&mut session, "l11", r"m \033[1mm\033[0m", 3);
    let line = dump_row(&mut session, row);
    assert!(has_flag(cell_by_col(&line, 2), "bold"), "column 2 is bold: {line}");
    let image = screenshot(&mut session);
    let regular = ink(&image, cell_rect(&shown, 0, row));
    let bold = ink(&image, cell_rect(&shown, 2, row));
    assert!(regular > 0, "the regular m is drawn");
    assert!(bold > regular, "bold m ink {bold} must exceed regular m ink {regular}");
}

#[test]
fn l12_decorations_follow_baseline_and_colour_rules() {
    let mut session = session_for_test(800, 600);
    let (shown, row) = print_cells(&mut session, "l12", DECORATED_BODY, DECORATED_CELLS);
    let line = dump_row(&mut session, row);
    let image = screenshot(&mut session);

    // Underlines: the topmost underline pixel is not above the cell baseline, and it has the
    // underline colour (SGR 58 when set, otherwise the foreground colour). The inverse blank
    // in column 5 has the terminal background as its foreground, so its underline has it too.
    let baseline = baseline_y(&shown, row);
    let inverse = cell_by_col(&line, 5);
    assert_eq!(inverse["fg"], "#123456", "inverse foreground: {inverse}");
    for col in [0, 1, 2, 3, 5u64] {
        let cell = cell_by_col(&line, col);
        let expected = expected_underline(cell);
        let rect = cell_rect(&shown, col as usize, row);
        let top = (rect.1..rect.3).find(|&y| {
            (rect.0..rect.2).any(|x| close(pixel_rgb(&image, (x, y)), expected))
        });
        let top = top.unwrap_or_else(|| {
            panic!("no underline pixel of colour {expected:?} in column {col}: {cell}")
        });
        assert!(
            top >= baseline,
            "underline top {top} is above the baseline {baseline} in column {col}"
        );
    }

    // Strikethrough: a band across the whole cell in the cell's foreground colour.
    let struck = cell_by_col(&line, 4);
    let expected = rgb(struck["fg"].as_str().expect("fg"));
    let rect = cell_rect(&shown, 4, row);
    let width = f64::from(rect.2 - rect.0);
    let band_rows = (rect.1..rect.3)
        .filter(|&y| {
            let matching = (rect.0..rect.2)
                .filter(|&x| close(pixel_rgb(&image, (x, y)), expected))
                .count();
            matching as f64 >= 0.9 * width
        })
        .count();
    assert!(band_rows > 0, "no strikethrough band of colour {expected:?} in column 4");

    // The dim `x` in column 6: underline and strikethrough are both bands across the cell in
    // its dimmed foreground, the underline at or below the baseline and the strike above it.
    let dim = cell_by_col(&line, 6);
    assert_eq!(dim["fg"], DIM_DEFAULT_FG, "dim foreground: {dim}");
    assert_eq!(dim["underline_color"], Value::Null, "dim cell has no SGR 58: {dim}");
    let expected = rgb(DIM_DEFAULT_FG);
    let rect = cell_rect(&shown, 6, row);
    let width = f64::from(rect.2 - rect.0);
    let bands: Vec<i32> = (rect.1..rect.3)
        .filter(|&y| {
            let matching = (rect.0..rect.2)
                .filter(|&x| close(pixel_rgb(&image, (x, y)), expected))
                .count();
            matching as f64 >= 0.9 * width
        })
        .collect();
    assert!(
        bands.iter().any(|&y| y >= baseline),
        "no underline band of colour {expected:?} at or below the baseline {baseline} in column 6: {bands:?}"
    );
    assert!(
        bands.iter().any(|&y| y < baseline),
        "no strikethrough band of colour {expected:?} above the baseline {baseline} in column 6: {bands:?}"
    );
}

#[test]
fn l13_underline_stays_inside_its_row() {
    let mut session = session_for_test(800, 600);
    let (shown, row) = print_cells(&mut session, "l13", DECORATED_BODY, DECORATED_CELLS);
    let mut current = shown;
    for scale in ["1.0", "1.333"] {
        let previous = current["terminal"]["layout"]["char_h"].clone();
        run_ok(&mut session, format!("scale {scale}\n").as_bytes());
        current = if scale == "1.0" {
            dump(&mut session)
        } else {
            // A new scale changes the cell height; the layout follows on the next frame.
            wait_until(&mut session, 5000, "layout follows scale 1.333", |dump| {
                dump["terminal"]["layout"]["char_h"] != previous
            })
        };
        let image = screenshot(&mut session);
        let first = cell_rect(&current, 0, row);
        let right = cell_rect(&current, DECORATED_CELLS as usize - 1, row).2;
        let row_rect = (first.0, first.1, right, first.3);
        assert!(
            ink(&image, row_rect) > 0,
            "decorations are drawn in the row at scale {scale}"
        );
        // The row below must stay empty, so no underline can reach y >= row_bottom. Column 5 is
        // left out: its inverse background fills the cell up to the next row's snapped top edge,
        // which can lie below `row_bottom`, and its underline has the background colour anyway.
        let below_bottom = cell_rect(&current, 0, row + 1).3;
        for (from, to) in [(0, 4), (6, 6)] {
            let span = span_rect(&current, from, to, row);
            let below = (span.0, span.3, span.2, below_bottom);
            let stray = pixels(below)
                .filter(|&point| is_ink(pixel_rgb(&image, point)))
                .count();
            assert_eq!(stray, 0, "pixels below columns {from}..={to} at scale {scale}");
        }
    }
}

#[test]
fn l14_strikethrough_is_centred_on_the_x_height() {
    let mut session = session_for_test(800, 600);
    let (shown, row) = print_cells(&mut session, "l14", r"x \033[9mx\033[0m", 3);
    let line = dump_row(&mut session, row);
    assert!(has_flag(cell_by_col(&line, 2), "strike"), "column 2 is struck: {line}");
    let image = screenshot(&mut session);

    // Middle of the ink of the plain x, measured from the pixels.
    let plain = cell_rect(&shown, 0, row);
    let ink_rows: Vec<i32> = (plain.1..plain.3)
        .filter(|&y| (plain.0..plain.2).any(|x| is_ink(pixel_rgb(&image, (x, y)))))
        .collect();
    let (Some(&top), Some(&bottom)) = (ink_rows.first(), ink_rows.last()) else {
        panic!("the plain x is drawn in row {row}");
    };
    let ink_middle = f64::from(top + bottom) / 2.0;

    // Middle of the strike band: rows where the struck cell is ink across 90% of its width.
    let struck = cell_rect(&shown, 2, row);
    let width = f64::from(struck.2 - struck.0);
    let band: Vec<i32> = (struck.1..struck.3)
        .filter(|&y| {
            let inked = (struck.0..struck.2)
                .filter(|&x| is_ink(pixel_rgb(&image, (x, y))))
                .count();
            inked as f64 >= 0.9 * width
        })
        .collect();
    let (Some(&first), Some(&last)) = (band.first(), band.last()) else {
        panic!("no strikethrough band across the struck x in row {row}");
    };
    let band_middle = f64::from(first + last) / 2.0;
    assert!(
        (band_middle - ink_middle).abs() <= 1.0,
        "strike middle {band_middle} is more than 1 px from the x ink middle {ink_middle}"
    );
}

#[test]
fn l17_emoji_is_drawn_the_same_in_bold_and_regular() {
    let found = EMOJI_FONT_PATHS.iter().any(|path| Path::new(path).exists());
    assert!(
        found,
        "L17 needs NotoColorEmoji.ttf at one of {EMOJI_FONT_PATHS:?}; none exists"
    );
    let mut session = session_for_test(800, 600);
    let probe = wait_until(&mut session, 5000, "terminal layout", |dump| {
        dump["terminal"]["layout"]["char_w"].is_f64()
    });
    // Glyphs are placed at fractional x for fractional cell widths, so the bold emoji goes in
    // a column whose offset from column 0 is a whole number of pixels: both then sit on the
    // same pixel grid and only the face differs.
    let char_w = probe["terminal"]["layout"]["char_w"].as_f64().expect("char_w");
    let column = (3..=40u64)
        .find(|&col| {
            let offset = col as f64 * char_w;
            (offset - offset.round()).abs() < 1e-3
        })
        .expect("a column whose offset from column 0 is a whole number of pixels");
    let body = format!("😀{}\\033[1m😀\\033[0m", " ".repeat(column as usize - 2));
    let (shown, row) = print_cells(&mut session, "l17", &body, column + 2);
    let line = dump_row(&mut session, row);
    assert!(has_flag(cell_by_col(&line, column), "bold"), "bold column {column}: {line}");
    let image = screenshot(&mut session);

    // Both emoji occupy two cells; compare them pixel by pixel from their top-left corners.
    let regular = span_rect(&shown, 0, 1, row);
    let bold = span_rect(&shown, column as usize, column as usize + 1, row);
    assert!(ink(&image, regular) > 0, "the regular emoji is drawn");
    let width = (regular.2 - regular.0).min(bold.2 - bold.0);
    let height = (regular.3 - regular.1).min(bold.3 - bold.1);
    for dy in 0..height {
        for dx in 0..width {
            let a = pixel_rgb(&image, (regular.0 + dx, regular.1 + dy));
            let b = pixel_rgb(&image, (bold.0 + dx, bold.1 + dy));
            assert!(
                close(a, b),
                "bold emoji pixel ({dx}, {dy}) is {b:?}, regular is {a:?}"
            );
        }
    }
}
