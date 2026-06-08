//! Reference-counted reuse, record update, projection, and dup/drop unit tests.
//!
//! These exercise the in-place machinery directly: [`fai_drop_reuse`] resets a
//! dead cell to a token, [`fai_reuse`] rebuilds into it (or allocates when it
//! cannot), [`fai_record_update`] overwrites a unique record's field in place,
//! and [`fai_data_field`]/[`fai_data_tag`] read through a borrowed base. Every
//! test holds the global [`lock`] and asserts reference-count balance (the live
//! count returns to its start); reuse tests additionally pin the cumulative
//! allocation counter, the observable signal that a recycle did *not* allocate.

use super::*;
use crate::tests::lock;

/// A value past the 63-bit immediate range, so [`fai_box_int`] heap-allocates it
/// (and it is therefore counted by the live-object counter).
const BIG: i64 = 1 << 62;

/// A heap (boxed) `Int`.
fn big() -> Value {
    fai_box_int(BIG)
}

/// The reference count of a boxed value.
fn rc_of(v: Value) -> u64 {
    // SAFETY: `v` is a boxed object pointer.
    unsafe { read_u64(as_obj(v), RC_OFFSET) }
}

/// Builds a boxed data value `{ tag, fields… }`, transferring ownership of each
/// field in.
fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: `fields` holds `len` owned values.
    unsafe { fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

/// Projects field `i` of `v` as a plain `i64` (borrowing `v`); balances the dup
/// that [`fai_data_field`] performs.
fn field_int(v: Value, i: i64) -> i64 {
    let f = fai_data_field(v, i);
    let n = unbox_int(f);
    fai_drop(f);
    n
}

/// Releases a reuse token produced by [`fai_drop_reuse`] in tests that exercise
/// only the reset half: rebuilds a same-size all-zero value in place, then drops
/// it (freeing the cell).
fn discard_token(token: Value, nfields: i64) {
    let fields = vec![imm_int(0); nfields as usize];
    // SAFETY: `token` came from `fai_drop_reuse`; `fields` are owned immediates.
    let v = unsafe { fai_reuse(token, 0, nfields, fields.as_ptr()) };
    fai_drop(v);
}

// ===========================================================================
// fai_drop_reuse: reset a dead cell to a token, or fall back to a plain drop.
// ===========================================================================

#[test]
fn dropreuse_immediate_returns_no_reuse() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_drop_reuse(imm_int(7)), 0, "an immediate has no cell to reuse");
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_unit_returns_no_reuse() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_drop_reuse(FAI_UNIT), 0);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_unique_returns_the_cells_own_memory() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    let token = fai_drop_reuse(cell);
    assert_ne!(token, 0, "a unique cell yields a non-null token");
    assert_eq!(token, cell, "the token is the cell's own memory");
    discard_token(token, 2);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_unique_keeps_memory_live_for_rebuild() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    assert_eq!(live_count(), base + 1);
    let token = fai_drop_reuse(cell);
    assert_eq!(live_count(), base + 1, "reset keeps the memory (not freed)");
    discard_token(token, 2);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_unique_sets_refcount_to_zero() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1)]);
    let token = fai_drop_reuse(cell);
    assert_eq!(rc_of(token), 0, "the reset cell is owned by no one");
    discard_token(token, 1);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_unique_releases_a_boxed_field() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[big(), imm_int(2)]);
    assert_eq!(live_count(), base + 2, "cell plus its boxed field are live");
    let token = fai_drop_reuse(cell);
    assert_eq!(live_count(), base + 1, "the boxed field was released; the cell kept");
    discard_token(token, 2);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_unique_releases_every_boxed_field() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[big(), big(), big()]);
    assert_eq!(live_count(), base + 4);
    let token = fai_drop_reuse(cell);
    assert_eq!(live_count(), base + 1, "all three boxed fields released");
    discard_token(token, 3);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_shared_returns_no_reuse() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1)]);
    fai_dup(cell); // rc = 2
    assert_eq!(fai_drop_reuse(cell), 0, "a shared cell cannot be reused");
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_shared_only_decrements() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1)]);
    fai_dup(cell); // rc = 2
    fai_drop_reuse(cell);
    assert_eq!(rc_of(cell), 1, "the shared reset just dropped one reference");
    assert_eq!(live_count(), base + 1, "the cell is still live");
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_shared_preserves_boxed_fields() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[big()]);
    fai_dup(cell); // rc = 2
    assert_eq!(live_count(), base + 2);
    fai_drop_reuse(cell); // shared: releases nothing, just decrements
    assert_eq!(live_count(), base + 2, "a shared reset releases no fields");
    fai_drop(cell); // now frees the cell and its boxed field
    assert_eq!(live_count(), base);
}

#[test]
fn dropreuse_zero_field_cell_round_trips() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[]); // a boxed nullary cell
    let token = fai_drop_reuse(cell);
    assert_eq!(token, cell, "even a zero-field cell yields its memory");
    discard_token(token, 0);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// fai_reuse: rebuild into a token in place, or allocate when it cannot.
// ===========================================================================

#[test]
fn reuse_null_token_allocates_fresh() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let fields = [imm_int(1), imm_int(2)];
    // SAFETY: the null token forces a fresh allocation; fields are owned.
    let v = unsafe { fai_reuse(0, 5, 2, fields.as_ptr()) };
    assert_eq!(allocations(), 1, "a null token allocates");
    assert_eq!(fai_data_tag(v), imm_int(5));
    assert_eq!(field_int(v, 0), 1);
    assert_eq!(field_int(v, 1), 2);
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_same_size_rebuilds_in_place_without_allocating() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    let token = fai_drop_reuse(cell);
    reset_allocations();
    let fields = [imm_int(7), imm_int(8)];
    // SAFETY: `token` is a reset 2-field cell; the rebuild matches its size.
    let v = unsafe { fai_reuse(token, 3, 2, fields.as_ptr()) };
    assert_eq!(allocations(), 0, "a same-size rebuild reuses the cell");
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_in_place_returns_the_same_pointer() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    let token = fai_drop_reuse(cell);
    let fields = [imm_int(7), imm_int(8)];
    // SAFETY: same-size rebuild into the reset cell.
    let v = unsafe { fai_reuse(token, 3, 2, fields.as_ptr()) };
    assert_eq!(v, cell, "the rebuilt value occupies the reused memory");
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_in_place_writes_tag_fields_and_refcount() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    let token = fai_drop_reuse(cell);
    let fields = [imm_int(11), imm_int(22)];
    // SAFETY: same-size rebuild into the reset cell.
    let v = unsafe { fai_reuse(token, 4, 2, fields.as_ptr()) };
    assert_eq!(rc_of(v), 1, "the rebuilt cell is uniquely owned");
    assert_eq!(fai_data_tag(v), imm_int(4));
    assert_eq!(field_int(v, 0), 11);
    assert_eq!(field_int(v, 1), 22);
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_in_place_accepts_boxed_fields() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    let token = fai_drop_reuse(cell);
    let fields = [big(), imm_int(2)];
    // SAFETY: same-size rebuild; the boxed field's ownership transfers in.
    let v = unsafe { fai_reuse(token, 0, 2, fields.as_ptr()) };
    assert_eq!(live_count(), base + 2, "the reused cell plus its boxed field are live");
    assert_eq!(field_int(v, 0), BIG);
    fai_drop(v); // releases the cell and its boxed field
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_smaller_size_frees_token_and_allocates() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2), imm_int(3)]); // 3 fields
    let token = fai_drop_reuse(cell);
    reset_allocations();
    let fields = [imm_int(9)];
    // SAFETY: the token is a 3-field cell; a 1-field rebuild cannot fit it.
    let v = unsafe { fai_reuse(token, 1, 1, fields.as_ptr()) };
    // The fresh allocation is proven by the counter, not by `v != cell`: the
    // token's memory was freed, so the allocator may legitimately hand it back.
    assert_eq!(allocations(), 1, "a smaller rebuild frees the token and allocates");
    assert_eq!(field_int(v, 0), 9);
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_larger_size_frees_token_and_allocates() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1)]); // 1 field
    let token = fai_drop_reuse(cell);
    reset_allocations();
    let fields = [imm_int(1), imm_int(2), imm_int(3)];
    // SAFETY: the token is a 1-field cell; a 3-field rebuild cannot fit it.
    let v = unsafe { fai_reuse(token, 0, 3, fields.as_ptr()) };
    // The fresh allocation is proven by the counter, not by `v != cell`: the
    // token's memory was freed, so the allocator may legitimately hand it back.
    assert_eq!(allocations(), 1, "a larger rebuild frees the token and allocates");
    assert_eq!(field_int(v, 2), 3);
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn reuse_mismatch_keeps_live_count_balanced() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    let token = fai_drop_reuse(cell);
    let fields = [imm_int(5)];
    // SAFETY: size mismatch frees the token, then allocates the new value.
    let v = unsafe { fai_reuse(token, 0, 1, fields.as_ptr()) };
    assert_eq!(live_count(), base + 1, "one object freed, one allocated");
    fai_drop(v);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// Reuse round-trips: the headline win is recycling without allocating.
// ===========================================================================

#[test]
fn roundtrip_unique_cell_recycles_with_zero_allocations() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    reset_allocations();
    let token = fai_drop_reuse(cell);
    let fields = [imm_int(7), imm_int(8)];
    // SAFETY: same-size rebuild into the just-reset unique cell.
    let v = unsafe { fai_reuse(token, 0, 2, fields.as_ptr()) };
    assert_eq!(allocations(), 0, "a unique cell is recycled, never reallocated");
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn roundtrip_shared_cell_allocates_and_preserves_original() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    fai_dup(cell); // shared: rc = 2
    reset_allocations();
    let token = fai_drop_reuse(cell); // shared -> null token, rc -> 1
    assert_eq!(token, 0);
    let fields = [imm_int(7), imm_int(8)];
    // SAFETY: a null token allocates a fresh value.
    let v = unsafe { fai_reuse(token, 0, 2, fields.as_ptr()) };
    assert_eq!(allocations(), 1, "a shared cell forces a fresh allocation");
    assert_ne!(v, cell, "the original is untouched");
    assert_eq!(field_int(cell, 0), 1, "original field 0 intact");
    assert_eq!(field_int(cell, 1), 2, "original field 1 intact");
    fai_drop(cell);
    fai_drop(v);
    assert_eq!(live_count(), base);
}

#[test]
fn roundtrip_three_sequential_reuses_allocate_nothing() {
    let _g = lock();
    let base = live_count();
    // A small chain of same-size recycles (as a map over a unique spine would do):
    // each cell is reset and immediately rebuilt in place.
    let mut cell = data(1, &[imm_int(0), imm_int(0)]);
    reset_allocations();
    for k in 1..=3 {
        let token = fai_drop_reuse(cell);
        let fields = [imm_int(k), imm_int(k * 10)];
        // SAFETY: a same-size rebuild into the reset cell.
        cell = unsafe { fai_reuse(token, 1, 2, fields.as_ptr()) };
    }
    assert_eq!(allocations(), 0, "every recycle reused the same memory");
    assert_eq!(field_int(cell, 0), 3);
    assert_eq!(field_int(cell, 1), 30);
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// fai_record_update: overwrite a unique record's field in place; copy if shared.
// ===========================================================================

#[test]
fn recupd_unique_updates_in_place_no_allocation() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2)]);
    reset_allocations();
    let updated = fai_record_update(rec, imm_int(1), imm_int(9));
    assert_eq!(updated, rec, "in-place update keeps the same object");
    assert_eq!(allocations(), 0, "no allocation for an in-place update");
    assert_eq!(field_int(updated, 1), 9);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_unique_updates_field_zero() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2)]);
    let updated = fai_record_update(rec, imm_int(0), imm_int(42));
    assert_eq!(field_int(updated, 0), 42);
    assert_eq!(field_int(updated, 1), 2, "the other field is unchanged");
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_unique_updates_middle_of_three_fields() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2), imm_int(3)]);
    let updated = fai_record_update(rec, imm_int(1), imm_int(20));
    assert_eq!(field_int(updated, 0), 1);
    assert_eq!(field_int(updated, 1), 20);
    assert_eq!(field_int(updated, 2), 3);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_unique_releases_old_boxed_field() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[big(), imm_int(2)]);
    assert_eq!(live_count(), base + 2, "record plus its boxed field");
    let updated = fai_record_update(rec, imm_int(0), imm_int(5));
    assert_eq!(live_count(), base + 1, "the replaced boxed field was released");
    assert_eq!(field_int(updated, 0), 5);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_unique_stores_a_boxed_new_value() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2)]);
    let updated = fai_record_update(rec, imm_int(1), big());
    assert_eq!(live_count(), base + 2, "the record plus its new boxed field are live");
    assert_eq!(field_int(updated, 1), BIG);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_unique_twice_keeps_same_pointer() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2)]);
    reset_allocations();
    let once = fai_record_update(rec, imm_int(0), imm_int(7));
    let twice = fai_record_update(once, imm_int(1), imm_int(8));
    assert_eq!(twice, rec, "both in-place updates kept the same memory");
    assert_eq!(allocations(), 0, "neither update allocated");
    assert_eq!(field_int(twice, 0), 7);
    assert_eq!(field_int(twice, 1), 8);
    fai_drop(twice);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_shared_copies_and_allocates_once() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2)]);
    fai_dup(rec); // shared
    reset_allocations();
    let updated = fai_record_update(fai_dup(rec), imm_int(1), imm_int(9));
    assert_eq!(allocations(), 1, "a shared update copies");
    assert_ne!(updated, rec, "the copy is a different object");
    fai_drop(rec);
    fai_drop(rec);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_shared_leaves_original_unchanged() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[imm_int(1), imm_int(2)]);
    fai_dup(rec); // shared
    let updated = fai_record_update(fai_dup(rec), imm_int(0), imm_int(99));
    assert_eq!(field_int(rec, 0), 1, "the shared original keeps its field");
    assert_eq!(field_int(updated, 0), 99, "the copy carries the new field");
    fai_drop(rec);
    fai_drop(rec);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn recupd_shared_dups_copied_boxed_fields() {
    let _g = lock();
    let base = live_count();
    let rec = data(0, &[big(), imm_int(2)]); // boxed field at slot 0
    fai_dup(rec); // shared; rc = 2
    assert_eq!(live_count(), base + 2);
    // Update slot 1, so slot 0's boxed field is copied (duplicated) into the new
    // record rather than replaced.
    let updated = fai_record_update(fai_dup(rec), imm_int(1), imm_int(7));
    assert_eq!(live_count(), base + 3, "original, its field (now shared), and the copy");
    assert_eq!(field_int(updated, 0), BIG, "the boxed field was shared into the copy");
    fai_drop(rec);
    fai_drop(rec);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// fai_data_tag / fai_data_field: read through a borrowed base.
// ===========================================================================

#[test]
fn proj_tag_of_boxed_value() {
    let _g = lock();
    let base = live_count();
    let cell = data(7, &[imm_int(1)]);
    assert_eq!(fai_data_tag(cell), imm_int(7), "reads the constructor tag");
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn proj_tag_of_nullary_immediate() {
    let _g = lock();
    let base = live_count();
    // A nullary constructor is an immediate whose payload is its tag.
    assert_eq!(fai_data_tag(imm_int(3)), imm_int(3));
    assert_eq!(live_count(), base);
}

#[test]
fn proj_tag_borrows_the_base() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1)]);
    let _ = fai_data_tag(cell);
    assert_eq!(rc_of(cell), 1, "reading the tag does not consume the base");
    assert_eq!(live_count(), base + 1, "the base is still live");
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn proj_field_borrows_base_and_dups_the_field() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[big()]);
    let f = fai_data_field(cell, 0); // borrows cell, dups the field
    assert_eq!(rc_of(cell), 1, "the base is borrowed, not consumed");
    assert_eq!(rc_of(f), 2, "the projected field was duplicated out");
    fai_drop(f);
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn proj_repeated_reads_then_single_base_drop() {
    let _g = lock();
    let base = live_count();
    let cell = data(1, &[imm_int(10), big()]);
    // Several borrowing reads of the still-live base.
    assert_eq!(fai_data_tag(cell), imm_int(1));
    assert_eq!(field_int(cell, 0), 10);
    assert_eq!(field_int(cell, 1), BIG);
    assert_eq!(field_int(cell, 0), 10);
    assert_eq!(rc_of(cell), 1, "borrowing reads never changed the base count");
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn proj_field_outlives_base_drop() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[big()]);
    let f = fai_data_field(cell, 0); // dups the boxed field
    fai_drop(cell); // base released; the field survives (it was duplicated)
    assert_eq!(live_count(), base + 1, "the projected field is still alive");
    assert_eq!(unbox_int(f), BIG);
    fai_drop(f);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// dup / drop over data values.
// ===========================================================================

#[test]
fn dupdrop_data_balances() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[imm_int(1), imm_int(2)]);
    fai_dup(cell);
    assert_eq!(rc_of(cell), 2);
    fai_drop(cell);
    assert_eq!(rc_of(cell), 1);
    fai_drop(cell);
    assert_eq!(live_count(), base);
}

#[test]
fn dupdrop_data_releases_boxed_fields_at_zero() {
    let _g = lock();
    let base = live_count();
    let cell = data(0, &[big(), big()]);
    assert_eq!(live_count(), base + 3, "cell plus two boxed fields");
    fai_drop(cell);
    assert_eq!(live_count(), base, "dropping the cell released both fields");
}

#[test]
fn dupdrop_nested_data_releases_recursively() {
    let _g = lock();
    let base = live_count();
    let inner = data(0, &[big()]); // cell + boxed field
    let outer = data(1, &[inner, imm_int(0)]); // owns inner
    assert_eq!(live_count(), base + 3, "outer + inner + boxed leaf");
    fai_drop(outer);
    assert_eq!(live_count(), base, "the whole tree was released");
}

#[test]
fn dupdrop_shared_inner_released_once_per_owner() {
    let _g = lock();
    let base = live_count();
    let inner = data(0, &[imm_int(5)]);
    fai_dup(inner); // shared between two owners
    let a = data(1, &[inner, imm_int(0)]);
    // SAFETY of accounting: `inner` (rc 2) is owned by `a` and still by us.
    assert_eq!(live_count(), base + 2, "inner + a");
    fai_drop(a); // drops inner once (rc 2 -> 1); inner survives
    assert_eq!(live_count(), base + 1, "inner outlives a");
    fai_drop(inner);
    assert_eq!(live_count(), base);
}

// ===========================================================================
// Iterative drop: an arbitrarily deep structure is released without overflowing
// the native stack (a recursive child scan would crash here).
// ===========================================================================

/// Builds a cons list `[1, 1, …]` of `n` immediate-headed cells, from the tail up
/// (iteratively, so construction itself does not recurse).
fn deep_int_list(n: usize) -> Value {
    let mut list = imm_int(NIL_TAG);
    for _ in 0..n {
        list = data(CONS_TAG, &[imm_int(1), list]);
    }
    list
}

#[test]
fn drop_of_a_very_deep_list_does_not_overflow_the_stack() {
    let _g = lock();
    let base = live_count();
    // Far deeper than any native call stack would tolerate under recursive drop.
    let list = deep_int_list(1_000_000);
    assert_eq!(live_count(), base + 1_000_000, "one cell per element");
    fai_drop(list);
    assert_eq!(live_count(), base, "the whole spine was released, leak-free");
}

#[test]
fn drop_of_a_deep_list_with_boxed_heads_releases_every_cell_and_head() {
    let _g = lock();
    let base = live_count();
    // Boxed heads are reference-counted children that are *not* the last field, so
    // they exercise the worklist (and its heap spill past the inline buffer).
    let n = 100_000usize;
    let mut list = imm_int(NIL_TAG);
    for _ in 0..n {
        list = data(CONS_TAG, &[big(), list]);
    }
    assert_eq!(live_count(), base + 2 * n as i64, "a cell and a boxed head each");
    fai_drop(list);
    assert_eq!(live_count(), base, "every cell and head released, leak-free");
}

#[test]
fn drop_reuse_of_a_deep_unique_list_releases_the_spine_iteratively() {
    let _g = lock();
    let base = live_count();
    // Resetting the head of a *unique* deep list releases its single tail child,
    // which (being unique) cascades down the whole spine — iteratively, no
    // overflow. The head cell's own memory is returned as a reuse token.
    let list = deep_int_list(1_000_000);
    let token = fai_drop_reuse(list);
    assert_ne!(token, NO_REUSE, "the unique head cell yields its memory");
    assert_eq!(live_count(), base + 1, "only the reset head cell remains");
    discard_token(token, 2);
    assert_eq!(live_count(), base, "the reset cell is released, leak-free");
}
