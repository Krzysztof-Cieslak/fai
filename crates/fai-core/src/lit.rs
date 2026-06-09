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

/// Decodes a float lexeme (`3.14`, `1_000.0`, `1e9`) into its IEEE-754 bits.
#[must_use]
pub fn decode_float(raw: &str) -> u64 {
    let cleaned: String = raw.chars().filter(|&c| c != '_').collect();
    cleaned.parse::<f64>().unwrap_or(0.0).to_bits()
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let lower = s.get(..prefix.len())?.to_ascii_lowercase();
    if lower == prefix { s.get(prefix.len()..) } else { None }
}

/// Decodes a char lexeme (`'a'`, `'\n'`, `'\u{1F600}'`, including its surrounding
/// quotes and escape) into its Unicode scalar value. Escapes were validated by
/// the lexer. Returns `None` only for a malformed lexeme (which the lexer rules
/// out), so callers fall back to a default.
#[must_use]
pub fn decode_char(raw: &str) -> Option<char> {
    let inner = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(raw);
    let mut chars = inner.chars();
    let first = chars.next()?;
    if first != '\\' {
        return Some(first);
    }
    decode_escape(&mut chars)
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
        match decode_escape(&mut chars) {
            Some(decoded) => push_char(&mut out, decoded),
            None => out.push(b'\\'),
        }
    }
    out
}

/// Decodes one escape sequence (the text after the backslash) into a `char`,
/// shared by string and char literals. The lexer guarantees the escape is
/// well-formed; an unrecognized escape yields its trailing character verbatim.
fn decode_escape(chars: &mut std::str::Chars<'_>) -> Option<char> {
    match chars.next()? {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        '0' => Some('\0'),
        '\\' => Some('\\'),
        '"' => Some('"'),
        '\'' => Some('\''),
        'u' => decode_unicode_escape(chars),
        other => Some(other),
    }
}

/// Decodes a `\u{XXXX}` escape (the lexer guarantees the braces and hex digits).
fn decode_unicode_escape(chars: &mut std::str::Chars<'_>) -> Option<char> {
    if chars.next() != Some('{') {
        return None;
    }
    let mut hex = String::new();
    for c in chars.by_ref() {
        if c == '}' {
            break;
        }
        hex.push(c);
    }
    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
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

    #[test]
    fn decodes_plain_char() {
        assert_eq!(decode_char("'a'"), Some('a'));
        assert_eq!(decode_char("'F'"), Some('F'));
        assert_eq!(decode_char("' '"), Some(' '));
    }

    #[test]
    fn decodes_char_escapes() {
        assert_eq!(decode_char("'\\n'"), Some('\n'));
        assert_eq!(decode_char("'\\t'"), Some('\t'));
        assert_eq!(decode_char("'\\r'"), Some('\r'));
        assert_eq!(decode_char("'\\0'"), Some('\0'));
        assert_eq!(decode_char("'\\\\'"), Some('\\'));
        assert_eq!(decode_char("'\\''"), Some('\''));
    }

    #[test]
    fn decodes_char_unicode_escape() {
        assert_eq!(decode_char("'\\u{41}'"), Some('A'));
        assert_eq!(decode_char("'\\u{1F600}'"), Some('\u{1F600}'));
    }
}
