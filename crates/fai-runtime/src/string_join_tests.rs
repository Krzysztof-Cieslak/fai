//! Joined strings use one output allocation and preserve borrowed input storage.

use super::*;
use crate::tests::lock;

#[track_caller]
fn check(parts: &[&str], separator: &str, array: bool) {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let values: Vec<_> = parts.iter().map(|part| make_str(part)).collect();
    let collection = if array { array_of_strings(&values) } else { list_of_strings(&values) };
    let sep = make_str(separator);
    reset_allocations();
    let result = if array {
        fai_array_join_borrowed(sep, collection)
    } else {
        fai_string_join_borrowed(sep, collection)
    };
    assert_eq!(allocations(), 1, "only the final string buffer is allocated");
    assert_eq!(read_string(result), parts.join(separator).as_bytes());
    for (value, text) in values.iter().zip(parts) {
        assert_eq!(read_string(*value), text.as_bytes(), "borrowed parts stay intact");
    }
    assert_eq!(read_string(sep), separator.as_bytes());
    fai_drop(result);
    fai_drop(sep);
    fai_drop(collection);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn empty_list_joins_to_empty_string() {
    check(&[], "unused", false);
}

#[test]
fn single_list_element_omits_the_separator() {
    check(&["λ🙂"], "unused", false);
}

#[test]
fn empty_list_elements_still_have_separators() {
    check(&["", "", ""], "λ🙂", false);
}

#[test]
fn empty_array_joins_to_empty_string() {
    check(&[], "unused", true);
}

#[test]
fn single_array_element_omits_the_separator() {
    check(&["λ🙂"], "unused", true);
}

#[test]
fn empty_array_separator_concatenates_all_bytes() {
    check(&["λ", "", "🙂\0", "end"], "", true);
}

#[test]
fn list_join_borrows_sliced_parts_and_separator() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let text = "λ🙂".repeat(32);
    let base = make_str(&text);
    let part = fai_string_take(imm_int(32), fai_dup(base));
    let sep = fai_string_drop(imm_int(32), fai_dup(base));
    let list = list_of_strings(&[part, fai_dup(part)]);
    let expected = format!("{}{}{}", "λ🙂".repeat(16), "λ🙂".repeat(16), "λ🙂".repeat(16));
    reset_allocations();
    let result = fai_string_join_borrowed(sep, list);
    assert_eq!(read_string(result), expected.as_bytes());
    assert_eq!(allocations(), 1);
    assert_eq!(read_string(base), text.as_bytes());
    fai_drop(base);
    fai_drop(list);
    fai_drop(sep);
    assert_eq!(read_string(result), expected.as_bytes(), "output owns its bytes");
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn joined_length_rejects_separator_overflow() {
    assert_eq!(joined_string_len([1, 1].into_iter(), usize::MAX), None);
}

#[test]
fn joined_length_rejects_payload_overflow() {
    assert_eq!(joined_string_len([usize::MAX, 1].into_iter(), 0), None);
}

#[test]
fn joined_length_rejects_allocation_layout_overflow() {
    assert_eq!(joined_string_len([isize::MAX as usize].into_iter(), 0), None);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(prop_oneof![Just('a'), Just('λ'), Just('🙂'), Just('\0')], 0..24)
            .prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]
        #[test]
        fn joins_match_utf8_bytes(parts in prop::collection::vec(text(), 0..24), sep in text(), array in any::<bool>()) {
            let parts: Vec<_> = parts.iter().map(String::as_str).collect();
            check(&parts, &sep, array);
        }
    }
}
