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
fn unique_growth_keeps_spare_capacity_below_the_mapping_boundary() {
    let _guard = lock();
    let before = (live_count(), live_bytes(), allocation_stats::current());
    let result = fai_bytes_concat(make_bytes(&[0; 8192]), make_bytes(&[255; 127]));
    // SAFETY: result owns an inline Bytes buffer and its complete live payload.
    unsafe {
        assert_eq!(object_size(as_obj(result)), large_pages::THRESHOLD - ALIGN);
        assert_eq!(&bytes_bytes(result)[..8192], &[0; 8192]);
        assert_eq!(&bytes_bytes(result)[8192..], &[255; 127]);
    }
    assert_eq!(allocation_stats::current().mapped_bytes, before.2.mapped_bytes);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), (before.0, before.1));
    assert_eq!(allocation_stats::current().system_bytes, before.2.system_bytes);
}

#[test]
fn crossing_the_last_unmapped_capacity_still_uses_a_mapping() {
    let _guard = lock();
    let before = (live_count(), live_bytes(), allocation_stats::current());
    let last_small = large_pages::THRESHOLD - ALIGN - STRING_BYTES_OFFSET;
    let value = fai_bytes_concat(make_bytes(&[13; 8192]), make_bytes(&vec![10; last_small - 8192]));
    assert_eq!(allocation_stats::current().mapped_bytes, before.2.mapped_bytes);
    let result = fai_bytes_concat(value, make_bytes(&[255]));
    assert!(allocation_stats::current().mapped_bytes > before.2.mapped_bytes);
    // SAFETY: all byte reads remain inside the owned result's initialized prefix.
    unsafe {
        assert_eq!(bytes_bytes(result).len(), last_small + 1);
        assert_eq!(bytes_bytes(result)[last_small], 255);
        assert!(object_size(as_obj(result)) >= large_pages::THRESHOLD);
    }
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), (before.0, before.1));
    assert_eq!(allocation_stats::current().mapped_bytes, before.2.mapped_bytes);
}

#[test]
fn required_mapping_sized_payload_is_not_clamped() {
    let _guard = lock();
    let before = (live_count(), live_bytes(), allocation_stats::current());
    let result = fai_bytes_concat(make_bytes(&[0; 8192]), make_bytes(&[128; 8192]));
    assert!(allocation_stats::current().mapped_bytes > before.2.mapped_bytes);
    // SAFETY: the owned result has the complete concatenated payload.
    unsafe {
        assert_eq!(string_cap(result), 16384);
        assert_eq!(bytes_bytes(result).len(), 16384);
        assert_eq!(&bytes_bytes(result)[8192..], &[128; 8192]);
    }
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), (before.0, before.1));
    assert_eq!(allocation_stats::current().mapped_bytes, before.2.mapped_bytes);
}

#[test]
fn growth_resumes_power_of_two_sizes_after_the_boundary_stop() {
    let _guard = lock();
    let before = (live_count(), live_bytes(), allocation_stats::current());
    let first = fai_bytes_concat(make_bytes(&[0; 8192]), make_bytes(&[255; 127]));
    let result = fai_bytes_concat(first, make_bytes(&vec![128; 32768 - 8319]));
    // SAFETY: Bytes' capacity and payload are initialized in the owned result.
    unsafe {
        assert_eq!(string_cap(result), 32768);
        assert_eq!(bytes_bytes(result).len(), 32768);
        assert!(bytes_bytes(result)[8319..].iter().all(|&byte| byte == 128));
    }
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), (before.0, before.1));
    assert_eq!(allocation_stats::current().mapped_bytes, before.2.mapped_bytes);
}

#[test]
fn shared_near_boundary_left_keeps_its_contents_and_copies_tightly() {
    let _guard = lock();
    let before = (live_count(), live_bytes());
    let left = make_bytes(&[0; 8192]);
    let result = fai_bytes_concat(fai_dup(left), make_bytes(&[255; 127]));
    // SAFETY: both immutable values remain owned throughout the comparisons.
    unsafe {
        assert_eq!(bytes_bytes(left), &[0; 8192]);
        assert_eq!(&bytes_bytes(result)[8192..], &[255; 127]);
        assert_eq!(
            object_size(as_obj(result)),
            checked_buffer_size(STRING_BYTES_OFFSET, 8319, 1).unwrap()
        );
    }
    fai_drop(result);
    fai_drop(left);
    assert_eq!((live_count(), live_bytes()), before);
}

#[test]
fn repeated_boundary_growth_has_bounded_live_and_backing_storage() {
    let _guard = lock();
    let before = (live_count(), live_bytes(), allocation_stats::current());
    let observe = |rounds| {
        let result = std::thread::spawn(move || {
            reset_allocations();
            for _ in 0..rounds {
                let value = fai_bytes_concat(make_bytes(&[0; 8192]), make_bytes(&[255; 127]));
                assert_eq!(allocation_stats::current().mapped_bytes, before.2.mapped_bytes);
                fai_drop(value);
            }
            assert_eq!((live_count(), live_bytes()), (before.0, before.1));
            let retained = allocation_stats::current();
            assert_eq!(retained.system_bytes, before.2.system_bytes);
            assert_eq!(retained.mapped_bytes, before.2.mapped_bytes);
            (peak_live_bytes(), allocation_stats::peak_bytes(), retained)
        })
        .join()
        .unwrap();
        assert_eq!(allocation_stats::current(), before.2);
        result
    };
    assert_eq!(observe(12), observe(1200));
}

#[test]
fn a_boundary_grown_buffer_survives_cross_thread_transfer() {
    let _guard = lock();
    let before = (live_count(), live_bytes(), allocation_stats::current());
    let value = std::thread::spawn(|| {
        fai_mark_shared(fai_bytes_concat(make_bytes(&[0; 8192]), make_bytes(&[255; 127])))
    })
    .join()
    .unwrap();
    // SAFETY: joining transfers this shared buffer's sole reference to this thread.
    unsafe {
        assert_eq!(bytes_bytes(value).len(), 8319);
        assert_eq!(&bytes_bytes(value)[8192..], &[255; 127]);
    }
    fai_drop(value);
    assert_eq!((live_count(), live_bytes()), (before.0, before.1));
    assert_eq!(allocation_stats::current(), before.2);
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
        fn boundary_growth_preserves_contents_and_shared_aliases(
            left in prop::collection::vec(any::<u8>(), 8100..8300),
            right in prop::collection::vec(any::<u8>(), 1..8300),
            shared in any::<bool>(),
        ) {
            let _guard = crate::tests::lock();
            let before = (super::live_count(), super::live_bytes());
            let a = super::make_bytes(&left);
            let b = super::make_bytes(&right);
            let passed = if shared { super::fai_dup(a) } else { a };
            let result = super::fai_bytes_concat(passed, super::fai_dup(b));
            let expected: Vec<_> = left.iter().chain(&right).copied().collect();
            let tight = super::checked_buffer_size(super::STRING_BYTES_OFFSET, expected.len(), 1).unwrap();
            // SAFETY: result and right are owned, and left is retained when shared.
            let (actual, aliases_intact, size) = unsafe {
                (super::bytes_bytes(result).to_vec(),
                 super::bytes_bytes(b) == right.as_slice() && (!shared || super::bytes_bytes(a) == left.as_slice()),
                 super::object_size(super::as_obj(result)))
            };
            super::fai_drop(result);
            super::fai_drop(b);
            if shared { super::fai_drop(a); }
            prop_assert_eq!((super::live_count(), super::live_bytes()), before);
            prop_assert_eq!(actual, expected);
            prop_assert!(aliases_intact);
            prop_assert!(size >= tight);
            if tight < super::large_pages::THRESHOLD {
                prop_assert!(size < super::large_pages::THRESHOLD);
            }
        }

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
