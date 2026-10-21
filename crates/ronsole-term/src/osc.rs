//! OSC title helpers shared by the terminal parser.

pub const TERMINAL_TITLE_MAX_BYTES: usize = 256;

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
