//! Immutable empty buffers and unchanged text avoid replacement allocations.
use super::*;
use crate::tests::lock;

#[test]
fn formatting_minimum_integer_fills_the_signed_buffer() {
    let _guard = lock();
    let result = fai_int_to_string(make_int(i64::MIN));
    assert_eq!(read_string(result), b"-9223372036854775808");
    fai_drop(result);
}

#[test]
fn formatting_maximum_integer_omits_the_sign() {
    let _guard = lock();
    let result = fai_int_to_string(make_int(i64::MAX));
    assert_eq!(read_string(result), b"9223372036854775807");
    fai_drop(result);
}

#[test]
fn formatting_zero_keeps_one_digit() {
    let _guard = lock();
    let result = fai_int_to_string(make_int(0));
    assert_eq!(read_string(result), b"0");
    fai_drop(result);
}

#[test]
fn empty_bytes_share_immortal_storage() {
    let _guard = lock();
    let base = (live_count(), live_bytes());
    reset_allocations();
    let first = make_bytes(&[]);
    let second = make_bytes(&[]);
    assert_eq!(first, second);
    assert_eq!(allocations(), 0);
    fai_mark_shared(first);
    fai_drop(fai_dup(first));
    fai_drop(first);
    fai_drop(second);
    // SAFETY: the static empty buffer remains live after arbitrary owned drops.
    assert_eq!(unsafe { rc_load(as_obj(first)) }, IMMORTAL_RC);
    assert_eq!((live_count(), live_bytes()), base);
}

#[test]
fn appending_to_the_empty_singleton_keeps_it_empty() {
    let _guard = lock();
    let empty = make_bytes(&[]);
    let result = fai_bytes_concat(empty, make_bytes(&[0, 255]));
    // SAFETY: both the owned result and immortal empty value are live Bytes.
    unsafe {
        assert_eq!(bytes_bytes(result), [0, 255]);
        assert!(bytes_bytes(empty).is_empty());
    }
    fai_drop(result);
}

#[test]
fn unchanged_lowercase_text_reuses_storage() {
    let _guard = lock();
    let original = make_str("content-type\0");
    reset_allocations();
    let result = fai_to_lower_borrowed(original);
    assert_eq!(result, original);
    assert_eq!(allocations(), 0);
    let changed = fai_string_concat(result, make_str("!"));
    assert_eq!(read_string(original), b"content-type\0");
    fai_drop(original);
    fai_drop(changed);
}

#[test]
fn unicode_lowercase_keeps_contextual_and_expanding_mapping() {
    let _guard = lock();
    let result = fai_to_lower(make_str("İ ΟΣ 😀"));
    assert_eq!(read_string(result), "i̇ ος 😀".as_bytes());
    fai_drop(result);
}

#[test]
fn unchanged_unicode_trim_reuses_storage() {
    let _guard = lock();
    let original = make_str("café😀");
    reset_allocations();
    let result = fai_trim_borrowed(original);
    assert_eq!(result, original);
    assert_eq!(allocations(), 0);
    fai_drop(original);
    fai_drop(result);
}

#[test]
fn trimming_unicode_whitespace_preserves_the_inner_text() {
    let _guard = lock();
    let result = fai_trim(make_str("\u{2003}café😀\u{a0}"));
    assert_eq!(read_string(result), "café😀".as_bytes());
    fai_drop(result);
}

#[test]
fn an_unsplit_string_keeps_its_original_buffer() {
    let _guard = lock();
    let original = make_str("unchanged");
    let separator = make_str(":");
    reset_allocations();
    let result = fai_string_split_borrowed(separator, original);
    assert_eq!(allocations(), 1, "only the list cell is needed");
    // SAFETY: result is the nonempty split list, with a String head.
    assert_eq!(unsafe { read_i64(as_obj(result), data_offset(as_obj(result))) }, original);
    fai_drop(original);
    fai_drop(separator);
    fai_drop(result);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn integer_rendering_matches_rust(value in any::<i64>()) {
            let _guard = lock();
            let base = (live_count(), live_bytes());
            let output = fai_int_to_string(make_int(value));
            let actual = read_string(output).to_vec();
            fai_drop(output);
            prop_assert_eq!(actual, value.to_string().into_bytes());
            prop_assert_eq!((live_count(), live_bytes()), base);
        }

        #[test]
        fn text_transforms_match_unicode_rules(value in ".{0,256}") {
            let _guard = lock();
            let base = (live_count(), live_bytes());
            let text = make_str(&value);
            let lower = fai_to_lower_borrowed(text);
            let trimmed = fai_trim(text);
            let actual_lower = read_string(lower).to_vec();
            let actual_trim = read_string(trimmed).to_vec();
            fai_drop(lower);
            fai_drop(trimmed);
            prop_assert_eq!(actual_lower, value.to_lowercase().into_bytes());
            prop_assert_eq!(actual_trim, value.trim().as_bytes());
            prop_assert_eq!((live_count(), live_bytes()), base);
        }
    }
}
