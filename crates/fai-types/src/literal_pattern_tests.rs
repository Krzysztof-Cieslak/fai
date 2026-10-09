//! Usefulness keys are decoded values, while diagnostics retain source spelling.

use fai_db::{Db, Diag};

#[track_caller]
fn compare_patterns(first: &str, second: &str, redundant: bool) {
    let source = format!(
        "module M\n// é🌍\nlet select x =\n  match x with\n  | {first} -> 1\n  | {second} -> 2\n  | _ -> 3\n"
    );
    let mut db = fai_db::FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.clone());
    let errors = crate::check_file::accumulated::<Diag>(&db, db.source_file(id).unwrap());
    let error = errors.iter().find(|error| error.0.code == crate::UNREACHABLE_ARM);
    assert_eq!(error.is_some(), redundant, "{errors:?}");
    if let Some(error) = error {
        let span = error.0.primary.range();
        assert_eq!(
            &source[span.start().to_usize()..span.end().to_usize()],
            format!("| {second} -> 2")
        );
        assert_eq!(error.0.message, "this match arm is unreachable");
    }
}

#[test]
fn hexadecimal_and_decimal_patterns_coincide() {
    compare_patterns("255", "0xff", true);
}
#[test]
fn octal_and_binary_patterns_coincide() {
    compare_patterns("0o17", "0b1111", true);
}
#[test]
fn separated_digits_coincide() {
    compare_patterns("1000", "1_000", true);
}
#[test]
fn full_width_patterns_use_wrapping_bits() {
    compare_patterns("-1", "0xffffffffffffffff", true);
}
#[test]
fn negative_radix_patterns_coincide() {
    compare_patterns("-255", "-0xff", true);
}
#[test]
fn float_exponent_spellings_coincide() {
    compare_patterns("1.0", "1e0", true);
}
#[test]
fn signed_float_zeros_remain_distinct() {
    compare_patterns("0.0", "-0.0", false);
}
#[test]
fn string_unicode_escapes_coincide() {
    compare_patterns("\"é\"", "\"\\u{e9}\"", true);
}
#[test]
fn char_unicode_escapes_coincide() {
    compare_patterns("'a'", "'\\u{61}'", true);
}
#[test]
fn nested_literals_are_normalized() {
    compare_patterns("({ value = 255 }, 'a')", "({ value = 0xff }, '\\u{61}')", true);
}
#[test]
fn invalid_integer_does_not_alias_zero() {
    compare_patterns("18446744073709551616", "0", false);
}
