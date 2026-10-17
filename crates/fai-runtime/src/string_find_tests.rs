//! Substring search borrows inputs and reports Unicode-scalar coordinates.
use super::*;
use crate::tests::lock;

#[test]
fn finding_a_unicode_separator_uses_scalar_coordinates() {
    let _guard = lock();
    let base = (live_count(), live_bytes());
    let source = make_str("é😀:a:b");
    let needle = make_str(":");
    reset_allocations();
    assert_eq!(read_int(fai_string_find_borrowed(needle, source)), 2);
    assert_eq!(allocations(), 0);
    assert_eq!(read_string(source), "é😀:a:b".as_bytes());
    fai_drop(source);
    fai_drop(needle);
    assert_eq!((live_count(), live_bytes()), base);
}

#[test]
fn absent_separator_does_not_build_pieces() {
    let _guard = lock();
    let source = make_str("unchanged");
    let needle = make_str(":");
    reset_allocations();
    assert_eq!(read_int(fai_string_find_borrowed(needle, source)), -1);
    assert_eq!(allocations(), 0);
    fai_drop(source);
    fai_drop(needle);
}

#[test]
fn empty_separator_matches_an_empty_source() {
    let _guard = lock();
    assert_eq!(read_int(fai_string_find(make_str(""), make_str(""))), 0);
}

#[test]
fn search_coordinates_are_relative_to_a_view() {
    let _guard = lock();
    let source = make_str(&format!("{}:tail", "é".repeat(64)));
    let view = fai_string_drop(make_int(32), source);
    assert_eq!(read_int(fai_string_find(make_str(":"), view)), 32);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn search_matches_standard_unicode_coordinates(source in ".{0,128}", needle in ".{0,12}") {
            let _guard = lock();
            let base = (live_count(), live_bytes());
            let expected = source.find(&needle).map_or(-1, |i| source[..i].chars().count() as i64);
            let result = read_int(fai_string_find(make_str(&needle), make_str(&source)));
            prop_assert_eq!(result, expected);
            prop_assert_eq!((live_count(), live_bytes()), base);
        }
    }
}
