//! Property-based tests for literal decoding and lowering totality.

use proptest::prelude::*;

use crate::{decode_int, decode_string};

/// Escapes `s` into a Fai string lexeme that [`decode_string`] must invert.
fn escape(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

proptest! {
    #[test]
    fn decimal_literals_round_trip(n in any::<i64>()) {
        prop_assert_eq!(decode_int(&n.to_string()), Some(n));
    }

    #[test]
    fn hex_literals_round_trip(n in any::<u64>()) {
        prop_assert_eq!(decode_int(&format!("0x{n:x}")), Some(n as i64));
        prop_assert_eq!(decode_int(&format!("0X{n:X}")), Some(n as i64));
    }

    #[test]
    fn octal_and_binary_round_trip(n in any::<u32>()) {
        prop_assert_eq!(decode_int(&format!("0o{n:o}")), Some(i64::from(n)));
        prop_assert_eq!(decode_int(&format!("0b{n:b}")), Some(i64::from(n)));
    }

    #[test]
    fn underscores_are_ignored(n in 0i64..1_000_000_000) {
        let digits = n.to_string();
        let spaced: String = digits.chars().flat_map(|c| [c, '_']).collect();
        prop_assert_eq!(decode_int(&spaced), Some(n));
    }

    #[test]
    fn string_literals_round_trip(s in ".*") {
        prop_assert_eq!(decode_string(&escape(&s)), s.into_bytes());
    }

    #[test]
    fn string_decoding_never_panics(raw in ".*") {
        // Even malformed lexemes (no surrounding quotes, dangling escapes) must
        // not panic.
        let _ = decode_string(&raw);
        let _ = decode_string(&format!("\"{raw}"));
    }
}
