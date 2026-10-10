//! ASCII evidence across construction, mutation, views and structural readers.

use super::*;

fn ascii(value: Value) -> bool {
    // SAFETY: callers pass a live String constructed by this test module.
    unsafe { string_is_ascii(value) }
}

fn length(value: Value) -> i64 {
    read_int(fai_string_length_borrowed(value))
}

#[test]
fn construction_records_ascii_without_confusing_unicode() {
    let _guard = tests::lock();
    let a = make_string(b"a\0b");
    let b = make_string("é😀".as_bytes());
    assert!(ascii(a));
    assert!(!ascii(b));
    assert_eq!((length(a), length(b)), (3, 2));
    fai_drop(a);
    fai_drop(b);
}

#[test]
fn unique_ascii_append_preserves_the_fact() {
    let _guard = tests::lock();
    let a = make_string(b"a");
    let result = fai_string_concat(a, make_string(b"b"));
    assert_eq!(result, a);
    assert!(ascii(result));
    assert_eq!(length(result), 2);
    fai_drop(result);
}

#[test]
fn unique_unicode_append_drops_the_ascii_fact() {
    let _guard = tests::lock();
    let a = make_string(b"a");
    let result = fai_string_concat(a, make_string("λ".as_bytes()));
    assert_eq!(result, a);
    assert!(!ascii(result));
    assert_eq!(length(result), 2);
    fai_drop(result);
}

#[test]
fn reused_right_buffer_drops_the_ascii_fact_for_a_unicode_prefix() {
    let _guard = tests::lock();
    let left = make_string("λ".as_bytes());
    let right = make_string(b"b");
    let result = fai_string_concat(fai_dup(left), right);
    assert_eq!(result, right);
    assert!(!ascii(result));
    assert_eq!(length(result), 2);
    fai_drop(left);
    fai_drop(result);
}

#[test]
fn growth_preserves_ascii_evidence() {
    let _guard = tests::lock();
    let result = fai_string_concat(make_string(b"123456789"), make_string(b"abcdefghijklmnop"));
    assert!(ascii(result));
    assert_eq!(length(result), 25);
    fai_drop(result);
}

#[test]
fn a_shared_ascii_value_keeps_its_fact_after_a_unicode_append() {
    let _guard = tests::lock();
    let original = make_string(b"original");
    let result = fai_string_concat(fai_dup(original), make_string("λ".as_bytes()));
    assert!(ascii(original));
    assert_eq!(length(original), 8);
    assert!(!ascii(result));
    assert_eq!(length(result), 9);
    fai_drop(result);
    fai_drop(original);
}

#[test]
fn views_inherit_the_original_base_fact_after_copy_on_share() {
    let _guard = tests::lock();
    let base = make_string(&[b'a'; 128]);
    let view = fai_string_take(imm_int(64), fai_dup(base));
    assert!(ascii(view));
    let changed = fai_string_concat(base, make_string("λ".as_bytes()));
    assert!(ascii(view));
    assert_eq!((length(view), length(changed)), (64, 129));
    assert!(!ascii(changed));
    fai_drop(view);
    fai_drop(changed);
}

#[test]
fn joined_ascii_and_unicode_values_get_sound_facts() {
    let _guard = tests::lock();
    let ascii_parts = cons_list(&[make_string(b"a"), make_string(b"b")]);
    let ascii_joined = fai_string_join(make_string(b","), ascii_parts);
    let unicode_parts = cons_list(&[make_string(b"a"), make_string("λ".as_bytes())]);
    let unicode_joined = fai_string_join(make_string(b","), unicode_parts);
    assert!(ascii(ascii_joined));
    assert!(!ascii(unicode_joined));
    assert_eq!((length(ascii_joined), length(unicode_joined)), (3, 3));
    fai_drop(ascii_joined);
    fai_drop(unicode_joined);
}

#[test]
fn conservative_legacy_strings_compare_and_hash_by_contents() {
    let _guard = tests::lock();
    let pointer = alloc_string(3, 3);
    // SAFETY: the fresh inline String reserves exactly three live bytes.
    unsafe { std::ptr::copy_nonoverlapping(b"abc".as_ptr(), pointer.add(STRING_BYTES_OFFSET), 3) };
    let legacy = from_obj(pointer);
    let known = make_string(b"abc");
    assert!(!ascii(legacy));
    assert!(ascii(known));
    assert_eq!(length(legacy), 3);
    assert_eq!(fai_equal_borrowed(legacy, known), 3);
    assert_eq!(fai_compare_borrowed(legacy, known), imm_int(0));
    assert_eq!(fai_hash_borrowed(legacy), fai_hash_borrowed(known));
    fai_drop(legacy);
    fai_drop(known);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn append_and_view_evidence_matches_utf8_contents(
            a in prop_oneof!["[a-z]{0,80}", ".{0,80}"],
            b in prop_oneof!["[a-z]{0,80}", ".{0,80}"],
            count in 0usize..160,
            shared in any::<bool>(),
        ) {
            let _guard = tests::lock();
            let baseline = (live_count(), live_bytes());
            let left = make_string(a.as_bytes());
            let retained = shared.then(|| fai_dup(left));
            let joined = fai_string_concat(left, make_string(b.as_bytes()));
            let expected = format!("{a}{b}");
            prop_assert!(!ascii(joined) || expected.is_ascii());
            prop_assert_eq!(length(joined), expected.chars().count() as i64);
            let view = fai_string_take(imm_int(count as i64), fai_dup(joined));
            let prefix: String = expected.chars().take(count).collect();
            prop_assert!(!ascii(view) || prefix.is_ascii());
            prop_assert_eq!(read_string(view), prefix.as_bytes());
            prop_assert_eq!(length(view), prefix.chars().count() as i64);
            if let Some(original) = retained {
                prop_assert_eq!(read_string(original), a.as_bytes());
                fai_drop(original);
            }
            fai_drop(view);
            fai_drop(joined);
            prop_assert_eq!((live_count(), live_bytes()), baseline);
        }
    }
}
