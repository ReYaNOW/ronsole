use super::*;
use crate::scroll::{ScrollbarThumb, scrollbar_drag_target, scrollbar_thumb};
use crate::search::{
    Rect, SearchRefreshCause, TerminalSearchGeometry, TerminalSearchState, terminal_search_geometry,
};
use crate::terminal::{
    CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT, Cell, TermGrid, Terminal, TerminalColor,
    normalized_selection_bounds, terminal_color_rgba, terminal_should_render_zero_width,
};

pub(crate) const TERMINAL_TEXT_SCALE: f32 = 1.05;
const TERMINAL_TAB_BAR_PAD: f32 = 8.0;
const TERMINAL_TAB_BAR_TOP: f32 = 6.0;
const TERMINAL_TAB_HEIGHT: f32 = 32.0;
const TERMINAL_TAB_BOTTOM_GAP: f32 = 4.0;
const TERMINAL_TAB_NATURAL_CHROME: f32 = 56.0;
const TERMINAL_TAB_CLOSE_SIZE: f32 = 20.0;
const TERMINAL_TAB_CLOSE_HIT_PAD: f32 = 4.0;
const TERMINAL_TAB_CLOSE_RIGHT_PADDING: f32 = TERMINAL_TAB_TEXT_PAD - TERMINAL_TAB_CLOSE_HIT_PAD;
const TERMINAL_TAB_TEXT_PAD: f32 = 16.0;
const TERMINAL_ADD_SIZE: f32 = 20.0;
const TAB_STRIP_EDGE_FADE_ALPHA: f32 = 0.4;
const TAB_STRIP_EDGE_FADE_WIDTH: f32 = 40.0;
const SEARCH_MATCH: [f32; 4] = [0.60, 0.60, 0.60, 0.35];
const SEARCH_ACTIVE: [f32; 4] = [1.0, 0.60, 0.0, 0.50];
const SCROLLBAR: [f32; 4] = [0.70, 0.33, 0.54, 0.80];

#[derive(Clone, Copy)]
enum SearchIcon {
    Close = 0,
    Previous = 1,
    Next = 2,
    Case = 3,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TerminalScrollbarLayout {
    pub track: Rect,
    pub thumb: Rect,
    pub max_scroll: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TerminalTabStripLayout {
    pub rect: Rect,
    pub max_scroll: f32,
    pub add: Option<Rect>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TerminalTabHitbox {
    pub body: Option<Rect>,
    pub close: Option<Rect>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalTabHit {
    #[default]
    None,
    Body(usize),
    Close(usize),
    Add,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TerminalUiLayout {
    pub body: Rect,
    pub text_x: f32,
    pub char_w: f32,
    pub char_h: f32,
    pub total_lines: usize,
    pub visible_rows: usize,
    pub cols: usize,
    pub max_scroll: f32,
    pub scroll_offset: f32,
    pub bottom_pad: f32,
    pub scale: f32,
    pub scrollbar: Option<TerminalScrollbarLayout>,
    pub search: Option<TerminalSearchGeometry>,
}

#[inline]
pub(crate) fn terminal_tab_body_top(scale: f32) -> f32 {
    ((TERMINAL_TAB_BAR_TOP + TERMINAL_TAB_HEIGHT + TERMINAL_TAB_BOTTOM_GAP) * scale).round()
}

#[inline]
fn terminal_tab_content_width(widths: &[f32], scale: f32) -> f32 {
    let pad = TERMINAL_TAB_BAR_PAD * scale;
    let add_size = (TERMINAL_ADD_SIZE * scale).round().max(1.0);
    pad + widths.iter().sum::<f32>() + pad + add_size + pad
}

#[inline]
fn terminal_tab_max_scroll(widths: &[f32], viewport_w: f32, scale: f32) -> f32 {
    (terminal_tab_content_width(widths, scale) - viewport_w.max(0.0)).max(0.0)
}

#[inline]
fn terminal_tab_add_x(base_x: f32, widths: &[f32], scale: f32) -> f32 {
    (base_x + widths.iter().sum::<f32>() + TERMINAL_TAB_BAR_PAD * scale).round()
}

#[inline]
fn terminal_tab_strip_rect(width: f32, scale: f32) -> Rect {
    Rect {
        x: 0.0,
        y: (TERMINAL_TAB_BAR_TOP * scale).round(),
        w: width.max(0.0),
        h: (TERMINAL_TAB_HEIGHT * scale).round().max(1.0),
    }
}

#[inline]
fn terminal_tab_close_geometry(tab_x: f32, tab_w: f32, strip: Rect, scale: f32) -> (Rect, Rect) {
    let icon_size = TERMINAL_TAB_CLOSE_SIZE * scale;
    let icon_x = tab_x + tab_w - TERMINAL_TAB_CLOSE_RIGHT_PADDING * scale - icon_size;
    let icon_y = (strip.y + (strip.h - icon_size) * 0.5 - 1.5 * scale).round();
    let icon = Rect {
        x: icon_x,
        y: icon_y,
        w: icon_size,
        h: icon_size,
    };
    let hit = Rect {
        x: icon_x - TERMINAL_TAB_CLOSE_HIT_PAD * scale,
        y: icon_y - TERMINAL_TAB_CLOSE_HIT_PAD * scale,
        w: icon_size + TERMINAL_TAB_CLOSE_HIT_PAD * 2.0 * scale,
        h: icon_size + TERMINAL_TAB_CLOSE_HIT_PAD * 2.0 * scale,
    };
    (icon, hit)
}

#[inline]
fn terminal_tab_show_close(tab_w: f32, scale: f32, active: bool, hovered: bool) -> bool {
    tab_w >= 56.0 * scale && (active || hovered)
}

fn terminal_tab_hitbox_geometry(
    tab_x: f32,
    tab_w: f32,
    strip: Rect,
    show_close: bool,
    scale: f32,
) -> TerminalTabHitbox {
    let visible_body = clipped_rect(
        Rect {
            x: tab_x,
            y: strip.y,
            w: tab_w,
            h: strip.h,
        },
        strip,
    );
    if !show_close {
        return TerminalTabHitbox {
            body: visible_body,
            close: None,
        };
    }

    let (close_icon, close_hit) = terminal_tab_close_geometry(tab_x, tab_w, strip, scale);
    if clipped_rect(close_icon, strip).is_none() {
        return TerminalTabHitbox {
            body: visible_body,
            close: None,
        };
    }
    let visible_close = clipped_rect(close_hit, strip);
    let body = if let (Some(body), Some(close)) = (visible_body, visible_close) {
        let right = close.x.min(body.x + body.w);
        (right > body.x).then_some(Rect {
            x: body.x,
            y: body.y,
            w: right - body.x,
            h: body.h,
        })
    } else {
        visible_body
    };
    TerminalTabHitbox {
        body,
        close: visible_close,
    }
}

#[inline]
fn fit_centered_rect(
    window_w: f32,
    window_h: f32,
    desired_w: f32,
    desired_h: f32,
    margin: f32,
) -> Rect {
    let window_w = window_w.max(0.0);
    let window_h = window_h.max(0.0);
    let margin = margin.max(0.0);
    let w = desired_w.max(0.0).min((window_w - margin * 2.0).max(0.0));
    let h = desired_h.max(0.0).min((window_h - margin * 2.0).max(0.0));
    Rect {
        x: ((window_w - w) * 0.5).max(0.0).round(),
        y: ((window_h - h) * 0.5).max(0.0).round(),
        w: w.round(),
        h: h.round(),
    }
}

fn settings_placeholder_layout(width: f32, height: f32, scale: f32, progress: f32) -> Rect {
    let scale = scale.max(0.0);
    let mut outer = fit_centered_rect(
        width,
        height,
        1000.0 * scale,
        700.0 * scale,
        20.0 * scale,
    );
    let start_y = height.max(0.0) + 100.0 * scale;
    let target_y = outer.y;
    outer.y = (start_y + (target_y - start_y) * progress.clamp(0.0, 1.0)).round();
    outer
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsPlaceholderLine {
    x: f32,
    baseline_y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsPlaceholderContentLayout {
    clip: Option<Rect>,
    title: Option<SettingsPlaceholderLine>,
    description: Option<SettingsPlaceholderLine>,
}

fn settings_placeholder_content_layout(
    modal: Rect,
    scale: f32,
) -> SettingsPlaceholderContentLayout {
    let scale = scale.max(0.0);
    if modal.w <= 0.0 || modal.h <= 0.0 {
        return SettingsPlaceholderContentLayout::default();
    }

    let pad_x = (28.0 * scale).min(modal.w * 0.2).max(0.0);
    let pad_y = (20.0 * scale).min(modal.h * 0.2).max(0.0);
    let clip = Rect {
        x: modal.x + pad_x,
        y: modal.y + pad_y,
        w: (modal.w - pad_x * 2.0).max(0.0),
        h: (modal.h - pad_y * 2.0).max(0.0),
    };
    let min_title_room = (12.0 * scale).max(1.0);
    if clip.w <= 0.0 || clip.h < min_title_room {
        return SettingsPlaceholderContentLayout {
            clip: (clip.w > 0.0 && clip.h > 0.0).then_some(clip),
            ..SettingsPlaceholderContentLayout::default()
        };
    }

    let bottom_inset = (6.0 * scale).min(clip.h * 0.2).max(0.0);
    let safe_bottom = clip.y + clip.h - bottom_inset;
    let title = SettingsPlaceholderLine {
        x: (modal.x + 28.0 * scale)
            .clamp(clip.x, clip.x + clip.w)
            .round(),
        baseline_y: (modal.y + 52.0 * scale)
            .clamp(clip.y, safe_bottom.max(clip.y))
            .round(),
    };
    let description_y = (title.baseline_y + 36.0 * scale).round();
    let description = (description_y <= safe_bottom + 0.001).then_some(SettingsPlaceholderLine {
        x: title.x,
        baseline_y: description_y,
    });

    SettingsPlaceholderContentLayout {
        clip: Some(clip),
        title: Some(title),
        description,
    }
}

fn clipped_rect(rect: Rect, clip: Rect) -> Option<Rect> {
    let x1 = rect.x.max(clip.x);
    let y1 = rect.y.max(clip.y);
    let x2 = (rect.x + rect.w).min(clip.x + clip.w);
    let y2 = (rect.y + rect.h).min(clip.y + clip.h);
    (x2 > x1 && y2 > y1).then_some(Rect { x: x1, y: y1, w: x2 - x1, h: y2 - y1 })
}

pub(crate) fn terminal_text_padding(scale: f32) -> (f32, f32) {
    (8.0 * scale, 8.0 * scale)
}

pub(crate) fn terminal_text_viewport_height(content_h: f32, scale: f32) -> f32 {
    let (top, bottom) = terminal_text_padding(scale);
    (content_h - top - bottom).max(0.0)
}

pub(crate) fn terminal_visible_rows(content_h: f32, char_h: f32, scale: f32) -> usize {
    (terminal_text_viewport_height(content_h, scale) / char_h.max(0.0001))
        .floor()
        .max(2.0) as usize
}

#[inline]
fn terminal_dimensions_changed(
    cols: usize,
    rows: usize,
    new_cols: usize,
    new_rows: usize,
) -> bool {
    cols != new_cols || rows != new_rows
}

fn prepare_terminal_grid_for_render(
    grid: &mut TermGrid,
    search: &mut TerminalSearchState,
    new_cols: usize,
    new_rows: usize,
) -> bool {
    let resized = terminal_dimensions_changed(grid.cols, grid.visible_rows, new_cols, new_rows);
    if resized {
        grid.resize(new_cols, new_rows);
    }
    if search.shown {
        search.refresh_for_grid(grid, SearchRefreshCause::Grid);
    }
    resized
}

pub(crate) fn terminal_max_scroll(
    total_lines: usize,
    char_h: f32,
    content_h: f32,
    scale: f32,
) -> f32 {
    (total_lines as f32 * char_h - terminal_text_viewport_height(content_h, scale)).max(0.0)
}

pub(crate) fn terminal_render_scroll_offset(current: f32, max_scroll: f32, is_alt: bool) -> f32 {
    if is_alt { 0.0 } else { current.clamp(0.0, max_scroll).round() }
}

pub(crate) fn visible_row_range(
    total_lines: usize,
    visible_rows: usize,
    scroll_offset: f32,
    char_h: f32,
) -> std::ops::Range<usize> {
    if total_lines == 0 || visible_rows == 0 || char_h <= 0.0 {
        return 0..0;
    }
    let scrolled_lines = (scroll_offset.max(0.0) / char_h).floor() as usize;
    let bottom = total_lines
        .saturating_sub(1)
        .saturating_sub(scrolled_lines.min(total_lines.saturating_sub(1)));
    let start = bottom.saturating_sub(visible_rows);
    let end = (bottom + 1).min(total_lines);
    start..end
}

pub(crate) fn terminal_scrollbar_layout(
    body: Rect,
    scale: f32,
    char_h: f32,
    total_lines: usize,
    current_scroll: f32,
) -> Option<TerminalScrollbarLayout> {
    let s = scale;
    let track = Rect {
        x: body.x + body.w - 12.0 * s,
        y: body.y + 4.0 * s,
        w: 8.0 * s,
        h: (body.h - 8.0 * s).max(1.0),
    };
    let max_scroll = terminal_max_scroll(total_lines, char_h, body.h, s);
    if max_scroll <= 0.0 {
        return None;
    }
    let viewport_h = terminal_text_viewport_height(body.h, s);
    let content_h = total_lines as f32 * char_h;
    let scroll_from_top = max_scroll - current_scroll.clamp(0.0, max_scroll);
    let thumb = scrollbar_thumb(
        track.y,
        track.h,
        viewport_h,
        content_h,
        scroll_from_top,
        20.0 * s,
    )?;
    Some(TerminalScrollbarLayout {
        track,
        thumb: Rect { x: track.x, y: thumb.start, w: track.w, h: thumb.len },
        max_scroll,
    })
}

pub(crate) fn terminal_scrollbar_drag_target(
    pointer_y: f32,
    layout: TerminalScrollbarLayout,
    drag_offset: Option<f32>,
) -> Option<(f32, f32)> {
    let (offset, scroll_from_top) = scrollbar_drag_target(
        pointer_y,
        layout.track.y,
        layout.track.h,
        ScrollbarThumb { start: layout.thumb.y, len: layout.thumb.h },
        layout.max_scroll,
        drag_offset,
    )?;
    Some((offset, layout.max_scroll - scroll_from_top))
}

fn terminal_glyph_anchor(
    c: char,
    glyph: GlyphInfo,
    cell_x: f32,
    cell_y: f32,
    cell_w: f32,
    cell_h: f32,
    baseline_y: f32,
    scale: f32,
) -> (f32, f32, f32) {
    if !terminal_force_text_presentation(c) || glyph.mode == COLOR_ATLAS_MODE {
        return (cell_x, baseline_y, scale);
    }

    let max_w = cell_w * 0.70;
    let max_h = cell_h * 0.58;
    let fit_scale = scale
        .min(max_w / glyph.width.max(1.0))
        .min(max_h / glyph.height.max(1.0));
    let fitted_w = glyph.width * fit_scale;
    let fitted_h = glyph.height * fit_scale;
    let x = cell_x + (cell_w - fitted_w) * 0.5 - glyph.offset_x * fit_scale;
    let y = cell_y + (cell_h - fitted_h) * 0.5 + glyph.offset_y * fit_scale;
    (x, y, fit_scale)
}

#[inline]
fn terminal_cell_span_x_bounds(
    text_x: f32,
    cell_index: usize,
    width_cells: usize,
    char_w: f32,
) -> (f32, f32) {
    (
        (text_x + cell_index as f32 * char_w).round(),
        (text_x + (cell_index + width_cells.max(1)) as f32 * char_w).round(),
    )
}

#[inline]
fn terminal_cell_x_bounds(text_x: f32, cell_index: usize, char_w: f32) -> (f32, f32) {
    terminal_cell_span_x_bounds(text_x, cell_index, 1, char_w)
}

#[inline]
fn terminal_render_color(color: TerminalColor, palette: TerminalPalette) -> [f32; 4] {
    if color.is_default_background() {
        palette.bg
    } else {
        terminal_color_rgba(color)
    }
}

#[inline]
fn terminal_dim_color(mut color: [f32; 4]) -> [f32; 4] {
    const DIM_MULTIPLIER: f32 = 0.66;
    color[0] *= DIM_MULTIPLIER;
    color[1] *= DIM_MULTIPLIER;
    color[2] *= DIM_MULTIPLIER;
    color
}

#[inline]
fn terminal_cell_render_colors(
    cell: &Cell,
    palette: TerminalPalette,
) -> ([f32; 4], Option<[f32; 4]>) {
    let source_fg = terminal_render_color(cell.fg, palette);
    let source_bg = terminal_render_color(cell.bg, palette);
    let (mut foreground, background) = if cell.is_inverse() {
        (source_bg, Some(source_fg))
    } else {
        (
            source_fg,
            (!cell.bg.is_default_background()).then_some(source_bg),
        )
    };
    if cell.is_dim() {
        foreground = terminal_dim_color(foreground);
    }
    (foreground, background)
}

#[inline]
fn terminal_underline_rect(
    text_x: f32,
    cell_index: usize,
    width_cells: usize,
    char_w: f32,
    draw_y: f32,
    char_h: f32,
    scale: f32,
) -> Rect {
    let (x1, x2) = terminal_cell_span_x_bounds(text_x, cell_index, width_cells, char_w);
    let height = scale.round().max(1.0);
    Rect {
        x: x1,
        y: (draw_y + char_h - height).round(),
        w: (x2 - x1).max(1.0),
        h: height,
    }
}

#[inline]
fn terminal_row_draw_y(
    body: Rect,
    bottom_pad: f32,
    char_h: f32,
    total_lines: usize,
    row: usize,
    scroll_offset: f32,
) -> f32 {
    let offset_from_bottom = total_lines.saturating_sub(1).saturating_sub(row);
    body.y + body.h - bottom_pad - char_h - offset_from_bottom as f32 * char_h + scroll_offset
}

#[inline]
fn one_line_next_x(current_x: f32, advance: f32) -> f32 {
    (current_x + advance).round()
}

#[inline]
fn one_line_cursor_hits_before_advance(target_x: f32, current_x: f32, advance: f32) -> bool {
    target_x <= current_x + advance * 0.5
}

fn one_line_cursor_from_advances(
    advances: impl IntoIterator<Item = f32>,
    target_x: f32,
) -> usize {
    let mut current_x = 0.0;
    let mut cursor = 0usize;
    for advance in advances {
        if one_line_cursor_hits_before_advance(target_x, current_x, advance) {
            return cursor;
        }
        current_x = one_line_next_x(current_x, advance);
        cursor += 1;
    }
    cursor
}

fn one_line_metrics_from_advances(
    advances: impl IntoIterator<Item = f32>,
    cursor: usize,
) -> (f32, f32) {
    let mut cursor_x = 0.0;
    let mut total_width = 0.0;
    for (index, advance) in advances.into_iter().enumerate() {
        if index < cursor {
            cursor_x = one_line_next_x(cursor_x, advance);
        }
        total_width = one_line_next_x(total_width, advance);
    }
    (cursor_x, total_width)
}

fn one_line_scroll_for_metrics(
    cursor_x: f32,
    total_width: f32,
    visible_width: f32,
    mut scroll_x: f32,
) -> f32 {
    let visible_width = visible_width.max(1.0);
    if cursor_x - scroll_x > visible_width {
        scroll_x = cursor_x - visible_width;
    } else if cursor_x < scroll_x {
        scroll_x = cursor_x;
    }
    scroll_x
        .min((total_width - visible_width).max(0.0))
        .max(0.0)
}

impl Renderer {
    pub(super) fn prewarm_search_icons(&mut self) {
        for icon in [
            SearchIcon::Close,
            SearchIcon::Previous,
            SearchIcon::Next,
            SearchIcon::Case,
        ] {
            let _ = self.search_icon(icon);
        }
    }

    fn search_icon(&mut self, icon: SearchIcon) -> Option<GlyphInfo> {
        let index = icon as usize;
        if let Some(cached) = self.search_icons[index] {
            return Some(cached);
        }
        let svg = match icon {
            SearchIcon::Close => include_bytes!("../icons/window-close.svg").as_slice(),
            SearchIcon::Previous => include_bytes!("../icons/go-up.svg").as_slice(),
            SearchIcon::Next => include_bytes!("../icons/go-down.svg").as_slice(),
            SearchIcon::Case => include_bytes!("../icons/format-text-uppercase.svg").as_slice(),
        };
        let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default()).ok()?;
        let size = tree.size();
        let target = 64.0f32;
        let scale = target / size.width().max(size.height()).max(1.0);
        let scaled_w = size.width() * scale;
        let scaled_h = size.height() * scale;
        let mut pixmap = tiny_skia::Pixmap::new(target as u32, target as u32)?;
        let transform = tiny_skia::Transform::from_row(
            scale,
            0.0,
            0.0,
            scale,
            (target - scaled_w) * 0.5,
            (target - scaled_h) * 0.5,
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let mut alpha = Vec::with_capacity((target * target) as usize);
        alpha.extend(pixmap.data().chunks_exact(4).map(|pixel| pixel[3]));
        let entry = self.upload_alpha(target as i32, target as i32, &alpha)?;
        let glyph = GlyphInfo {
            u: entry.u,
            v: entry.v,
            uw: entry.uw,
            vh: entry.vh,
            width: target,
            height: target,
            offset_x: 0.0,
            offset_y: target,
            advance: target,
            mode: 0.0,
        };
        self.search_icons[index] = Some(glyph);
        Some(glyph)
    }

    fn draw_search_icon(&mut self, icon: SearchIcon, rect: Rect, color: [f32; 4]) {
        let Some(glyph) = self.search_icon(icon) else {
            return;
        };
        let size = rect.w.min(rect.h) * 0.56;
        let x = (rect.x + (rect.w - size) * 0.5).round();
        let y = (rect.y + (rect.h - size) * 0.5).round();
        self.push_quad(
            x,
            y,
            size,
            size,
            glyph.u,
            glyph.v,
            glyph.uw,
            glyph.vh,
            color,
            glyph.mode,
            [0.0, 0.0, 0.0],
        );
    }

    fn terminal_glyph(&mut self, c: char, presentation: u8) -> Option<GlyphInfo> {
        let prefer_color = if terminal_force_text_presentation(c) {
            Some(false)
        } else if presentation == CELL_PRESENTATION_TEXT {
            Some(false)
        } else if presentation == CELL_PRESENTATION_EMOJI {
            Some(true)
        } else {
            None
        };
        self.get_glyph(c, prefer_color)
    }

    fn terminal_char_width(&self) -> f32 {
        self.ascii_advances[b'A' as usize].max(1.0) * TERMINAL_TEXT_SCALE
    }

    fn search_text_advance(&mut self, c: char, text_scale: f32) -> f32 {
        self.one_line_ui_char_layout(c, text_scale).1
    }

    pub(crate) fn terminal_cursor_from_input_x(
        &mut self,
        text: &str,
        x: f32,
        scroll_x: f32,
    ) -> usize {
        let target = (x + scroll_x).max(0.0);
        let mut chars = text.chars();
        let advances = std::iter::from_fn(|| {
            chars.next().map(|c| self.search_text_advance(c, 1.0))
        });
        one_line_cursor_from_advances(advances, target)
    }

    fn search_text_metrics(&mut self, text: &str, cursor: usize) -> (f32, f32) {
        let mut chars = text.chars();
        let advances = std::iter::from_fn(|| {
            chars.next().map(|c| self.search_text_advance(c, 1.0))
        });
        one_line_metrics_from_advances(advances, cursor)
    }

    fn search_cursor_px(&mut self, text: &str, cursor: usize) -> f32 {
        self.search_text_metrics(text, cursor).0
    }

    fn search_selection_px(&mut self, search: &TerminalSearchState) -> Option<(f32, f32)> {
        let anchor = search.selection_anchor?;
        if anchor == search.cursor {
            return None;
        }
        let start = anchor.min(search.cursor);
        let end = anchor.max(search.cursor);
        Some((
            self.search_cursor_px(&search.query, start),
            self.search_cursor_px(&search.query, end),
        ))
    }

    fn draw_search_overlay(
        &mut self,
        search: &mut TerminalSearchState,
        body: Rect,
    ) -> TerminalSearchGeometry {
        let s = self.scale_factor;
        let palette = self.palette;
        let geometry = terminal_search_geometry(self.width, body.y, s);
        let radius = 6.0 * s;
        self.push_rounded_rect(
            geometry.outer.x,
            geometry.outer.y,
            geometry.outer.w,
            geometry.outer.h,
            radius,
            [0.18, 0.20, 0.22, 1.0],
        );
        self.push_rounded_rect(
            geometry.outer.x - 1.0,
            geometry.outer.y - 1.0,
            geometry.outer.w + 2.0,
            geometry.outer.h + 2.0,
            radius,
            palette.accent_with_alpha(0.6),
        );
        self.push_rounded_rect(
            geometry.outer.x,
            geometry.outer.y,
            geometry.outer.w,
            geometry.outer.h,
            radius,
            palette.minimap_bg,
        );

        if geometry.input.w > 0.0 {
            let border = (1.0 * s).round().max(1.0);
            let input_radius = (4.0 * s).round().max(0.0);
            let input_border = if search.focused {
                [0.60, 0.35, 0.85, 1.0]
            } else {
                [1.0, 1.0, 1.0, 0.14]
            };
            self.push_rounded_rect(
                geometry.input.x,
                geometry.input.y,
                geometry.input.w,
                geometry.input.h,
                input_radius,
                input_border,
            );
            self.push_rounded_rect(
                geometry.input.x + border,
                geometry.input.y + border,
                (geometry.input.w - border * 2.0).max(0.0),
                (geometry.input.h - border * 2.0).max(0.0),
                (input_radius - border).max(0.0),
                [0.08, 0.09, 0.12, 1.0],
            );

            let (cursor_px, total_text_width) =
                self.search_text_metrics(&search.query, search.cursor);
            let horizontal_padding = (5.0 * s).round();
            search.scroll_x = one_line_scroll_for_metrics(
                cursor_px,
                total_text_width,
                geometry.text_viewport_w,
                search.scroll_x,
            );
            let scroll_x = search.scroll_x.round();
            let text_x = (geometry.input.x.round() + horizontal_padding - scroll_x).round();
            let baseline = (geometry.input.y + geometry.input.h * 0.5 + 5.5 * s).round();
            let selection_y = (geometry.input.y + 5.0 * s).round();
            let selection_h = (geometry.input.h - 10.0 * s).round().max(1.0);

            self.flush();
            self.set_clip(
                geometry.input.x.round() + horizontal_padding,
                geometry.input.y.round(),
                geometry.text_viewport_w.round().max(0.0),
                geometry.input.h.round().max(1.0),
            );
            if let Some((selection_start, selection_end)) = self.search_selection_px(search) {
                self.push_rounded_rect(
                    (text_x + selection_start).round(),
                    selection_y,
                    (selection_end - selection_start).round().max(1.0),
                    selection_h,
                    0.0,
                    palette.accent,
                );
            }
            self.draw_ui_text(&search.query, text_x, baseline, palette.fg, 1.0);
            if search.focused && search.selection_anchor.is_none_or(|anchor| anchor == search.cursor) {
                self.push_rounded_rect(
                    (text_x + cursor_px).round(),
                    selection_y,
                    (1.5 * s).round().max(1.0),
                    selection_h,
                    0.0,
                    palette.fg,
                );
            }
            self.flush();
            self.clear_clip();
        }

        let muted = [0.82, 0.82, 0.84, 1.0];
        self.draw_search_icon(SearchIcon::Close, geometry.close, muted);
        if geometry.show_nav {
            self.draw_search_icon(SearchIcon::Next, geometry.next, muted);
            self.draw_search_icon(SearchIcon::Previous, geometry.previous, muted);
        }
        if geometry.show_case {
            self.draw_search_icon(
                SearchIcon::Case,
                geometry.case_toggle,
                if search.case_sensitive { palette.accent } else { muted },
            );
        }

        if !search.query.is_empty() || !search.results.is_empty() {
            let mut counter = std::mem::take(&mut self.scratch_buffer);
            counter.clear();
            if search.results.is_empty() {
                if geometry.counter_reserve > 0.0 {
                    counter.push_str("Нет");
                }
            } else {
                use std::fmt::Write as _;
                let _ = write!(
                    counter,
                    "{}/{}",
                    search.current.unwrap_or(0) + 1,
                    search.results.len()
                );
            }
            if !counter.is_empty() {
                let counter_x = geometry.input.x + geometry.input.w + 10.0 * s;
                let baseline = (geometry.input.y + geometry.input.h * 0.5 + 5.5 * s).round();
                self.draw_ui_text(&counter, counter_x, baseline, [0.6, 0.6, 0.6, 1.0], 0.9);
            }
            self.scratch_buffer = counter;
        }
        geometry
    }

    fn terminal_ui_text_width(&mut self, text: &str, scale: f32) -> f32 {
        let mut width = 0.0;
        for c in text.chars() {
            let (_, advance) = self.one_line_ui_char_layout(c, scale);
            width = (width + advance).round();
        }
        width
    }

    fn draw_ui_text_clipped(
        &mut self,
        text: &str,
        mut x: f32,
        max_x: f32,
        baseline_y: f32,
        color: [f32; 4],
        scale: f32,
    ) {
        for c in text.chars() {
            let (glyph, advance) = self.one_line_ui_char_layout(c, scale);
            if x + advance > max_x {
                break;
            }
            if let Some(glyph) = glyph {
                self.push_glyph(x, baseline_y, glyph, color, scale);
            }
            x = (x + advance).round();
        }
    }

    fn draw_terminal_tab_edge_fades(&mut self, scroll_x: f32, max_scroll: f32, strip: Rect) {
        if max_scroll <= 0.0 || strip.w <= 0.0 {
            return;
        }
        let fade_w = (TAB_STRIP_EDGE_FADE_WIDTH * self.scale_factor)
            .min(strip.w * 0.25)
            .max(1.0);
        let left_alpha = (scroll_x.clamp(0.0, max_scroll) / fade_w).clamp(0.0, 1.0)
            * TAB_STRIP_EDGE_FADE_ALPHA;
        let right_alpha = ((max_scroll - scroll_x.clamp(0.0, max_scroll)) / fade_w)
            .clamp(0.0, 1.0)
            * TAB_STRIP_EDGE_FADE_ALPHA;
        let bands = 4;
        let band_w = (fade_w / bands as f32).max(1.0);
        for index in 0..bands {
            let t = 1.0 - index as f32 / bands as f32;
            if left_alpha > 0.0 {
                self.push_rounded_rect(
                    strip.x + index as f32 * band_w,
                    strip.y,
                    band_w + 1.0,
                    strip.h,
                    0.0,
                    [self.palette.bg[0], self.palette.bg[1], self.palette.bg[2], left_alpha * t],
                );
            }
            if right_alpha > 0.0 {
                self.push_rounded_rect(
                    strip.x + strip.w - (index as f32 + 1.0) * band_w,
                    strip.y,
                    band_w + 1.0,
                    strip.h,
                    0.0,
                    [self.palette.bg[0], self.palette.bg[1], self.palette.bg[2], right_alpha * t],
                );
            }
        }
    }

    fn draw_terminal_tabs(
        &mut self,
        terminals: &[Terminal],
        active_terminal: usize,
        scroll_x: f32,
        drag: Option<&crate::tabs::TabDragState>,
        pointer_x: f32,
        pointer_y: f32,
    ) {
        let s = self.scale_factor;
        let strip = terminal_tab_strip_rect(self.width, s);
        let pad = TERMINAL_TAB_BAR_PAD * s;
        let add_size = (TERMINAL_ADD_SIZE * s).round().max(1.0);

        let mut titles = std::mem::take(&mut self.terminal_tab_display_titles);
        titles.resize_with(terminals.len(), || String::with_capacity(96));
        for (terminal, title) in terminals.iter().zip(titles.iter_mut()) {
            terminal.write_display_title(title);
        }

        let mut widths = std::mem::take(&mut self.terminal_tab_widths);
        widths.clear();
        widths.reserve(terminals.len());
        for title in &titles {
            widths.push(
                (self.terminal_ui_text_width(title, 1.0) + TERMINAL_TAB_NATURAL_CHROME * s)
                    .round()
                    .max(72.0 * s),
            );
        }
        let max_scroll = terminal_tab_max_scroll(&widths, strip.w, s);
        let scroll_x = scroll_x.clamp(0.0, max_scroll);
        let base_x = (strip.x + pad - scroll_x.round()).round();
        self.terminal_tab_base_x = base_x;

        let mut actual_xs = std::mem::take(&mut self.terminal_tab_actual_xs);
        let mut order = std::mem::take(&mut self.terminal_tab_order);
        let dragged_idx = crate::tabs::tab_drag_layout(
            base_x,
            &widths,
            drag,
            &mut actual_xs,
            &mut order,
        );
        self.terminal_tab_animation_active = crate::tabs::update_tab_x_animation(
            &mut self.terminal_tab_x_anim,
            &actual_xs,
            dragged_idx,
        );
        let mut render_order = std::mem::take(&mut self.terminal_tab_render_order);
        crate::tabs::tab_drag_render_order(&order, dragged_idx, &mut render_order);

        let mut hitboxes = std::mem::take(&mut self.terminal_tab_hitboxes);
        hitboxes.clear();
        hitboxes.resize(terminals.len(), TerminalTabHitbox::default());

        self.flush();
        self.set_clip(strip.x, strip.y, strip.w, strip.h);
        for &idx in &render_order {
            if idx >= widths.len() || idx >= self.terminal_tab_x_anim.len() {
                continue;
            }
            let tab_x = self.terminal_tab_x_anim[idx].round();
            let tab_w = widths[idx].round().max(1.0);
            let raw_body = Rect { x: tab_x, y: strip.y, w: tab_w, h: strip.h };
            let visible_body = clipped_rect(raw_body, strip);
            let hovered = visible_body.is_some_and(|rect| rect.contains(pointer_x, pointer_y));
            let active = idx == active_terminal;
            let mut bg = self.palette.bg;
            if hovered {
                bg = [
                    (bg[0] + 0.02).min(1.0),
                    (bg[1] + 0.02).min(1.0),
                    (bg[2] + 0.02).min(1.0),
                    bg[3],
                ];
            }
            self.push_rounded_rect(tab_x, strip.y, tab_w, strip.h, 0.0, bg);
            if active {
                self.push_rounded_rect(
                    tab_x,
                    strip.y + strip.h - (2.0 * s).round().max(1.0),
                    tab_w,
                    (2.0 * s).round().max(1.0),
                    0.0,
                    self.palette.accent,
                );
            }
            if idx + 1 < terminals.len() {
                self.push_rounded_rect(
                    tab_x + tab_w - 1.0,
                    strip.y + strip.h * 0.25,
                    1.0,
                    strip.h * 0.5,
                    0.0,
                    [self.palette.fg[0], self.palette.fg[1], self.palette.fg[2], 0.14],
                );
            }

            let can_show_close = tab_w >= 56.0 * s;
            let show_close = terminal_tab_show_close(tab_w, s, active, hovered);
            hitboxes[idx] = terminal_tab_hitbox_geometry(tab_x, tab_w, strip, show_close, s);
            let close_icon = show_close
                .then(|| terminal_tab_close_geometry(tab_x, tab_w, strip, s).0);

            let title_x = (tab_x + TERMINAL_TAB_TEXT_PAD * s).round();
            let title_max_w = (tab_w - if can_show_close { 56.0 * s } else { 32.0 * s })
                .max(0.0);
            let title_max_x = title_x + title_max_w;
            let baseline = (strip.y + strip.h * 0.5 + 5.0 * s).round();
            let title_color = if active { self.palette.fg } else { [self.palette.fg[0], self.palette.fg[1], self.palette.fg[2], 0.72] };
            if title_max_x > title_x {
                self.draw_ui_text_clipped(&titles[idx], title_x, title_max_x, baseline, title_color, 1.0);
            }
            if let Some(close_icon) = close_icon.filter(|icon| clipped_rect(*icon, strip).is_some()) {
                self.draw_search_icon(SearchIcon::Close, close_icon, [0.82, 0.82, 0.84, 1.0]);
            }
        }

        let add_x = terminal_tab_add_x(base_x, &widths, s);
        let add_y = (strip.y + (strip.h - add_size) * 0.5).round();
        let add_raw = Rect { x: add_x, y: add_y, w: add_size, h: add_size };
        let add = clipped_rect(add_raw, strip);
        if add.is_some() {
            let hovered = add.is_some_and(|rect| rect.contains(pointer_x, pointer_y));
            if hovered {
                self.push_rounded_rect(
                    add_raw.x,
                    add_raw.y,
                    add_raw.w,
                    add_raw.h,
                    4.0 * s,
                    [self.palette.fg[0], self.palette.fg[1], self.palette.fg[2], 0.10],
                );
            }
            let thickness = (1.5 * s).round().max(1.0);
            let arm = (9.0 * s).round().max(thickness);
            let cx = (add_raw.x + add_raw.w * 0.5).round();
            let cy = (add_raw.y + add_raw.h * 0.5).round();
            self.push_rounded_rect(
                cx - arm * 0.5,
                cy - thickness * 0.5,
                arm,
                thickness,
                thickness * 0.5,
                [0.82, 0.82, 0.84, 1.0],
            );
            self.push_rounded_rect(
                cx - thickness * 0.5,
                cy - arm * 0.5,
                thickness,
                arm,
                thickness * 0.5,
                [0.82, 0.82, 0.84, 1.0],
            );
        }
        self.flush();
        self.clear_clip();
        self.draw_terminal_tab_edge_fades(scroll_x, max_scroll, strip);
        self.flush();

        self.terminal_tab_strip_layout = TerminalTabStripLayout { rect: strip, max_scroll, add };
        self.terminal_tab_display_titles = titles;
        self.terminal_tab_widths = widths;
        self.terminal_tab_actual_xs = actual_xs;
        self.terminal_tab_order = order;
        self.terminal_tab_render_order = render_order;
        self.terminal_tab_hitboxes = hitboxes;
    }

    fn draw_settings_placeholder(&mut self, progress: f32) {
        let progress = progress.clamp(0.0, 1.0);
        if progress <= 0.0 {
            return;
        }
        let s = self.scale_factor;
        let smooth = progress * progress * (3.0 - 2.0 * progress);
        self.push_rounded_rect(
            0.0,
            0.0,
            self.width,
            self.height,
            0.0,
            [0.0, 0.0, 0.0, 0.60 * smooth],
        );
        let modal = settings_placeholder_layout(self.width, self.height, s, progress);
        let radius = (12.0 * s).round().max(1.0);
        self.push_rounded_rect(
            modal.x,
            modal.y,
            modal.w,
            modal.h,
            radius,
            [0.18, 0.19, 0.24, 1.0],
        );
        let border = (1.0 * s).round().max(1.0).min(modal.h);
        if modal.w > 0.0 && border > 0.0 {
            self.push_rounded_rect(
                modal.x,
                modal.y,
                modal.w,
                border,
                0.0,
                self.palette.accent,
            );
        }
        self.flush();
        let content = settings_placeholder_content_layout(modal, s);
        if let Some(clip) = content.clip {
            self.set_clip(clip.x, clip.y, clip.w, clip.h);
            if let Some(title) = content.title {
                self.draw_ui_text(
                    "Settings",
                    title.x,
                    title.baseline_y,
                    self.palette.fg,
                    1.05,
                );
            }
            if let Some(description) = content.description {
                self.draw_ui_text(
                    "Terminal settings will be added later.",
                    description.x,
                    description.baseline_y,
                    [self.palette.fg[0], self.palette.fg[1], self.palette.fg[2], 0.62],
                    0.90,
                );
            }
            self.flush();
            self.clear_clip();
        }
    }

    pub(crate) fn terminal_tab_hit_test(&self, x: f32, y: f32) -> TerminalTabHit {
        for (idx, hitbox) in self.terminal_tab_hitboxes.iter().enumerate() {
            if hitbox.close.is_some_and(|rect| rect.contains(x, y)) {
                return TerminalTabHit::Close(idx);
            }
            if hitbox.body.is_some_and(|rect| rect.contains(x, y)) {
                return TerminalTabHit::Body(idx);
            }
        }
        if self
            .terminal_tab_strip_layout
            .add
            .is_some_and(|rect| rect.contains(x, y))
        {
            TerminalTabHit::Add
        } else {
            TerminalTabHit::None
        }
    }

    pub(crate) fn terminal_tab_strip_layout(&self) -> TerminalTabStripLayout {
        self.terminal_tab_strip_layout
    }

    pub(crate) fn terminal_tab_drag_destination(
        &self,
        drag: &crate::tabs::TabDragState,
    ) -> Option<usize> {
        crate::tabs::tab_drag_placement(
            self.terminal_tab_base_x,
            &self.terminal_tab_widths,
            Some(drag),
        )
        .map(|placement| placement.destination)
    }

    pub(crate) fn terminal_tab_reveal_target(
        &self,
        active_idx: usize,
        reveal_tail: bool,
        current_target: f32,
    ) -> f32 {
        if active_idx >= self.terminal_tab_widths.len() {
            return 0.0;
        }
        let max_scroll = self.terminal_tab_strip_layout.max_scroll;
        if reveal_tail && active_idx + 1 == self.terminal_tab_widths.len() {
            return max_scroll;
        }
        let pad = TERMINAL_TAB_BAR_PAD * self.scale_factor;
        let viewport_w = (self.terminal_tab_strip_layout.rect.w - pad * 2.0).max(0.0);
        let target = crate::tabs::tab_strip_reveal_target(
            &self.terminal_tab_widths,
            active_idx,
            viewport_w,
            current_target,
            8.0 * self.scale_factor,
        );
        target.clamp(0.0, max_scroll)
    }

    pub(crate) fn terminal_tab_animation_active(&self) -> bool {
        self.terminal_tab_animation_active
    }

    pub(crate) fn render_terminal_app(
        &mut self,
        terminals: &[Terminal],
        active_terminal: usize,
        search: &mut TerminalSearchState,
        focused: bool,
        tab_scroll_x: f32,
        drag: Option<&crate::tabs::TabDragState>,
        pointer_x: f32,
        pointer_y: f32,
        settings_progress: f32,
    ) -> TerminalUiLayout {
        let layout = if let Some(terminal) = terminals.get(active_terminal) {
            self.render_terminal_body(terminal, search, focused && settings_progress <= 0.0)
        } else {
            let palette = self.palette;
            unsafe {
                self.gl.clear_color(palette.bg[0], palette.bg[1], palette.bg[2], palette.bg[3]);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
            TerminalUiLayout::default()
        };
        self.draw_terminal_tabs(
            terminals,
            active_terminal,
            tab_scroll_x,
            drag,
            pointer_x,
            pointer_y,
        );
        self.draw_settings_placeholder(settings_progress);
        layout
    }

    pub(crate) fn render_terminal_body(
        &mut self,
        terminal: &Terminal,
        search: &mut TerminalSearchState,
        focused: bool,
    ) -> TerminalUiLayout {
        let palette = self.palette;
        unsafe {
            self.gl.clear_color(palette.bg[0], palette.bg[1], palette.bg[2], palette.bg[3]);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let s = self.scale_factor;
        let body_top = terminal_tab_body_top(s);
        let body = Rect {
            x: 0.0,
            y: body_top,
            w: self.width,
            h: (self.height - body_top).max(0.0),
        };
        let char_w = self.terminal_char_width();
        let char_h = self.line_height * TERMINAL_TEXT_SCALE;
        let visible_rows = terminal_visible_rows(body.h, char_h, s);
        let new_cols = ((body.w - 20.0 * s) / char_w).floor().max(10.0) as usize;
        let new_rows = visible_rows;

        let mut grid = crate::platform::lock_recover(&terminal.grid);
        let resized =
            prepare_terminal_grid_for_render(&mut grid, search, new_cols, new_rows);
        grid.mark_presentation_layout_ready();
        if resized {
            drop(grid);
            terminal.resize_pty(
                u16::try_from(new_cols.min(u16::MAX as usize)).unwrap_or(u16::MAX),
                u16::try_from(new_rows.min(u16::MAX as usize)).unwrap_or(u16::MAX),
            );
            grid = crate::platform::lock_recover(&terminal.grid);
            prepare_terminal_grid_for_render(&mut grid, search, new_cols, new_rows);
        }
        let presentation_visible = grid.presentation_visible();
        grid.dirty = false;
        let scrollback_len = if grid.is_alt { 0 } else { grid.scrollback.len() };
        let total_lines = scrollback_len + grid.lines.len();
        let max_scroll = if grid.is_alt { 0.0 } else { terminal_max_scroll(total_lines, char_h, body.h, s) };
        let scroll_offset = terminal_render_scroll_offset(terminal.scroll_y.current, max_scroll, grid.is_alt);
        let text_x = body.x + 10.0 * s;
        let (_, bottom_pad) = terminal_text_padding(s);

        self.flush();
        self.set_clip(body.x, body.y, body.w, body.h);
        if presentation_visible {
            for i in visible_row_range(total_lines, visible_rows, scroll_offset, char_h) {
                let draw_y = terminal_row_draw_y(
                    body,
                    bottom_pad,
                    char_h,
                    total_lines,
                    i,
                    scroll_offset,
                );
                if draw_y + char_h < body.y || draw_y > body.y + body.h {
                    continue;
                }
                let row = if i < scrollback_len { &grid.scrollback[i] } else { &grid.lines[i - scrollback_len] };
                let row_matches = if search.shown { search.row_matches(i) } else { &[] };
                let active_search_match = search.active_match();
                let mut row_match_index = 0usize;
                for (cell_index, cell) in row.iter().take(grid.cols).enumerate() {
                    let (x, next_x) = terminal_cell_x_bounds(text_x, cell_index, char_w);
                    let cell_w = (next_x - x).max(1.0);
                    let logical_cell_index = if cell.is_wide_spacer() {
                        cell_index.saturating_sub(1)
                    } else {
                        cell_index
                    };
                    let (fg, mut bg) = terminal_cell_render_colors(cell, palette);
                    if let Some((sx, sy, ex, ey)) = grid.selection {
                        let (start_x, start_y, end_x, end_y) = normalized_selection_bounds(sx, sy, ex, ey);
                        let selected = if i > start_y && i < end_y { true }
                            else if i == start_y && i == end_y { logical_cell_index >= start_x && logical_cell_index <= end_x }
                            else if i == start_y { logical_cell_index >= start_x }
                            else if i == end_y { logical_cell_index <= end_x }
                            else { false };
                        if selected { bg = Some(palette.accent); }
                    }
                    while row_match_index < row_matches.len()
                        && row_matches[row_match_index].end_x < logical_cell_index
                    {
                        row_match_index += 1;
                    }
                    if let Some(item) = row_matches.get(row_match_index) {
                        if logical_cell_index >= item.start_x && logical_cell_index <= item.end_x {
                            let active = active_search_match.is_some_and(|active| active == *item);
                            bg = Some(if active { SEARCH_ACTIVE } else { SEARCH_MATCH });
                        }
                    }
                    if let Some(color) = bg { self.push_rounded_rect(x, draw_y, cell_w, char_h, 0.0, color); }
                    if cell.c != ' ' && !cell.is_wide_spacer() {
                        let glyph_cell_w = if cell.is_wide() {
                            let (_, wide_x2) =
                                terminal_cell_span_x_bounds(text_x, cell_index, 2, char_w);
                            (wide_x2 - x).max(1.0)
                        } else {
                            cell_w
                        };
                        if let Some(glyph) = self.terminal_glyph(cell.c, cell.presentation) {
                            let baseline = draw_y + self.baseline_offset * TERMINAL_TEXT_SCALE;
                            let (gx, gy, gs) = terminal_glyph_anchor(cell.c, glyph, x, draw_y, glyph_cell_w, char_h, baseline, TERMINAL_TEXT_SCALE);
                            let (qx, qy, qw, qh) = glyph_quad_rect(gx, gy, glyph, gs);
                            self.push_quad(qx, qy, qw, qh, glyph.u, glyph.v, glyph.uw, glyph.vh, fg, glyph.mode, [0.0, 0.0, 0.0]);
                        }
                        for &extra in cell.zero_width() {
                            if !terminal_should_render_zero_width(extra) {
                                continue;
                            }
                            if let Some(glyph) = self.terminal_glyph(extra, cell.presentation) {
                                let baseline = draw_y + self.baseline_offset * TERMINAL_TEXT_SCALE;
                                let (gx, gy, gs) = terminal_glyph_anchor(extra, glyph, x, draw_y, glyph_cell_w, char_h, baseline, TERMINAL_TEXT_SCALE);
                                let (qx, qy, qw, qh) = glyph_quad_rect(gx, gy, glyph, gs);
                                self.push_quad(qx, qy, qw, qh, glyph.u, glyph.v, glyph.uw, glyph.vh, fg, glyph.mode, [0.0, 0.0, 0.0]);
                            }
                        }
                    }
                    if cell.is_underlined() && !cell.is_wide_spacer() {
                        let underline = terminal_underline_rect(
                            text_x,
                            cell_index,
                            if cell.is_wide() { 2 } else { 1 },
                            char_w,
                            draw_y,
                            char_h,
                            s,
                        );
                        self.push_rounded_rect(
                            underline.x,
                            underline.y,
                            underline.w,
                            underline.h,
                            0.0,
                            fg,
                        );
                    }
                }
            }

            if focused && grid.cursor_visible {
                let cursor_offset = grid.lines.len().saturating_sub(1).saturating_sub(grid.cur_y);
                let cursor_y = body.y + body.h - bottom_pad - char_h
                    - cursor_offset as f32 * char_h + scroll_offset;
                if cursor_y + char_h >= body.y && cursor_y <= body.y + body.h {
                    let (x1, x2) = terminal_cell_x_bounds(text_x, grid.cur_x, char_w);
                    self.push_rounded_rect(x1, cursor_y, (x2 - x1).max(1.0), char_h, 0.0, [1.0, 1.0, 1.0, 0.5]);
                }
            }
        }
        let scrollbar = if presentation_visible {
            terminal_scrollbar_layout(body, s, char_h, total_lines, terminal.scroll_y.current)
        } else { None };
        if let Some(scrollbar) = scrollbar {
            self.push_rounded_rect(scrollbar.thumb.x, scrollbar.thumb.y, scrollbar.thumb.w, scrollbar.thumb.h, scrollbar.thumb.w / 2.0, SCROLLBAR);
        }
        self.flush();
        self.clear_clip();
        drop(grid);

        if focused {
            let border = (2.0 * s).max(1.0).round();
            self.push_rounded_rect(body.x, body.y, body.w, border, 0.0, palette.accent);
            self.push_rounded_rect(
                body.x,
                body.y + body.h - border,
                body.w,
                border,
                0.0,
                palette.accent,
            );
            self.push_rounded_rect(body.x, body.y, border, body.h, 0.0, palette.accent);
            self.push_rounded_rect(
                body.x + body.w - border,
                body.y,
                border,
                body.h,
                0.0,
                palette.accent,
            );
        }

        let search_geometry = search.shown.then(|| self.draw_search_overlay(search, body));
        self.flush();

        TerminalUiLayout {
            body,
            text_x,
            char_w,
            char_h,
            total_lines,
            visible_rows,
            cols: new_cols,
            max_scroll,
            scroll_offset,
            bottom_pad,
            scale: s,
            scrollbar,
            search: search_geometry,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_glyph(width: f32, height: f32, mode: f32) -> GlyphInfo {
        GlyphInfo {
            u: 0.0,
            v: 0.0,
            uw: 0.0,
            vh: 0.0,
            width,
            height,
            offset_x: 1.0,
            offset_y: 12.0,
            advance: width,
            mode,
        }
    }

    #[test]
    fn visible_rows_only_cover_viewport_plus_one_overscan_row() {
        assert_eq!(visible_row_range(10_000, 30, 0.0, 20.0), 9969..10000);
        assert_eq!(visible_row_range(10_000, 30, 40.0, 20.0), 9967..9998);
        assert!(visible_row_range(2, 30, 0.0, 20.0).len() <= 2);
    }

    #[test]
    fn rendered_scroll_offset_rounds_pixels_without_quantizing_animation_time() {
        assert_eq!(terminal_render_scroll_offset(3.4, 100.0, false), 3.0);
        assert_eq!(terminal_render_scroll_offset(3.6, 100.0, false), 4.0);
        assert_eq!(terminal_render_scroll_offset(30.0, 100.0, true), 0.0);
    }

    #[test]
    fn terminal_scrollbar_maps_bottom_to_bottom_and_top_to_top() {
        let body = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        let bottom = terminal_scrollbar_layout(body, 1.0, 20.0, 100, 0.0).unwrap();
        let top = terminal_scrollbar_layout(body, 1.0, 20.0, 100, bottom.max_scroll).unwrap();

        assert!((bottom.thumb.y + bottom.thumb.h - (bottom.track.y + bottom.track.h)).abs() < 0.01);
        assert!((top.thumb.y - top.track.y).abs() < 0.01);

        let (_, drag_top) = terminal_scrollbar_drag_target(
            bottom.track.y,
            bottom,
            Some(0.0),
        )
        .unwrap();
        let (_, drag_bottom) = terminal_scrollbar_drag_target(
            bottom.track.y + bottom.track.h,
            bottom,
            Some(0.0),
        )
        .unwrap();
        assert!((drag_top - bottom.max_scroll).abs() < 0.01);
        assert!(drag_bottom.abs() < 0.01);
    }

    #[test]
    fn terminal_scrollbar_middle_position_round_trips_through_drag_mapping() {
        let body = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        let max_scroll = terminal_max_scroll(100, 20.0, body.h, 1.0);
        let current = max_scroll * 0.37;
        let layout = terminal_scrollbar_layout(body, 1.0, 20.0, 100, current).unwrap();
        let pointer = layout.thumb.y + layout.thumb.h * 0.5;
        let (_, target) = terminal_scrollbar_drag_target(pointer, layout, None).unwrap();
        assert!((target - current).abs() < 0.01);
    }

    #[test]
    fn fractional_terminal_viewport_drives_exact_max_scroll_and_top_geometry() {
        let body = Rect { x: 0.0, y: 0.0, w: 800.0, h: 584.0 };
        let viewport = terminal_text_viewport_height(body.h, 1.0);
        assert_eq!(viewport, 568.0);
        assert_eq!(terminal_visible_rows(body.h, 20.0, 1.0), 28);
        let max_scroll = terminal_max_scroll(40, 20.0, body.h, 1.0);
        assert_eq!(max_scroll, 232.0);
        assert_ne!(max_scroll, (40 - 28) as f32 * 20.0);

        let layout = terminal_scrollbar_layout(body, 1.0, 20.0, 40, max_scroll).unwrap();
        assert!((layout.thumb.y - layout.track.y).abs() < 0.01);
        let (_, bottom_pad) = terminal_text_padding(1.0);
        let top_row_y = terminal_row_draw_y(body, bottom_pad, 20.0, 40, 0, max_scroll);
        assert_eq!(top_row_y, 8.0);
        assert_eq!(visible_row_range(40, 28, max_scroll, 20.0).start, 0);
    }

    #[test]
    fn forced_text_checkmark_uses_rriter_width_and_height_fit() {
        let glyph = test_glyph(14.0, 20.0, 0.0);
        let cell_x = 20.0;
        let row_y = 40.0;
        let cell_w = 10.0;
        let char_h = 20.0;
        let scale = TERMINAL_TEXT_SCALE;
        let (x, y, fit_scale) = terminal_glyph_anchor(
            '✓',
            glyph,
            cell_x,
            row_y,
            cell_w,
            char_h,
            row_y + 15.0,
            scale,
        );
        assert!(fit_scale < scale);
        let fitted_w = glyph.width * fit_scale;
        let fitted_h = glyph.height * fit_scale;
        assert!(fitted_w <= cell_w * 0.70 + 0.001);
        assert!(fitted_h <= char_h * 0.58 + 0.001);
        let quad_left = x + glyph.offset_x * fit_scale;
        let quad_top = y - glyph.offset_y * fit_scale;
        assert!(quad_left >= cell_x - 0.001);
        assert!(quad_left + fitted_w <= cell_x + cell_w + 0.001);
        assert!(quad_top >= row_y - 0.001);
        assert!(quad_top + fitted_h <= row_y + char_h + 0.001);
    }

    #[test]
    fn regular_glyph_and_color_emoji_keep_unmodified_terminal_anchor() {
        let regular = test_glyph(14.0, 20.0, 0.0);
        assert_eq!(
            terminal_glyph_anchor('A', regular, 20.0, 40.0, 10.0, 20.0, 55.0, 1.05),
            (20.0, 55.0, 1.05)
        );
        let emoji = test_glyph(14.0, 20.0, COLOR_ATLAS_MODE);
        assert_eq!(
            terminal_glyph_anchor('✓', emoji, 20.0, 40.0, 10.0, 20.0, 55.0, 1.05),
            (20.0, 55.0, 1.05)
        );
    }

    #[test]
    fn fractional_cell_widths_are_pixel_contiguous_without_accumulated_drift() {
        let text_x = 10.333;
        let char_w = 9.75;
        let mut previous_end = None;
        for index in 0..128 {
            let (x1, x2) = terminal_cell_x_bounds(text_x, index, char_w);
            assert!(x2 >= x1);
            if let Some(end) = previous_end {
                assert_eq!(x1, end);
            }
            previous_end = Some(x2);
        }
        assert_eq!(
            previous_end.unwrap(),
            (text_x + 128.0 * char_w).round()
        );
    }

    #[test]
    fn fractional_wide_glyph_spans_use_two_real_snapped_cell_boundaries() {
        for char_w in [7.4, 10.4, 9.75] {
            let text_x = 0.0;
            for index in 0..16 {
                let (x1, x2) = terminal_cell_span_x_bounds(text_x, index, 2, char_w);
                assert_eq!(x1, (text_x + index as f32 * char_w).round());
                assert_eq!(x2, (text_x + (index + 2) as f32 * char_w).round());
            }
        }

        let (single_x1, single_x2) = terminal_cell_x_bounds(0.0, 0, 7.4);
        let (wide_x1, wide_x2) = terminal_cell_span_x_bounds(0.0, 0, 2, 7.4);
        assert_eq!(single_x2 - single_x1, 7.0);
        assert_eq!(wide_x2 - wide_x1, 15.0);
        assert_ne!(wide_x2 - wide_x1, 2.0 * (single_x2 - single_x1));

        let mut previous_end = None;
        for index in (0..16).step_by(2) {
            let (x1, x2) = terminal_cell_span_x_bounds(0.333, index, 2, 7.4);
            if let Some(end) = previous_end {
                assert_eq!(x1, end);
            }
            previous_end = Some(x2);
        }
    }

    #[test]
    fn inverse_colors_preserve_default_background_identity_and_explicit_ansi_colors() {
        let palette = TerminalPalette::new([0.2, 0.3, 0.4, 1.0]);
        let mut default_cell = Cell::default();
        default_cell.set_sgr_style(true, false, false);
        let (default_fg, default_bg) = terminal_cell_render_colors(&default_cell, palette);
        assert_eq!(default_fg, palette.bg);
        assert_eq!(default_bg, Some(terminal_color_rgba(TerminalColor::default_foreground())));

        let mut explicit = Cell::default();
        explicit.set_char(
            'X',
            TerminalColor::indexed(1),
            TerminalColor::indexed(4),
            false,
        );
        explicit.set_sgr_style(true, false, false);
        let (fg, bg) = terminal_cell_render_colors(&explicit, palette);
        assert_eq!(fg, terminal_color_rgba(TerminalColor::indexed(4)));
        assert_eq!(bg, Some(terminal_color_rgba(TerminalColor::indexed(1))));
        assert_ne!(default_fg, terminal_color_rgba(TerminalColor::indexed(0)));
    }

    #[test]
    fn dim_affects_only_effective_foreground_rgb() {
        let palette = TerminalPalette::new([0.2, 0.3, 0.4, 1.0]);
        let mut cell = Cell::default();
        cell.set_char(
            'X',
            TerminalColor::rgb(150, 120, 90),
            TerminalColor::indexed(4),
            false,
        );
        cell.set_sgr_style(false, false, true);
        let (fg, bg) = terminal_cell_render_colors(&cell, palette);
        let source = terminal_color_rgba(TerminalColor::rgb(150, 120, 90));
        assert!((fg[0] - source[0] * 0.66).abs() < 0.0001);
        assert!((fg[1] - source[1] * 0.66).abs() < 0.0001);
        assert!((fg[2] - source[2] * 0.66).abs() < 0.0001);
        assert_eq!(fg[3], source[3]);
        assert_eq!(bg, Some(terminal_color_rgba(TerminalColor::indexed(4))));
    }

    #[test]
    fn underline_span_uses_snapped_normal_and_wide_boundaries() {
        let normal = terminal_underline_rect(0.0, 0, 1, 7.4, 10.25, 20.0, 1.333_333_3);
        let wide = terminal_underline_rect(0.0, 0, 2, 7.4, 10.25, 20.0, 1.333_333_3);
        assert_eq!(normal.x, 0.0);
        assert_eq!(normal.w, 7.0);
        assert_eq!(wide.x, 0.0);
        assert_eq!(wide.w, 15.0);
        assert_eq!(normal.h, 1.0);
        assert_eq!(normal.y, wide.y);

        let adjacent = terminal_underline_rect(0.0, 2, 2, 7.4, 10.25, 20.0, 1.333_333_3);
        assert_eq!(wide.x + wide.w, adjacent.x);
    }

    #[test]
    fn resize_refreshes_search_to_the_new_grid_generation_before_render() {
        let mut grid = TermGrid::new(16, 2);
        for (x, c) in "needle12345".chars().enumerate() {
            grid.lines[0][x].c = c;
        }
        grid.content_generation = grid.content_generation.wrapping_add(1);
        let mut search = TerminalSearchState::default();
        search.open();
        search.insert_text("needle12345");
        assert!(search.recompute_if_needed(&grid, SearchRefreshCause::User));
        assert_eq!(search.results.len(), 1);
        let old_generation = grid.content_generation;

        assert!(prepare_terminal_grid_for_render(
            &mut grid,
            &mut search,
            10,
            2,
        ));

        assert_ne!(grid.content_generation, old_generation);
        assert_eq!(
            search.scanned_grid_generation_for_test(),
            grid.content_generation
        );
        assert!(search.results.is_empty());
        assert_eq!(search.current, None);

        let post_resize_generation = grid.content_generation;
        grid.lines[1][0].c = 'x';
        grid.content_generation = grid.content_generation.wrapping_add(1);
        assert!(!prepare_terminal_grid_for_render(
            &mut grid,
            &mut search,
            10,
            2,
        ));
        assert_ne!(grid.content_generation, post_resize_generation);
        assert_ne!(
            search.scanned_grid_generation_for_test(),
            grid.content_generation
        );
        assert!(search.pending_passive_refresh_deadline().is_some());
    }

    #[test]
    fn snapped_fractional_advances_drive_cursor_hit_testing_without_drift() {
        let scale = 1.333_333_3;
        let snapped = Renderer::snapped_text_advance(7.4, scale);
        assert_eq!(snapped, 10.0);
        assert_eq!(Renderer::fallback_ui_text_advance(scale), 13.0);

        let advances = std::iter::repeat_n(snapped, 24);
        assert_eq!(one_line_cursor_from_advances(advances.clone(), 4.9), 0);
        assert_eq!(one_line_cursor_from_advances(advances.clone(), 5.1), 1);
        assert_eq!(one_line_cursor_from_advances(advances.clone(), 204.9), 20);
        assert_eq!(one_line_cursor_from_advances(advances, 205.1), 21);

        let (cursor_x, total_width) =
            one_line_metrics_from_advances(std::iter::repeat_n(snapped, 24), 20);
        assert_eq!(cursor_x, 200.0);
        assert_eq!(total_width, 240.0);
    }

    #[test]
    fn one_line_zero_width_format_chars_do_not_add_visual_advance() {
        let scale = 1.333_333_3;
        let regular = Renderer::one_line_ui_char_advance('A', Some(7.4), scale);
        assert_eq!(regular, 10.0);
        for zero_width in ['\u{FE0E}', '\u{FE0F}', '\u{200D}'] {
            let hidden = Renderer::one_line_ui_char_advance(zero_width, None, scale);
            assert_eq!(hidden, 0.0, "format char {zero_width:?}");
            let advances = [regular, hidden, regular];
            let (after_a, _) = one_line_metrics_from_advances(advances, 1);
            let (after_format, total_width) = one_line_metrics_from_advances(advances, 2);
            assert_eq!(after_a, 10.0, "format char {zero_width:?}");
            assert_eq!(after_format, after_a, "format char {zero_width:?}");
            assert_eq!(total_width, 20.0, "format char {zero_width:?}");
        }
    }

    #[test]
    fn zero_width_format_chars_have_no_phantom_hit_test_interval() {
        let advances = [
            Renderer::one_line_ui_char_advance('A', Some(10.0), 1.0),
            Renderer::one_line_ui_char_advance('\u{FE0F}', None, 1.0),
            Renderer::one_line_ui_char_advance('B', Some(10.0), 1.0),
        ];
        assert_eq!(one_line_cursor_from_advances(advances, 4.9), 0);
        assert_eq!(one_line_cursor_from_advances(advances, 5.1), 1);
        assert_eq!(one_line_cursor_from_advances(advances, 10.0), 1);
        assert_eq!(one_line_cursor_from_advances(advances, 10.1), 2);
        assert_eq!(one_line_cursor_from_advances(advances, 14.9), 2);
        assert_eq!(one_line_cursor_from_advances(advances, 15.1), 3);
    }

    #[test]
    fn horizontal_scroll_uses_zero_width_format_visual_width() {
        let advances = [
            Renderer::one_line_ui_char_advance('A', Some(10.0), 1.0),
            Renderer::one_line_ui_char_advance('\u{FE0F}', None, 1.0),
            Renderer::one_line_ui_char_advance('B', Some(10.0), 1.0),
        ];
        let (cursor_x, total_width) = one_line_metrics_from_advances(advances, 3);
        assert_eq!(cursor_x, 20.0);
        assert_eq!(total_width, 20.0);
        assert_eq!(
            one_line_scroll_for_metrics(cursor_x, total_width, 20.0, 9.0),
            0.0
        );
    }

    #[test]
    fn search_horizontal_scroll_resets_when_viewport_grows() {
        let cursor_x = 240.0;
        let total_width = 240.0;
        let narrow = one_line_scroll_for_metrics(cursor_x, total_width, 80.0, 0.0);
        assert_eq!(narrow, 160.0);

        let wide = one_line_scroll_for_metrics(cursor_x, total_width, 300.0, narrow);
        assert_eq!(wide, 0.0);
    }

    #[test]
    fn search_horizontal_scroll_clamps_after_query_shortening() {
        let long_scroll = one_line_scroll_for_metrics(240.0, 240.0, 80.0, 0.0);
        assert_eq!(long_scroll, 160.0);

        let short_scroll = one_line_scroll_for_metrics(30.0, 30.0, 80.0, long_scroll);
        assert_eq!(short_scroll, 0.0);
        assert!(short_scroll <= (30.0f32 - 80.0f32).max(0.0));
    }

    #[test]
    fn pty_resize_is_needed_only_when_terminal_geometry_changes() {
        assert!(!terminal_dimensions_changed(120, 40, 120, 40));
        assert!(terminal_dimensions_changed(120, 40, 121, 40));
        assert!(terminal_dimensions_changed(120, 40, 120, 41));
    }

    #[test]
    fn terminal_forced_text_check_marks_do_not_force_emoji() {
        assert!(terminal_force_text_presentation('✔'));
        assert!(terminal_force_text_presentation('✓'));
        assert!(!terminal_force_text_presentation('✅'));
    }

    #[test]
    fn terminal_tab_bar_reserves_body_space_at_fractional_scale() {
        assert_eq!(terminal_tab_body_top(1.0), 42.0);
        assert_eq!(terminal_tab_body_top(1.3333334), 56.0);
        assert_eq!(terminal_tab_body_top(1.5), 63.0);
    }

    #[test]
    fn terminal_tab_content_width_includes_plus_and_trailing_padding() {
        let widths = [120.0, 160.0];
        assert_eq!(terminal_tab_content_width(&widths, 1.0), 324.0);
        assert_eq!(terminal_tab_max_scroll(&widths, 324.0, 1.0), 0.0);
        assert_eq!(terminal_tab_max_scroll(&widths, 300.0, 1.0), 24.0);
        let base_x = 8.0 - 24.0;
        assert_eq!(terminal_tab_add_x(base_x, &widths, 1.0), 272.0);
    }

    #[test]
    fn clipped_terminal_tab_hitboxes_never_keep_zero_width_regions() {
        let strip = Rect { x: 0.0, y: 6.0, w: 100.0, h: 32.0 };
        assert_eq!(
            clipped_rect(Rect { x: 100.0, y: 6.0, w: 20.0, h: 32.0 }, strip),
            None
        );
        assert_eq!(
            clipped_rect(Rect { x: 90.0, y: 6.0, w: 20.0, h: 32.0 }, strip),
            Some(Rect { x: 90.0, y: 6.0, w: 10.0, h: 32.0 })
        );
    }

    #[test]
    fn settings_placeholder_fits_tiny_and_large_windows_at_fractional_scale() {
        for (width, height) in [
            (0.0, 0.0),
            (10.0, 10.0),
            (40.0, 40.0),
            (100.0, 80.0),
            (200.0, 100.0),
            (500.0, 300.0),
            (1000.0, 700.0),
            (1600.0, 1000.0),
        ] {
            for scale in [1.0, 1.3333333] {
                let rect = settings_placeholder_layout(width, height, scale, 1.0);
                assert!(rect.x.is_finite());
                assert!(rect.y.is_finite());
                assert!(rect.w.is_finite());
                assert!(rect.h.is_finite());
                assert!(rect.x >= 0.0 && rect.y >= 0.0);
                assert!(rect.w >= 0.0 && rect.h >= 0.0);
                assert!(rect.x + rect.w <= width.max(0.0) + 0.51);
                assert!(rect.y + rect.h <= height.max(0.0) + 0.51);
            }
        }
        assert_eq!(
            settings_placeholder_layout(100.0, 80.0, 1.0, 1.0),
            Rect {
                x: 20.0,
                y: 20.0,
                w: 60.0,
                h: 40.0,
            },
        );
    }

    #[test]
    fn settings_modal_vertical_motion_uses_raw_progress_while_backdrop_can_smoothstep() {
        let start = settings_placeholder_layout(100.0, 80.0, 1.0, 0.0);
        let quarter = settings_placeholder_layout(100.0, 80.0, 1.0, 0.25);
        let mid = settings_placeholder_layout(100.0, 80.0, 1.0, 0.5);
        let end = settings_placeholder_layout(100.0, 80.0, 1.0, 1.0);
        assert_eq!(start.y, 180.0);
        assert_eq!(quarter.y, 140.0);
        assert_eq!(mid.y, 100.0);
        assert_eq!(end.y, 20.0);
        assert_eq!(mid.y, ((start.y + end.y) * 0.5).round());
        let smooth_quarter = 0.25_f32 * 0.25 * (3.0 - 2.0 * 0.25);
        let eased_quarter_y = (start.y + (end.y - start.y) * smooth_quarter).round();
        assert_ne!(quarter.y, eased_quarter_y);
    }

    #[test]
    fn settings_placeholder_content_stays_inside_tiny_fitted_modal() {
        let modal = settings_placeholder_layout(100.0, 80.0, 1.0, 1.0);
        assert_eq!(
            modal,
            Rect {
                x: 20.0,
                y: 20.0,
                w: 60.0,
                h: 40.0,
            }
        );
        let content = settings_placeholder_content_layout(modal, 1.0);
        let clip = content.clip.expect("tiny modal should retain a bounded title clip");
        let title = content.title.expect("100x80 modal should retain its title");
        assert_eq!(clip, Rect { x: 32.0, y: 28.0, w: 36.0, h: 24.0 });
        assert_eq!(title, SettingsPlaceholderLine { x: 48.0, baseline_y: 47.0 });
        assert!(title.x >= modal.x && title.x <= modal.x + modal.w);
        assert!(title.baseline_y >= modal.y && title.baseline_y <= modal.y + modal.h);
        assert_eq!(content.description, None);
        assert!(clip.x >= modal.x && clip.y >= modal.y);
        assert!(clip.x + clip.w <= modal.x + modal.w + 0.001);
        assert!(clip.y + clip.h <= modal.y + modal.h + 0.001);
    }

    #[test]
    fn settings_placeholder_description_hides_when_modal_height_is_too_small() {
        let modal = settings_placeholder_layout(200.0, 100.0, 1.0, 1.0);
        let content = settings_placeholder_content_layout(modal, 1.0);
        let title = content.title.expect("title should fit 200x100 modal");
        assert!(title.baseline_y <= modal.y + modal.h);
        assert_eq!(content.description, None);
    }

    #[test]
    fn settings_placeholder_content_is_finite_and_bounded_at_fractional_scale() {
        let modal = settings_placeholder_layout(100.0, 80.0, 1.3333333, 1.0);
        let content = settings_placeholder_content_layout(modal, 1.3333333);
        let clip = content.clip.expect("fractional tiny modal should keep finite content clip");
        let title = content.title.expect("fractional tiny modal should keep title");
        for value in [clip.x, clip.y, clip.w, clip.h, title.x, title.baseline_y] {
            assert!(value.is_finite());
        }
        assert!(clip.x >= modal.x && clip.y >= modal.y);
        assert!(clip.x + clip.w <= modal.x + modal.w + 0.001);
        assert!(clip.y + clip.h <= modal.y + modal.h + 0.001);
        assert!(title.x >= clip.x && title.x <= clip.x + clip.w + 0.001);
        assert!(title.baseline_y >= clip.y && title.baseline_y <= clip.y + clip.h + 0.001);
    }

    #[test]
    fn settings_placeholder_text_is_hidden_without_usable_content_area() {
        for (width, height) in [(0.0, 0.0), (10.0, 10.0), (40.0, 40.0)] {
            let modal = settings_placeholder_layout(width, height, 1.0, 1.0);
            let content = settings_placeholder_content_layout(modal, 1.0);
            assert_eq!(content.title, None);
            assert_eq!(content.description, None);
        }
    }

    #[test]
    fn settings_placeholder_large_window_preserves_title_and_description_layout() {
        let modal = settings_placeholder_layout(1100.0, 720.0, 1.0, 1.0);
        let content = settings_placeholder_content_layout(modal, 1.0);
        assert_eq!(modal, Rect { x: 50.0, y: 20.0, w: 1000.0, h: 680.0 });
        assert_eq!(
            content.title,
            Some(SettingsPlaceholderLine { x: 78.0, baseline_y: 72.0 })
        );
        assert_eq!(
            content.description,
            Some(SettingsPlaceholderLine { x: 78.0, baseline_y: 108.0 })
        );
    }

    #[test]
    fn inactive_non_hovered_terminal_tab_has_no_close_hitbox_and_full_visible_body() {
        let strip = Rect {
            x: 0.0,
            y: 6.0,
            w: 240.0,
            h: 32.0,
        };
        assert!(!terminal_tab_show_close(180.0, 1.0, false, false));
        let hitbox = terminal_tab_hitbox_geometry(10.0, 180.0, strip, false, 1.0);
        assert_eq!(hitbox.close, None);
        assert_eq!(
            hitbox.body,
            Some(Rect {
                x: 10.0,
                y: 6.0,
                w: 180.0,
                h: 32.0,
            }),
        );
    }

    #[test]
    fn active_or_hovered_terminal_tab_close_is_disjoint_from_body() {
        let strip = Rect {
            x: 0.0,
            y: 6.0,
            w: 240.0,
            h: 32.0,
        };
        for (active, hovered) in [(true, false), (false, true)] {
            let show_close = terminal_tab_show_close(180.0, 1.0, active, hovered);
            assert!(show_close);
            let hitbox = terminal_tab_hitbox_geometry(10.0, 180.0, strip, show_close, 1.0);
            let body = hitbox.body.expect("visible tab body");
            let close = hitbox.close.expect("visible close hitbox");
            assert!(body.x + body.w <= close.x);
            assert!(body.w > 0.0);
            assert!(close.w > 0.0);
        }
    }

    #[test]
    fn clipped_terminal_close_never_leaves_invisible_click_region() {
        let strip = Rect {
            x: 0.0,
            y: 6.0,
            w: 100.0,
            h: 32.0,
        };
        let left = terminal_tab_hitbox_geometry(-170.0, 180.0, strip, true, 1.0);
        assert_eq!(left.close, None);
        assert_eq!(
            left.body,
            Some(Rect {
                x: 0.0,
                y: 6.0,
                w: 10.0,
                h: 32.0,
            }),
        );

        let right = terminal_tab_hitbox_geometry(80.0, 180.0, strip, true, 1.0);
        assert_eq!(right.close, None);
        assert_eq!(right.body, Some(Rect { x: 80.0, y: 6.0, w: 20.0, h: 32.0 }));

        let hidden = terminal_tab_hitbox_geometry(100.0, 180.0, strip, false, 1.0);
        assert_eq!(hidden.body, None);
        assert_eq!(hidden.close, None);
    }

    #[test]
    fn previous_frame_inactive_tab_state_stores_no_close_hitbox() {
        let strip = Rect {
            x: 0.0,
            y: 6.0,
            w: 240.0,
            h: 32.0,
        };
        let stored_previous_frame = terminal_tab_hitbox_geometry(0.0, 180.0, strip, false, 1.0);
        assert!(stored_previous_frame.close.is_none());
        assert!(stored_previous_frame.body.is_some());
    }

    #[test]
    fn terminal_close_hitbox_stays_right_of_title_body_region() {
        let strip = Rect {
            x: 0.0,
            y: 6.0,
            w: 240.0,
            h: 32.0,
        };
        let (_, close) = terminal_tab_close_geometry(0.0, 180.0, strip, 1.0);
        let body = terminal_tab_hitbox_geometry(0.0, 180.0, strip, true, 1.0)
            .body
            .expect("body remains visible");
        assert_eq!(close.x, 144.0);
        assert!(body.x + body.w <= close.x);
        assert!(close.x + close.w <= 180.0);
    }
}
