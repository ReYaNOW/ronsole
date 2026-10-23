//! Decoders for OSC payloads: RFC 4648 base64 (OSC 52) and percent-encoding (OSC 7),
//! and the plain-URL finder used for Ctrl+click.

use crate::cell::Cell;
use std::ops::Range;

/// Schemes that start a plain URL.
const URL_SCHEMES: [&str; 3] = ["http://", "https://", "file://"];

/// Cells searched on each side of the clicked cell for a plain URL.
pub(crate) const URL_SEARCH_RADIUS: usize = 8192;

/// Opening and closing brackets, one pair per entry; see [`bracket_of`].
const BRACKET_PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

/// Cell range of the plain URL around `col` in a logical line, or `None`.
///
/// The token is the run of cells around `col` up to a space or control
/// character. Within it, the last scheme starting at or before `col` wins;
/// trailing punctuation and unbalanced closing brackets are cut. Wide-character
/// spacers belong to the wide character before them, and the range includes them.
/// Only [`URL_SEARCH_RADIUS`] cells on each side of `col` are examined: a token
/// that does not end within that window is too long, and gives `None`.
pub fn find_url(line: &[Cell], col: usize) -> Option<Range<usize>> {
    if col >= line.len() {
        return None;
    }
    let col = if line[col].is_wide_spacer() {
        col.checked_sub(1)?
    } else {
        col
    };
    if is_separator(&line[col]) {
        return None;
    }
    let window_start = col.saturating_sub(URL_SEARCH_RADIUS);
    let window_end = line
        .len()
        .min(col.saturating_add(URL_SEARCH_RADIUS).saturating_add(1));
    let start = line[window_start..col]
        .iter()
        .rposition(is_separator)
        .map_or(window_start, |index| window_start + index + 1);
    let end = match line[col..window_end].iter().position(is_separator) {
        Some(offset) => col + offset,
        // The token continues past the window, so the URL end is unknown.
        None if window_end < line.len() => return None,
        None => line.len(),
    };

    // Characters of the token with their cell index; spacers carry no character.
    let token: Vec<(char, usize)> = (start..end)
        .filter(|&index| !line[index].is_wide_spacer())
        .map(|index| (line[index].ch(), index))
        .collect();

    let mut matched = None;
    for (position, &(_, index)) in token.iter().enumerate() {
        if index > col {
            break;
        }
        for scheme in URL_SCHEMES {
            if starts_with_scheme(&token, position, scheme) {
                matched = Some((position, scheme.len()));
            }
        }
    }
    let (scheme_position, scheme_len) = matched?;
    let body_start = scheme_position + scheme_len;

    // Bracket counts of the candidate `token[scheme_position..token_end]`, updated
    // as characters are cut, so trimming takes time linear in the token length.
    let mut brackets = [[0usize; 2]; BRACKET_PAIRS.len()];
    for &(ch, _) in &token[scheme_position..] {
        if let Some((pair, closing)) = bracket_of(ch) {
            brackets[pair][usize::from(closing)] += 1;
        }
    }
    let mut token_end = token.len();
    while token_end > body_start && is_cuttable(token[token_end - 1].0, &brackets) {
        if let Some((pair, closing)) = bracket_of(token[token_end - 1].0) {
            brackets[pair][usize::from(closing)] -= 1;
        }
        token_end -= 1;
    }
    if token_end <= body_start {
        return None;
    }

    let first_cell = token[scheme_position].1;
    let mut end_cell = token[token_end - 1].1 + 1;
    while end_cell < line.len() && line[end_cell].is_wide_spacer() {
        end_cell += 1;
    }
    (first_cell <= col && col < end_cell).then_some(first_cell..end_cell)
}

/// Space and control characters end a URL token; wide-character spacers never do.
fn is_separator(cell: &Cell) -> bool {
    !cell.is_wide_spacer() && (cell.ch().is_whitespace() || cell.ch().is_control())
}

/// Whether the token at `position` begins with the ASCII `scheme`.
fn starts_with_scheme(token: &[(char, usize)], position: usize, scheme: &str) -> bool {
    scheme.chars().enumerate().all(|(offset, expected)| {
        token
            .get(position + offset)
            .is_some_and(|&(ch, _)| ch == expected)
    })
}

/// Position in [`BRACKET_PAIRS`] of `ch`, and whether `ch` is the closing bracket.
fn bracket_of(ch: char) -> Option<(usize, bool)> {
    BRACKET_PAIRS
        .iter()
        .enumerate()
        .find_map(|(pair, &(opening, closing))| {
            if ch == opening {
                Some((pair, false))
            } else if ch == closing {
                Some((pair, true))
            } else {
                None
            }
        })
}

/// Whether the candidate URL's last character `last` must be cut: trailing
/// sentence punctuation, or a closing bracket with no opening bracket before it.
/// `brackets` holds the candidate's counts as `[opening, closing]` per pair.
fn is_cuttable(last: char, brackets: &[[usize; 2]]) -> bool {
    match last {
        '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
        _ => bracket_of(last)
            .is_some_and(|(pair, closing)| closing && brackets[pair][1] > brackets[pair][0]),
    }
}

/// Decodes standard base64. Trailing `=` padding is optional; when present it
/// may only close the input, at most two signs, and the whole input must be a
/// multiple of four bytes long. A data length of one more than a multiple of
/// four is invalid. Unused trailing bits are accepted.
pub(crate) fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let data_len = input
        .iter()
        .position(|&byte| byte == b'=')
        .unwrap_or(input.len());
    let padding = &input[data_len..];
    if !padding.is_empty()
        && (padding.len() > 2
            || input.len() % 4 != 0
            || !padding.iter().all(|&byte| byte == b'='))
    {
        return None;
    }
    let data = &input[..data_len];
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    let mut accumulator: u32 = 0;
    let mut pending_bits: u32 = 0;
    for &byte in data {
        // Only the low `pending_bits + 8` bits are ever read, so bits shifted
        // out of the 32-bit accumulator are never needed.
        accumulator = (accumulator << 6) | u32::from(base64_value(byte)?);
        pending_bits += 6;
        if pending_bits >= 8 {
            pending_bits -= 8;
            out.push((accumulator >> pending_bits) as u8);
        }
    }
    Some(out)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes `%XX` escapes with hex digits of either case. A `%` without two
/// following hex digits is invalid; every other byte is copied as is.
pub(crate) fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.iter();
    while let Some(&byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let high = hex_value(*bytes.next()?)?;
        let low = hex_value(*bytes.next()?)?;
        out.push((high << 4) | low);
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
