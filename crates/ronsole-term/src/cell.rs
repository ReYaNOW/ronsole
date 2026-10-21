use std::sync::Arc;

use unicode_width::UnicodeWidthChar;
use vte::Params;

use crate::color::TerminalColor;

pub const CELL_PRESENTATION_AUTO: u8 = 0;
pub const CELL_PRESENTATION_TEXT: u8 = 1;
pub const CELL_PRESENTATION_EMOJI: u8 = 2;

/// Style flags of a [`Cell`]. The underline style is kept in a separate
/// 3-bit field and is read through [`Cell::underline`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const WIDE: CellFlags = CellFlags(1 << 0);
    pub const WIDE_SPACER: CellFlags = CellFlags(1 << 1);
    pub const INVERSE: CellFlags = CellFlags(1 << 2);
    pub const BOLD: CellFlags = CellFlags(1 << 3);
    pub const DIM: CellFlags = CellFlags(1 << 4);
    /// Drawn as blank; set only by the host. Any write to the cell clears it,
    /// so rewritten content is never hidden.
    pub const HOST_HIDDEN: CellFlags = CellFlags(1 << 5);
    pub const ITALIC: CellFlags = CellFlags(1 << 6);
    pub const STRIKE: CellFlags = CellFlags(1 << 7);
    pub const CONCEAL: CellFlags = CellFlags(1 << 8);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: CellFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = CellFlags;

    fn bitor(self, rhs: CellFlags) -> CellFlags {
        CellFlags(self.0 | rhs.0)
    }
}

const UNDERLINE_SHIFT: u16 = 9;
const UNDERLINE_MASK: u16 = 0b111 << UNDERLINE_SHIFT;
const WIDE_FLAGS: u16 = CellFlags::WIDE.0 | CellFlags::WIDE_SPACER.0;

/// Underline style of a cell, set by SGR 4 and cleared by SGR 24.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum UnderlineStyle {
    #[default]
    None = 0,
    Single = 1,
    Double = 2,
    Curly = 3,
    Dotted = 4,
    Dashed = 5,
}

/// OSC 8 hyperlink target attached to a cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hyperlink {
    pub id: Option<Box<str>>,
    pub uri: Box<str>,
}

// pub until Task 3 moves the grid
#[doc(hidden)]
pub const TERMINAL_CELL_EXTRA_MAX_CHARS: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CellExtra {
    zero_width: Vec<char>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    c: char,
    fg: TerminalColor,
    bg: TerminalColor,
    presentation: u8,
    flags: CellFlags,
    extra: Option<Arc<CellExtra>>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: TerminalColor::default_foreground(),
            bg: TerminalColor::default_background(),
            presentation: CELL_PRESENTATION_AUTO,
            flags: CellFlags::empty(),
            extra: None,
        }
    }
}

fn underline_bits(style: UnderlineStyle) -> u16 {
    (style as u16) << UNDERLINE_SHIFT
}

impl Cell {
    /// Test constructor; the `test-support` feature exposes it to dependent crates.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_for_test(
        c: char,
        fg: TerminalColor,
        bg: TerminalColor,
        flags: CellFlags,
        underline: UnderlineStyle,
    ) -> Self {
        Self {
            c,
            fg,
            bg,
            presentation: CELL_PRESENTATION_AUTO,
            flags: CellFlags(flags.0 | underline_bits(underline)),
            extra: None,
        }
    }

    pub fn ch(&self) -> char {
        self.c
    }

    pub fn fg(&self) -> TerminalColor {
        self.fg
    }

    pub fn bg(&self) -> TerminalColor {
        self.bg
    }

    pub fn presentation(&self) -> u8 {
        self.presentation
    }

    pub fn flags(&self) -> CellFlags {
        self.flags
    }

    pub fn underline(&self) -> UnderlineStyle {
        match (self.flags.0 & UNDERLINE_MASK) >> UNDERLINE_SHIFT {
            1 => UnderlineStyle::Single,
            2 => UnderlineStyle::Double,
            3 => UnderlineStyle::Curly,
            4 => UnderlineStyle::Dotted,
            5 => UnderlineStyle::Dashed,
            _ => UnderlineStyle::None,
        }
    }

    /// Underline colour is not stored yet; it is added with SGR 58 support.
    pub fn underline_color(&self) -> Option<TerminalColor> {
        None
    }

    /// Hyperlinks are not stored yet; OSC 8 support adds the storage.
    pub fn hyperlink(&self) -> Option<&Arc<Hyperlink>> {
        None
    }

    pub fn is_wide(&self) -> bool {
        self.flags.contains(CellFlags::WIDE)
    }

    pub fn is_wide_spacer(&self) -> bool {
        self.flags.contains(CellFlags::WIDE_SPACER)
    }

    pub fn is_inverse(&self) -> bool {
        self.flags.contains(CellFlags::INVERSE)
    }

    pub fn is_dim(&self) -> bool {
        self.flags.contains(CellFlags::DIM)
    }

    pub fn is_hidden(&self) -> bool {
        self.flags.contains(CellFlags::HOST_HIDDEN)
    }

    pub fn zero_width(&self) -> &[char] {
        self.extra
            .as_deref()
            .map_or(&[], |extra| extra.zero_width.as_slice())
    }

    // Write methods below are pub until Task 3 moves the grid into the crate;
    // Task 3 narrows them to pub(crate).

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub(crate) fn blank_with_background(bg: TerminalColor) -> Self {
        Self { bg, ..Self::default() }
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub(crate) fn set_ch(&mut self, c: char) {
        self.c = c;
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub fn set_char(&mut self, c: char, fg: TerminalColor, bg: TerminalColor, wide: bool) {
        self.c = c;
        self.fg = fg;
        self.bg = bg;
        self.presentation = CELL_PRESENTATION_AUTO;
        self.flags = if wide { CellFlags::WIDE } else { CellFlags::empty() };
        self.extra = None;
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub fn set_wide_spacer(&mut self, fg: TerminalColor, bg: TerminalColor) {
        self.c = ' ';
        self.fg = fg;
        self.bg = bg;
        self.presentation = CELL_PRESENTATION_AUTO;
        self.flags = CellFlags::WIDE_SPACER;
        self.extra = None;
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub(crate) fn set_presentation(&mut self, presentation: u8) {
        self.presentation = presentation;
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub fn set_sgr_style(&mut self, inverse: bool, underline: UnderlineStyle, dim: bool) {
        let mut bits = self.flags.0 & WIDE_FLAGS;
        if inverse {
            bits |= CellFlags::INVERSE.0;
        }
        bits |= underline_bits(underline);
        if dim {
            bits |= CellFlags::DIM.0;
        }
        self.flags = CellFlags(bits);
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub(crate) fn set_hidden(&mut self, hidden: bool) {
        if hidden {
            self.flags = CellFlags(self.flags.0 | CellFlags::HOST_HIDDEN.0);
        } else {
            self.flags = CellFlags(self.flags.0 & !CellFlags::HOST_HIDDEN.0);
        }
    }

    // pub until Task 3 moves the grid
    #[doc(hidden)]
    pub fn push_zero_width(&mut self, c: char) {
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

    pub fn append_text_to(&self, output: &mut String) {
        if self.is_wide_spacer() {
            return;
        }
        output.push(self.c);
        output.extend(self.zero_width().iter().copied());
    }
}

/// Glyphs the renderer always draws with text presentation (check marks).
#[inline(always)]
pub fn terminal_force_text_presentation(c: char) -> bool {
    matches!(c, '✔' | '✓')
}

// pub until Task 3 moves the grid
#[doc(hidden)]
#[inline(always)]
pub fn terminal_presentation_selector(c: char) -> Option<u8> {
    match c {
        '\u{FE0E}' => Some(CELL_PRESENTATION_TEXT),
        '\u{FE0F}' => Some(CELL_PRESENTATION_EMOJI),
        _ => None,
    }
}

// pub until Task 3 moves the grid
#[doc(hidden)]
#[inline(always)]
pub fn is_terminal_zero_width_format(c: char) -> bool {
    let u = c as u32;
    c == '\u{200D}' || (0xFE00..=0xFE0F).contains(&u) || (0xE0100..=0xE01EF).contains(&u)
}

// pub until Task 3 moves the grid
#[doc(hidden)]
#[inline(always)]
pub fn terminal_is_emoji_modifier(c: char) -> bool {
    matches!(c as u32, 0x1F3FB..=0x1F3FF)
}

// pub until Task 3 moves the grid
#[doc(hidden)]
#[inline(always)]
pub fn terminal_char_width(c: char) -> usize {
    if is_terminal_zero_width_format(c) || terminal_is_emoji_modifier(c) {
        0
    } else {
        UnicodeWidthChar::width(c).unwrap_or(0).min(2)
    }
}

#[inline(always)]
pub fn terminal_should_render_zero_width(c: char) -> bool {
    !is_terminal_zero_width_format(c)
        && !terminal_is_emoji_modifier(c)
        && UnicodeWidthChar::width(c) == Some(0)
}

// pub until Task 3 moves the grid
#[doc(hidden)]
pub fn terminal_effective_foreground(
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

// pub until Task 3 moves the grid
#[doc(hidden)]
pub fn apply_ansi_sgr(
    params: &Params,
    fg: &mut TerminalColor,
    bg: &mut TerminalColor,
    bold: &mut bool,
    dim: &mut bool,
    underline: &mut UnderlineStyle,
    inverse: &mut bool,
) {
    if params.is_empty() {
        *fg = TerminalColor::default_foreground();
        *bg = TerminalColor::default_background();
        *bold = false;
        *dim = false;
        *underline = UnderlineStyle::None;
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
                *underline = UnderlineStyle::None;
                *inverse = false;
            }
            1 => *bold = true,
            2 => *dim = true,
            4 => {
                *underline = if param.get(1).copied() == Some(0) {
                    UnderlineStyle::None
                } else {
                    UnderlineStyle::Single
                }
            }
            7 => *inverse = true,
            22 => {
                *bold = false;
                *dim = false;
            }
            24 => *underline = UnderlineStyle::None,
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
                // The probe keeps a bool; the style is mapped in and out around the call.
                let mut underline = if self.underline {
                    UnderlineStyle::Single
                } else {
                    UnderlineStyle::None
                };
                apply_ansi_sgr(
                    params,
                    &mut self.fg,
                    &mut self.bg,
                    &mut self.bold,
                    &mut self.dim,
                    &mut underline,
                    &mut self.inverse,
                );
                self.underline = underline != UnderlineStyle::None;
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
        cell.set_sgr_style(true, UnderlineStyle::Single, true);
        assert!(cell.is_wide());
        assert!(cell.is_inverse());
        assert!(cell.underline() == UnderlineStyle::Single);
        assert!(cell.is_dim());
        assert!(std::mem::size_of::<Cell>() <= 24);

        let blank = Cell::blank_with_background(TerminalColor::indexed(1));
        assert_eq!(blank.ch(), ' ');
        assert_eq!(blank.bg(), TerminalColor::indexed(1));
        assert!(blank.fg().is_default_foreground());
        assert!(!blank.is_inverse());
        assert!(blank.underline() == UnderlineStyle::None);
        assert!(!blank.is_dim());
    }

    #[test]
    fn cell_size_fits_24_bytes() {
        assert!(std::mem::size_of::<Cell>() <= 24);
    }

    struct UnderlineProbe {
        underline: UnderlineStyle,
    }

    impl Perform for UnderlineProbe {
        fn csi_dispatch(
            &mut self,
            params: &Params,
            _intermediates: &[u8],
            _ignore: bool,
            action: char,
        ) {
            if action == 'm' {
                let mut fg = TerminalColor::default_foreground();
                let mut bg = TerminalColor::default_background();
                let (mut bold, mut dim, mut inverse) = (false, false, false);
                apply_ansi_sgr(
                    params,
                    &mut fg,
                    &mut bg,
                    &mut bold,
                    &mut dim,
                    &mut self.underline,
                    &mut inverse,
                );
            }
        }
    }

    #[test]
    fn sgr_underline_sets_single_style() {
        let mut probe = UnderlineProbe {
            underline: UnderlineStyle::None,
        };
        Parser::new().advance(&mut probe, b"\x1b[4m");
        assert_eq!(probe.underline, UnderlineStyle::Single);
        Parser::new().advance(&mut probe, b"\x1b[24m");
        assert_eq!(probe.underline, UnderlineStyle::None);
    }

    #[test]
    fn ordinary_cells_keep_rare_unicode_storage_out_of_line() {
        let cell = Cell::default();
        assert!(cell.extra.is_none());
        assert!(cell.zero_width().is_empty());
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
