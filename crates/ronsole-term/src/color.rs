use crate::cell::{Cell, CellFlags};

const TRUECOLOR_TAG: u32 = 1 << 24;
const DEFAULT_FOREGROUND_TAG: u32 = 1 << 25;
const DEFAULT_BACKGROUND_TAG: u32 = 1 << 26;

/// Dim scales the foreground channels by this factor.
const DIM_FACTOR: f32 = 0.66;

/// An 8-bit RGB colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Source of a colour, as stored in a [`TerminalColor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorKind {
    DefaultForeground,
    DefaultBackground,
    Indexed(u8),
    Rgb(Rgb),
}

/// A colour as written by the application: an indexed colour, a truecolor
/// value, or a reference to the default foreground or background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct TerminalColor(u32);

impl TerminalColor {
    pub const fn indexed(index: u8) -> Self {
        Self(index as u32)
    }

    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(TRUECOLOR_TAG | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }

    pub const fn default_foreground() -> Self {
        Self(DEFAULT_FOREGROUND_TAG)
    }

    pub const fn default_background() -> Self {
        Self(DEFAULT_BACKGROUND_TAG)
    }

    pub const fn indexed_value(self) -> Option<u8> {
        if self.0 <= u8::MAX as u32 {
            Some(self.0 as u8)
        } else {
            None
        }
    }

    pub const fn rgb_value(self) -> Option<(u8, u8, u8)> {
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

    pub const fn is_default_foreground(self) -> bool {
        self.0 == DEFAULT_FOREGROUND_TAG
    }

    pub const fn is_default_background(self) -> bool {
        self.0 == DEFAULT_BACKGROUND_TAG
    }

    pub fn kind(self) -> ColorKind {
        if self.is_default_foreground() {
            ColorKind::DefaultForeground
        } else if self.is_default_background() {
            ColorKind::DefaultBackground
        } else if let Some((red, green, blue)) = self.rgb_value() {
            ColorKind::Rgb(Rgb {
                r: red,
                g: green,
                b: blue,
            })
        } else {
            ColorKind::Indexed(self.0 as u8)
        }
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

/// Colour of an xterm 256-colour palette index. Indices 0-15 belong to the
/// theme and return `None`.
pub fn xterm_256_rgb(index: u8) -> Option<Rgb> {
    if index < 16 {
        return None;
    }
    if index < 232 {
        let cube = index - 16;
        let level = |component: u8| if component == 0 { 0 } else { 55 + component * 40 };
        return Some(Rgb {
            r: level(cube / 36),
            g: level((cube % 36) / 6),
            b: level(cube % 6),
        });
    }
    let gray = 8 + (index - 232) * 10;
    Some(Rgb {
        r: gray,
        g: gray,
        b: gray,
    })
}

/// Theme colours used to resolve [`TerminalColor`] values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermPalette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
    /// Foreground for bold text drawn in the default foreground colour. `None`
    /// keeps the regular foreground for bold text.
    pub bold_foreground: Option<Rgb>,
}

/// Resolved colours of one cell, after inverse and dim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellColors {
    pub fg: Rgb,
    pub bg: Rgb,
    /// True when the background source, after inverse, is the default background.
    pub bg_is_default: bool,
}

impl TermPalette {
    pub fn rgb(&self, color: TerminalColor) -> Rgb {
        match color.kind() {
            ColorKind::DefaultForeground => self.foreground,
            ColorKind::DefaultBackground => self.background,
            ColorKind::Indexed(index) if index < 16 => self.ansi[index as usize],
            ColorKind::Indexed(index) => xterm_256_rgb(index).unwrap_or(self.foreground),
            ColorKind::Rgb(rgb) => rgb,
        }
    }

    /// Resolves the colours of `cell`: inverse swaps the colour sources, then
    /// dim scales only the resulting foreground.
    pub fn cell_colors(&self, cell: &Cell) -> CellColors {
        let flags = cell.flags();
        let mut fg_source = cell.fg();
        let mut bg_source = cell.bg();
        if flags.contains(CellFlags::INVERSE) {
            std::mem::swap(&mut fg_source, &mut bg_source);
        }
        // Bold colour replaces only a non-inverse default foreground, as the renderer does.
        let mut fg = match self.bold_foreground {
            Some(bold)
                if flags.contains(CellFlags::BOLD)
                    && !flags.contains(CellFlags::INVERSE)
                    && fg_source.is_default_foreground() =>
            {
                bold
            }
            _ => self.rgb(fg_source),
        };
        if flags.contains(CellFlags::DIM) {
            fg = Rgb {
                r: dim_channel(fg.r),
                g: dim_channel(fg.g),
                b: dim_channel(fg.b),
            };
        }
        CellColors {
            fg,
            bg: self.rgb(bg_source),
            bg_is_default: bg_source.is_default_background(),
        }
    }
}

fn dim_channel(value: u8) -> u8 {
    (f32::from(value) * DIM_FACTOR).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::UnderlineStyle;

    fn test_palette() -> TermPalette {
        TermPalette {
            foreground: Rgb {
                r: 200,
                g: 201,
                b: 202,
            },
            background: Rgb {
                r: 10,
                g: 11,
                b: 12,
            },
            cursor: Rgb { r: 1, g: 2, b: 3 },
            ansi: std::array::from_fn(|index| Rgb {
                r: index as u8,
                g: 0,
                b: 0,
            }),
            bold_foreground: None,
        }
    }

    #[test]
    fn bold_default_foreground_uses_bold_colour_when_set() {
        let mut palette = test_palette();
        let bold_cell = Cell::new_for_test(
            'x',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            CellFlags::BOLD,
            UnderlineStyle::None,
        );
        // Unset: bold text keeps the regular default foreground.
        assert_eq!(palette.cell_colors(&bold_cell).fg, palette.foreground);

        let bold = Rgb {
            r: 250,
            g: 251,
            b: 252,
        };
        palette.bold_foreground = Some(bold);
        assert_eq!(palette.cell_colors(&bold_cell).fg, bold);

        let plain_cell = Cell::new_for_test(
            'x',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            CellFlags::default(),
            UnderlineStyle::None,
        );
        assert_eq!(palette.cell_colors(&plain_cell).fg, palette.foreground);
    }

    #[test]
    fn bold_colour_does_not_replace_explicit_indexed_foreground() {
        let mut palette = test_palette();
        palette.bold_foreground = Some(Rgb {
            r: 250,
            g: 251,
            b: 252,
        });
        let cell = Cell::new_for_test(
            'x',
            TerminalColor::indexed(1),
            TerminalColor::default_background(),
            CellFlags::BOLD,
            UnderlineStyle::None,
        );
        assert_eq!(palette.cell_colors(&cell).fg, palette.ansi[1]);
    }

    #[test]
    fn xterm_256_rgb_matches_cube_and_grey() {
        assert_eq!(xterm_256_rgb(15), None);
        assert_eq!(xterm_256_rgb(16), Some(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(xterm_256_rgb(196), Some(Rgb { r: 255, g: 0, b: 0 }));
        assert_eq!(xterm_256_rgb(231), Some(Rgb { r: 255, g: 255, b: 255 }));
        assert_eq!(xterm_256_rgb(232), Some(Rgb { r: 8, g: 8, b: 8 }));
        assert_eq!(xterm_256_rgb(255), Some(Rgb { r: 238, g: 238, b: 238 }));
    }

    #[test]
    fn terminal_color_kind_reports_source() {
        assert_eq!(
            TerminalColor::default_background().kind(),
            ColorKind::DefaultBackground
        );
        assert_eq!(TerminalColor::indexed(4).kind(), ColorKind::Indexed(4));
        assert_eq!(
            TerminalColor::rgb(1, 2, 3).kind(),
            ColorKind::Rgb(Rgb { r: 1, g: 2, b: 3 })
        );
    }

    #[test]
    fn bold_colour_does_not_replace_default_foreground_under_inverse() {
        let mut palette = test_palette();
        palette.bold_foreground = Some(Rgb {
            r: 250,
            g: 251,
            b: 252,
        });
        // Inverse takes the foreground from the cell background, which is the default
        // foreground here, so only the bold-rule guard keeps the bold colour out.
        let cell = Cell::new_for_test(
            'x',
            TerminalColor::indexed(4),
            TerminalColor::default_foreground(),
            CellFlags::BOLD | CellFlags::INVERSE,
            UnderlineStyle::None,
        );
        assert_eq!(palette.cell_colors(&cell).fg, palette.foreground);
    }

    #[test]
    fn cell_colors_inverse_keeps_default_background_identity() {
        let palette = test_palette();
        // Inverse swaps the sources: the background comes from the default
        // background tag, so it must stay identified as the default background.
        let cell = Cell::new_for_test(
            'x',
            TerminalColor::default_background(),
            TerminalColor::indexed(1),
            CellFlags::INVERSE,
            UnderlineStyle::None,
        );
        let colors = palette.cell_colors(&cell);
        assert_eq!(colors.fg, palette.ansi[1]);
        assert_eq!(colors.bg, palette.background);
        assert!(colors.bg_is_default);
    }

    #[test]
    fn cell_colors_inverse_of_default_cell_is_not_default_background() {
        let palette = test_palette();
        let cell = Cell::new_for_test(
            'x',
            TerminalColor::default_foreground(),
            TerminalColor::default_background(),
            CellFlags::INVERSE,
            UnderlineStyle::None,
        );
        let colors = palette.cell_colors(&cell);
        assert_eq!(colors.fg, palette.background);
        assert_eq!(colors.bg, palette.foreground);
        assert!(!colors.bg_is_default);
    }

    #[test]
    fn cell_colors_dim_scales_foreground_only() {
        let palette = test_palette();
        let cell = Cell::new_for_test(
            'x',
            TerminalColor::rgb(255, 255, 255),
            TerminalColor::indexed(4),
            CellFlags::DIM,
            UnderlineStyle::None,
        );
        let colors = palette.cell_colors(&cell);
        // 255 * 0.66 = 168.3, rounded to 168.
        assert_eq!(colors.fg, Rgb { r: 168, g: 168, b: 168 });
        assert_eq!(colors.bg, palette.ansi[4]);
        assert!(!colors.bg_is_default);
    }
}
