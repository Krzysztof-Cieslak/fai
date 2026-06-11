//! Differential tests for **scalar-float data cells** (a `Float` field stored as
//! raw `f64` bits under a per-shape descriptor) against the all-boxed reference
//! representation (every field a uniform word, a `Float` a boxed cell).
//!
//! Each generic runtime walker — structural equality, ordering, generic field
//! projection, in-place/copy record update, drop, and reuse — must give the same
//! observable result on a scalar cell as on the boxed reference, and must leave
//! the live-object count balanced (no leak, no double-free). The harness builds a
//! cell both ways from one field list and asserts agreement; bugs in the bitmap
//! handling (a scalar slot dereferenced as a pointer, or a boxed child skipped)
//! surface as a mismatch, a crash, or a leak.
//!
//! Cases run under the global runtime [`lock`] and the live counter (compiled in
//! only under `debug_assertions`, the default for `cargo test`).

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::*;
use crate::tests::lock;

/// A field of a test cell, in its logical form (independent of representation).
#[derive(Clone, Copy, Debug)]
enum Field {
    /// An unboxed `f64` slot (scalar in the cell, a boxed `Float` in the
    /// reference).
    Scalar(f64),
    /// A uniform immediate `Int` slot (no allocation).
    Imm(i64),
    /// A uniform boxed `Int` slot — a reference-counted heap child, so a cell
    /// mixing it with scalar slots exercises releasing a boxed sibling while
    /// skipping the scalar ones.
    Boxed(i64),
}

/// Encodes a small integer as an immediate `Int`.
fn imm(n: i64) -> Value {
    (n << 1) | 1
}

/// Forces `n` outside the 63-bit immediate range so `fai_box_int` allocates.
fn force_box(n: i64) -> i64 {
    (n & ((1i64 << 40) - 1)) | (1i64 << 62)
}

/// The scalar bitmap implied by a field list.
fn bitmap_of(fields: &[Field]) -> u64 {
    fields
        .iter()
        .enumerate()
        .fold(0u64, |b, (i, f)| if matches!(f, Field::Scalar(_)) { b | (1u64 << i) } else { b })
}

/// The slot word for the **scalar** representation (a float is raw bits).
fn scalar_word(f: &Field) -> i64 {
    match f {
        Field::Scalar(x) => x.to_bits() as i64,
        Field::Imm(n) => imm(*n),
        Field::Boxed(n) => fai_box_int(force_box(*n)),
    }
}

/// Builds the scalar cell: float fields are raw bits under a per-shape descriptor.
fn build_scalar(tag: i64, fields: &[Field]) -> Value {
    let desc = intern_data_descriptor(bitmap_of(fields));
    let words: Vec<i64> = fields.iter().map(scalar_word).collect();
    // SAFETY: `desc`'s bitmap matches `words`; each word is owned unless scalar.
    unsafe { fai_make_data_scalar(desc, tag, fields.len() as i64, words.as_ptr()) }
}

/// Builds the reference cell: every field uniform (a float is a boxed `Float`),
/// under the shared all-uniform descriptor.
fn build_reference(tag: i64, fields: &[Field]) -> Value {
    let words: Vec<i64> = fields
        .iter()
        .map(|f| match f {
            Field::Scalar(x) => fai_box_float(x.to_bits() as i64),
            Field::Imm(n) => imm(*n),
            Field::Boxed(n) => fai_box_int(force_box(*n)),
        })
        .collect();
    // SAFETY: every word is an owned uniform value.
    unsafe { fai_make_data(tag, fields.len() as i64, words.as_ptr()) }
}

/// A uniform value of a field, as passed to `fai_record_update` (a float is boxed).
fn uniform_value(f: Field) -> Value {
    match f {
        Field::Scalar(x) => fai_box_float(x.to_bits() as i64),
        Field::Imm(n) => imm(n),
        Field::Boxed(n) => fai_box_int(force_box(n)),
    }
}

fn eq_bool(a: Value, b: Value) -> bool {
    fai_equal_borrowed(a, b) == 3
}

fn cmp_ord(a: Value, b: Value) -> i64 {
    fai_compare_borrowed(a, b) >> 1
}

/// Asserts the scalar and reference cells agree on equality, ordering, and every
/// field projection, then drops everything and asserts no leak.
fn diff_check(tag: i64, fa: &[Field], fb: &[Field]) -> Result<(), TestCaseError> {
    let base = live_count();
    let sa = build_scalar(tag, fa);
    let sb = build_scalar(tag, fb);
    let ra = build_reference(tag, fa);
    let rb = build_reference(tag, fb);

    prop_assert_eq!(eq_bool(sa, sb), eq_bool(ra, rb), "equality mismatch");
    prop_assert_eq!(cmp_ord(sa, sb), cmp_ord(ra, rb), "ordering mismatch");

    // Each field of `fa` projects to a value equal to the reference's field — a
    // scalar slot boxes to a `Float`, a uniform slot duplicates the word.
    for i in 0..fa.len() {
        let sf = fai_data_field(sa, i as i64);
        let rf = fai_data_field(ra, i as i64);
        prop_assert!(eq_bool(sf, rf), "field {} projection mismatch", i);
        fai_drop(sf);
        fai_drop(rf);
    }

    fai_drop(sa);
    fai_drop(sb);
    fai_drop(ra);
    fai_drop(rb);
    prop_assert_eq!(live_count(), base, "leak after diff_check");
    Ok(())
}

/// Asserts a record update at `slot` (to `newf`) yields a cell equal to one built
/// fresh with that field replaced, leak-free. `shared` dups the cell first so the
/// copy path runs; otherwise the in-place path runs.
fn update_check(
    tag: i64,
    fields: &[Field],
    slot: usize,
    newf: Field,
    shared: bool,
) -> Result<(), TestCaseError> {
    let base = live_count();
    let cell = build_scalar(tag, fields);
    let keep = if shared {
        fai_dup(cell);
        Some(cell)
    } else {
        None
    };
    let mut expected_fields = fields.to_vec();
    expected_fields[slot] = newf;
    let expected = build_scalar(tag, &expected_fields);

    let updated = fai_record_update(cell, imm(slot as i64), uniform_value(newf));
    prop_assert!(eq_bool(updated, expected), "update mismatch at slot {}", slot);

    fai_drop(updated);
    fai_drop(expected);
    if let Some(c) = keep {
        fai_drop(c);
    }
    prop_assert_eq!(live_count(), base, "leak after update_check");
    Ok(())
}

/// Asserts a unique cell resets to a reuse token and rebuilds in place into an
/// equal cell, leak-free.
fn reuse_check(tag: i64, fields: &[Field]) -> Result<(), TestCaseError> {
    let base = live_count();
    let cell = build_scalar(tag, fields);
    let token = fai_drop_reuse(cell);
    let desc = intern_data_descriptor(bitmap_of(fields));
    let words: Vec<i64> = fields.iter().map(scalar_word).collect();
    // SAFETY: `token` is from `fai_drop_reuse`; `desc`/`words` match the shape.
    let rebuilt =
        unsafe { fai_reuse_scalar(desc, token, tag, fields.len() as i64, words.as_ptr()) };
    let expected = build_scalar(tag, fields);
    prop_assert!(eq_bool(rebuilt, expected), "reuse mismatch");
    fai_drop(rebuilt);
    fai_drop(expected);
    prop_assert_eq!(live_count(), base, "leak after reuse_check");
    Ok(())
}

#[track_caller]
fn diff(tag: i64, fa: &[Field], fb: &[Field]) {
    let _g = lock();
    diff_check(tag, fa, fb).expect("scalar cell matches boxed reference");
}

#[track_caller]
fn update(tag: i64, fields: &[Field], slot: usize, newf: Field, shared: bool) {
    let _g = lock();
    update_check(tag, fields, slot, newf, shared).expect("record update matches");
}

#[track_caller]
fn reuse(tag: i64, fields: &[Field]) {
    let _g = lock();
    reuse_check(tag, fields).expect("reuse matches");
}

use Field::{Boxed, Imm, Scalar};

#[test]
fn vec2_equal_compare_project() {
    diff(0, &[Scalar(1.0), Scalar(2.0)], &[Scalar(1.0), Scalar(2.0)]);
}

#[test]
fn vec2_unequal() {
    diff(0, &[Scalar(1.0), Scalar(2.0)], &[Scalar(1.0), Scalar(9.0)]);
}

#[test]
fn single_scalar_field() {
    diff(0, &[Scalar(5.5)], &[Scalar(-5.5)]);
}

#[test]
fn accel_three_floats_and_an_int() {
    let a = [Scalar(1.0), Scalar(2.0), Scalar(3.0), Imm(0)];
    let b = [Scalar(1.0), Scalar(2.0), Scalar(3.0), Imm(1)];
    diff(0, &a, &b);
}

#[test]
fn scalar_field_beside_a_boxed_child() {
    // The drop/scan must release the boxed `Int` sibling while skipping the scalar.
    diff(0, &[Scalar(3.0), Boxed(7)], &[Scalar(3.0), Boxed(7)]);
}

#[test]
fn scalar_field_beside_a_boxed_child_unequal() {
    diff(0, &[Scalar(3.0), Boxed(7)], &[Scalar(3.0), Boxed(8)]);
}

#[test]
fn different_tags_order_by_tag() {
    diff(1, &[Scalar(1.0)], &[Scalar(1.0)]);
}

#[test]
fn negative_zero_distinct_from_zero_by_bits() {
    // Bit equality (matching boxed `Float`): -0.0 and 0.0 differ.
    diff(0, &[Scalar(-0.0)], &[Scalar(0.0)]);
}

#[test]
fn nan_handling_matches_reference() {
    diff(0, &[Scalar(f64::NAN)], &[Scalar(f64::NAN)]);
    diff(0, &[Scalar(f64::INFINITY)], &[Scalar(f64::NEG_INFINITY)]);
}

#[test]
fn update_scalar_field_in_place() {
    update(0, &[Scalar(1.0), Scalar(2.0)], 0, Scalar(9.0), false);
}

#[test]
fn update_scalar_field_shared_copies() {
    update(0, &[Scalar(1.0), Scalar(2.0)], 1, Scalar(9.0), true);
}

#[test]
fn update_uniform_field_in_place() {
    update(0, &[Scalar(1.0), Imm(2)], 1, Imm(7), false);
}

#[test]
fn update_flips_float_to_int_in_place() {
    // The slot's float-ness changes, so the result needs a new (interned) bitmap.
    update(0, &[Scalar(1.0), Scalar(2.0)], 0, Imm(7), false);
}

#[test]
fn update_flips_int_to_float_shared() {
    update(0, &[Imm(1), Scalar(2.0)], 0, Scalar(3.0), true);
}

#[test]
fn update_flips_float_to_boxed_int() {
    update(0, &[Scalar(1.0), Imm(0)], 0, Boxed(5), false);
}

#[test]
fn reuse_vec2() {
    reuse(0, &[Scalar(1.0), Scalar(2.0)]);
}

#[test]
fn reuse_mixed() {
    reuse(0, &[Scalar(1.0), Imm(2), Boxed(3)]);
}

/// A field strategy spanning all three representations.
fn arb_field() -> impl Strategy<Value = Field> {
    prop_oneof![
        any::<f64>().prop_map(Field::Scalar),
        any::<i32>().prop_map(|n| Field::Imm(i64::from(n))),
        any::<i32>().prop_map(|n| Field::Boxed(i64::from(n))),
    ]
}

fn arb_fields() -> impl Strategy<Value = Vec<Field>> {
    prop::collection::vec(arb_field(), 1..=6)
}

/// A pair of fields of the **same** representation but (independently) chosen
/// values, so two cells built from these pairs share a shape — the only valid
/// input to structural equality/ordering (comparison is between same-typed
/// values, hence same scalar bitmap).
fn arb_field_pair() -> impl Strategy<Value = (Field, Field)> {
    prop_oneof![
        (any::<f64>(), any::<f64>()).prop_map(|(a, b)| (Field::Scalar(a), Field::Scalar(b))),
        (any::<i32>(), any::<i32>())
            .prop_map(|(a, b)| (Field::Imm(i64::from(a)), Field::Imm(i64::from(b)))),
        (any::<i32>(), any::<i32>())
            .prop_map(|(a, b)| (Field::Boxed(i64::from(a)), Field::Boxed(i64::from(b)))),
    ]
}

fn arb_field_pairs() -> impl Strategy<Value = Vec<(Field, Field)>> {
    prop::collection::vec(arb_field_pair(), 1..=6)
}

#[test]
fn prop_scalar_cells_match_boxed_reference() {
    let _g = lock();
    let strategy = (0i64..4, arb_field_pairs());
    TestRunner::default()
        .run(&strategy, |(tag, pairs)| {
            let fa: Vec<Field> = pairs.iter().map(|p| p.0).collect();
            let fb: Vec<Field> = pairs.iter().map(|p| p.1).collect();
            diff_check(tag, &fa, &fb)
        })
        .expect("scalar cells match the boxed reference");
}

#[test]
fn prop_record_update_matches() {
    let _g = lock();
    let strategy =
        (0i64..4, arb_fields(), arb_field(), any::<bool>(), any::<prop::sample::Index>());
    TestRunner::default()
        .run(&strategy, |(tag, fields, newf, shared, idx)| {
            let slot = idx.index(fields.len());
            update_check(tag, &fields, slot, newf, shared)
        })
        .expect("record update matches the fresh-build reference");
}

#[test]
fn prop_reuse_matches() {
    let _g = lock();
    TestRunner::default()
        .run(&(0i64..4, arb_fields()), |(tag, fields)| reuse_check(tag, &fields))
        .expect("reuse matches a fresh build");
}
