use std::sync::Arc;

use unicode_width::UnicodeWidthChar;
use vte::Params;

pub(crate) const ANSI_16_COLORS: [[f32; 4]; 16] = [
    [0.10, 0.10, 0.10, 1.0],
    [0.95, 0.30, 0.30, 1.0],
    [0.30, 0.85, 0.30, 1.0],
    [0.90, 0.85, 0.20, 1.0],
    [0.30, 0.60, 1.00, 1.0],
    [0.90, 0.35, 0.90, 1.0],
    [0.20, 0.85, 0.85, 1.0],
    [0.90, 0.90, 0.90, 1.0],
    [0.45, 0.45, 0.45, 1.0],
    [1.00, 0.40, 0.40, 1.0],
    [0.40, 1.00, 0.40, 1.0],
    [1.00, 1.00, 0.40, 1.0],
    [0.50, 0.70, 1.00, 1.0],
    [1.00, 0.50, 1.00, 1.0],
    [0.40, 1.00, 1.00, 1.0],
    [1.00, 1.00, 1.00, 1.0],
];

const TRUECOLOR_TAG: u32 = 1 << 24;
const DEFAULT_FOREGROUND_TAG: u32 = 1 << 25;
const DEFAULT_BACKGROUND_TAG: u32 = 1 << 26;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct TerminalColor(u32);

impl TerminalColor {
    pub(crate) const fn indexed(index: u8) -> Self {
        Self(index as u32)
    }

    pub(crate) const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(TRUECOLOR_TAG | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }

    pub(crate) const fn default_foreground() -> Self {
        Self(DEFAULT_FOREGROUND_TAG)
    }

    pub(crate) const fn default_background() -> Self {
        Self(DEFAULT_BACKGROUND_TAG)
    }

    pub(crate) const fn indexed_value(self) -> Option<u8> {
        if self.0 <= u8::MAX as u32 {
            Some(self.0 as u8)
        } else {
            None
        }
    }

    pub(crate) const fn rgb_value(self) -> Option<(u8, u8, u8)> {
        if self.0 & TRUECOLOR_TAG == 0 {
            None
        } else {
            Some((
                ((self.0 >> 16) & 0xff) as u8,
                ((self.0 >> 8) & 0xff) as u8,
                (self.0 & 0xff) as u8,
            ))
        }
    }

    pub(crate) const fn is_default_foreground(self) -> bool {
        self.0 == DEFAULT_FOREGROUND_TAG
    }

    pub(crate) const fn is_default_background(self) -> bool {
        self.0 == DEFAULT_BACKGROUND_TAG
    }
}

impl Default for TerminalColor {
    fn default() -> Self {
        Self::indexed(0)
    }
}

impl PartialEq<u8> for TerminalColor {
    fn eq(&self, other: &u8) -> bool {
        self.indexed_value() == Some(*other)
    }
}

pub(crate) const CELL_PRESENTATION_AUTO: u8 = 0;
pub(crate) const CELL_PRESENTATION_TEXT: u8 = 1;
pub(crate) const CELL_PRESENTATION_EMOJI: u8 = 2;

const CELL_FLAG_WIDE: u8 = 1 << 0;
const CELL_FLAG_WIDE_SPACER: u8 = 1 << 1;
const CELL_FLAG_INVERSE: u8 = 1 << 2;
const CELL_FLAG_UNDERLINE: u8 = 1 << 3;
const CELL_FLAG_DIM: u8 = 1 << 4;
pub(crate) const TERMINAL_CELL_EXTRA_MAX_CHARS: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CellExtra {
    zero_width: Vec<char>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub(crate) fg: TerminalColor,
    pub(crate) bg: TerminalColor,
    pub presentation: u8,
    flags: u8,
    extra: Option<Arc<CellExtra>>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: TerminalColor::default_foreground(),
            bg: TerminalColor::default_background(),
            presentation: CELL_PRESENTATION_AUTO,
            flags: 0,
            extra: None,
        }
    }
}

impl Cell {
    pub(crate) fn blank_with_background(bg: TerminalColor) -> Self {
        Self { bg, ..Self::default() }
    }

    pub(crate) fn set_char(
        &mut self,
        c: char,
        fg: TerminalColor,
        bg: TerminalColor,
        wide: bool,
    ) {
        self.c = c;
        self.fg = fg;
        self.bg = bg;
        self.presentation = CELL_PRESENTATION_AUTO;
        self.flags = if wide { CELL_FLAG_WIDE } else { 0 };
        self.extra = None;
    }

    pub(crate) fn set_wide_spacer(&mut self, fg: TerminalColor, bg: TerminalColor) {
        self.c = ' ';
        self.fg = fg;
        self.bg = bg;
        self.presentation = CELL_PRESENTATION_AUTO;
        self.flags = CELL_FLAG_WIDE_SPACER;
        self.extra = None;
    }

    pub(crate) fn is_wide(&self) -> bool {
        self.flags & CELL_FLAG_WIDE != 0
    }

    pub(crate) fn is_wide_spacer(&self) -> bool {
        self.flags & CELL_FLAG_WIDE_SPACER != 0
    }

    pub(crate) fn set_sgr_style(&mut self, inverse: bool, underline: bool, dim: bool) {
        self.flags &= CELL_FLAG_WIDE | CELL_FLAG_WIDE_SPACER;
        if inverse {
            self.flags |= CELL_FLAG_INVERSE;
        }
        if underline {
            self.flags |= CELL_FLAG_UNDERLINE;
        }
        if dim {
            self.flags |= CELL_FLAG_DIM;
        }
    }

    pub(crate) fn is_inverse(&self) -> bool {
        self.flags & CELL_FLAG_INVERSE != 0
    }

    #[cfg(test)]
    pub(crate) fn is_underlined(&self) -> bool {
        self.flags & CELL_FLAG_UNDERLINE != 0
    }

    pub(crate) fn is_dim(&self) -> bool {
        self.flags & CELL_FLAG_DIM != 0
    }

    pub(crate) fn push_zero_width(&mut self, c: char) {
        if self.zero_width().len() >= TERMINAL_CELL_EXTRA_MAX_CHARS {
            return;
        }
        let extra = self.extra.get_or_insert_with(|| {
            Arc::new(CellExtra {
                zero_width: Vec::with_capacity(4),
            })
        });
        Arc::make_mut(extra).zero_width.push(c);
    }

    pub(crate) fn zero_width(&self) -> &[char] {
        self.extra
            .as_deref()
            .map_or(&[], |extra| extra.zero_width.as_slice())
    }

    pub(crate) fn append_text_to(&self, output: &mut String) {
        if self.is_wide_spacer() {
            return;
        }
        output.push(self.c);
        output.extend(self.zero_width().iter().copied());
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MouseTrackingMode {
    #[default]
    None,
    Press,
    ButtonMotion,
    AnyMotion,
}

impl MouseTrackingMode {
    pub(crate) const fn enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[inline(always)]
pub(crate) fn terminal_presentation_selector(c: char) -> Option<u8> {
    match c {
        '\u{FE0E}' => Some(CELL_PRESENTATION_TEXT),
        '\u{FE0F}' => Some(CELL_PRESENTATION_EMOJI),
        _ => None,
    }
}

#[inline(always)]
pub(crate) fn is_terminal_zero_width_format(c: char) -> bool {
    let u = c as u32;
    c == '\u{200D}' || (0xFE00..=0xFE0F).contains(&u) || (0xE0100..=0xE01EF).contains(&u)
}

#[inline(always)]
pub(crate) fn terminal_is_emoji_modifier(c: char) -> bool {
    matches!(c as u32, 0x1F3FB..=0x1F3FF)
}

#[inline(always)]
pub(crate) fn terminal_char_width(c: char) -> usize {
    if is_terminal_zero_width_format(c) || terminal_is_emoji_modifier(c) {
        0
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0).min(2)
    }
}

#[inline(always)]
pub(crate) fn terminal_should_render_zero_width(c: char) -> bool {
    !is_terminal_zero_width_format(c)
        && !terminal_is_emoji_modifier(c)
        && UnicodeWidthChar::width(c) == Some(0)
}

pub(crate) fn terminal_color_rgba(color: TerminalColor) -> [f32; 4] {
    if color.is_default_foreground() {
        return ANSI_16_COLORS[7];
    }
    if color.is_default_background() {
        return ANSI_16_COLORS[0];
    }
    if let Some(index) = color.indexed_value() {
        if index < 16 {
            return ANSI_16_COLORS[index as usize];
        }
        if index < 232 {
            let cube = index - 16;
            let red = cube / 36;
            let green = (cube % 36) / 6;
            let blue = cube % 6;
            let level = |component: u8| {
                if component == 0 {
                    0
                } else {
                    55 + component * 40
                }
            };
            return rgb_rgba(level(red), level(green), level(blue));
        }
        let gray = 8 + (index - 232) * 10;
        return rgb_rgba(gray, gray, gray);
    }
    let (red, green, blue) = color.rgb_value().unwrap_or((255, 255, 255));
    rgb_rgba(red, green, blue)
}

#[inline]
fn rgb_rgba(red: u8, green: u8, blue: u8) -> [f32; 4] {
    const DENOMINATOR: f32 = 255.0;
    [
        red as f32 / DENOMINATOR,
        green as f32 / DENOMINATOR,
        blue as f32 / DENOMINATOR,
        1.0,
    ]
}

pub(crate) fn terminal_effective_foreground(
    foreground: TerminalColor,
    bold: bool,
) -> TerminalColor {
    if bold
        && let Some(index) = foreground.indexed_value()
        && index < 8
    {
        TerminalColor::indexed(index + 8)
    } else {
        foreground
    }
}

pub(crate) fn apply_ansi_sgr(
    params: &Params,
    fg: &mut TerminalColor,
    bg: &mut TerminalColor,
    bold: &mut bool,
    dim: &mut bool,
    underline: &mut bool,
    inverse: &mut bool,
) {
    if params.is_empty() {
        *fg = TerminalColor::default_foreground();
        *bg = TerminalColor::default_background();
        *bold = false;
        *dim = false;
        *underline = false;
        *inverse = false;
        return;
    }

    let mut params = params.iter();
    while let Some(param) = params.next() {
        let Some(value) = param.first().copied() else {
            continue;
        };
        match value {
            0 => {
                *fg = TerminalColor::default_foreground();
                *bg = TerminalColor::default_background();
                *bold = false;
                *dim = false;
                *underline = false;
                *inverse = false;
            }
            1 => *bold = true,
            2 => *dim = true,
            4 => *underline = param.get(1).copied() != Some(0),
            7 => *inverse = true,
            22 => {
                *bold = false;
                *dim = false;
            }
            24 => *underline = false,
            27 => *inverse = false,
            30..=37 => *fg = TerminalColor::indexed((value - 30) as u8),
            40..=47 => *bg = TerminalColor::indexed((value - 40) as u8),
            90..=97 => *fg = TerminalColor::indexed((value - 90 + 8) as u8),
            100..=107 => *bg = TerminalColor::indexed((value - 100 + 8) as u8),
            39 => *fg = TerminalColor::default_foreground(),
            49 => *bg = TerminalColor::default_background(),
            38 | 48 => {
                if let Some(color) = extended_color(param, &mut params) {
                    if value == 38 {
                        *fg = color;
                    } else {
                        *bg = color;
                    }
                }
            }
            _ => {}
        }
    }
}

fn extended_color<'a, I>(first: &[u16], params: &mut I) -> Option<TerminalColor>
where
    I: Iterator<Item = &'a [u16]>,
{
    if first.len() > 1 {
        return extended_color_from_subparams(&first[1..]);
    }

    let mode = params.next()?.first().copied()?;
    match mode {
        5 => Some(TerminalColor::indexed(
            params.next()?.first().copied()?.min(u8::MAX as u16) as u8,
        )),
        2 => {
            let red = params.next()?.first().copied()?;
            let green = params.next()?.first().copied()?;
            let blue = params.next()?.first().copied()?;
            Some(TerminalColor::rgb(
                red.min(255) as u8,
                green.min(255) as u8,
                blue.min(255) as u8,
            ))
        }
        _ => None,
    }
}

fn extended_color_from_subparams(params: &[u16]) -> Option<TerminalColor> {
    match params.first().copied()? {
        5 => Some(TerminalColor::indexed(
            params.get(1).copied()?.min(255) as u8,
        )),
        2 => {
            let components = if params.len() >= 5 {
                &params[params.len() - 3..]
            } else {
                params.get(1..4)?
            };
            Some(TerminalColor::rgb(
                components[0].min(255) as u8,
                components[1].min(255) as u8,
                components[2].min(255) as u8,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vte::{Parser, Perform};

    struct SgrProbe {
        fg: TerminalColor,
        bg: TerminalColor,
        bold: bool,
        dim: bool,
        underline: bool,
        inverse: bool,
    }

    impl Perform for SgrProbe {
        fn csi_dispatch(
            &mut self,
            params: &Params,
            _intermediates: &[u8],
            _ignore: bool,
            action: char,
        ) {
            if action == 'm' {
                apply_ansi_sgr(
                    params,
                    &mut self.fg,
                    &mut self.bg,
                    &mut self.bold,
                    &mut self.dim,
                    &mut self.underline,
                    &mut self.inverse,
                );
            }
        }
    }

    #[test]
    fn sgr_truecolor_and_extended_palette_are_compact_and_lossless() {
        let mut probe = SgrProbe {
            fg: TerminalColor::indexed(7),
            bg: TerminalColor::indexed(0),
            bold: false,
            dim: false,
            underline: false,
            inverse: false,
        };
        Parser::new().advance(
            &mut probe,
            b"\x1b[38;2;1;2;3;48;5;200m\x1b[38:2::4:5:6m",
        );
        assert_eq!(probe.fg.rgb_value(), Some((4, 5, 6)));
        assert_eq!(probe.bg.indexed_value(), Some(200));
        assert_eq!(std::mem::size_of::<TerminalColor>(), 4);
    }

    #[test]
    fn sgr_intensity_preserves_logical_default_and_explicit_bright_colors() {
        let default = TerminalColor::default_foreground();
        assert_eq!(terminal_effective_foreground(default, true), default);
        assert_eq!(
            terminal_effective_foreground(TerminalColor::indexed(1), true),
            TerminalColor::indexed(9)
        );
        assert_eq!(
            terminal_effective_foreground(TerminalColor::indexed(9), false),
            TerminalColor::indexed(9)
        );
        assert_eq!(
            terminal_effective_foreground(TerminalColor::indexed(9), true),
            TerminalColor::indexed(9)
        );
    }

    #[test]
    fn sgr_common_style_state_toggles_and_reset_are_independent() {
        let mut probe = SgrProbe {
            fg: TerminalColor::default_foreground(),
            bg: TerminalColor::default_background(),
            bold: false,
            dim: false,
            underline: false,
            inverse: false,
        };
        Parser::new().advance(&mut probe, b"\x1b[1;2;4;7m");
        assert!(probe.bold);
        assert!(probe.dim);
        assert!(probe.underline);
        assert!(probe.inverse);
        Parser::new().advance(&mut probe, b"\x1b[22;24;27m");
        assert!(!probe.bold);
        assert!(!probe.dim);
        assert!(!probe.underline);
        assert!(!probe.inverse);
        Parser::new().advance(&mut probe, b"\x1b[1;2;4;7;31;44m\x1b[0m");
        assert!(probe.fg.is_default_foreground());
        assert!(probe.bg.is_default_background());
        assert!(!probe.bold && !probe.dim && !probe.underline && !probe.inverse);
    }

    #[test]
    fn sgr_colon_underline_zero_cancels_underline_without_disabling_supported_styles() {
        let mut probe = SgrProbe {
            fg: TerminalColor::default_foreground(),
            bg: TerminalColor::default_background(),
            bold: false,
            dim: false,
            underline: false,
            inverse: false,
        };
        let mut parser = Parser::new();
        parser.advance(&mut probe, b"\x1b[4m");
        assert!(probe.underline);
        parser.advance(&mut probe, b"\x1b[4:0m");
        assert!(!probe.underline);
        parser.advance(&mut probe, b"\x1b[4:2m");
        assert!(probe.underline);
        parser.advance(&mut probe, b"\x1b[24m");
        assert!(!probe.underline);
    }

    #[test]
    fn cell_style_flags_share_compact_storage_with_wide_state() {
        let mut cell = Cell::default();
        cell.set_char(
            '界',
            TerminalColor::indexed(1),
            TerminalColor::indexed(4),
            true,
        );
        cell.set_sgr_style(true, true, true);
        assert!(cell.is_wide());
        assert!(cell.is_inverse());
        assert!(cell.is_underlined());
        assert!(cell.is_dim());
        assert!(std::mem::size_of::<Cell>() <= 24);

        let blank = Cell::blank_with_background(TerminalColor::indexed(1));
        assert_eq!(blank.c, ' ');
        assert_eq!(blank.bg, TerminalColor::indexed(1));
        assert!(blank.fg.is_default_foreground());
        assert!(!blank.is_inverse());
        assert!(!blank.is_underlined());
        assert!(!blank.is_dim());
    }

    #[test]
    fn xterm_palette_mapping_covers_cube_and_grayscale() {
        assert_eq!(terminal_color_rgba(TerminalColor::indexed(16)), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(terminal_color_rgba(TerminalColor::indexed(231)), [1.0, 1.0, 1.0, 1.0]);
        let gray = terminal_color_rgba(TerminalColor::indexed(232));
        assert!((gray[0] - 8.0 / 255.0).abs() < f32::EPSILON);
        assert_eq!(gray[0], gray[1]);
        assert_eq!(gray[1], gray[2]);
    }

    #[test]
    fn default_background_is_distinct_from_explicit_ansi_black() {
        let cell = Cell::default();
        assert!(cell.bg.is_default_background());
        assert!(!TerminalColor::indexed(0).is_default_background());
        assert_eq!(terminal_color_rgba(TerminalColor::indexed(0)), ANSI_16_COLORS[0]);
    }

    #[test]
    fn ordinary_cells_keep_rare_unicode_storage_out_of_line() {
        let cell = Cell::default();
        assert!(cell.zero_width().is_empty());
        assert!(cell.extra.is_none());
        assert!(std::mem::size_of::<Cell>() <= 24);
    }

    #[test]
    fn zero_width_storage_is_hard_bounded_per_cell() {
        let mut cell = Cell::default();
        cell.set_char(
            'A',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            false,
        );
        for _ in 0..100_000 {
            cell.push_zero_width('\u{0301}');
        }
        assert_eq!(cell.zero_width().len(), TERMINAL_CELL_EXTRA_MAX_CHARS);
    }

    #[test]
    fn common_emoji_clusters_fit_inside_zero_width_bound() {
        let mut family = Cell::default();
        family.set_char(
            '👨',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            true,
        );
        for c in ['\u{200D}', '👩', '\u{200D}', '👧', '\u{200D}', '👦'] {
            family.push_zero_width(c);
        }
        assert_eq!(
            family.zero_width(),
            &['\u{200D}', '👩', '\u{200D}', '👧', '\u{200D}', '👦']
        );

        let mut skin_tone = Cell::default();
        skin_tone.set_char(
            '👍',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            true,
        );
        skin_tone.push_zero_width('🏽');
        assert_eq!(skin_tone.zero_width(), &['🏽']);
        assert!(family.zero_width().len() < TERMINAL_CELL_EXTRA_MAX_CHARS);
    }
}
