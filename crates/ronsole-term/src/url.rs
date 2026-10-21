//! Decoders for OSC payloads: RFC 4648 base64 (OSC 52) and percent-encoding (OSC 7),
//! and the plain-URL finder used for Ctrl+click.

use crate::cell::Cell;
use std::ops::Range;

/// Schemes that start a plain URL.
const URL_SCHEMES: [&str; 3] = ["http://", "https://", "file://"];

/// Cell range of the plain URL around `col` in a logical line, or `None`.
///
/// The token is the run of cells around `col` up to a space or control
/// character. Within it, the last scheme starting at or before `col` wins;
/// trailing punctuation and unbalanced closing brackets are cut. Wide-character
/// spacers belong to the wide character before them, and the range includes them.
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
    let start = line[..col]
        .iter()
        .rposition(is_separator)
        .map_or(0, |index| index + 1);
    let end = line[col..]
        .iter()
        .position(is_separator)
        .map_or(line.len(), |offset| col + offset);

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

    let mut token_end = token.len();
    while token_end > body_start && is_cuttable(&token[scheme_position..token_end]) {
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

/// Whether the last character of `candidate` must be cut from the URL: trailing
/// sentence punctuation, or a closing bracket with no opening bracket before it.
fn is_cuttable(candidate: &[(char, usize)]) -> bool {
    let Some(&(last, _)) = candidate.last() else {
        return false;
    };
    let opening = match last {
        '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => return true,
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '>' => '<',
        _ => return false,
    };
    let count = |wanted: char| candidate.iter().filter(|&&(ch, _)| ch == wanted).count();
    count(last) > count(opening)
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
