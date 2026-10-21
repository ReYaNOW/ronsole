//! SGR (`CSI ... m`) parsing into the terminal pen.
//!
//! vte yields one slice per `;`-separated parameter, so colon subparameters
//! such as `4:3` or `58:2::r:g:b` stay inside the parameter that owns them.

use vte::Params;

use crate::cell::{Pen, UnderlineStyle};
use crate::color::TerminalColor;

/// Applies one `CSI ... m` sequence to `pen`. Unknown codes are ignored.
pub(crate) fn apply_sgr(pen: &mut Pen, params: &Params) {
    if params.is_empty() {
        pen.reset_sgr();
        return;
    }

    let mut params = params.iter();
    while let Some(param) = params.next() {
        let Some(value) = param.first().copied() else {
            continue;
        };
        match value {
            0 => pen.reset_sgr(),
            1 => pen.bold = true,
            2 => pen.dim = true,
            3 => pen.italic = true,
            4 => pen.underline = underline_style(param.get(1).copied()),
            7 => pen.inverse = true,
            8 => pen.conceal = true,
            9 => pen.strike = true,
            21 => pen.underline = UnderlineStyle::Double,
            22 => {
                pen.bold = false;
                pen.dim = false;
            }
            23 => pen.italic = false,
            24 => pen.underline = UnderlineStyle::None,
            27 => pen.inverse = false,
            28 => pen.conceal = false,
            29 => pen.strike = false,
            30..=37 => pen.fg = TerminalColor::indexed((value - 30) as u8),
            40..=47 => pen.bg = TerminalColor::indexed((value - 40) as u8),
            90..=97 => pen.fg = TerminalColor::indexed((value - 90 + 8) as u8),
            100..=107 => pen.bg = TerminalColor::indexed((value - 100 + 8) as u8),
            38 | 48 => {
                if let Some(color) = extended_color(param, &mut params) {
                    if value == 38 {
                        pen.fg = color;
                    } else {
                        pen.bg = color;
                    }
                }
            }
            39 => pen.fg = TerminalColor::default_foreground(),
            49 => pen.bg = TerminalColor::default_background(),
            58 => {
                if let Some(color) = underline_color(param, &mut params) {
                    pen.set_underline_color(Some(color));
                }
            }
            59 => pen.set_underline_color(None),
            _ => {}
        }
    }
}

/// Maps the subparameter of `4:x` to an underline style. Plain `4` and unknown
/// subparameters are single underlines.
fn underline_style(subparam: Option<u16>) -> UnderlineStyle {
    match subparam {
        Some(0) => UnderlineStyle::None,
        Some(2) => UnderlineStyle::Double,
        Some(3) => UnderlineStyle::Curly,
        Some(4) => UnderlineStyle::Dotted,
        Some(5) => UnderlineStyle::Dashed,
        _ => UnderlineStyle::Single,
    }
}

fn clamp_u8(value: u16) -> u8 {
    value.min(255) as u8
}

fn extended_color<'a, I>(first: &[u16], params: &mut I) -> Option<TerminalColor>
where
    I: Iterator<Item = &'a [u16]>,
{
    if first.len() > 1 {
        return extended_color_from_subparams(&first[1..]);
    }
    semicolon_color(params)
}

/// Parses the operand of SGR 58. Colon forms must match one of the two
/// layouts exactly (`58:5:n`, `58:2:r:g:b`, `58:2::r:g:b`); anything else is
/// dropped without consuming the following parameters. The semicolon form
/// consumes its components from `params`.
fn underline_color<'a, I>(first: &[u16], params: &mut I) -> Option<TerminalColor>
where
    I: Iterator<Item = &'a [u16]>,
{
    match first {
        [_] => semicolon_color(params),
        [_, 5, index] => Some(TerminalColor::indexed(clamp_u8(*index))),
        [_, 2, red, green, blue] => Some(TerminalColor::rgb(
            clamp_u8(*red),
            clamp_u8(*green),
            clamp_u8(*blue),
        )),
        // Empty colour-space subparameter: `58:2::r:g:b`.
        [_, 2, _, red, green, blue] => Some(TerminalColor::rgb(
            clamp_u8(*red),
            clamp_u8(*green),
            clamp_u8(*blue),
        )),
        _ => None,
    }
}

/// Reads the legacy `;`-separated components that follow `38`, `48` or `58`.
fn semicolon_color<'a, I>(params: &mut I) -> Option<TerminalColor>
where
    I: Iterator<Item = &'a [u16]>,
{
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

    #[test]
    fn underline_style_maps_known_subparams_and_defaults_to_single() {
        assert_eq!(underline_style(None), UnderlineStyle::Single);
        assert_eq!(underline_style(Some(1)), UnderlineStyle::Single);
        assert_eq!(underline_style(Some(9)), UnderlineStyle::Single);
        assert_eq!(underline_style(Some(0)), UnderlineStyle::None);
        assert_eq!(underline_style(Some(5)), UnderlineStyle::Dashed);
    }
}
