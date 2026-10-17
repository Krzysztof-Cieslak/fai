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

mod proptests {
    use super::concat;
    use proptest::prelude::*;

    proptest! {
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
