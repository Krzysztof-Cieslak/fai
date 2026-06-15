//! `Array` runtime unit tests: the contiguous, growable buffer intrinsics.
//!
//! These exercise the five intrinsics directly — [`fai_array_with_capacity`],
//! [`fai_array_length`], [`fai_array_get`], [`fai_array_set`], [`fai_array_push`]
//! (and the borrowed readers) — plus the structural [`values_equal`]/
//! [`values_compare`] branches and the drop child-scan. Every test holds the
//! global [`lock`] and asserts reference-count balance (the live count returns to
//! its start); the in-place/grow tests additionally pin the cumulative allocation
//! counter, the observable signal that a mutation recycled in place (no
//! allocation) or grew (exactly one). Both counters are compiled in only under
//! `debug_assertions`, so these assertions are meaningful in a debug build (the
//! default for `cargo test`).

use super::*;
use crate::tests::lock;

/// A value past the 63-bit immediate range, so [`fai_box_int`] heap-allocates it
/// (and it is therefore counted by the live-object counter).
const BIG: i64 = 1 << 62;

/// A heap (boxed) `Int`.
fn big() -> Value {
    fai_box_int(BIG)
}

/// Builds an array from `elems` via `withCapacity(len)` then in-place `push`es, so
/// construction takes exactly one allocation. Transfers ownership of each element.
fn arr_from(elems: &[Value]) -> Value {
    let mut a = fai_array_with_capacity(imm_int(elems.len() as i64));
    for &e in elems {
        a = fai_array_push(a, e);
    }
    a
}

/// Reads element `i` of `a` as a plain `i64`, borrowing `a` (balancing the dup
/// that [`fai_array_get_borrowed`] performs).
fn arr_get_int(a: Value, i: i64) -> i64 {
    let e = fai_array_get_borrowed(a, imm_int(i));
    let n = unbox_int(e);
    fai_drop(e);
    n
}

/// An array's live length.
fn len_of(a: Value) -> usize {
    // SAFETY: `a` is a boxed array.
    unsafe { array_len(a) }
}

// ===========================================================================
// Construction & length.
// ===========================================================================

#[test]
fn with_capacity_is_empty_and_leak_free() {
    let _g = lock();
    let base = live_count();
    let a = fai_array_with_capacity(imm_int(8));
    assert_eq!(len_of(a), 0, "a fresh array is empty");
    assert_eq!(live_count(), base + 1);
    fai_drop(a);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn builder_allocates_once() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let a = arr_from(&[imm_int(1), imm_int(2), imm_int(3)]);
    assert_eq!(allocations(), 1, "withCapacity allocates one buffer; in-place pushes none");
    assert_eq!(len_of(a), 3);
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn length_consuming_drops_the_array() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    assert_eq!(unbox_int(fai_array_length(a)), 2);
    assert_eq!(live_count(), base, "fai_array_length consumes its operand");
}

#[test]
fn length_borrowed_does_not_consume() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    assert_eq!(unbox_int(fai_array_length_borrowed(a)), 2);
    assert_eq!(live_count(), base + 1, "the borrowed reader keeps the array");
    fai_drop(a);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// get.
// ===========================================================================

#[test]
fn get_consuming_returns_element_and_drops_array() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(10), imm_int(20), imm_int(30)]);
    let e = fai_array_get(a, imm_int(1));
    assert_eq!(unbox_int(e), 20);
    assert_eq!(live_count(), base, "the array is consumed; the immediate carries no count");
}

#[test]
fn get_consuming_keeps_a_boxed_element_alive() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[big(), big()]);
    assert_eq!(live_count(), base + 3, "the array plus two boxed elements");
    let e = fai_array_get(a, imm_int(0));
    assert_eq!(unbox_int(e), BIG);
    assert_eq!(
        live_count(),
        base + 1,
        "array and the other element freed; the returned one survives"
    );
    fai_drop(e);
    assert_eq!(live_count(), base);
}

#[test]
fn get_borrowed_does_not_consume() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[big()]);
    let e = fai_array_get_borrowed(a, imm_int(0));
    assert_eq!(unbox_int(e), BIG);
    fai_drop(e);
    fai_drop(a);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// set: in place when unique, copy when shared.
// ===========================================================================

#[test]
fn set_unique_is_in_place() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2), imm_int(3)]);
    reset_allocations();
    let a = fai_array_set(a, imm_int(1), imm_int(20));
    assert_eq!(allocations(), 0, "a unique set overwrites in place");
    assert_eq!(arr_get_int(a, 1), 20);
    assert_eq!(arr_get_int(a, 0), 1, "other elements untouched");
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn set_unique_releases_the_old_boxed_element() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[big(), imm_int(2)]);
    assert_eq!(live_count(), base + 2, "array plus one boxed element");
    let a = fai_array_set(a, imm_int(0), imm_int(7));
    assert_eq!(live_count(), base + 1, "the replaced boxed element was released");
    assert_eq!(arr_get_int(a, 0), 7);
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn set_shared_copies() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    fai_dup(a); // now shared (rc 2)
    reset_allocations();
    let b = fai_array_set(a, imm_int(0), imm_int(9));
    assert_eq!(allocations(), 1, "a shared set copies the buffer");
    assert_eq!(arr_get_int(b, 0), 9, "the copy is updated");
    assert_eq!(arr_get_int(a, 0), 1, "the original is unchanged");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// push: in place with spare capacity, grow when full, copy when shared.
// ===========================================================================

#[test]
fn push_unique_within_capacity_is_in_place() {
    let _g = lock();
    let base = live_count();
    let a = fai_array_with_capacity(imm_int(4));
    reset_allocations();
    let a = fai_array_push(a, imm_int(1));
    let a = fai_array_push(a, imm_int(2));
    assert_eq!(allocations(), 0, "pushes within capacity append in place");
    assert_eq!(len_of(a), 2);
    assert_eq!(arr_get_int(a, 1), 2);
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn push_unique_full_grows_once_and_frees_old() {
    let _g = lock();
    let base = live_count();
    // Capacity 2, filled.
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    assert_eq!(live_count(), base + 1);
    reset_allocations();
    let a = fai_array_push(a, imm_int(3)); // full → grow
    assert_eq!(allocations(), 1, "growing reallocates exactly once");
    assert_eq!(live_count(), base + 1, "the old buffer is freed; one array stays live");
    assert_eq!(len_of(a), 3);
    assert_eq!(arr_get_int(a, 2), 3);
    assert_eq!(arr_get_int(a, 0), 1, "moved elements are intact");
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn push_grow_moves_boxed_elements_without_leak() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[big(), big()]); // cap 2, full, two boxed elements
    assert_eq!(live_count(), base + 3);
    let a = fai_array_push(a, big()); // grows, moving the two boxed elements
    assert_eq!(live_count(), base + 4, "three boxed elements plus one array");
    fai_drop(a);
    assert_eq!(live_count(), base, "every boxed element freed");
}

#[test]
fn push_shared_copies() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    fai_dup(a); // shared
    reset_allocations();
    let b = fai_array_push(a, imm_int(3));
    assert_eq!(allocations(), 1, "a shared push copies");
    assert_eq!(len_of(a), 2, "the original keeps its length");
    assert_eq!(len_of(b), 3, "the copy has the new element");
    assert_eq!(arr_get_int(b, 2), 3);
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// Drop: the child scan releases live elements (not spare capacity).
// ===========================================================================

#[test]
fn drop_releases_boxed_elements() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[big(), big()]);
    assert_eq!(live_count(), base + 3);
    fai_drop(a);
    assert_eq!(live_count(), base, "the array and its boxed elements are freed");
}

#[test]
fn drop_ignores_spare_capacity() {
    let _g = lock();
    let base = live_count();
    // Capacity 8, only one live element: the seven spare slots are uninitialized
    // and must not be scanned on drop.
    let mut a = fai_array_with_capacity(imm_int(8));
    a = fai_array_push(a, big());
    assert_eq!(live_count(), base + 2);
    fai_drop(a);
    assert_eq!(live_count(), base, "only the one live element was released");
}

#[test]
fn nested_array_drop_is_leak_free() {
    let _g = lock();
    let base = live_count();
    let inner1 = arr_from(&[imm_int(1)]);
    let inner2 = arr_from(&[imm_int(2)]);
    let outer = arr_from(&[inner1, inner2]);
    assert_eq!(live_count(), base + 3, "outer plus two inner arrays");
    fai_drop(outer);
    assert_eq!(live_count(), base, "nested arrays freed");
}

// ===========================================================================
// Structural equality & ordering.
// ===========================================================================

#[test]
fn equal_arrays_are_equal() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    let b = arr_from(&[imm_int(1), imm_int(2)]);
    assert!(values_equal(a, b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn different_lengths_are_unequal() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    let b = arr_from(&[imm_int(1), imm_int(2), imm_int(3)]);
    assert!(!values_equal(a, b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn different_elements_are_unequal() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    let b = arr_from(&[imm_int(1), imm_int(9)]);
    assert!(!values_equal(a, b));
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn capacity_does_not_affect_equality() {
    let _g = lock();
    let base = live_count();
    // `a` is tight (cap 2); `b` carries slack (cap 8). Same elements ⇒ equal.
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    let mut b = fai_array_with_capacity(imm_int(8));
    b = fai_array_push(b, imm_int(1));
    b = fai_array_push(b, imm_int(2));
    assert!(values_equal(a, b), "capacity is invisible to equality");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn compare_is_lexicographic() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    let b = arr_from(&[imm_int(2)]);
    assert_eq!(values_compare(a, b), std::cmp::Ordering::Less, "1 < 2 at index 0");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn compare_shorter_prefix_is_less() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(1), imm_int(2)]);
    let b = arr_from(&[imm_int(1), imm_int(2), imm_int(0)]);
    assert_eq!(
        values_compare(a, b),
        std::cmp::Ordering::Less,
        "a prefix sorts before the longer array"
    );
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn equal_arrays_compare_equal() {
    let _g = lock();
    let base = live_count();
    let a = arr_from(&[imm_int(5), imm_int(6)]);
    let b = arr_from(&[imm_int(5), imm_int(6)]);
    assert_eq!(values_compare(a, b), std::cmp::Ordering::Equal);
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// Unboxed `Array Float`: elements are raw `f64` slots, not pointers to boxed
// floats. A float `push` self-tags the buffer (`FAI_FLOAT_ARRAY_DESC`); the
// buffer is a reference-counting leaf; structural ops compare/hash by `f64` bits.
// ===========================================================================

/// A boxed `Float` carrying `f` (one heap allocation, as `fai_box_float` always
/// boxes).
fn boxed_float(f: f64) -> Value {
    fai_box_float(f.to_bits() as i64)
}

/// Builds an `Array Float` from `elems` via `withCapacity` + in-place `push`es.
/// Each element is boxed (transiently) for the push, which unboxes it and stores
/// the raw `f64`, self-tagging the buffer — so the finished array holds **no**
/// per-element boxes.
fn arr_from_floats(elems: &[f64]) -> Value {
    let mut a = fai_array_with_capacity(imm_int(elems.len() as i64));
    for &e in elems {
        a = fai_array_push(a, boxed_float(e));
    }
    a
}

/// Reads float element `i` of `a`, borrowing `a` (the borrowed get re-boxes a raw
/// slot into a fresh owned `Float`, released here).
fn arr_get_float(a: Value, i: i64) -> f64 {
    let e = fai_array_get_borrowed(a, imm_int(i));
    let f = unbox_float(e);
    fai_drop(e);
    f
}

#[test]
fn float_array_stores_no_persistent_element_boxes() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0, 3.0]);
    // The headline win: only the buffer is live — the three floats are inline raw
    // `f64` slots, not heap `Float` cells (contrast the boxed-`Int` builder, which
    // leaves three element boxes live).
    assert_eq!(live_count(), base + 1, "only the buffer is live; no per-element float boxes");
    assert_eq!(len_of(a), 3);
    fai_drop(a);
    assert_eq!(live_count(), base, "leak-free (a float array is a drop leaf)");
}

#[test]
fn float_array_get_reads_the_raw_value() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[10.0, 20.0, 30.0]);
    assert_eq!(arr_get_float(a, 1), 20.0);
    assert_eq!(arr_get_float(a, 2), 30.0);
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn float_array_get_consuming_reboxes_and_frees_the_buffer() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.5, 2.5]);
    assert_eq!(live_count(), base + 1, "just the buffer");
    let e = fai_array_get(a, imm_int(0));
    assert_eq!(unbox_float(e), 1.5);
    assert_eq!(live_count(), base + 1, "buffer freed; the re-boxed float survives");
    fai_drop(e);
    assert_eq!(live_count(), base);
}

#[test]
fn float_array_set_unique_is_in_place() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0, 3.0]);
    // Box the new value before snapshotting allocations, so the counter isolates
    // the set itself (which must not copy the buffer).
    let v = boxed_float(20.0);
    reset_allocations();
    let a = fai_array_set(a, imm_int(1), v);
    assert_eq!(allocations(), 0, "a unique float set overwrites in place (no buffer copy)");
    assert_eq!(arr_get_float(a, 1), 20.0);
    assert_eq!(arr_get_float(a, 0), 1.0, "other elements untouched");
    fai_drop(a);
    assert_eq!(live_count(), base, "the consumed value box was released");
}

#[test]
fn float_array_set_shared_copies_raw_slots() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0]);
    fai_dup(a); // now shared (rc 2)
    let v = boxed_float(9.0);
    reset_allocations();
    let b = fai_array_set(a, imm_int(0), v);
    assert_eq!(allocations(), 1, "a shared set copies the buffer once");
    assert_eq!(arr_get_float(b, 0), 9.0, "the copy is updated");
    assert_eq!(arr_get_float(a, 0), 1.0, "the original is unchanged");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn float_array_push_within_capacity_is_in_place() {
    let _g = lock();
    let base = live_count();
    let mut a = fai_array_with_capacity(imm_int(4));
    a = fai_array_push(a, boxed_float(1.0));
    a = fai_array_push(a, boxed_float(2.0));
    assert_eq!(len_of(a), 2);
    assert_eq!(arr_get_float(a, 1), 2.0);
    // The buffer is the only survivor — the two transient value boxes were freed.
    assert_eq!(live_count(), base + 1);
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn float_array_push_full_grows_and_preserves_values() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0]); // cap 2, full
    let a = fai_array_push(a, boxed_float(3.0)); // full → grow (raw word move)
    assert_eq!(len_of(a), 3);
    assert_eq!(arr_get_float(a, 2), 3.0);
    assert_eq!(arr_get_float(a, 0), 1.0, "moved elements are intact");
    assert_eq!(live_count(), base + 1, "old buffer freed; no element boxes");
    fai_drop(a);
    assert_eq!(live_count(), base);
}

#[test]
fn float_array_push_shared_copies_raw_slots() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0]);
    fai_dup(a); // shared
    let b = fai_array_push(a, boxed_float(3.0));
    assert_eq!(len_of(a), 2, "the original keeps its length");
    assert_eq!(len_of(b), 3, "the copy has the new element");
    assert_eq!(arr_get_float(b, 2), 3.0);
    assert_eq!(arr_get_float(a, 1), 2.0, "the shared original's elements are intact");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn float_arrays_are_structurally_equal_by_value() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0]);
    let b = arr_from_floats(&[1.0, 2.0]);
    assert!(values_equal(a, b), "same float content compares equal");
    let c = arr_from_floats(&[1.0, 9.0]);
    assert!(!values_equal(a, c), "differing float content is unequal");
    fai_drop(a);
    fai_drop(b);
    fai_drop(c);
    assert_eq!(live_count(), base);
}

#[test]
fn float_arrays_compare_lexicographically() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0]);
    let b = arr_from_floats(&[1.0, 3.0]);
    assert_eq!(values_compare(a, b), std::cmp::Ordering::Less, "2.0 < 3.0 at index 1");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn float_array_compare_uses_ieee_total_order() {
    let _g = lock();
    let base = live_count();
    // -0.0 and +0.0 differ in bits, so they are unequal and -0.0 sorts first —
    // exactly the total order a boxed `Float` uses.
    let neg = arr_from_floats(&[-0.0]);
    let pos = arr_from_floats(&[0.0]);
    assert!(!values_equal(neg, pos), "-0.0 and +0.0 are bit-unequal");
    assert_eq!(values_compare(neg, pos), std::cmp::Ordering::Less, "-0.0 sorts before +0.0");
    fai_drop(neg);
    fai_drop(pos);
    assert_eq!(live_count(), base);
}

#[test]
fn equal_float_arrays_hash_equally() {
    let _g = lock();
    let base = live_count();
    let a = arr_from_floats(&[1.0, 2.0, 3.0]);
    let b = arr_from_floats(&[1.0, 2.0, 3.0]);
    assert_eq!(values_hash(a), values_hash(b), "equal float arrays hash equally");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

#[test]
fn empty_float_array_is_handled_as_a_plain_array() {
    let _g = lock();
    let base = live_count();
    // An empty `Array Float` is never pushed, so it keeps the plain descriptor —
    // harmless, since no walker loops over its (zero) elements.
    let a = arr_from_floats(&[]);
    let b = arr_from_floats(&[]);
    assert_eq!(len_of(a), 0);
    assert!(values_equal(a, b), "two empty arrays are equal");
    assert_eq!(values_compare(a, b), std::cmp::Ordering::Equal);
    let nonempty = arr_from_floats(&[1.0]);
    assert!(!values_equal(a, nonempty), "empty differs from non-empty");
    assert_eq!(
        values_compare(a, nonempty),
        std::cmp::Ordering::Less,
        "the empty array sorts first"
    );
    fai_drop(a);
    fai_drop(b);
    fai_drop(nonempty);
    assert_eq!(live_count(), base, "leak-free, including the plain-tagged empties");
}
