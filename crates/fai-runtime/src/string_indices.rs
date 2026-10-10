//! Full-width string indices are owned arguments, including clamped indices.

use super::*;

fn checked(operation: impl FnOnce(Value) -> Value, expected: &[u8]) {
    let _guard = tests::lock();
    let baseline = (live_count(), live_bytes());
    let result = operation(make_string(b"abc"));
    assert_eq!(read_string(result), expected);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn take_releases_a_maximum_index() {
    checked(|text| fai_string_take(make_int(i64::MAX), text), b"abc");
}

#[test]
fn take_releases_a_minimum_index() {
    checked(|text| fai_string_take(make_int(i64::MIN), text), b"");
}

#[test]
fn drop_releases_a_maximum_index() {
    checked(|text| fai_string_drop(make_int(i64::MAX), text), b"");
}

#[test]
fn drop_releases_a_minimum_index() {
    checked(|text| fai_string_drop(make_int(i64::MIN), text), b"abc");
}

#[test]
fn substring_releases_a_maximum_length() {
    checked(|text| fai_string_substring(imm_int(0), make_int(i64::MAX), text), b"abc");
}

#[test]
fn substring_releases_both_references_to_a_shared_index_box() {
    checked(
        |text| {
            let index = make_int(i64::MAX);
            fai_string_substring(fai_dup(index), index, text)
        },
        b"",
    );
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_owned_indices_are_released(count in any::<i64>()) {
            let _guard = tests::lock();
            let baseline = (live_count(), live_bytes());
            let text = "aé😀z";
            let prefix = fai_string_take(make_int(count), make_string(text.as_bytes()));
            let suffix = fai_string_drop(make_int(count), make_string(text.as_bytes()));
            let count = usize::try_from(count.max(0)).unwrap_or(usize::MAX);
            let expected_prefix: String = text.chars().take(count).collect();
            let expected_suffix: String = text.chars().skip(count).collect();
            prop_assert_eq!(read_string(prefix), expected_prefix.as_bytes());
            prop_assert_eq!(read_string(suffix), expected_suffix.as_bytes());
            fai_drop(prefix);
            fai_drop(suffix);
            prop_assert_eq!((live_count(), live_bytes()), baseline);
        }
    }
}
