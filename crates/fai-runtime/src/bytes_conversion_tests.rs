//! Ownership-preserving conversions between binary and UTF-8 inline buffers.
use super::*;
use crate::tests::lock;

#[test]
fn empty_inline_buffers_transfer_both_ways() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let text = make_str("");
    reset_allocations();
    let bytes = fai_bytes_from_string(text);
    let restored = fai_bytes_to_string(bytes);
    assert_eq!(restored, text);
    assert_eq!(allocations(), 0);
    fai_drop(restored);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn unique_string_transfers_its_storage_to_bytes() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let text = make_str("A\0é😀");
    reset_allocations();
    let bytes = fai_bytes_from_string(text);
    assert_eq!(bytes, text);
    assert_eq!(allocations(), 0);
    // SAFETY: the result is now an owned binary buffer.
    assert_eq!(unsafe { bytes_bytes(bytes) }, "A\0é😀".as_bytes());
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn unique_bytes_transfer_and_set_the_non_ascii_string_layout() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes("é😀".as_bytes());
    reset_allocations();
    let text = fai_bytes_to_string(bytes);
    assert_eq!(text, bytes);
    assert_eq!(allocations(), 0);
    assert_eq!(read_int(fai_string_length_borrowed(text)), 2);
    // SAFETY: the result owns valid UTF-8 and is not ASCII.
    assert!(!unsafe { string_is_ascii(text) });
    fai_drop(text);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn repeated_ascii_round_trips_need_no_new_buffers() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let original = make_str("unchanged");
    let mut text = original;
    reset_allocations();
    for _ in 0..10_000 {
        text = fai_bytes_to_string(fai_bytes_from_string(text));
    }
    assert_eq!(text, original);
    assert_eq!(allocations(), 0);
    // SAFETY: every round trip preserves valid ASCII bytes.
    assert!(unsafe { string_is_ascii(text) });
    assert_eq!(read_int(fai_string_length_borrowed(text)), 9);
    fai_drop(text);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_string_and_bytes_keep_distinct_kinds() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let text = make_str("same bytes");
    let bytes = fai_bytes_from_string(fai_dup(text));
    assert_ne!(text, bytes);
    assert_eq!(fai_equal(fai_dup(text), fai_dup(bytes)), from_bool(false));
    let restored = fai_bytes_to_string(fai_dup(bytes));
    assert_ne!(restored, bytes);
    assert_eq!(fai_equal(fai_dup(text), restored), from_bool(true));
    // SAFETY: both retained values keep their original representations.
    unsafe {
        assert_eq!(object_kind(as_obj(text)), KIND_STRING);
        assert_eq!(object_kind(as_obj(bytes)), KIND_BYTES);
    }
    fai_drop(text);
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn substring_views_copy_only_the_visible_bytes() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let original = make_str(&"é".repeat(64));
    let view = fai_string_take(make_int(32), fai_dup(original));
    // SAFETY: taking a large fraction of this string returns a slice view.
    assert!(unsafe { is_string_slice(view) });
    let bytes = fai_bytes_from_string(view);
    assert_ne!(bytes, view);
    // SAFETY: the converted buffer owns exactly the view's UTF-8 bytes.
    assert_eq!(unsafe { bytes_bytes(bytes) }, "é".repeat(32).as_bytes());
    assert_eq!(read_int(fai_string_length_borrowed(original)), 64);
    fai_drop(original);
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn cross_thread_conversion_preserves_the_shared_binary_value() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes("é".as_bytes());
    fai_mark_shared(bytes);
    let owned = fai_dup(bytes);
    std::thread::spawn(move || {
        let text = fai_bytes_to_string(owned);
        assert_ne!(text, owned);
        assert_eq!(read_int(fai_string_length_borrowed(text)), 1);
        fai_drop(text);
    })
    .join()
    .unwrap();
    // SAFETY: the retained binary alias was never re-tagged.
    assert_eq!(unsafe { bytes_bytes(bytes) }, "é".as_bytes());
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn unicode_round_trips_preserve_content(chars in prop::collection::vec(any::<char>(), 0..256)) {
            let _guard = lock();
            let baseline = (live_count(), live_bytes());
            let expected: String = chars.into_iter().collect();
            let original = make_str(&expected);
            let restored = fai_bytes_to_string(fai_bytes_from_string(original));
            // SAFETY: both conversions preserve the original valid UTF-8.
            let actual = unsafe { string_bytes(restored).to_vec() };
            fai_drop(restored);
            prop_assert_eq!(actual, expected.as_bytes());
            prop_assert_eq!((live_count(), live_bytes()), baseline);
        }
    }
}
