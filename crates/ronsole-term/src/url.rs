//! Decoders for OSC payloads: RFC 4648 base64 (OSC 52) and percent-encoding (OSC 7).

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
