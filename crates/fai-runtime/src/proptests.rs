//! Property-based tests for the runtime invariants.
//!
//! All cases run under the global [`lock`] (the live-object counter and console
//! sink are process-global) and assert **reference-count balance** — the live
//! count returns to its starting value — alongside value correctness. The
//! `TestRunner` is driven manually so the lock is held across every case.
//!
//! The live-object counter is compiled in only under `debug_assertions`, so the
//! balance assertions hold only in a debug build (the default for `cargo test`).

use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

use super::*;
use crate::tests::lock;

/// Runs `body` over many generated `strategy` values with the runtime lock held.
fn check<S, F>(strategy: S, body: F)
where
    S: Strategy,
    F: Fn(S::Value) -> Result<(), TestCaseError>,
{
    let _guard = lock();
    TestRunner::default().run(&strategy, body).expect("property holds");
}

const TRUE: Value = 3;
const FALSE: Value = 1;

unsafe extern "C" fn code_add(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: two owned arguments; `fai_int_add` consumes both.
    unsafe { fai_int_add(*args, *args.add(1)) }
}

#[test]
fn prop_box_int_round_trips_and_tags_correctly() {
    check(any::<i64>(), |n| {
        let base = live_count();
        let v = fai_box_int(n);
        prop_assert_eq!(unbox_int(v), n);
        // Immediate exactly when the value fits 63 bits.
        prop_assert_eq!(is_boxed(v), !fits_immediate(n));
        fai_drop(v);
        prop_assert_eq!(live_count(), base);
        Ok(())
    });
}

#[test]
fn prop_arithmetic_matches_wrapping() {
    check((any::<i64>(), any::<i64>()), |(a, b)| {
        let base = live_count();
        let (xa, xb) = (fai_box_int(a), fai_box_int(b));
        for (got, expected) in [
            (fai_int_add(fai_dup(xa), fai_dup(xb)), a.wrapping_add(b)),
            (fai_int_sub(fai_dup(xa), fai_dup(xb)), a.wrapping_sub(b)),
            (fai_int_mul(fai_dup(xa), fai_dup(xb)), a.wrapping_mul(b)),
        ] {
            prop_assert_eq!(unbox_int(got), expected);
            fai_drop(got);
        }
        fai_drop(xa);
        fai_drop(xb);
        prop_assert_eq!(live_count(), base);
        Ok(())
    });
}

#[test]
fn prop_division_matches_wrapping() {
    let nonzero = (any::<i64>(), any::<i64>().prop_filter("nonzero divisor", |b| *b != 0));
    check(nonzero, |(a, b)| {
        let base = live_count();
        let div = fai_int_div(fai_box_int(a), fai_box_int(b));
        prop_assert_eq!(unbox_int(div), a.wrapping_div(b));
        fai_drop(div);
        let rem = fai_int_rem(fai_box_int(a), fai_box_int(b));
        prop_assert_eq!(unbox_int(rem), a.wrapping_rem(b));
        fai_drop(rem);
        prop_assert_eq!(live_count(), base);
        Ok(())
    });
}

#[test]
fn prop_comparisons_match() {
    check((any::<i64>(), any::<i64>()), |(a, b)| {
        let want = |c: bool| if c { TRUE } else { FALSE };
        prop_assert_eq!(fai_int_lt(fai_box_int(a), fai_box_int(b)), want(a < b));
        prop_assert_eq!(fai_int_le(fai_box_int(a), fai_box_int(b)), want(a <= b));
        prop_assert_eq!(fai_int_gt(fai_box_int(a), fai_box_int(b)), want(a > b));
        prop_assert_eq!(fai_int_ge(fai_box_int(a), fai_box_int(b)), want(a >= b));
        Ok(())
    });
}

#[test]
fn prop_equality_is_reflexive_and_correct_on_ints() {
    check((any::<i64>(), any::<i64>()), |(a, b)| {
        let base = live_count();
        prop_assert_eq!(fai_equal(fai_box_int(a), fai_box_int(a)), TRUE);
        let want = if a == b { TRUE } else { FALSE };
        prop_assert_eq!(fai_equal(fai_box_int(a), fai_box_int(b)), want);
        prop_assert_eq!(live_count(), base);
        Ok(())
    });
}

#[test]
fn prop_string_concat_preserves_bytes() {
    check(
        (
            proptest::collection::vec(any::<u8>(), 0..64),
            proptest::collection::vec(any::<u8>(), 0..64),
        ),
        |(a, b)| {
            let base = live_count();
            let sa = make_string(&a);
            let sb = make_string(&b);
            let cat = fai_string_concat(sa, sb);
            // SAFETY: `cat` is a live boxed string; copy out before dropping it.
            let got = unsafe { string_bytes(cat) }.to_vec();
            let mut expected = a.clone();
            expected.extend_from_slice(&b);
            prop_assert_eq!(got, expected);
            fai_drop(cat);
            prop_assert_eq!(live_count(), base);
            Ok(())
        },
    );
}

#[test]
fn prop_string_equality_matches_bytes() {
    let bytes = || proptest::collection::vec(any::<u8>(), 0..32);
    check((bytes(), bytes()), |(a, b)| {
        let base = live_count();
        let want = if a == b { TRUE } else { FALSE };
        prop_assert_eq!(fai_equal(make_string(&a), make_string(&b)), want);
        prop_assert_eq!(live_count(), base);
        Ok(())
    });
}

#[test]
fn prop_apply_n_is_split_invariant() {
    // Applying a 2-ary closure as [a, b], or [a] then [b], yields the same sum
    // and leaves nothing live.
    check((any::<i64>(), any::<i64>(), 0u8..=2), |(a, b, split)| {
        let base = live_count();
        // SAFETY: a freshly built arity-2 closure; args are owned immediates/ints.
        let result = unsafe {
            let add = fai_make_closure(code_add as *const u8, 2, 0, std::ptr::null());
            match split {
                0 => {
                    let args = [fai_box_int(a), fai_box_int(b)];
                    fai_apply_n(add, 2, args.as_ptr())
                }
                _ => {
                    let first = [fai_box_int(a)];
                    let pap = fai_apply_n(add, 1, first.as_ptr());
                    let second = [fai_box_int(b)];
                    fai_apply_n(pap, 1, second.as_ptr())
                }
            }
        };
        prop_assert_eq!(unbox_int(result), a.wrapping_add(b));
        fai_drop(result);
        prop_assert_eq!(live_count(), base);
        Ok(())
    });
}

#[test]
fn prop_reference_counting_is_balanced() {
    // Build a boxed value, duplicate it `dups` times (so the count is dups + 1),
    // then drop it that many times: the object is freed exactly at zero.
    check(0usize..32, |dups| {
        let base = live_count();
        let value = fai_box_int(1 << 62); // boxed (heap) so it is actually counted
        prop_assert_eq!(live_count(), base + 1);
        for _ in 0..dups {
            fai_dup(value);
        }
        for _ in 0..dups {
            fai_drop(value);
        }
        prop_assert_eq!(live_count(), base + 1, "still one owner after balanced dup/drop");
        fai_drop(value);
        prop_assert_eq!(live_count(), base, "freed at the final drop");
        Ok(())
    });
}

#[test]
fn prop_dup_drop_on_immediates_never_allocates() {
    check(any::<i64>().prop_filter("immediate", |n| fits_immediate(*n)), |n| {
        let base = live_count();
        let v = imm_int(n);
        fai_dup(v);
        fai_drop(v);
        prop_assert_eq!(live_count(), base, "immediates are not heap objects");
        Ok(())
    });
}
