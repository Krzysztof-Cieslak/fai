//! Shared-count races, exercised on real runtime objects and under Miri.

use std::sync::Barrier;

use super::*;
use crate::tests::lock;

/// Keeps an owning reference on each thread while count changes race with an
/// inspection. The caller's reference survives the joined threads.
fn churn(value: Value, inspect: impl Fn(Value) + Sync) {
    fai_mark_shared(value);
    let start = Barrier::new(3);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let owned = fai_dup(value);
            let inspect = &inspect;
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for _ in 0..16 {
                    inspect(owned);
                    fai_drop(fai_dup(owned));
                }
                fai_drop(owned);
            });
        }
        start.wait();
        for _ in 0..16 {
            inspect(value);
            fai_drop(fai_dup(value));
        }
    });
}

#[test]
fn shared_count_and_repeated_marking_are_race_free() {
    let _guard = lock();
    let base = live_count();
    let value = fai_box_int(i64::MAX);
    churn(value, |v| {
        fai_mark_shared(v);
    });
    // SAFETY: all worker threads joined; the original reference remains live.
    assert_eq!(unsafe { read_u64(as_obj(value), RC_OFFSET) }, MT_FLAG | 1);
    fai_drop(value);
    assert_eq!(live_count(), base);
}

#[test]
fn shared_record_uniqueness_checks_are_race_free() {
    let _guard = lock();
    let base = live_count();
    let fields = [fai_box_int(i64::MAX)];
    // SAFETY: the field transfers into a one-field data cell.
    let value = unsafe { fai_make_data(0, 1, fields.as_ptr()) };
    churn(value, |v| {
        let updated = fai_record_update(fai_dup(v), imm_int(0), imm_int(42));
        assert_eq!(fai_data_field(updated, 0), imm_int(42));
        fai_drop(updated);
    });
    let field = fai_data_field(value, 0);
    assert_eq!(unbox_int(field), i64::MAX);
    fai_drop(field);
    fai_drop(value);
    assert_eq!(live_count(), base);
}

#[test]
fn shared_array_set_uniqueness_checks_are_race_free() {
    let _guard = lock();
    let base = live_count();
    let value = fai_array_push(fai_array_with_capacity(imm_int(4)), imm_int(7));
    churn(value, |v| {
        let updated = fai_array_set(fai_dup(v), imm_int(0), imm_int(42));
        assert_eq!(fai_array_get(updated, imm_int(0)), imm_int(42));
    });
    assert_eq!(fai_array_get(value, imm_int(0)), imm_int(7));
    assert_eq!(live_count(), base);
}

#[test]
fn shared_array_push_uniqueness_checks_are_race_free() {
    let _guard = lock();
    let base = live_count();
    let value = fai_array_push(fai_array_with_capacity(imm_int(4)), imm_int(7));
    churn(value, |v| {
        let updated = fai_array_push(fai_dup(v), imm_int(42));
        assert_eq!(fai_array_length(updated), imm_int(2));
    });
    assert_eq!(fai_array_length(value), imm_int(1));
    assert_eq!(live_count(), base);
}

#[test]
fn shared_string_uniqueness_checks_are_race_free() {
    let _guard = lock();
    let base = live_count();
    let value = make_string(b"base");
    churn(value, |v| {
        let joined = fai_string_concat(fai_dup(v), make_string(b"!"));
        assert_eq!(read_string(joined), b"base!");
        fai_drop(joined);
    });
    assert_eq!(read_string(value), b"base");
    fai_drop(value);
    assert_eq!(live_count(), base);
}

#[test]
fn shared_cell_can_be_reused_and_published_again() {
    let _guard = lock();
    let base = live_count();
    let fields = [fai_box_int(i64::MAX)];
    // SAFETY: one owned field transfers into the data cell.
    let value = unsafe { fai_make_data(0, 1, fields.as_ptr()) };
    churn(value, |_| {});
    let token = fai_drop_reuse(value);
    assert_eq!(token, value);
    let fields = [fai_box_int(i64::MIN)];
    // SAFETY: the token is exclusively owned and has room for one field.
    let reused = unsafe { fai_reuse(token, 0, 1, fields.as_ptr()) };
    // SAFETY: the reconstructed cell is owned by this thread alone.
    assert_eq!(unsafe { rc_load(as_obj(reused)) }, 1);
    churn(reused, |_| {});
    fai_drop(reused);
    assert_eq!(live_count(), base);
}

#[test]
fn compact_metadata_can_be_read_during_shared_count_churn() {
    let _guard = lock();
    let baseline = live_count();
    let fields = [make_int(11), make_int(22)];
    // SAFETY: the two owned immediate fields initialize one compact data cell.
    let value = unsafe { fai_make_data(123, 2, fields.as_ptr()) };
    churn(value, |v| {
        // SAFETY: churn retains an owning reference while other tasks update counts.
        unsafe {
            assert_eq!(object_kind(as_obj(v)), KIND_DATA);
            assert_eq!(object_size(as_obj(v)), 24);
            assert_eq!(object_data_tag(as_obj(v)), 123);
            assert_eq!(data_len(as_obj(v)), 2);
        }
    });
    fai_drop(value);
    assert_eq!(live_count(), baseline);
}

#[test]
fn immortal_count_is_unchanged_by_concurrent_access() {
    let _guard = lock();
    let value = fai_none_value();
    churn(value, |v| {
        fai_mark_shared(v);
    });
    // SAFETY: the immortal count is constant; every worker has joined.
    assert_eq!(unsafe { read_u64(as_obj(value), RC_OFFSET) }, IMMORTAL_RC);
}
