//! Decoding literal lexemes (kept raw by the parser) into Core values.

pub use fai_syntax::decode_int_literal as decode_int;

/// Decodes a float lexeme (`3.14`, `1_000.0`, `1e9`) into its IEEE-754 bits.
#[must_use]
pub fn decode_float(raw: &str) -> u64 {
    fai_syntax::decode_float_literal(raw).unwrap_or(0)
}

/// Decodes a char lexeme (`'a'`, `'\n'`, `'\u{1F600}'`, including its surrounding
/// quotes and escape) into its Unicode scalar value. Escapes were validated by
/// the lexer. Returns `None` only for a malformed lexeme (which the lexer rules
/// out), so callers fall back to a default.
#[must_use]
pub fn decode_char(raw: &str) -> Option<char> {
    fai_syntax::decode_char_literal(raw)
}

/// Decodes a string lexeme (including its surrounding quotes and escapes) into
/// its UTF-8 bytes. Escapes were validated by the lexer.
#[must_use]
pub fn decode_string(raw: &str) -> Vec<u8> {
    fai_syntax::decode_string_literal(raw).unwrap_or_default()
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

    #[test]
    fn decodes_multibyte_char() {
        assert_eq!(decode_char("'é'"), Some('é'));
        assert_eq!(decode_char("'😀'"), Some('😀'));
    }

    #[test]
    fn shared_escape_helper_keeps_strings_working() {
        // The char decoder shares `decode_escape` with the string decoder; this
        // guards that refactor (the string path must be unchanged).
        assert_eq!(decode_string("\"a\\n\\t\\\\b\""), b"a\n\t\\b");
        assert_eq!(decode_string("\"\\u{1F600}\""), "\u{1F600}".as_bytes());
    }
}
