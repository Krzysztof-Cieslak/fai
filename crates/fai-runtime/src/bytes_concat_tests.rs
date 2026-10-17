//! Binary concatenation preserves content, aliases and owned-buffer lifetimes.
use super::*;
use crate::tests::lock;

#[track_caller]
fn concat(left: &[u8], right: &[u8]) -> Vec<u8> {
    let _guard = lock();
    let baseline = live_count();
    let a = make_bytes(left);
    let b = make_bytes(right);
    let result = fai_bytes_concat(fai_dup(a), fai_dup(b));
    // SAFETY: the retained inputs and returned result are live Bytes values.
    let (output, unchanged) = unsafe {
        (bytes_bytes(result).to_vec(), bytes_bytes(a) == left && bytes_bytes(b) == right)
    };
    fai_drop(a);
    fai_drop(b);
    fai_drop(result);
    assert!(unchanged);
    assert_eq!(live_count(), baseline);
    output
}

#[test]
fn empty_left_preserves_binary_right() {
    assert_eq!(concat(&[], &[0, 255, 128]), [0, 255, 128]);
}

#[test]
fn empty_right_preserves_binary_left() {
    assert_eq!(concat(&[0, 255, 128], &[]), [0, 255, 128]);
}

#[test]
fn two_empty_inputs_produce_empty_bytes() {
    assert!(concat(&[], &[]).is_empty());
}

#[test]
fn binary_content_is_copied_without_utf8_interpretation() {
    assert_eq!(concat(&[0, 255, 13], &[10, 128, 0]), [0, 255, 13, 10, 128, 0]);
}

#[test]
fn the_same_input_can_supply_both_halves() {
    let _guard = lock();
    let baseline = live_count();
    let input = make_bytes(&[0, 128, 255]);
    let result = fai_bytes_concat(fai_dup(input), fai_dup(input));
    // SAFETY: both values are still owned and contain binary payloads.
    let output = unsafe { bytes_bytes(result).to_vec() };
    // SAFETY: the original input retains its own reference through both copies.
    let original = unsafe { bytes_bytes(input).to_vec() };
    fai_drop(input);
    fai_drop(result);
    assert_eq!(output, [0, 128, 255, 0, 128, 255]);
    assert_eq!(original, [0, 128, 255]);
    assert_eq!(live_count(), baseline);
}

#[test]
fn large_buffers_cross_the_pool_and_mapping_boundaries() {
    let left = vec![19; 16384];
    let right = vec![251; 32768];
    let output = concat(&left, &right);
    assert_eq!(output.len(), left.len() + right.len());
    assert_eq!(&output[..left.len()], left.as_slice());
    assert_eq!(&output[left.len()..], right.as_slice());
}

#[test]
fn unique_left_appends_within_spare_capacity() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let left = make_bytes(&[0, 255, 128]);
    let right = make_bytes(&[13, 10]);
    reset_allocations();
    let result = fai_bytes_concat(left, right);
    assert_eq!(result, left);
    assert_eq!(allocations(), 0);
    // SAFETY: result is an owned Bytes buffer with its updated initialized range.
    assert_eq!(unsafe { bytes_bytes(result) }, [0, 255, 128, 13, 10]);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn full_unique_left_grows_geometrically() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let left = make_bytes(&[0; 8]);
    let right = make_bytes(&[255]);
    reset_allocations();
    let result = fai_bytes_concat(left, right);
    assert_ne!(result, left);
    assert_eq!(allocations(), 1);
    // SAFETY: Bytes have the same inline length/capacity layout as Strings.
    unsafe {
        assert_eq!(string_cap(result), 16);
        assert_eq!(bytes_bytes(result), [0, 0, 0, 0, 0, 0, 0, 0, 255]);
    }
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn empty_concat_transfers_the_other_buffer() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let input = make_bytes(&[0, 255]);
    let empty = make_bytes(&[]);
    reset_allocations();
    let result = fai_bytes_concat(empty, fai_dup(input));
    assert_eq!(result, input);
    assert_eq!(allocations(), 0);
    fai_drop(result);
    fai_drop(input);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_left_copies_even_when_its_capacity_would_fit() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let left = make_bytes(&[0, 255]);
    let right = make_bytes(&[1]);
    reset_allocations();
    let result = fai_bytes_concat(fai_dup(left), right);
    assert_ne!(result, left);
    assert_eq!(allocations(), 1);
    // SAFETY: both left and result retain owned references.
    unsafe {
        assert_eq!(bytes_bytes(left), [0, 255]);
        assert_eq!(bytes_bytes(result), [0, 255, 1]);
    }
    fai_drop(result);
    fai_drop(left);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_across_threads_keeps_the_original_contents() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let input = make_bytes(&[128, 0, 255]);
    fai_mark_shared(input);
    let retained = fai_dup(input);
    let worker = std::thread::spawn(move || {
        let result = fai_bytes_concat(input, make_bytes(&[42]));
        // SAFETY: this thread owns the new result until after the comparison.
        assert_eq!(unsafe { bytes_bytes(result) }, [128, 0, 255, 42]);
        fai_drop(result);
    });
    worker.join().unwrap();
    // SAFETY: retained owns the original immutable input across the handoff.
    assert_eq!(unsafe { bytes_bytes(retained) }, [128, 0, 255]);
    fai_drop(retained);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn repeated_appends_allocate_logarithmically_across_large_mappings() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let chunk = make_bytes(&[255; 1024]);
    let mut value = make_bytes(&[]);
    reset_allocations();
    for _ in 0..1024 {
        value = fai_bytes_concat(value, fai_dup(chunk));
    }
    assert!(allocations() <= 12, "unique appends should only allocate on growth");
    // SAFETY: the final buffer owns all appended bytes.
    unsafe {
        assert_eq!(bytes_bytes(value).len(), 1024 * 1024);
        assert!(bytes_bytes(value).iter().all(|byte| *byte == 255));
    }
    fai_drop(value);
    fai_drop(chunk);
    assert_eq!((live_count(), live_bytes()), baseline);
}

mod proptests {
    use super::concat;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn unique_binary_appends_match_flattened_chunks(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..300), 0..80),
        ) {
            let _guard = crate::tests::lock();
            let baseline = (super::live_count(), super::live_bytes());
            let mut result = super::make_bytes(&[]);
            for chunk in &chunks {
                result = super::fai_bytes_concat(result, super::make_bytes(chunk));
            }
            let expected = chunks.concat();
            // SAFETY: result is an owned buffer; the copy ends before its release.
            let actual = unsafe { super::bytes_bytes(result).to_vec() };
            super::fai_drop(result);
            prop_assert_eq!((super::live_count(), super::live_bytes()), baseline);
            prop_assert_eq!(actual, expected);
        }

        #[test]
        fn arbitrary_binary_inputs_match_vector_concatenation(
            left in prop::collection::vec(any::<u8>(), 0..2048),
            right in prop::collection::vec(any::<u8>(), 0..2048),
        ) {
            let actual = concat(&left, &right);
            let mut expected = left;
            expected.extend(right);
            prop_assert_eq!(actual, expected);
        }
    }
}
