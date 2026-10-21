//! OSC payload handling: titles, colour queries, clipboard writes and working directory.

use crate::color::{Rgb, TerminalColor};
use crate::term::Term;
use crate::url::{base64_decode, percent_decode};

pub const TERMINAL_TITLE_MAX_BYTES: usize = 256;

/// Largest capacity the shared OSC join buffer keeps between sequences.
const OSC_JOIN_KEEP_BYTES: usize = 65_536;

/// Largest decoded OSC 52 payload: 4 MiB.
const OSC52_MAX_DECODED_BYTES: usize = 4 * 1024 * 1024;

/// Largest OSC 52 payload before decoding: 4 * ceil(4 MiB / 3).
const OSC52_MAX_ENCODED_BYTES: usize = 5_592_408;

/// Largest OSC 7 path after percent-decoding.
const OSC7_MAX_PATH_BYTES: usize = 4096;

pub(crate) fn terminal_programmed_title(parts: &[&[u8]]) -> Option<String> {
    let capacity = parts
        .iter()
        .fold(parts.len().saturating_sub(1), |total, part| {
            total.saturating_add(part.len())
        })
        .min(TERMINAL_TITLE_MAX_BYTES);
    let mut title = String::with_capacity(capacity);

    'parts: for (index, part) in parts.iter().enumerate() {
        if index != 0 {
            if title.len() == TERMINAL_TITLE_MAX_BYTES {
                break;
            }
            title.push(';');
        }
        let part = std::str::from_utf8(part).ok()?;
        for ch in part.chars() {
            if ch.is_control() {
                continue;
            }
            if title.len() + ch.len_utf8() > TERMINAL_TITLE_MAX_BYTES {
                break 'parts;
            }
            title.push(ch);
        }
    }

    let first_non_whitespace = title
        .char_indices()
        .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))?;
    let trimmed_len = title.trim_end().len();
    title.truncate(trimmed_len);
    if first_non_whitespace != 0 {
        title.drain(..first_non_whitespace);
    }
    (!title.is_empty()).then_some(title)
}

/// Runs `parse` with the join buffer taken from `term`, then clears the buffer,
/// shrinks it if a large payload grew it, and puts it back.
pub(crate) fn with_joined<T>(term: &mut Term, parse: impl FnOnce(&mut Vec<u8>) -> T) -> T {
    let mut joined = std::mem::take(&mut term.osc_join);
    let result = parse(&mut joined);
    joined.clear();
    if joined.capacity() > OSC_JOIN_KEEP_BYTES {
        joined.shrink_to(OSC_JOIN_KEEP_BYTES);
    }
    term.osc_join = joined;
    result
}

/// Replaces the contents of `joined` with `parts` glued back together by `;`,
/// the separator vte split them on.
fn join_into(joined: &mut Vec<u8>, parts: &[&[u8]]) {
    joined.clear();
    for (index, part) in parts.iter().enumerate() {
        if index != 0 {
            joined.push(b';');
        }
        joined.extend_from_slice(part);
    }
}

/// Text of an OSC 52 write, `OSC 52 ; Pc ; Pd`. Returns `None` for selections
/// other than clipboard (`c`) or `s`, for reads (`Pd` = `?`, which is not
/// base64 and so yields no reply), and for invalid, empty or oversized payloads.
pub(crate) fn parse_osc52_text(params: &[&[u8]], joined: &mut Vec<u8>) -> Option<String> {
    let selection = params.get(1)?;
    let selects_clipboard =
        selection.is_empty() || selection.contains(&b'c') || selection.contains(&b's');
    if !selects_clipboard {
        return None;
    }
    let data = params.get(2..)?;
    // Check the raw length before joining, so an oversized payload is never copied.
    let encoded_len =
        data.iter().map(|part| part.len()).sum::<usize>() + data.len().saturating_sub(1);
    if encoded_len > OSC52_MAX_ENCODED_BYTES {
        return None;
    }
    join_into(joined, data);
    let decoded = base64_decode(joined.as_slice())?;
    if decoded.is_empty() || decoded.len() > OSC52_MAX_DECODED_BYTES {
        return None;
    }
    String::from_utf8(decoded).ok()
}

/// Host and percent-decoded path of an OSC 7 `file://host/path` URI. Returns
/// `None` for other schemes, a missing path, invalid escapes, a NUL byte, a
/// non-UTF-8 host, or a path longer than [`OSC7_MAX_PATH_BYTES`].
pub(crate) fn parse_osc7_uri(params: &[&[u8]], joined: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    join_into(joined, params.get(1..)?);
    let rest = joined.strip_prefix(b"file://")?;
    // The path starts at the first `/`, so it always starts with `/` once decoded.
    let path_start = rest.iter().position(|&byte| byte == b'/')?;
    let host = std::str::from_utf8(&rest[..path_start]).ok()?.to_owned();
    let path = percent_decode(&rest[path_start..])?;
    if path.len() > OSC7_MAX_PATH_BYTES || path.contains(&0) {
        return None;
    }
    Some((host, path))
}

/// Queues one `OSC 4 ; index ; rgb:…` reply per `index ; ?` pair. Pairs that
/// set a colour, and indices that are not decimal numbers in 0-255, are skipped.
pub(crate) fn queue_palette_queries(term: &mut Term, params: &[&[u8]], bell_terminated: bool) {
    let Some(pairs) = params.get(1..) else {
        return;
    };
    for pair in pairs.chunks_exact(2) {
        if pair[1] != b"?" {
            continue;
        }
        let Some(index) = palette_index(pair[0]) else {
            continue;
        };
        let rgb = term.palette.rgb(TerminalColor::indexed(index));
        let msg = format!(
            "\x1B]4;{index};{}{}",
            rgb_spec(rgb),
            terminator(bell_terminated)
        );
        term.queue_reply(msg.as_bytes());
    }
}

/// Queues the reply to an `OSC 10/11/12 ; ?` query with the matching palette
/// colour. `selector` is `10`, `11` or `12`; setting a dynamic colour is a no-op.
pub(crate) fn queue_dynamic_color_query(
    term: &mut Term,
    selector: &[u8],
    params: &[&[u8]],
    bell_terminated: bool,
) {
    if params.len() < 2 || params[1] != b"?" {
        return;
    }
    let (ident, rgb) = match selector {
        b"10" => ("10", term.palette.foreground),
        b"11" => ("11", term.palette.background),
        _ => ("12", term.palette.cursor),
    };
    let msg = format!(
        "\x1B]{ident};{}{}",
        rgb_spec(rgb),
        terminator(bell_terminated)
    );
    term.queue_reply(msg.as_bytes());
}

/// `rgb:` spec with each 8-bit channel written twice in lowercase hex, as xterm does.
fn rgb_spec(rgb: Rgb) -> String {
    format!(
        "rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}",
        rgb.r, rgb.g, rgb.b
    )
}

/// Reply terminator: BEL when the query ended with BEL, otherwise ST.
fn terminator(bell_terminated: bool) -> &'static str {
    if bell_terminated { "\x07" } else { "\x1B\\" }
}

/// Decimal palette index in 0-255; any other text is `None`.
fn palette_index(text: &[u8]) -> Option<u8> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}
