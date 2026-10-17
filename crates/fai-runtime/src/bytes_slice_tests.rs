//! Byte slices retain value semantics when a complete range transfers its input.
use super::*;
use crate::tests::lock;

#[track_caller]
fn whole(start: i64, end: i64, input: &[u8]) {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes(input);
    let lo = make_int(start);
    let hi = make_int(end);
    reset_allocations();
    let result = fai_bytes_slice(lo, hi, bytes);
    assert_eq!(result, bytes);
    assert_eq!(allocations(), 0);
    // SAFETY: result owns the transferred buffer and all initialized bytes.
    assert_eq!(unsafe { bytes_bytes(result) }, input);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn full_width_transfers_binary_content() {
    whole(0, 4, &[0, 255, 128, 42]);
}

#[test]
fn clamped_full_width_releases_boxed_indices() {
    whole(i64::MIN, i64::MAX, &[0, 255, 128, 42]);
}

#[test]
fn empty_buffer_transfers_after_index_clamping() {
    whole(i64::MAX, i64::MIN, &[]);
}

#[test]
fn large_full_width_transfers_its_mapping() {
    whole(0, 32768, &vec![255; 32768]);
}

#[test]
fn partial_range_copies_and_releases_boxed_bounds() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let original = make_bytes(&[0, 128, 255]);
    let result = fai_bytes_slice(make_int(1), make_int(i64::MAX), fai_dup(original));
    assert_ne!(result, original);
    // SAFETY: both values retain independent owned references.
    unsafe {
        assert_eq!(bytes_bytes(result), [128, 255]);
        assert_eq!(bytes_bytes(original), [0, 128, 255]);
    }
    fai_drop(original);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn transferred_alias_cannot_mutate_the_original() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let original = make_bytes(&[0, 255, 128]);
    let result = fai_bytes_slice(make_int(0), make_int(3), fai_dup(original));
    assert_eq!(result, original);
    let appended = fai_bytes_concat(result, make_bytes(&[42]));
    // SAFETY: the original alias and appended value both remain owned.
    unsafe {
        assert_eq!(bytes_bytes(original), [0, 255, 128]);
        assert_eq!(bytes_bytes(appended), [0, 255, 128, 42]);
    }
    fai_drop(original);
    fai_drop(appended);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_across_threads_transfers_without_changing_its_count() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let original = make_bytes(&[0, 128, 255]);
    fai_mark_shared(original);
    let transferred = fai_dup(original);
    std::thread::spawn(move || {
        let result = fai_bytes_slice(make_int(0), make_int(3), transferred);
        assert_eq!(result, transferred);
        fai_drop(result);
    })
    .join()
    .unwrap();
    // SAFETY: the retained reference owns the immutable original buffer.
    assert_eq!(unsafe { bytes_bytes(original) }, [0, 128, 255]);
    fai_drop(original);
    assert_eq!((live_count(), live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_ranges_match_clamped_binary_slices(
            input in prop::collection::vec(any::<u8>(), 0..512),
            start in -600i64..600,
            end in -600i64..600,
        ) {
            let _guard = lock();
            let baseline = (live_count(), live_bytes());
            let bytes = make_bytes(&input);
            let result = fai_bytes_slice(make_int(start), make_int(end), fai_dup(bytes));
            let lo = start.clamp(0, input.len() as i64) as usize;
            let hi = end.clamp(lo as i64, input.len() as i64) as usize;
            // SAFETY: both values remain owned until after their content is copied.
            let (actual, original) = unsafe {
                (bytes_bytes(result).to_vec(), bytes_bytes(bytes).to_vec())
            };
            fai_drop(bytes);
            fai_drop(result);
            prop_assert_eq!(actual.as_slice(), &input[lo..hi]);
            prop_assert_eq!(original, input);
            prop_assert_eq!((live_count(), live_bytes()), baseline);
        }
    }
}
