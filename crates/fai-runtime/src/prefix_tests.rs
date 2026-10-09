//! Prefix reversal preserves values, shared tails, and bounded ownership.

use super::*;

fn list(values: &[i64]) -> Value {
    cons_list(&values.iter().copied().map(make_int).collect::<Vec<_>>())
}

fn contents(value: Value) -> Vec<i64> {
    let mut cursor = fai_dup(value);
    let mut values = Vec::new();
    while is_boxed(cursor) {
        let head = fai_data_field(cursor, 0);
        values.push(read_int(head));
        fai_drop(head);
        let next = fai_data_field(cursor, 1);
        fai_drop(cursor);
        cursor = next;
    }
    values
}

#[track_caller]
fn check(values: &[i64], count: i64, expected: &[i64], shared: bool) {
    let _guard = tests::lock();
    let baseline = (live_count(), live_bytes());
    let input = list(values);
    let original = shared.then(|| fai_dup(input));
    let count_value = make_int(count);
    reset_allocations();
    let result = fai_list_reverse_prefix(count_value, input);
    let expected_allocations = if shared { count.max(0).min(values.len() as i64) } else { 0 };
    assert_eq!(allocations(), expected_allocations);
    assert_eq!(contents(result), expected);
    if let Some(original) = original {
        assert_eq!(contents(original), values);
        fai_drop(original);
    }
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn unique_prefix_reuses_every_cell() {
    check(&[1, 2, 3, 4, 5], 3, &[3, 2, 1, 4, 5], false);
}

#[test]
fn shared_prefix_copies_only_the_reversed_cells() {
    check(&[1, 2, 3, 4, 5], 3, &[3, 2, 1, 4, 5], true);
}

#[test]
fn negative_full_width_count_returns_the_input() {
    check(&[1, 2], i64::MIN, &[1, 2], false);
}

#[test]
fn oversized_count_reverses_the_whole_list() {
    check(&[i64::MAX, i64::MIN], i64::MAX, &[i64::MIN, i64::MAX], false);
}

#[test]
fn empty_list_stays_empty() {
    check(&[], 20, &[], false);
}

#[test]
fn zero_count_does_not_copy_shared_cells() {
    check(&[1, 2], 0, &[1, 2], true);
}

#[test]
fn a_shared_tail_is_retained_during_unique_prefix_reuse() {
    let _guard = tests::lock();
    let baseline = (live_count(), live_bytes());
    let input = list(&[1, 2, 3, 4, 5]);
    let second = fai_data_field(input, 1);
    let original_tail = fai_data_field(second, 1);
    fai_drop(second);
    reset_allocations();
    let result = fai_list_reverse_prefix(make_int(4), input);
    assert_eq!(allocations(), 2);
    assert_eq!(contents(result), [4, 3, 2, 1, 5]);
    assert_eq!(contents(original_tail), [3, 4, 5]);
    fai_drop(result);
    fai_drop(original_tail);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_marked_unique_cells_still_reuse() {
    let _guard = tests::lock();
    let baseline = (live_count(), live_bytes());
    let input = list(&[1, 2, 3, 4]);
    fai_mark_shared(input);
    reset_allocations();
    let result = fai_list_reverse_prefix(make_int(3), input);
    assert_eq!(allocations(), 0);
    assert_eq!(contents(result), [3, 2, 1, 4]);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn boxed_float_heads_keep_their_bits() {
    let _guard = tests::lock();
    let baseline = (live_count(), live_bytes());
    let bits = [0x8000_0000_0000_0000u64, 0x7ff8_0000_0000_1234];
    let input = cons_list(&bits.map(|bits| fai_box_float(bits as i64)));
    let result = fai_list_reverse_prefix(make_int(2), input);
    let first = fai_data_field(result, 0);
    let tail = fai_data_field(result, 1);
    let second = fai_data_field(tail, 0);
    assert_eq!([read_float(first).to_bits(), read_float(second).to_bits()], [bits[1], bits[0]]);
    fai_drop(first);
    fai_drop(second);
    fai_drop(tail);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[cfg(not(miri))]
#[test]
fn deep_unique_prefix_uses_no_fresh_cells_or_recursive_drop_stack() {
    let _guard = tests::lock();
    let baseline = (live_count(), live_bytes());
    let values: Vec<_> = (0..100_000).map(make_int).collect();
    let input = cons_list(&values);
    reset_allocations();
    let result = fai_list_reverse_prefix(make_int(100_000), input);
    assert_eq!(allocations(), 0);
    let first = fai_data_field(result, 0);
    assert_eq!(read_int(first), 99_999);
    fai_drop(first);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[cfg(not(miri))]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prefix_reversal_matches_vector_model(values in proptest::collection::vec(any::<i64>(), 0..48), count in -8i64..64, shared in any::<bool>()) {
            let mut expected = values.clone();
            let n = (count.max(0) as usize).min(expected.len());
            expected[..n].reverse();
            check(&values, count, &expected, shared);
        }
    }
}
