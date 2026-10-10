//! Immediate-pair fast paths agree with the general physical representations.

use super::*;
use crate::tests::lock;

fn compact(first: Value, second: Value) -> Value {
    let fields = [first, second];
    // SAFETY: the two initialized fields transfer their owned references.
    unsafe { fai_make_data(0, 2, fields.as_ptr()) }
}

fn extended(first: Value, second: Value) -> Value {
    let object = alloc_obj(DATA_FIELDS_OFFSET + 16, &FAI_DATA_DESC);
    // SAFETY: the legacy data allocation has a tag and two initialized uniform
    // fields. Its stored size encodes exactly this field count for destruction.
    unsafe {
        write_u64(object, DATA_TAG_OFFSET, 0);
        write_i64(object, DATA_FIELDS_OFFSET, first);
        write_i64(object, DATA_FIELDS_OFFSET + 8, second);
    }
    from_obj(object)
}

#[test]
fn immediate_pairs_match_extended_hash_and_equality() {
    let _guard = lock();
    let baseline = live_count();
    let a = compact(imm_int(-7), imm_int(42));
    let b = extended(imm_int(-7), imm_int(42));
    assert!(immediate_pair(a).is_some());
    assert!(immediate_pair(b).is_none());
    assert!(values_equal(a, b));
    assert_eq!(values_hash(a), values_hash(b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), baseline);
}

#[test]
fn immediate_pair_equality_checks_both_fields() {
    let _guard = lock();
    let baseline = live_count();
    let a = compact(imm_int(1), imm_int(2));
    let b = compact(imm_int(1), imm_int(3));
    assert!(!values_equal(a, b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), baseline);
}

#[test]
fn boxed_integer_fields_keep_the_general_path() {
    let _guard = lock();
    let baseline = live_count();
    let a = compact(make_int(i64::MIN), make_int(i64::MAX));
    let b = extended(make_int(i64::MIN), make_int(i64::MAX));
    assert!(immediate_pair(a).is_none());
    assert!(values_equal(a, b));
    assert_eq!(values_hash(a), values_hash(b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), baseline);
}

#[test]
fn raw_float_fields_keep_the_general_path() {
    let _guard = lock();
    let baseline = live_count();
    let fields = [(-0.0f64).to_bits() as i64, 1.25f64.to_bits() as i64];
    let descriptor = intern_data_descriptor(3);
    // SAFETY: both fields contain raw Float bits, matching the scalar bitmap.
    let raw = unsafe { fai_make_data_scalar(descriptor, 0, 2, fields.as_ptr()) };
    let boxed = compact(fai_box_float(fields[0]), fai_box_float(fields[1]));
    assert!(immediate_pair(raw).is_none());
    assert!(values_equal(raw, boxed));
    assert_eq!(values_hash(raw), values_hash(boxed));
    fai_drop(raw);
    fai_drop(boxed);
    assert_eq!(live_count(), baseline);
}

#[test]
fn a_different_constructor_tag_is_not_a_plain_pair() {
    let _guard = lock();
    let baseline = live_count();
    let fields = [imm_int(1), imm_int(2)];
    // SAFETY: both fields are initialized owned immediates.
    let tagged = unsafe { fai_make_data(1, 2, fields.as_ptr()) };
    let pair = compact(fields[0], fields[1]);
    assert!(immediate_pair(tagged).is_none());
    assert!(!values_equal(tagged, pair));
    fai_drop(tagged);
    fai_drop(pair);
    assert_eq!(live_count(), baseline);
}

#[test]
fn shared_count_flags_do_not_change_pair_contents() {
    let _guard = lock();
    let baseline = live_count();
    let pair = fai_mark_shared(compact(imm_int(3), imm_int(5)));
    let alias = fai_dup(pair);
    assert_eq!(immediate_pair(pair), Some([imm_int(3), imm_int(5)]));
    assert!(values_equal(pair, alias));
    assert_eq!(values_hash(pair), values_hash(alias));
    fai_drop(alias);
    fai_drop(pair);
    assert_eq!(live_count(), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]
        #[test]
        fn pair_fast_path_matches_legacy_layout(a in -(1i64 << 62)..(1i64 << 62), b in -(1i64 << 62)..(1i64 << 62)) {
            let _guard = lock();
            let baseline = live_count();
            let pair = compact(imm_int(a), imm_int(b));
            let legacy = extended(imm_int(a), imm_int(b));
            prop_assert!(values_equal(pair, legacy));
            prop_assert_eq!(values_hash(pair), values_hash(legacy));
            fai_drop(pair);
            fai_drop(legacy);
            prop_assert_eq!(live_count(), baseline);
        }
    }
}
