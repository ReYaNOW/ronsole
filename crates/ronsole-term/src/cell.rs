use std::sync::Arc;

use unicode_width::UnicodeWidthChar;

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

/// Maximum number of zero-width characters stored in one cell's extra data;
/// further ones are dropped by `Cell::push_zero_width`.
pub const TERMINAL_CELL_EXTRA_MAX_CHARS: usize = 24;

/// Per-cell data that is rare: zero-width characters, and the underline colour
/// and hyperlink of the pen that wrote the cell. Cells written by one pen share
/// one `Arc`; `zero_width` is copied on write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CellExtra {
    zero_width: Vec<char>,
    underline_color: Option<TerminalColor>,
    hyperlink: Option<Arc<Hyperlink>>,
}

/// SGR and hyperlink state that the next written cells take over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pen {
    pub(crate) fg: TerminalColor,
    pub(crate) bg: TerminalColor,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) inverse: bool,
    pub(crate) conceal: bool,
    pub(crate) strike: bool,
    pub(crate) underline: UnderlineStyle,
    pub(crate) extra: Option<Arc<CellExtra>>,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: TerminalColor::default_foreground(),
            bg: TerminalColor::default_background(),
            bold: false,
            dim: false,
            italic: false,
            inverse: false,
            conceal: false,
            strike: false,
            underline: UnderlineStyle::None,
            extra: None,
        }
    }
}

impl Pen {
    /// SGR 0 and the empty `CSI m`. The open hyperlink is kept because OSC 8
    /// owns it, not SGR.
    pub(crate) fn reset_sgr(&mut self) {
        self.fg = TerminalColor::default_foreground();
        self.bg = TerminalColor::default_background();
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.inverse = false;
        self.conceal = false;
        self.strike = false;
        self.underline = UnderlineStyle::None;
        self.set_underline_color(None);
    }

    pub(crate) fn underline_color(&self) -> Option<TerminalColor> {
        self.extra.as_deref().and_then(|extra| extra.underline_color)
    }

    pub(crate) fn hyperlink(&self) -> Option<&Arc<Hyperlink>> {
        self.extra.as_deref().and_then(|extra| extra.hyperlink.as_ref())
    }

    /// Sets the underline colour for cells written from now on.
    pub(crate) fn set_underline_color(&mut self, color: Option<TerminalColor>) {
        if self.underline_color() != color {
            let hyperlink = self.hyperlink().cloned();
            self.set_extra(color, hyperlink);
        }
    }

    /// Opens (`Some`) or closes (`None`) the hyperlink for cells written from now on.
    pub(crate) fn set_hyperlink(&mut self, hyperlink: Option<Arc<Hyperlink>>) {
        if self.hyperlink() != hyperlink.as_ref() {
            let underline_color = self.underline_color();
            self.set_extra(underline_color, hyperlink);
        }
    }

    /// Replaces the shared extra. It is `None` when neither field is set, so
    /// plain text is written without allocating.
    fn set_extra(
        &mut self,
        underline_color: Option<TerminalColor>,
        hyperlink: Option<Arc<Hyperlink>>,
    ) {
        self.extra = if underline_color.is_none() && hyperlink.is_none() {
            None
        } else {
            Some(Arc::new(CellExtra {
                zero_width: Vec::new(),
                underline_color,
                hyperlink,
            }))
        };
    }
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

    /// Underline colour set by SGR 58; `None` means the default colour.
    pub fn underline_color(&self) -> Option<TerminalColor> {
        self.extra.as_deref().and_then(|extra| extra.underline_color)
    }

    /// OSC 8 hyperlink the cell was written under, if any.
    pub fn hyperlink(&self) -> Option<&Arc<Hyperlink>> {
        self.extra.as_deref().and_then(|extra| extra.hyperlink.as_ref())
    }

    pub(crate) fn extra(&self) -> Option<&Arc<CellExtra>> {
        self.extra.as_ref()
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

    pub(crate) fn blank_with_background(bg: TerminalColor) -> Self {
        Self { bg, ..Self::default() }
    }

    pub(crate) fn set_ch(&mut self, c: char) {
        self.c = c;
    }

    pub(crate) fn set_char(&mut self, c: char, fg: TerminalColor, bg: TerminalColor, wide: bool) {
        self.c = c;
        self.fg = fg;
        self.bg = bg;
        self.presentation = CELL_PRESENTATION_AUTO;
        self.flags = if wide { CellFlags::WIDE } else { CellFlags::empty() };
        self.extra = None;
    }

    pub(crate) fn set_wide_spacer(&mut self, fg: TerminalColor, bg: TerminalColor) {
        self.c = ' ';
        self.fg = fg;
        self.bg = bg;
        self.presentation = CELL_PRESENTATION_AUTO;
        self.flags = CellFlags::WIDE_SPACER;
        self.extra = None;
    }

    pub(crate) fn set_presentation(&mut self, presentation: u8) {
        self.presentation = presentation;
    }

    /// Copies the pen's style and shared extra into the cell. Keeps the wide
    /// bits and clears HOST_HIDDEN, which only the host sets.
    pub(crate) fn set_sgr_style(&mut self, pen: &Pen) {
        let mut bits = self.flags.0 & WIDE_FLAGS;
        for (set, flag) in [
            (pen.bold, CellFlags::BOLD),
            (pen.dim, CellFlags::DIM),
            (pen.italic, CellFlags::ITALIC),
            (pen.inverse, CellFlags::INVERSE),
            (pen.conceal, CellFlags::CONCEAL),
            (pen.strike, CellFlags::STRIKE),
        ] {
            if set {
                bits |= flag.0;
            }
        }
        bits |= underline_bits(pen.underline);
        self.flags = CellFlags(bits);
        self.extra = pen.extra.clone();
    }

    pub(crate) fn set_hidden(&mut self, hidden: bool) {
        if hidden {
            self.flags = CellFlags(self.flags.0 | CellFlags::HOST_HIDDEN.0);
        } else {
            self.flags = CellFlags(self.flags.0 & !CellFlags::HOST_HIDDEN.0);
        }
    }

    pub(crate) fn push_zero_width(&mut self, c: char) {
        if self.zero_width().len() >= TERMINAL_CELL_EXTRA_MAX_CHARS {
            return;
        }
        let extra = self.extra.get_or_insert_with(|| {
            Arc::new(CellExtra {
                zero_width: Vec::with_capacity(4),
                underline_color: None,
                hyperlink: None,
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
pub fn terminal_should_render_zero_width(c: char) -> bool {
    !is_terminal_zero_width_format(c)
        && !terminal_is_emoji_modifier(c)
        && UnicodeWidthChar::width(c) == Some(0)
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn cell_style_flags_share_compact_storage_with_wide_state() {
        let mut cell = Cell::default();
        cell.set_char(
            '界',
            TerminalColor::indexed(1),
            TerminalColor::indexed(4),
            true,
        );
        let pen = Pen {
            inverse: true,
            underline: UnderlineStyle::Single,
            dim: true,
            ..Pen::default()
        };
        cell.set_sgr_style(&pen);
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
