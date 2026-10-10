//! Structural-hash runtime unit tests for [`fai_hash`]/[`fai_hash_borrowed`].
//!
//! The central invariant is that hashing agrees with structural equality:
//! `fai_equal(a, b)` ⇒ `fai_hash(a) == fai_hash(b)`. These tests exercise that
//! across every value kind (immediates, boxed `Int`/`Float`, inline-vs-slice
//! `String`, data cells, arrays, the niche `None` sentinel), plus the result
//! shape (a non-negative immediate) and the ownership convention (the consuming
//! form releases its operand; the borrowing form does not). Every test holds the
//! global [`lock`] and asserts reference-count balance (the live count returns to
//! its start); the counter is compiled in only under `debug_assertions`, the
//! default for `cargo test`.

use super::*;
use crate::tests::lock;

/// A value just past the 63-bit immediate range, so it must be boxed.
const BIG: i64 = 1 << 62;

/// Builds a boxed data cell with constructor `tag` and the given fields (ownership
/// of each field transfers in), via the constructor primitive.
fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: `fields` points to `fields.len()` owned values.
    unsafe { fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

/// Builds an array from `elems` (ownership of each transfers in).
fn arr(elems: &[Value]) -> Value {
    let mut a = fai_array_with_capacity(imm_int(elems.len() as i64));
    for &e in elems {
        a = fai_array_push(a, e);
    }
    a
}

#[test]
fn hash_result_is_a_nonnegative_immediate() {
    let _g = lock();
    let base = live_count();
    // A boxed operand still yields an immediate (odd low bit) that is non-negative.
    let h = fai_hash(fai_box_int(BIG));
    assert_eq!(h & 1, 1, "a hash is an immediate Int");
    assert!(h >> 1 >= 0, "a hash is non-negative");
    assert_eq!(live_count(), base, "the consuming form releases its operand");
}

#[test]
fn hash_is_deterministic() {
    let _g = lock();
    let base = live_count();
    let s1 = fai_int_to_string(imm_int(123_456));
    let s2 = fai_int_to_string(imm_int(123_456));
    assert_eq!(
        fai_hash_borrowed(s1),
        fai_hash_borrowed(s2),
        "equal strings hash equally and deterministically"
    );
    fai_drop(s1);
    fai_drop(s2);
    assert_eq!(live_count(), base, "the borrowing form leaves operands to the caller");
}

#[test]
fn distinct_small_ints_hash_distinctly() {
    // The immediate path is `mix64(payload)`, and `mix64` is a bijection, so a few
    // distinct small ints never collide once reduced (they differ in low bits).
    let _g = lock();
    let mut seen = std::collections::HashSet::new();
    for n in -8..=8 {
        assert!(seen.insert(fai_hash(imm_int(n))), "no collision among small ints (n = {n})");
    }
}

#[test]
fn boxed_int_hash_is_deterministic() {
    let _g = lock();
    let base = live_count();
    // Two separately-boxed copies of the same overflowed value hash equally.
    assert_eq!(fai_hash(fai_box_int(BIG)), fai_hash(fai_box_int(BIG)));
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn inline_string_and_slice_view_hash_equal() {
    let _g = lock();
    let base = live_count();
    // A 40-byte content `C` reached two ways: an inline buffer, and a slice view of
    // `C` inside a longer base. `drop 4` over a 44-byte base keeps a 40-byte
    // suffix, which is large enough (≥ 32, and 40*4 ≥ 44) to be returned as a view.
    let mut prefixed = make_string(b"bbbb");
    prefixed = fai_string_concat(prefixed, make_string(&[b'a'; 40]));
    let view = fai_string_drop(imm_int(4), prefixed); // a borrowing slice view of 40 'a's
    // SAFETY: `view` is a boxed string-like value.
    assert!(unsafe { is_string_slice(view) }, "the suffix is retained as a slice view, not copied");
    let inline = make_string(&[b'a'; 40]);
    assert_eq!(
        fai_equal_borrowed(view, inline),
        3,
        "the view and the inline string are equal by content"
    );
    assert_eq!(
        fai_hash_borrowed(view),
        fai_hash_borrowed(inline),
        "equal strings hash equally regardless of inline-vs-slice representation"
    );
    fai_drop(view);
    fai_drop(inline);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn data_cells_hash_structurally() {
    let _g = lock();
    let base = live_count();
    // Two separately-built `Some 5`-shaped cells (tag 1, one field) hash equally.
    let a = data(1, &[imm_int(5)]);
    let b = data(1, &[imm_int(5)]);
    assert_eq!(fai_hash_borrowed(a), fai_hash_borrowed(b), "equal data cells hash equally");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn data_cell_field_order_is_significant() {
    let _g = lock();
    let base = live_count();
    // `Pair 1 2` and `Pair 2 1` are unequal, so they should hash differently.
    let a = data(0, &[imm_int(1), imm_int(2)]);
    let b = data(0, &[imm_int(2), imm_int(1)]);
    assert_ne!(fai_hash_borrowed(a), fai_hash_borrowed(b), "field order affects the hash");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base, "leak-free");
}

#[track_caller]
fn distributed_pairs(keys: impl IntoIterator<Item = (i64, i64)>) {
    let _guard = lock();
    let baseline = live_count();
    let mut buckets = [0usize; 1024];
    let mut hashes = std::collections::HashSet::new();
    let mut count = 0;
    for (x, y) in keys {
        let pair = data(0, &[make_int(x), make_int(y)]);
        let hash = values_hash(pair);
        hashes.insert(hash);
        buckets[hash as usize & 1023] += 1;
        count += 1;
        fai_drop(pair);
    }
    assert_eq!(count, 4096);
    assert_eq!(hashes.len(), count, "structured pairs have distinct full hashes");
    assert!(buckets.iter().filter(|count| **count > 0).count() >= 970);
    assert!(*buckets.iter().max().unwrap() <= 24);
    assert_eq!(live_count(), baseline);
}

#[test]
fn grid_pair_keys_have_no_dense_bucket_clusters() {
    distributed_pairs((-32..32).flat_map(|x| (-32..32).map(move |y| (x, y))));
}

#[test]
fn equal_coordinate_keys_have_no_dense_bucket_clusters() {
    distributed_pairs((-2048..2048).map(|value| (value, value)));
}

#[test]
fn opposite_coordinate_keys_have_no_dense_bucket_clusters() {
    distributed_pairs((-2048..2048).map(|value| (value, -value)));
}

#[test]
fn raw_and_boxed_float_records_keep_identical_hashes() {
    let _guard = lock();
    let baseline = live_count();
    let fields =
        [0xfff8_0000_0000_1234u64 as i64, 0, 1.25f64.to_bits() as i64, (-0.0f64).to_bits() as i64];
    let descriptor = intern_data_descriptor(15);
    // SAFETY: all four fields are initialized raw Float slots.
    let raw = unsafe { fai_make_data_scalar(descriptor, 0, 4, fields.as_ptr()) };
    let boxed = data(0, &fields.map(|bits| fai_box_float(bits)));
    assert!(values_equal(raw, boxed));
    assert_eq!(values_hash(raw), values_hash(boxed));
    fai_drop(raw);
    fai_drop(boxed);
    assert_eq!(live_count(), baseline);
}

#[test]
fn immediate_pair_hash_keeps_its_exact_value() {
    let _guard = lock();
    let baseline = live_count();
    let pair = data(0, &[imm_int(-7), imm_int(42)]);
    assert_eq!(values_hash(pair), 0xe463_eab4_18b4_5b5a);
    fai_drop(pair);
    assert_eq!(live_count(), baseline);
}

#[test]
fn full_width_pair_hash_keeps_its_exact_value() {
    let _guard = lock();
    let baseline = live_count();
    let pair = data(0, &[make_int(i64::MIN), make_int(i64::MAX)]);
    assert_eq!(values_hash(pair), 0x809d_3de7_a19e_f370);
    fai_drop(pair);
    assert_eq!(live_count(), baseline);
}

#[test]
fn arrays_hash_structurally() {
    let _g = lock();
    let base = live_count();
    let a = arr(&[imm_int(1), imm_int(2), imm_int(3)]);
    let b = arr(&[imm_int(1), imm_int(2), imm_int(3)]);
    assert_eq!(fai_hash_borrowed(a), fai_hash_borrowed(b), "equal arrays hash equally");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn array_order_and_length_are_significant() {
    let _g = lock();
    let base = live_count();
    let a = arr(&[imm_int(1), imm_int(2), imm_int(3)]);
    let rev = arr(&[imm_int(3), imm_int(2), imm_int(1)]);
    let short = arr(&[imm_int(1), imm_int(2)]);
    assert_ne!(fai_hash_borrowed(a), fai_hash_borrowed(rev), "element order affects the hash");
    assert_ne!(fai_hash_borrowed(a), fai_hash_borrowed(short), "length affects the hash");
    fai_drop(a);
    fai_drop(rev);
    fai_drop(short);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn nested_data_hashes_equal() {
    let _g = lock();
    let base = live_count();
    // `Node (Some 5) [1, 2]` built twice hashes equally (recursion through both a
    // data field and an array field).
    let a = data(2, &[data(1, &[imm_int(5)]), arr(&[imm_int(1), imm_int(2)])]);
    let b = data(2, &[data(1, &[imm_int(5)]), arr(&[imm_int(1), imm_int(2)])]);
    assert_eq!(fai_hash_borrowed(a), fai_hash_borrowed(b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn niche_none_hashes_like_standard_none() {
    // The niche `None` sentinel and the standard `None` (the immediate nullary
    // tag-0, which `imm_int(0)` also is) compare equal, so they must hash equally.
    let _g = lock();
    let base = live_count();
    assert_eq!(
        fai_hash_borrowed(fai_none_value()),
        fai_hash_borrowed(imm_int(0)),
        "the niche None sentinel hashes like the standard None"
    );
    assert_eq!(live_count(), base, "leak-free (immortal sentinel, immediate operand)");
}
