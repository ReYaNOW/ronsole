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
const TERMINAL_TAB_NATURAL_CHROME: f32 = 56.0;
const TERMINAL_TAB_CLOSE_SIZE: f32 = 20.0;
const TERMINAL_TAB_CLOSE_HIT_PAD: f32 = 4.0;
const SEARCH_ICON_VISUAL_SCALE: f32 = 0.56;
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
    let strip = terminal_tab_strip_rect(0.0, scale);
    (strip.y + strip.h).round()
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
fn terminal_body_rect(width: f32, height: f32, scale: f32) -> Rect {
    let y = terminal_tab_body_top(scale);
    Rect {
        x: 0.0,
        y,
        w: width.max(0.0),
        h: (height.max(0.0) - y).max(0.0),
    }
}

#[inline]
fn terminal_focus_separator_rect(body: Rect, scale: f32) -> Option<Rect> {
    let h = (2.0 * scale.max(0.0)).round().max(1.0).min(body.h.max(0.0));
    (body.w > 0.0 && h > 0.0).then_some(Rect {
        x: body.x,
        y: body.y,
        w: body.w,
        h,
    })
}

#[inline]
fn terminal_tab_add_glyph_geometry(add: Rect, scale: f32) -> (Rect, Rect) {
    let max_extent = add.w.min(add.h).max(0.0);
    let thickness = (2.0 * scale.max(0.0)).round().max(1.0).min(max_extent);
    let arm = (12.0 * scale.max(0.0))
        .round()
        .max(thickness)
        .min(max_extent);
    let cx = add.x + add.w * 0.5;
    let cy = add.y + add.h * 0.5;
    let horizontal = Rect {
        x: (cx - arm * 0.5).round(),
        y: (cy - thickness * 0.5).round(),
        w: arm,
        h: thickness,
    };
    let vertical = Rect {
        x: (cx - thickness * 0.5).round(),
        y: (cy - arm * 0.5).round(),
        w: thickness,
        h: arm,
    };
    (horizontal, vertical)
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

fn settings_modal_rect(width: f32, height: f32, scale: f32, progress: f32) -> Rect {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SettingsTabSpec {
    tab: SettingsTab,
    title: &'static str,
}

const SETTINGS_TABS: [SettingsTabSpec; 2] = [
    SettingsTabSpec {
        tab: SettingsTab::General,
        title: "Основные",
    },
    SettingsTabSpec {
        tab: SettingsTab::Help,
        title: "Помощь",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SettingsHelpEntry {
    shortcut: &'static str,
    description: &'static str,
}

const SETTINGS_HELP_ENTRIES: [SettingsHelpEntry; 5] = [
    SettingsHelpEntry {
        shortcut: "F1",
        description: "Открыть/закрыть настройки",
    },
    SettingsHelpEntry {
        shortcut: "Ctrl + Shift + T",
        description: "Новая вкладка",
    },
    SettingsHelpEntry {
        shortcut: "Ctrl + 4",
        description: "Закрыть текущую вкладку",
    },
    SettingsHelpEntry {
        shortcut: "Ctrl + F",
        description: "Поиск в терминале",
    },
    SettingsHelpEntry {
        shortcut: "Esc",
        description: "Закрыть настройки или активный поиск",
    },
];

const SETTINGS_TITLE: &str = "Настройки";
const SETTINGS_FONT_LABEL: &str = "Размер шрифта";
const SETTINGS_SCROLL_LABEL: &str = "Чувствительность прокрутки";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsHit {
    #[default]
    None,
    Tab(SettingsTab),
    FontDecrease,
    FontIncrease,
    ScrollDecrease,
    ScrollIncrease,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsLine {
    x: f32,
    baseline_y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsRowLayout {
    label: SettingsLine,
    label_max_x: f32,
    minus: Rect,
    value: SettingsLine,
    value_max_x: f32,
    plus: Rect,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsHelpRowLayout {
    keycap: Rect,
    key: SettingsLine,
    key_max_x: f32,
    description: SettingsLine,
    description_max_x: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsSidebarTabMetrics {
    top: f32,
    row_h: f32,
    gap: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct SettingsLayout {
    modal: Rect,
    inner: Option<Rect>,
    divider: Option<Rect>,
    content_clip: Option<Rect>,
    content_body: Option<Rect>,
    tabs: [Option<Rect>; SETTINGS_TABS.len()],
    title: Option<SettingsLine>,
    font: Option<SettingsRowLayout>,
    scroll: Option<SettingsRowLayout>,
    help: [Option<SettingsHelpRowLayout>; SETTINGS_HELP_ENTRIES.len()],
}

impl SettingsLayout {
    fn hit_test(self, x: f32, y: f32) -> SettingsHit {
        for (index, spec) in SETTINGS_TABS.iter().enumerate() {
            if self.tabs[index].is_some_and(|rect| rect.contains(x, y)) {
                return SettingsHit::Tab(spec.tab);
            }
        }
        if let Some(row) = self.font {
            if row.minus.contains(x, y) {
                return SettingsHit::FontDecrease;
            }
            if row.plus.contains(x, y) {
                return SettingsHit::FontIncrease;
            }
        }
        if let Some(row) = self.scroll {
            if row.minus.contains(x, y) {
                return SettingsHit::ScrollDecrease;
            }
            if row.plus.contains(x, y) {
                return SettingsHit::ScrollIncrease;
            }
        }
        SettingsHit::None
    }
}

#[inline]
fn settings_tab_title(tab: SettingsTab) -> &'static str {
    SETTINGS_TABS
        .iter()
        .find(|spec| spec.tab == tab)
        .map_or(SETTINGS_TABS[0].title, |spec| spec.title)
}

fn settings_sidebar_tab_metrics(
    inner_h: f32,
    tab_count: usize,
    scale: f32,
) -> SettingsSidebarTabMetrics {
    let scale = scale.max(0.0);
    let top = (20.0 * scale).min((inner_h * 0.10).max(0.0));
    if tab_count == 0 {
        return SettingsSidebarTabMetrics {
            top,
            row_h: 0.0,
            gap: 0.0,
        };
    }
    let bottom = (20.0 * scale).min((inner_h - top).max(0.0) * 0.12);
    let available = (inner_h - top - bottom).max(0.0);
    let desired_gap = 4.0 * scale;
    let gap = if tab_count > 1 {
        desired_gap.min(available / (tab_count - 1) as f32)
    } else {
        0.0
    };
    let row_h = ((available - gap * tab_count.saturating_sub(1) as f32) / tab_count as f32)
        .clamp(0.0, 36.0 * scale);
    SettingsSidebarTabMetrics { top, row_h, gap }
}

fn settings_row_layout(clip: Rect, top: f32, scale: f32) -> Option<SettingsRowLayout> {
    let button = (30.0 * scale).round().max(1.0);
    let gap = (8.0 * scale).round().max(1.0);
    let value_w = (72.0 * scale).round().max(1.0);
    let control_w = button * 2.0 + value_w + gap * 2.0;
    let label_room = (64.0 * scale).round().max(1.0);
    if clip.w < control_w + label_room || top < clip.y || top + button > clip.y + clip.h {
        return None;
    }

    let plus = Rect {
        x: (clip.x + clip.w - button).round(),
        y: top.round(),
        w: button,
        h: button,
    };
    let value_x = (plus.x - gap - value_w).round();
    let minus = Rect {
        x: (value_x - gap - button).round(),
        y: top.round(),
        w: button,
        h: button,
    };
    let baseline_y = (top + button * 0.70).round();
    Some(SettingsRowLayout {
        label: SettingsLine {
            x: clip.x.round(),
            baseline_y,
        },
        label_max_x: (minus.x - gap).round(),
        minus,
        value: SettingsLine {
            x: (value_x + 8.0 * scale).round(),
            baseline_y,
        },
        value_max_x: (plus.x - gap).round(),
        plus,
    })
}

fn settings_help_row_layout(
    content: Rect,
    index: usize,
    scale: f32,
) -> Option<SettingsHelpRowLayout> {
    let row_step = (44.0 * scale).round().max(1.0);
    let row_h = (32.0 * scale).round().max(1.0);
    let top = (content.y + 8.0 * scale + index as f32 * row_step).round();
    if top < content.y || top + row_h > content.y + content.h {
        return None;
    }

    let keycap_h = (26.0 * scale).round().max(1.0).min(row_h);
    let keycap_y = (top + (row_h - keycap_h) * 0.5).round();
    let desired_keycap_w = (170.0 * scale).round().max(1.0);
    let keycap_w = desired_keycap_w
        .min((content.w * 0.38).round().max(0.0))
        .min(content.w);
    let remaining = (content.w - keycap_w).max(0.0);
    let gap = (24.0 * scale).round().min(remaining * 0.25).max(0.0);
    let description_x = (content.x + keycap_w + gap).round();
    if keycap_w <= 0.0 || description_x >= content.x + content.w {
        return None;
    }

    let baseline_y = (top + row_h * 0.68).round();
    let keycap = Rect {
        x: content.x.round(),
        y: keycap_y,
        w: keycap_w,
        h: keycap_h,
    };
    Some(SettingsHelpRowLayout {
        keycap,
        key: SettingsLine {
            x: (keycap.x + 10.0 * scale).round(),
            baseline_y,
        },
        key_max_x: (keycap.x + keycap.w - 8.0 * scale).round(),
        description: SettingsLine {
            x: description_x,
            baseline_y,
        },
        description_max_x: (content.x + content.w).round(),
    })
}

fn settings_layout(
    width: f32,
    height: f32,
    scale: f32,
    progress: f32,
    active_tab: SettingsTab,
) -> SettingsLayout {
    let scale = scale.max(0.0);
    let modal = settings_modal_rect(width, height, scale, progress);
    if modal.w <= 0.0 || modal.h <= 0.0 {
        return SettingsLayout {
            modal,
            ..SettingsLayout::default()
        };
    }

    let pad_top = (35.0 * scale).min(modal.h * 0.2).max(0.0);
    let pad_bottom = (30.0 * scale)
        .min((modal.h - pad_top).max(0.0) * 0.2)
        .max(0.0);
    let pad_h = (40.0 * scale).min(modal.w * 0.2).max(0.0);
    let inner_x = (modal.x + pad_h).round();
    let inner_y = (modal.y + pad_top).round();
    let inner_right = (modal.x + modal.w - pad_h).round();
    let inner_bottom = (modal.y + modal.h - pad_bottom).round();
    let inner = Rect {
        x: inner_x,
        y: inner_y,
        w: (inner_right - inner_x).max(0.0),
        h: (inner_bottom - inner_y).max(0.0),
    };
    if inner.w <= 0.0 || inner.h <= 0.0 {
        return SettingsLayout {
            modal,
            ..SettingsLayout::default()
        };
    }

    let sidebar_w = (200.0 * scale).min((inner.w * 0.35).max(0.0));
    let divider_w = (1.0 * scale).round().max(1.0).min(inner.w);
    let divider_x = (inner.x + sidebar_w)
        .round()
        .min(inner.x + inner.w - divider_w);
    let divider = Rect {
        x: divider_x,
        y: inner.y,
        w: divider_w,
        h: inner.h,
    };

    let tab_metrics = settings_sidebar_tab_metrics(inner.h, SETTINGS_TABS.len(), scale);
    let tab_inset = (10.0 * scale).min((sidebar_w * 0.2).max(0.0));
    let tab_w = (sidebar_w - tab_inset * 2.0).max(0.0);
    let mut tabs = [None; SETTINGS_TABS.len()];
    for (index, slot) in tabs.iter_mut().enumerate() {
        let y = (inner.y + tab_metrics.top + index as f32 * (tab_metrics.row_h + tab_metrics.gap))
            .round();
        if tab_w > 0.0
            && tab_metrics.row_h > 0.0
            && y >= inner.y
            && y + tab_metrics.row_h <= inner.y + inner.h + 0.001
        {
            *slot = Some(Rect {
                x: (inner.x + tab_inset).round(),
                y,
                w: tab_w.round(),
                h: tab_metrics.row_h.round(),
            });
        }
    }

    let pane_left = (divider.x + divider.w).min(inner.x + inner.w);
    let pane_w = (inner.x + inner.w - pane_left).max(0.0);
    let content_left_gap = (30.0 * scale).min(pane_w * 0.20).max(0.0);
    let content_right_gap = (18.0 * scale)
        .min((pane_w - content_left_gap).max(0.0) * 0.20)
        .max(0.0);
    let content_x = (pane_left + content_left_gap).round();
    let content_right = (inner.x + inner.w - content_right_gap).round();
    let content_clip = Rect {
        x: content_x,
        y: inner.y,
        w: (content_right - content_x).max(0.0),
        h: inner.h,
    };
    if content_clip.w <= 0.0 {
        return SettingsLayout {
            modal,
            inner: Some(inner),
            divider: Some(divider),
            tabs,
            ..SettingsLayout::default()
        };
    }

    let title_y = (inner.y + 40.0 * scale).round();
    let title = (title_y >= content_clip.y && title_y <= content_clip.y + content_clip.h)
        .then_some(SettingsLine {
            x: content_clip.x.round(),
            baseline_y: title_y,
        });
    let body_y = (inner.y + 70.0 * scale).round().min(inner.y + inner.h);
    let body_bottom = (inner.y + inner.h - 20.0 * scale).round().max(body_y);
    let content_body = Rect {
        x: content_clip.x,
        y: body_y,
        w: content_clip.w,
        h: (body_bottom - body_y).max(0.0),
    };

    let mut font = None;
    let mut scroll = None;
    let mut help = [None; SETTINGS_HELP_ENTRIES.len()];
    match active_tab {
        SettingsTab::General => {
            let first_top = (content_body.y + 12.0 * scale).round();
            let row_gap = (54.0 * scale).round().max(1.0);
            font = settings_row_layout(content_body, first_top, scale);
            scroll = settings_row_layout(content_body, first_top + row_gap, scale);
        }
        SettingsTab::Help => {
            for (index, slot) in help.iter_mut().enumerate() {
                *slot = settings_help_row_layout(content_body, index, scale);
            }
        }
    }

    SettingsLayout {
        modal,
        inner: Some(inner),
        divider: Some(divider),
        content_clip: Some(content_clip),
        content_body: Some(content_body),
        tabs,
        title,
        font,
        scroll,
        help,
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

    fn draw_search_icon(
        &mut self,
        icon: SearchIcon,
        rect: Rect,
        visual_scale: Option<f32>,
        color: [f32; 4],
    ) {
        let Some(glyph) = self.search_icon(icon) else {
            return;
        };
        let visual = visual_scale.map_or(rect, |scale| {
            let size = rect.w.min(rect.h) * scale;
            Rect {
                x: (rect.x + (rect.w - size) * 0.5).round(),
                y: (rect.y + (rect.h - size) * 0.5).round(),
                w: size,
                h: size,
            }
        });
        self.push_quad(
            visual.x,
            visual.y,
            visual.w,
            visual.h,
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
        self.draw_search_icon(
            SearchIcon::Close,
            geometry.close,
            Some(SEARCH_ICON_VISUAL_SCALE),
            muted,
        );
        if geometry.show_nav {
            self.draw_search_icon(
                SearchIcon::Next,
                geometry.next,
                Some(SEARCH_ICON_VISUAL_SCALE),
                muted,
            );
            self.draw_search_icon(
                SearchIcon::Previous,
                geometry.previous,
                Some(SEARCH_ICON_VISUAL_SCALE),
                muted,
            );
        }
        if geometry.show_case {
            self.draw_search_icon(
                SearchIcon::Case,
                geometry.case_toggle,
                Some(SEARCH_ICON_VISUAL_SCALE),
                if search.case_sensitive {
                    palette.accent
                } else {
                    muted
                },
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
            let close_hovered = hitboxes[idx]
                .close
                .is_some_and(|rect| rect.contains(pointer_x, pointer_y));
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
                if close_hovered {
                    if let Some(close_hit) = hitboxes[idx].close {
                        self.push_rounded_rect(
                            close_hit.x,
                            close_hit.y,
                            close_hit.w,
                            close_hit.h,
                            4.0 * s,
                            [
                                self.palette.fg[0],
                                self.palette.fg[1],
                                self.palette.fg[2],
                                0.10,
                            ],
                        );
                    }
                }
                let icon_color = if close_hovered {
                    self.palette.fg
                } else {
                    [
                        self.palette.fg[0],
                        self.palette.fg[1],
                        self.palette.fg[2],
                        0.80,
                    ]
                };
                self.draw_search_icon(SearchIcon::Close, close_icon, None, icon_color);
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
            let (horizontal, vertical) = terminal_tab_add_glyph_geometry(add_raw, s);
            self.push_rounded_rect(
                horizontal.x,
                horizontal.y,
                horizontal.w,
                horizontal.h,
                horizontal.h * 0.5,
                [0.82, 0.82, 0.84, 1.0],
            );
            self.push_rounded_rect(
                vertical.x,
                vertical.y,
                vertical.w,
                vertical.h,
                vertical.w * 0.5,
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

    fn draw_settings(
        &mut self,
        progress: f32,
        active_tab: SettingsTab,
        pointer_x: f32,
        pointer_y: f32,
        font_value: &str,
        scroll_value: &str,
    ) {
        let progress = progress.clamp(0.0, 1.0);
        if progress <= 0.0 {
            return;
        }
        let s = self.scale_factor;
        let smooth = progress * progress * (3.0 - 2.0 * progress);
        self.push_rect(
            0.0,
            0.0,
            self.width,
            self.height,
            [0.0, 0.0, 0.0, 0.60 * smooth],
        );

        let layout = settings_layout(self.width, self.height, s, progress, active_tab);
        let modal = layout.modal;
        if modal.w <= 0.0 || modal.h <= 0.0 {
            self.flush();
            return;
        }

        let outer_radius = (10.0 * s).round().max(1.0);
        self.push_rounded_rect(
            modal.x - 1.0,
            modal.y - 1.0,
            modal.w + 2.0,
            modal.h + 2.0,
            outer_radius,
            [0.224, 0.231, 0.251, 1.0],
        );
        self.push_rounded_rect_gradient(
            modal.x,
            modal.y,
            modal.w,
            modal.h,
            outer_radius,
            [
                [0.26, 0.20, 0.36, 1.0],
                [0.12, 0.13, 0.22, 1.0],
            ],
        );

        if let Some(inner) = layout.inner {
            let inner_radius = (8.0 * s).round().max(1.0);
            self.push_rounded_rect(
                inner.x - 1.0,
                inner.y - 1.0,
                inner.w + 2.0,
                inner.h + 2.0,
                inner_radius,
                [0.224, 0.231, 0.251, 0.80],
            );
            self.push_rounded_rect(
                inner.x,
                inner.y,
                inner.w,
                inner.h,
                inner_radius,
                [0.15, 0.16, 0.20, 1.0],
            );
        }
        if let Some(divider) = layout.divider {
            self.push_rect(
                divider.x,
                divider.y,
                divider.w,
                divider.h,
                [1.0, 1.0, 1.0, 0.05],
            );
        }
        if layout
            .inner
            .is_some_and(|inner| inner.y - modal.y >= 24.0 * s)
        {
            self.draw_ui_text_clipped(
                SETTINGS_TITLE,
                (modal.x + 40.0 * s).round(),
                (modal.x + modal.w - 40.0 * s).round(),
                (modal.y + 25.0 * s).round(),
                [0.875, 0.882, 0.902, 1.0],
                0.90,
            );
        }
        self.flush();

        let hovered = layout.hit_test(pointer_x, pointer_y);
        if let Some(inner) = layout.inner {
            self.set_clip(inner.x, inner.y, inner.w, inner.h);
            for (index, spec) in SETTINGS_TABS.iter().enumerate() {
                let Some(rect) = layout.tabs[index] else {
                    continue;
                };
                let is_active = spec.tab == active_tab;
                let is_hovered = hovered == SettingsHit::Tab(spec.tab);
                if is_active || is_hovered {
                    self.push_rounded_rect(
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h,
                        (6.0 * s).round().max(1.0),
                        [1.0, 1.0, 1.0, if is_active { 0.10 } else { 0.05 }],
                    );
                }
                let text_x = (rect.x + 15.0 * s).round();
                let baseline_y = (rect.y + rect.h * 0.5 + 5.0 * s).round();
                self.draw_ui_text_clipped(
                    spec.title,
                    text_x,
                    (rect.x + rect.w - 8.0 * s).round(),
                    baseline_y,
                    if is_active {
                        [1.0, 1.0, 1.0, 1.0]
                    } else {
                        [0.70, 0.70, 0.70, 1.0]
                    },
                    0.95,
                );
            }
            self.flush();
            self.clear_clip();
        }

        let Some(content_clip) = layout.content_clip else {
            return;
        };
        self.set_clip(
            content_clip.x,
            content_clip.y,
            content_clip.w,
            content_clip.h,
        );

        if let Some(title) = layout.title {
            let tab_title = settings_tab_title(active_tab);
            let title_x = (title.x - 14.0 * s).round();
            let pill_h = (30.0 * s).round().max(1.0);
            let pill_y = (title.baseline_y - 22.0 * s).round();
            let title_width = self.terminal_ui_text_width(tab_title, 1.1);
            let pill_w = (title_width + 28.0 * s)
                .round()
                .min((content_clip.x + content_clip.w - title_x).max(0.0));
            if pill_w > 0.0 {
                self.push_rounded_rect(
                    title_x - 1.0,
                    pill_y - 1.0,
                    pill_w + 2.0,
                    pill_h + 2.0,
                    (6.0 * s).round().max(1.0),
                    [0.35, 0.26, 0.48, 1.0],
                );
                self.push_rounded_rect(
                    title_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    (6.0 * s).round().max(1.0),
                    [0.26, 0.20, 0.36, 1.0],
                );
                self.draw_ui_text_clipped(
                    tab_title,
                    (title_x + 14.0 * s).round(),
                    (title_x + pill_w - 10.0 * s).round(),
                    title.baseline_y,
                    [1.0, 1.0, 1.0, 1.0],
                    1.1,
                );
            }
        }

        match active_tab {
            SettingsTab::General => {
                if let Some(row) = layout.font {
                    self.draw_settings_row(
                        row,
                        SETTINGS_FONT_LABEL,
                        font_value,
                        hovered,
                        SettingsHit::FontDecrease,
                        SettingsHit::FontIncrease,
                    );
                }
                if let Some(row) = layout.scroll {
                    self.draw_settings_row(
                        row,
                        SETTINGS_SCROLL_LABEL,
                        scroll_value,
                        hovered,
                        SettingsHit::ScrollDecrease,
                        SettingsHit::ScrollIncrease,
                    );
                }
            }
            SettingsTab::Help => {
                for (row, entry) in layout.help.iter().zip(SETTINGS_HELP_ENTRIES.iter()) {
                    if let Some(row) = *row {
                        self.draw_settings_help_row(row, *entry);
                    }
                }
            }
        }
        self.flush();
        self.clear_clip();
    }

    fn draw_settings_row(
        &mut self,
        row: SettingsRowLayout,
        label: &str,
        value: &str,
        hovered: SettingsHit,
        minus_hit: SettingsHit,
        plus_hit: SettingsHit,
    ) {
        let s = self.scale_factor;
        let normal = [0.224, 0.231, 0.251, 1.0];
        let hover = self.palette.accent_with_alpha(0.55);
        let radius = (5.0 * s).round().max(1.0);
        self.push_rounded_rect(
            row.minus.x,
            row.minus.y,
            row.minus.w,
            row.minus.h,
            radius,
            if hovered == minus_hit { hover } else { normal },
        );
        self.push_rounded_rect(
            row.plus.x,
            row.plus.y,
            row.plus.w,
            row.plus.h,
            radius,
            if hovered == plus_hit { hover } else { normal },
        );
        self.draw_ui_text_clipped(
            label,
            row.label.x,
            row.label_max_x,
            row.label.baseline_y,
            self.palette.fg,
            0.90,
        );
        self.draw_ui_text(
            "-",
            (row.minus.x + 10.0 * s).round(),
            row.label.baseline_y,
            self.palette.fg,
            0.90,
        );
        self.draw_ui_text_clipped(
            value,
            row.value.x,
            row.value_max_x,
            row.value.baseline_y,
            self.palette.fg,
            0.90,
        );
        self.draw_ui_text(
            "+",
            (row.plus.x + 8.0 * s).round(),
            row.label.baseline_y,
            self.palette.fg,
            0.90,
        );
    }

    fn draw_settings_help_row(&mut self, row: SettingsHelpRowLayout, entry: SettingsHelpEntry) {
        let s = self.scale_factor;
        let radius = (4.0 * s).round().max(1.0);
        self.push_rounded_rect(
            row.keycap.x - 1.0,
            row.keycap.y - 1.0,
            row.keycap.w + 2.0,
            row.keycap.h + 2.0,
            radius,
            [0.306, 0.319, 0.341, 1.0],
        );
        self.push_rounded_rect(
            row.keycap.x,
            row.keycap.y,
            row.keycap.w,
            row.keycap.h,
            radius,
            [0.224, 0.231, 0.251, 1.0],
        );
        self.draw_ui_text_clipped(
            entry.shortcut,
            row.key.x,
            row.key_max_x,
            row.key.baseline_y,
            [0.875, 0.882, 0.902, 1.0],
            0.95,
        );
        self.draw_ui_text_clipped(
            entry.description,
            row.description.x,
            row.description_max_x,
            row.description.baseline_y,
            [0.663, 0.690, 0.729, 1.0],
            1.0,
        );
    }

    pub(crate) fn settings_hit_test(
        &self,
        progress: f32,
        active_tab: SettingsTab,
        x: f32,
        y: f32,
    ) -> SettingsHit {
        settings_layout(
            self.width,
            self.height,
            self.scale_factor,
            progress,
            active_tab,
        )
        .hit_test(x, y)
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
        settings_tab: SettingsTab,
        settings_font_value: &str,
        settings_scroll_value: &str,
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
        self.draw_settings(
            settings_progress,
            settings_tab,
            pointer_x,
            pointer_y,
            settings_font_value,
            settings_scroll_value,
        );
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
        let body = terminal_body_rect(self.width, self.height, s);
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
                    if let Some(color) = bg {
                        self.push_rect(x, draw_y, cell_w, char_h, color);
                    }
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

        if focused && let Some(separator) = terminal_focus_separator_rect(body, s) {
            self.push_rounded_rect(
                separator.x,
                separator.y,
                separator.w,
                separator.h,
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
    fn fractional_terminal_row_background_quads_share_vertical_boundaries() {
        let mut reproduced_legacy_gap = false;
        for char_h in [27.3_f32, 34.65, 36.75, 40.95] {
            let body = Rect {
                x: 0.0,
                y: 0.0,
                w: 800.0,
                h: 100.3 + 2.0 * char_h,
            };
            let upper_y = terminal_row_draw_y(body, 0.0, char_h, 2, 0, 0.0);
            let lower_y = terminal_row_draw_y(body, 0.0, char_h, 2, 1, 0.0);
            let upper = super::super::quad_vertices(
                10.0,
                upper_y,
                20.0,
                char_h,
                -1.0,
                -1.0,
                0.0,
                0.0,
                [1.0; 4],
                super::super::SOLID_RECT_MODE,
                [0.0; 3],
            );
            let lower = super::super::quad_vertices(
                10.0,
                lower_y,
                20.0,
                char_h,
                -1.0,
                -1.0,
                0.0,
                0.0,
                [1.0; 4],
                super::super::SOLID_RECT_MODE,
                [0.0; 3],
            );

            assert_eq!(upper[2].pos[1], lower[0].pos[1]);
            let legacy_rounded_bottom = (upper_y + char_h.round()).round();
            reproduced_legacy_gap |= legacy_rounded_bottom != lower[0].pos[1];
        }
        assert!(reproduced_legacy_gap);
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
        for (scale, expected_top) in [(1.0, 38.0), (1.25, 48.0), (1.3333333, 51.0), (1.5, 57.0)] {
            let strip = terminal_tab_strip_rect(1280.0, scale);
            let body = terminal_body_rect(1280.0, 720.0, scale);
            assert_eq!(terminal_tab_body_top(scale), expected_top);
            assert_eq!(body.y, strip.y + strip.h);
            assert_eq!(body.y, expected_top);
            assert_eq!(body.x, 0.0);
            assert_eq!(body.w, 1280.0);
            assert_eq!(body.h, 720.0 - expected_top);
        }
    }

    #[test]
    fn terminal_focus_separator_matches_body_top_and_full_width_at_fractional_scale() {
        for scale in [1.0, 1.25, 1.3333333, 1.5] {
            let body = terminal_body_rect(1280.0, 720.0, scale);
            let separator = terminal_focus_separator_rect(body, scale)
                .expect("visible terminal body should have a separator");
            assert_eq!(separator.x, body.x);
            assert_eq!(separator.y, body.y);
            assert_eq!(separator.w, body.w);
            assert!(separator.h > 0.0 && separator.h <= body.h);
            for value in [separator.x, separator.y, separator.w, separator.h] {
                assert!(value.is_finite());
            }
            assert_eq!(separator.x.fract(), 0.0);
            assert_eq!(separator.y.fract(), 0.0);
            assert_eq!(separator.h.fract(), 0.0);
        }
    }

    #[test]
    fn terminal_plus_glyph_is_donor_sized_centered_and_bounded() {
        for scale in [1.0, 1.25, 1.3333333, 1.5] {
            let size = (TERMINAL_ADD_SIZE * scale).round().max(1.0);
            let add = Rect {
                x: 100.0,
                y: 20.0,
                w: size,
                h: size,
            };
            let (horizontal, vertical) = terminal_tab_add_glyph_geometry(add, scale);
            assert!(horizontal.w >= (11.0 * scale).round());
            assert!(vertical.h >= (11.0 * scale).round());
            assert!(horizontal.h >= (2.0 * scale).round().max(1.0));
            assert!(vertical.w >= (2.0 * scale).round().max(1.0));
            for rect in [horizontal, vertical] {
                for value in [rect.x, rect.y, rect.w, rect.h] {
                    assert!(value.is_finite());
                }
                assert!(rect.x >= add.x && rect.y >= add.y);
                assert!(rect.x + rect.w <= add.x + add.w);
                assert!(rect.y + rect.h <= add.y + add.h);
            }
            let add_cx = add.x + add.w * 0.5;
            let add_cy = add.y + add.h * 0.5;
            let horizontal_cx = horizontal.x + horizontal.w * 0.5;
            let horizontal_cy = horizontal.y + horizontal.h * 0.5;
            let vertical_cx = vertical.x + vertical.w * 0.5;
            let vertical_cy = vertical.y + vertical.h * 0.5;
            assert!((horizontal_cx - add_cx).abs() <= 0.5);
            assert!((horizontal_cy - add_cy).abs() <= 0.5);
            assert!((vertical_cx - add_cx).abs() <= 0.5);
            assert!((vertical_cy - add_cy).abs() <= 0.5);
        }
        let add = Rect {
            x: 0.0,
            y: 0.0,
            w: 20.0,
            h: 20.0,
        };
        let (horizontal, vertical) = terminal_tab_add_glyph_geometry(add, 1.0);
        assert_eq!(horizontal.w, 12.0);
        assert_eq!(horizontal.h, 2.0);
        assert_eq!(vertical.w, 2.0);
        assert_eq!(vertical.h, 12.0);
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
    fn settings_modal_fits_tiny_and_large_windows_at_fractional_scale() {
        for (width, height) in [
            (0.0, 0.0),
            (10.0, 10.0),
            (40.0, 40.0),
            (100.0, 80.0),
            (200.0, 100.0),
            (500.0, 300.0),
            (1100.0, 720.0),
            (1600.0, 1000.0),
        ] {
            for scale in [1.0, 1.25, 1.3333333, 1.5] {
                let rect = settings_modal_rect(width, height, scale, 1.0);
                for value in [rect.x, rect.y, rect.w, rect.h] {
                    assert!(value.is_finite());
                }
                assert!(rect.x >= 0.0 && rect.y >= 0.0);
                assert!(rect.w >= 0.0 && rect.h >= 0.0);
                assert!(rect.x + rect.w <= width.max(0.0) + 0.51);
                assert!(rect.y + rect.h <= height.max(0.0) + 0.51);
            }
        }
    }

    #[test]
    fn settings_modal_vertical_motion_uses_raw_progress_while_backdrop_can_smoothstep() {
        let start = settings_modal_rect(100.0, 80.0, 1.0, 0.0);
        let quarter = settings_modal_rect(100.0, 80.0, 1.0, 0.25);
        let mid = settings_modal_rect(100.0, 80.0, 1.0, 0.5);
        let end = settings_modal_rect(100.0, 80.0, 1.0, 1.0);
        assert_eq!(start.y, 180.0);
        assert_eq!(quarter.y, 140.0);
        assert_eq!(mid.y, 100.0);
        assert_eq!(end.y, 20.0);
        let smooth_quarter = 0.25_f32 * 0.25 * (3.0 - 2.0 * 0.25);
        let eased_quarter_y = (start.y + (end.y - start.y) * smooth_quarter).round();
        assert_ne!(quarter.y, eased_quarter_y);
    }

    #[test]
    fn settings_sidebar_tabs_fit_inside_inner_panel_at_fractional_scale() {
        for scale in [1.0, 1.25, 1.3333333, 1.5] {
            let layout = settings_layout(
                1100.0 * scale,
                720.0 * scale,
                scale,
                1.0,
                SettingsTab::General,
            );
            let inner = layout
                .inner
                .expect("large settings modal should have inner panel");
            let metrics = settings_sidebar_tab_metrics(inner.h, SETTINGS_TABS.len(), scale);
            let bottom = metrics.top
                + metrics.row_h * SETTINGS_TABS.len() as f32
                + metrics.gap * SETTINGS_TABS.len().saturating_sub(1) as f32;
            assert!(bottom <= inner.h + 0.001);
            for rect in layout.tabs.into_iter().flatten() {
                for value in [rect.x, rect.y, rect.w, rect.h] {
                    assert!(value.is_finite());
                }
                assert!(rect.x >= inner.x && rect.y >= inner.y);
                assert!(rect.x + rect.w <= inner.x + inner.w + 0.001);
                assert!(rect.y + rect.h <= inner.y + inner.h + 0.001);
            }
        }

        let tiny = settings_layout(100.0, 80.0, 1.3333333, 1.0, SettingsTab::General);
        for rect in tiny.tabs.into_iter().flatten() {
            for value in [rect.x, rect.y, rect.w, rect.h] {
                assert!(value.is_finite());
            }
        }
    }

    #[test]
    fn settings_tab_hit_testing_uses_the_drawn_sidebar_rects() {
        let layout = settings_layout(1100.0, 720.0, 1.0, 1.0, SettingsTab::General);
        let center = |rect: Rect| (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5);
        let general = layout.tabs[0].expect("general tab should fit");
        let help = layout.tabs[1].expect("help tab should fit");
        let (x, y) = center(general);
        assert_eq!(
            layout.hit_test(x, y),
            SettingsHit::Tab(SettingsTab::General)
        );
        let (x, y) = center(help);
        assert_eq!(layout.hit_test(x, y), SettingsHit::Tab(SettingsTab::Help));
        let inner = layout.inner.expect("inner panel should fit");
        assert_eq!(
            layout.hit_test(inner.x + inner.w - 2.0, inner.y + inner.h - 2.0),
            SettingsHit::None,
        );
    }

    #[test]
    fn settings_controls_are_bounded_disjoint_and_hit_test_the_drawn_geometry() {
        for scale in [1.0, 1.25, 1.3333333, 1.5] {
            let layout = settings_layout(
                1100.0 * scale,
                720.0 * scale,
                scale,
                1.0,
                SettingsTab::General,
            );
            let clip = layout
                .content_body
                .expect("large settings modal should have content body");
            let font = layout.font.expect("font controls should fit");
            let scroll = layout.scroll.expect("scroll controls should fit");
            for rect in [font.minus, font.plus, scroll.minus, scroll.plus] {
                for value in [rect.x, rect.y, rect.w, rect.h] {
                    assert!(value.is_finite());
                }
                assert!(rect.x >= clip.x && rect.y >= clip.y);
                assert!(rect.x + rect.w <= clip.x + clip.w + 0.001);
                assert!(rect.y + rect.h <= clip.y + clip.h + 0.001);
            }
            assert!(font.minus.x + font.minus.w < font.plus.x);
            assert!(font.minus.y + font.minus.h < scroll.minus.y);
            assert!(font.label_max_x <= font.minus.x);
            assert!(scroll.label_max_x <= scroll.minus.x);
            let center = |rect: Rect| (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5);
            let (x, y) = center(font.minus);
            assert_eq!(layout.hit_test(x, y), SettingsHit::FontDecrease);
            let (x, y) = center(font.plus);
            assert_eq!(layout.hit_test(x, y), SettingsHit::FontIncrease);
            let (x, y) = center(scroll.minus);
            assert_eq!(layout.hit_test(x, y), SettingsHit::ScrollDecrease);
            let (x, y) = center(scroll.plus);
            assert_eq!(layout.hit_test(x, y), SettingsHit::ScrollIncrease);
        }
    }

    #[test]
    fn help_tab_has_no_hidden_general_control_hitboxes() {
        let general = settings_layout(1100.0, 720.0, 1.0, 1.0, SettingsTab::General);
        let help = settings_layout(1100.0, 720.0, 1.0, 1.0, SettingsTab::Help);
        assert!(help.font.is_none());
        assert!(help.scroll.is_none());
        let font_minus = general.font.expect("general controls should fit").minus;
        let x = font_minus.x + font_minus.w * 0.5;
        let y = font_minus.y + font_minus.h * 0.5;
        assert_eq!(help.hit_test(x, y), SettingsHit::None);
    }

    #[test]
    fn help_keycaps_are_bounded_and_leave_a_separate_description_column() {
        for scale in [1.0, 1.25, 1.3333333, 1.5] {
            let layout =
                settings_layout(1100.0 * scale, 720.0 * scale, scale, 1.0, SettingsTab::Help);
            let body = layout.content_body.expect("help content should fit");
            let rows = layout.help.into_iter().flatten().collect::<Vec<_>>();
            assert_eq!(rows.len(), SETTINGS_HELP_ENTRIES.len());
            for row in rows {
                for value in [
                    row.keycap.x,
                    row.keycap.y,
                    row.keycap.w,
                    row.keycap.h,
                    row.description.x,
                    row.description.baseline_y,
                ] {
                    assert!(value.is_finite());
                }
                assert!(row.keycap.x >= body.x && row.keycap.y >= body.y);
                assert!(row.keycap.x + row.keycap.w <= body.x + body.w + 0.001);
                assert!(row.keycap.y + row.keycap.h <= body.y + body.h + 0.001);
                assert!(row.description.x > row.keycap.x + row.keycap.w);
                assert!(row.description_max_x <= body.x + body.w + 0.001);
            }
        }
    }

    #[test]
    fn settings_static_labels_and_help_shortcuts_are_russian_and_app_owned() {
        assert_eq!(SETTINGS_TITLE, "Настройки");
        assert_eq!(
            SETTINGS_TABS,
            [
                SettingsTabSpec {
                    tab: SettingsTab::General,
                    title: "Основные",
                },
                SettingsTabSpec {
                    tab: SettingsTab::Help,
                    title: "Помощь",
                },
            ],
        );
        assert_eq!(SETTINGS_FONT_LABEL, "Размер шрифта");
        assert_eq!(SETTINGS_SCROLL_LABEL, "Чувствительность прокрутки");
        assert_eq!(
            SETTINGS_HELP_ENTRIES,
            [
                SettingsHelpEntry {
                    shortcut: "F1",
                    description: "Открыть/закрыть настройки",
                },
                SettingsHelpEntry {
                    shortcut: "Ctrl + Shift + T",
                    description: "Новая вкладка",
                },
                SettingsHelpEntry {
                    shortcut: "Ctrl + 4",
                    description: "Закрыть текущую вкладку",
                },
                SettingsHelpEntry {
                    shortcut: "Ctrl + F",
                    description: "Поиск в терминале",
                },
                SettingsHelpEntry {
                    shortcut: "Esc",
                    description: "Закрыть настройки или активный поиск",
                },
            ],
        );
    }

    #[test]
    fn settings_layout_stays_finite_when_window_is_too_small_for_controls() {
        for (width, height) in [(0.0, 0.0), (10.0, 10.0), (40.0, 40.0), (100.0, 80.0)] {
            for tab in [SettingsTab::General, SettingsTab::Help] {
                let layout = settings_layout(width, height, 1.3333333, 1.0, tab);
                for value in [
                    layout.modal.x,
                    layout.modal.y,
                    layout.modal.w,
                    layout.modal.h,
                ] {
                    assert!(value.is_finite());
                }
                for rect in [
                    layout.inner,
                    layout.divider,
                    layout.content_clip,
                    layout.content_body,
                ]
                .into_iter()
                .flatten()
                {
                    for value in [rect.x, rect.y, rect.w, rect.h] {
                        assert!(value.is_finite());
                    }
                }
            }
        }
    }

    #[test]
    fn settings_hit_test_tracks_animated_modal_position() {
        let mid = settings_layout(1100.0, 720.0, 1.0, 0.5, SettingsTab::General);
        let final_layout = settings_layout(1100.0, 720.0, 1.0, 1.0, SettingsTab::General);
        let mid_minus = mid.font.expect("mid-animation controls should fit").minus;
        let final_minus = final_layout.font.expect("final controls should fit").minus;
        let mid_center = (
            mid_minus.x + mid_minus.w * 0.5,
            mid_minus.y + mid_minus.h * 0.5,
        );
        let final_center = (
            final_minus.x + final_minus.w * 0.5,
            final_minus.y + final_minus.h * 0.5,
        );
        assert_eq!(
            mid.hit_test(mid_center.0, mid_center.1),
            SettingsHit::FontDecrease
        );
        assert_eq!(
            mid.hit_test(final_center.0, final_center.1),
            SettingsHit::None
        );
        assert_eq!(
            final_layout.hit_test(final_center.0, final_center.1),
            SettingsHit::FontDecrease
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

    #[test]
    fn terminal_close_geometry_keeps_donor_visual_size_and_hit_padding() {
        for scale in [1.0_f32, 1.25, 1.333_333_3, 1.5] {
            let strip = Rect {
                x: 0.0,
                y: (6.0 * scale).round(),
                w: 360.0 * scale,
                h: (32.0 * scale).round(),
            };
            let (icon, hit) =
                terminal_tab_close_geometry(20.0 * scale, 180.0 * scale, strip, scale);
            let pad = TERMINAL_TAB_CLOSE_HIT_PAD * scale;

            assert!((icon.w - TERMINAL_TAB_CLOSE_SIZE * scale).abs() < 0.001);
            assert!((icon.h - TERMINAL_TAB_CLOSE_SIZE * scale).abs() < 0.001);
            assert!((hit.x - (icon.x - pad)).abs() < 0.001);
            assert!((hit.y - (icon.y - pad)).abs() < 0.001);
            assert!((hit.w - (icon.w + pad * 2.0)).abs() < 0.001);
            assert!((hit.h - (icon.h + pad * 2.0)).abs() < 0.001);
            for value in [icon.x, icon.y, icon.w, icon.h, hit.x, hit.y, hit.w, hit.h] {
                assert!(value.is_finite());
            }
        }
    }

    #[test]
    fn terminal_close_hover_depends_only_on_close_hitbox() {
        let strip = Rect {
            x: 0.0,
            y: 6.0,
            w: 240.0,
            h: 32.0,
        };
        let hitbox = terminal_tab_hitbox_geometry(10.0, 180.0, strip, true, 1.0);
        let body = hitbox.body.expect("visible tab body");
        let close = hitbox.close.expect("visible close hitbox");

        let close_hovered = |hitbox: TerminalTabHitbox, x: f32, y: f32| {
            hitbox.close.is_some_and(|rect| rect.contains(x, y))
        };
        assert!(!close_hovered(
            hitbox,
            body.x + body.w * 0.5,
            body.y + body.h * 0.5,
        ));
        assert!(close_hovered(
            hitbox,
            close.x + close.w * 0.5,
            close.y + close.h * 0.5,
        ));
        assert!(!close_hovered(
            TerminalTabHitbox {
                body: hitbox.body,
                close: None,
            },
            close.x + close.w * 0.5,
            close.y + close.h * 0.5,
        ));
    }
}
