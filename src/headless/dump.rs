//! Readers of the headless session state: `dump`, `dump row` and `mouse_cell`. Each reads the
//! active terminal under one `lock_snapshot`, so its `terminal` section is internally consistent.
//! Coordinates follow Q24: the visible area is `rows` rows ending at `bottom`, and rows are
//! counted from its top.

use super::session::{HeadlessSession, flow_name};
use crate::app::App;
use crate::renderer::{
    Renderer, TerminalPalette, TerminalUiLayout, terminal_cell_render_colors,
    terminal_cell_span_x_bounds, terminal_render_color, terminal_row_draw_y,
};
use crate::runtime::WindowRuntime;
use crate::terminal::Terminal;
use ronsole_term::{
    Cell, CellFlags, MouseTrackingMode, Term, UnderlineStyle, normalized_selection_bounds,
};
use serde_json::{Value, json};
use std::time::Instant;

/// Visible area of the active terminal: `rows` rows from absolute row `top`.
struct Window {
    top: usize,
    rows: usize,
    total: usize,
    scrollback: usize,
}

/// Computes the visible area of `grid` for a frame with `layout`, with no scroll before the first frame.
fn window(grid: &Term, layout: Option<&TerminalUiLayout>) -> Window {
    let total = grid.total_rows();
    let rows = grid.rows();
    let scrolled = layout
        .filter(|layout| layout.char_h > 0.0)
        .map_or(0, |layout| (layout.scroll_offset.max(0.0) / layout.char_h).floor() as usize);
    let bottom = total.saturating_sub(1).saturating_sub(scrolled);
    let top = (bottom + 1).saturating_sub(rows);
    Window {
        top,
        rows,
        total,
        scrollback: grid.scrollback_len(),
    }
}

/// Top-left corner `(x, y)` of cell `(0, 0)` of the visible area, unrounded (spec §5.1).
///
/// The formula matches the frame's text placement, so cell rectangles agree with what is drawn.
fn cell0(layout: &TerminalUiLayout, window: &Window) -> (f32, f32) {
    let y = terminal_row_draw_y(
        layout.body,
        layout.bottom_pad,
        layout.char_h,
        window.total,
        window.top,
        layout.scroll_offset,
    );
    (layout.text_x, y)
}

/// The frame layout, or `None` before the first frame drew one.
fn frame_layout(app: &App) -> Option<TerminalUiLayout> {
    let layout = app.interaction().layout;
    (layout != TerminalUiLayout::default()).then_some(layout)
}

/// Centre pixel `(x, y)` of cell `(col, row)` of the visible area, or `None` when the cell is
/// outside the visible area or its centre is outside `layout.body` (a clipped bottom row).
///
/// Cell edges use the spec's rounding (§5.1); the centre is `(floor((x0 + x1) / 2), floor(y0 + char_h / 2))`.
pub(crate) fn cell_center(
    layout: &TerminalUiLayout,
    top: usize,
    total: usize,
    rows: usize,
    col: usize,
    row: usize,
) -> Option<(i32, i32)> {
    if row >= rows || col >= layout.cols {
        return None;
    }
    let window = Window {
        top,
        rows,
        total,
        scrollback: 0,
    };
    let (cell0_x, cell0_y) = cell0(layout, &window);
    let (x0, x1) = terminal_cell_span_x_bounds(cell0_x, col, 1, layout.char_w);
    let y0 = cell0_y + row as f32 * layout.char_h;
    let x = ((x0 + x1) / 2.0).floor();
    let y = (y0 + layout.char_h / 2.0).floor();
    let body = layout.body;
    let inside = x >= body.x && x < body.x + body.w && y >= body.y && y < body.y + body.h;
    inside.then_some((x as i32, y as i32))
}

/// Centre of cell `(col, row)` of the active terminal in the latest frame, for `mouse_cell`.
pub(super) fn cell_center_at(session: &mut HeadlessSession, col: usize, row: usize) -> Option<(i32, i32)> {
    let app = session.app_mut();
    let layout = frame_layout(app)?;
    let terminal = app.active_terminal()?;
    let (_screen, grid) = terminal.lock_snapshot();
    let window = window(&grid, Some(&layout));
    cell_center(&layout, window.top, window.total, window.rows, col, row)
}

/// The full `dump` object (spec §5.1). Drains the external-request channel as it reads.
pub(crate) fn dump_json(session: &mut HeadlessSession) -> Value {
    let external = session.take_external_request();
    let last_report = session.last_report();
    let counters = session.counters();
    let (width, height) = session.size();
    let scale = session.scale();
    let app = session.app_mut();
    let clipboard = app.clipboard_text();
    let layout = frame_layout(app);
    let renderer = app.runtime().map(WindowRuntime::renderer);
    let wake_pending = app
        .runtime()
        .is_some_and(|runtime| runtime.wake_handle().pending());
    let active = app.active_terminal_index();
    let tabs: Vec<Value> = app
        .terminals()
        .iter()
        .enumerate()
        .map(|(index, terminal)| {
            let mut title = String::new();
            terminal.write_display_title(&mut title);
            let rect = renderer
                .and_then(|renderer| renderer.terminal_tab_body(index))
                .map_or(Value::Null, |[x, y, w, h]| json!([x, y, w, h]));
            json!({"index": index, "title": title, "active": index == active, "rect": rect})
        })
        .collect();
    let terminal = app.active_terminal().map_or(Value::Null, |terminal| {
        terminal_state(terminal, layout.as_ref(), renderer)
    });
    let deadline_ms = last_report.and_then(|report| report.deadline).map(|deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
    });
    json!({
        "size": [width, height],
        "scale": scale,
        "cursor_icon": counters.map_or_else(String::new, |counters| format!("{:?}", counters.cursor_kind)),
        "tabs": tabs,
        "terminal": terminal,
        "clipboard": {"mode": "memory", "text": clipboard},
        "external_request": external.map(|arg| json!({"kind": "open_url", "arg": arg})),
        "event_loop": {
            "control_flow": last_report.map_or("wait", |report| flow_name(report.flow)),
            "deadline_ms": deadline_ms,
            "wake_pending": wake_pending,
            "redraw_requested": counters.is_some_and(|counters| counters.redraw_requested),
            "frame_requests": counters.map_or(0, |counters| counters.frame_requests),
            "frames": counters.map_or(0, |counters| counters.frames),
        },
    })
}

/// The `terminal` section of `dump` for one tab.
fn terminal_state(
    terminal: &Terminal,
    layout: Option<&TerminalUiLayout>,
    renderer: Option<&Renderer>,
) -> Value {
    let (_screen, grid) = terminal.lock_snapshot();
    let window = window(&grid, layout);
    let text: Vec<String> = (0..window.rows)
        .map(|row| row_text(&grid, window.top + row))
        .collect();
    let cursor = {
        let (cursor_col, cursor_row) = grid.cursor();
        let absolute = grid.scrollback_len() + cursor_row;
        if absolute >= window.top && absolute - window.top < window.rows {
            json!({"row": absolute - window.top, "col": cursor_col})
        } else {
            Value::Null
        }
    };
    let top = window.top as i64;
    let selection = grid.selection().map_or(Value::Null, |(sx, sy, ex, ey)| {
        let (start_x, start_y, end_x, end_y) = normalized_selection_bounds(sx, sy, ex, ey);
        json!({
            "start": [start_y as i64 - top, start_x],
            "last": [end_y as i64 - top, end_x],
        })
    });
    // A link found before the latest content change is stale, as the frame treats it.
    let generation = grid.content_generation();
    let hovered_link = terminal
        .hovered_link()
        .filter(|(_, link_generation)| *link_generation == generation)
        .map_or(Value::Null, |(link, _)| {
        let ranges: Vec<Value> = link
            .cells
            .iter()
            .map(|(row, range)| json!([*row as i64 - top, range.start, range.end]))
            .collect();
        json!({"uri": link.uri.to_string(), "ranges": ranges})
    });
    let layout_json = match (layout, renderer) {
        (Some(layout), Some(renderer)) => {
            let (cell0_x, cell0_y) = cell0(layout, &window);
            json!({
                "body": [layout.body.x, layout.body.y, layout.body.w, layout.body.h],
                "cell0": [cell0_x, cell0_y],
                "char_w": layout.char_w,
                "char_h": layout.char_h,
                "baseline": renderer.terminal_text_baseline(),
                "x_height": renderer.terminal_x_height(),
            })
        }
        _ => Value::Null,
    };
    let tracking = match grid.mouse_tracking() {
        MouseTrackingMode::None => "none",
        MouseTrackingMode::Press => "press",
        MouseTrackingMode::ButtonMotion => "button_motion",
        MouseTrackingMode::AnyMotion => "any_motion",
    };
    let mut title = String::new();
    terminal.write_display_title(&mut title);
    json!({
        "cols": grid.cols(),
        "rows": window.rows,
        "viewport": {"top": window.top, "scrollback": window.scrollback, "total": window.total},
        "text": text,
        "cursor": cursor,
        "title": title,
        "modes": {
            "app_cursor_keys": grid.app_cursor_keys(),
            "bracketed_paste": grid.bracketed_paste(),
            "mouse_tracking": tracking,
            "mouse_sgr": grid.mouse_sgr(),
            "focus_reporting": grid.focus_reporting(),
            "kitty_flags": grid.kitty_flags(),
            "sync_pending": grid.sync_deadline().is_some(),
        },
        "selection": selection,
        "hovered_link": hovered_link,
        "layout": layout_json,
    })
}

/// Text of absolute row `index`, with trailing spaces removed.
fn row_text(grid: &Term, index: usize) -> String {
    let mut text = String::new();
    if let Some(cells) = grid.row(index) {
        for cell in cells.iter().take(grid.cols()) {
            cell.append_text_to(&mut text);
        }
    }
    text.truncate(text.trim_end_matches(' ').len());
    text
}

/// `dump row <row>` (spec §5.2) for row `row` of the visible area of the active tab.
pub(crate) fn dump_row_json(session: &mut HeadlessSession, row: usize) -> Result<Value, String> {
    let app = session.app_mut();
    let layout = frame_layout(app);
    let palette = app
        .runtime()
        .map(WindowRuntime::renderer)
        .map(Renderer::terminal_palette)
        .ok_or_else(|| "no headless runtime".to_string())?;
    let terminal = app
        .active_terminal()
        .ok_or_else(|| "no terminal tab".to_string())?;
    let (_screen, grid) = terminal.lock_snapshot();
    let window = window(&grid, layout.as_ref());
    if row >= window.rows {
        return Err("row out of range".to_string());
    }
    let cells: Vec<Value> = grid
        .row(window.top + row)
        .unwrap_or(&[])
        .iter()
        .take(grid.cols())
        .enumerate()
        .filter(|(_, cell)| !is_plain(cell))
        .map(|(col, cell)| cell_json(col, cell, palette))
        .collect();
    Ok(json!({"row": row, "cells": cells}))
}

/// Cell flags that `dump row` reports by name, in output order. `HOST_HIDDEN` is not listed,
/// so it is not an attribute for `is_plain` either.
const REPORTED_FLAGS: [(CellFlags, &str); 8] = [
    (CellFlags::BOLD, "bold"),
    (CellFlags::DIM, "dim"),
    (CellFlags::ITALIC, "italic"),
    (CellFlags::INVERSE, "inverse"),
    (CellFlags::STRIKE, "strike"),
    (CellFlags::CONCEAL, "conceal"),
    (CellFlags::WIDE, "wide"),
    (CellFlags::WIDE_SPACER, "wide_spacer"),
];

/// True for a blank cell with default colours and no other attribute; `dump row` leaves it out.
fn is_plain(cell: &Cell) -> bool {
    cell.ch() == ' '
        && cell.zero_width().is_empty()
        && !REPORTED_FLAGS
            .iter()
            .any(|(flag, _)| cell.flags().contains(*flag))
        && cell.fg().is_default_foreground()
        && cell.bg().is_default_background()
        && cell.underline() == UnderlineStyle::None
        && cell.underline_color().is_none()
        && cell.hyperlink().is_none()
}

/// One cell of `dump row`. Colours are resolved as the frame resolves them, including the
/// underline colour, which the frame takes from the cell's SGR 58 colour and not from `fg`.
fn cell_json(col: usize, cell: &Cell, palette: TerminalPalette) -> Value {
    let (foreground, background) = terminal_cell_render_colors(cell, palette);
    let mut ch = String::new();
    cell.append_text_to(&mut ch);
    let flags = cell.flags();
    let names: Vec<&str> = REPORTED_FLAGS
        .iter()
        .filter(|(flag, _)| flags.contains(*flag))
        .map(|(_, name)| *name)
        .collect();
    json!({
        "col": col,
        "ch": ch,
        "fg": hex(foreground),
        "bg": background.map(hex),
        "flags": names,
        "underline": underline_name(cell.underline()),
        "underline_color": cell
            .underline_color()
            .map(|color| hex(terminal_render_color(color, palette))),
        "link": cell.hyperlink().map(|link| link.uri.to_string()),
    })
}

/// `#rrggbb` of a rendered colour; channels are `round(c · 255)`.
fn hex(color: [f32; 4]) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(color[0]),
        channel(color[1]),
        channel(color[2])
    )
}

fn underline_name(style: UnderlineStyle) -> &'static str {
    match style {
        UnderlineStyle::None => "none",
        UnderlineStyle::Single => "single",
        UnderlineStyle::Double => "double",
        UnderlineStyle::Curly => "curly",
        UnderlineStyle::Dotted => "dotted",
        UnderlineStyle::Dashed => "dashed",
    }
}
