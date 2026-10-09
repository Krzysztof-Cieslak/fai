//! Shared semantic literal decoding for analysis and native lowering.

/// Decodes a float lexeme into its exact IEEE-754 bit pattern.
#[must_use]
pub fn decode_float_literal(raw: &str) -> Option<u64> {
    raw.chars().filter(|&c| c != '_').collect::<String>().parse::<f64>().ok().map(f64::to_bits)
}

/// Decodes a quoted character, including a validated Unicode escape.
#[must_use]
pub fn decode_char_literal(raw: &str) -> Option<char> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut chars = inner.chars();
    let first = chars.next()?;
    let value = if first == '\\' { decode_escape(&mut chars)? } else { first };
    chars.next().is_none().then_some(value)
}

/// Decodes a quoted UTF-8 string; malformed escapes return None.
#[must_use]
pub fn decode_string_literal(raw: &str) -> Option<Vec<u8>> {
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = inner.chars();
    let mut out = String::with_capacity(inner.len());
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { decode_escape(&mut chars)? } else { c });
    }
    Some(out.into_bytes())
}

fn decode_escape(chars: &mut std::str::Chars<'_>) -> Option<char> {
    match chars.next()? {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        '0' => Some('\0'),
        '\\' => Some('\\'),
        '"' => Some('"'),
        '\'' => Some('\''),
        'u' => {
            if chars.next()? != '{' {
                return None;
            }
            let mut digits = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    return u32::from_str_radix(&digits, 16).ok().and_then(char::from_u32);
                }
                digits.push(c);
            }
            None
        }
        _ => None,
    }
}

/// Decodes a decimal, hexadecimal, octal, or binary integer with separators.
/// The magnitude must fit `u64`; the result is its signed 64-bit bit pattern.
/// A leading minus applies wrapping negation, matching an integer expression.
#[must_use]
pub fn decode_int_literal(raw: &str) -> Option<i64> {
    let negative = raw.starts_with('-');
    let magnitude = raw.strip_prefix('-').unwrap_or(raw);
    let (radix, digits) = match magnitude.as_bytes().get(..2) {
        Some([b'0', b'x' | b'X']) => (16u64, &magnitude.as_bytes()[2..]),
        Some([b'0', b'o' | b'O']) => (8u64, &magnitude.as_bytes()[2..]),
        Some([b'0', b'b' | b'B']) => (2u64, &magnitude.as_bytes()[2..]),
        _ => (10u64, magnitude.as_bytes()),
    };
    let mut value = 0u64;
    let mut any = false;
    for &byte in digits {
        if byte == b'_' {
            continue;
        }
        let digit = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            b'a'..=b'f' => u64::from(byte - b'a' + 10),
            b'A'..=b'F' => u64::from(byte - b'A' + 10),
            _ => return None,
        };
        if digit >= radix {
            return None;
        }
        value = value.checked_mul(radix)?.checked_add(digit)?;
        any = true;
    }
    let value = value as i64;
    any.then_some(if negative { value.wrapping_neg() } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_width_decimal_is_a_bit_pattern() {
        assert_eq!(decode_int_literal("18446744073709551615"), Some(-1));
    }
    #[test]
    fn full_width_hex_is_a_bit_pattern() {
        assert_eq!(decode_int_literal("0xFFFF_FFFF_FFFF_FFFF"), Some(-1));
    }
    #[test]
    fn full_width_octal_is_a_bit_pattern() {
        assert_eq!(decode_int_literal("0o1777777777777777777777"), Some(-1));
    }
    #[test]
    fn full_width_binary_is_a_bit_pattern() {
        assert_eq!(decode_int_literal(&format!("0b{}", "1".repeat(64))), Some(-1));
    }
    #[test]
    fn decimal_overflow_is_rejected() {
        assert_eq!(decode_int_literal("18446744073709551616"), None);
    }
    #[test]
    fn hexadecimal_overflow_is_rejected() {
        assert_eq!(decode_int_literal("0x10000000000000000"), None);
    }
    #[test]
    fn octal_overflow_is_rejected() {
        assert_eq!(decode_int_literal("0o2000000000000000000000"), None);
    }
    #[test]
    fn binary_overflow_is_rejected() {
        assert_eq!(decode_int_literal(&format!("0b1{}", "0".repeat(64))), None);
    }
    #[test]
    fn negative_hex_uses_the_radix_after_the_sign() {
        assert_eq!(decode_int_literal("-0x1"), Some(-1));
    }
    #[test]
    fn negative_octal_uses_the_radix_after_the_sign() {
        assert_eq!(decode_int_literal("-0o1"), Some(-1));
    }
    #[test]
    fn negative_binary_uses_the_radix_after_the_sign() {
        assert_eq!(decode_int_literal("-0b1"), Some(-1));
    }
    #[test]
    fn signed_minimum_is_representable() {
        assert_eq!(decode_int_literal("-9223372036854775808"), Some(i64::MIN));
    }
    #[test]
    fn negative_full_width_magnitude_wraps_like_subtraction() {
        assert_eq!(decode_int_literal("-18446744073709551615"), Some(1));
    }
    #[test]
    fn very_long_magnitude_is_rejected() {
        assert_eq!(decode_int_literal(&"9".repeat(10000)), None);
    }
    #[test]
    fn empty_digit_sequence_is_rejected() {
        assert_eq!(decode_int_literal("-0x___"), None);
    }

    #[test]
    fn lexer_reports_the_complete_overflowing_literal_after_unicode() {
        let prefix = "// é😀\n";
        let literal = "18446744073709551617";
        let source = format!("{prefix}{literal}");
        let result = crate::lex(fai_span::SourceId::new(7), &source);
        assert_eq!(result.diagnostics.len(), 1);
        let error = &result.diagnostics[0];
        assert_eq!(error.code, crate::INVALID_NUMBER);
        assert_eq!(error.message, "integer literal magnitude exceeds 64 bits");
        assert_eq!(error.primary.start().raw() as usize, prefix.len());
        assert_eq!(error.primary.end().raw() as usize, source.len());
    }

    #[test]
    fn overflow_recovery_preserves_later_tokens_and_reports_each_error() {
        let source = "18446744073709551616 0x10000000000000000 let valid = 1";
        let result = crate::lex(fai_span::SourceId::new(0), source);
        assert_eq!(result.diagnostics.len(), 2);
        assert!(result.tokens.iter().any(|token| token.kind == crate::TokenKind::Let));
        assert_eq!(result.tokens[result.tokens.len() - 2].kind, crate::TokenKind::Int);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn every_u64_bit_pattern_decodes_in_every_radix(bits in any::<u64>(), radix in 0u8..4, negative in any::<bool>()) {
                let raw = match radix {
                    0 => bits.to_string(),
                    1 => format!("0x{bits:x}"),
                    2 => format!("0o{bits:o}"),
                    _ => format!("0b{bits:b}"),
                };
                let raw = if negative { format!("-{raw}") } else { raw };
                let expected = if negative { (bits as i64).wrapping_neg() } else { bits as i64 };
                prop_assert_eq!(decode_int_literal(&raw), Some(expected));
                prop_assert!(crate::lex(fai_span::SourceId::new(0), &raw).diagnostics.is_empty());
            }
        }
    }
}
