//! Decoding literal lexemes (kept raw by the parser) into Core values.

/// Decodes an integer lexeme (`42`, `0xFF`, `0o17`, `0b1010`, `1_000`) into an
/// `i64`. Underscores are separators; the value wraps to the 64-bit pattern when
/// it exceeds the signed range (e.g. a full-width hex literal).
#[must_use]
pub fn decode_int(raw: &str) -> Option<i64> {
    let cleaned: String = raw.chars().filter(|&c| c != '_').collect();
    let (radix, digits) = if let Some(rest) = strip_prefix_ci(&cleaned, "0x") {
        (16, rest)
    } else if let Some(rest) = strip_prefix_ci(&cleaned, "0o") {
        (8, rest)
    } else if let Some(rest) = strip_prefix_ci(&cleaned, "0b") {
        (2, rest)
    } else {
        (10, cleaned.as_str())
    };
    if let Ok(n) = i64::from_str_radix(digits, radix) {
        return Some(n);
    }
    // A literal that fills the top bit (e.g. 0xFFFF_FFFF_FFFF_FFFF) parses as
    // unsigned; reinterpret its bit pattern as i64.
    u64::from_str_radix(digits, radix).ok().map(|u| u as i64)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let lower = s.get(..prefix.len())?.to_ascii_lowercase();
    if lower == prefix { s.get(prefix.len()..) } else { None }
}

/// Decodes a string lexeme (including its surrounding quotes and escapes) into
/// its UTF-8 bytes. Escapes were validated by the lexer.
#[must_use]
pub fn decode_string(raw: &str) -> Vec<u8> {
    let inner = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(raw);
    let mut out = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            push_char(&mut out, c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('r') => out.push(b'\r'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('"') => out.push(b'"'),
            Some('\'') => out.push(b'\''),
            Some('u') => decode_unicode_escape(&mut chars, &mut out),
            Some(other) => push_char(&mut out, other),
            None => out.push(b'\\'),
        }
    }
    out
}

/// Decodes a `\u{XXXX}` escape (the lexer guarantees the braces and hex digits).
fn decode_unicode_escape(chars: &mut std::str::Chars<'_>, out: &mut Vec<u8>) {
    if chars.next() != Some('{') {
        return;
    }
    let mut hex = String::new();
    for c in chars.by_ref() {
        if c == '}' {
            break;
        }
        hex.push(c);
    }
    if let Ok(code) = u32::from_str_radix(&hex, 16)
        && let Some(ch) = char::from_u32(code)
    {
        push_char(out, ch);
    }
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_integers() {
        assert_eq!(decode_int("42"), Some(42));
        assert_eq!(decode_int("1_000"), Some(1000));
        assert_eq!(decode_int("0xFF"), Some(255));
        assert_eq!(decode_int("0o17"), Some(15));
        assert_eq!(decode_int("0b1010"), Some(10));
        assert_eq!(decode_int("0xFFFFFFFFFFFFFFFF"), Some(-1));
    }

    #[test]
    fn decodes_strings_and_escapes() {
        assert_eq!(decode_string("\"hi\""), b"hi");
        assert_eq!(decode_string("\"a\\nb\""), b"a\nb");
        assert_eq!(decode_string("\"\\t\\\\\\\"\""), b"\t\\\"");
        assert_eq!(decode_string("\"\\u{41}\""), b"A");
    }
}
