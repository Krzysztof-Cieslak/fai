//! Runtime unit tests.
//!
//! The live-object counter and the console sink are process-global, so every
//! test serializes on [`TEST_LOCK`] and asserts reference-count balance (the
//! live count returns to its starting value) around each scenario.

use std::sync::{Mutex, MutexGuard};

use super::*;

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Serializes runtime tests (shared global counter + sink) and tolerates a
/// poisoned lock from an earlier panicking test.
fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `true`/`false` as Fai `Bool` immediates.
const TRUE: Value = 3;
const FALSE: Value = 1;

/// A value just past the 63-bit immediate range, so it must be boxed.
const BIG: i64 = 1 << 62;

fn int_eq(v: Value, n: i64) -> bool {
    let boxed = fai_box_int(n);
    let eq = fai_equal(v, boxed);
    fai_drop(boxed);
    eq == TRUE
}

// --- Test "Fai functions" (the compiled-code ABI). ------------------------

unsafe extern "C" fn code_id(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: `args` holds one owned value; returning it transfers ownership.
    unsafe { *args }
}

unsafe extern "C" fn code_add(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: `args` holds two owned values.
    unsafe {
        let a = *args;
        let b = *args.add(1);
        let r = fai_int_add(a, b);
        fai_drop(a);
        fai_drop(b);
        r
    }
}

unsafe extern "C" fn code_const(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: returns arg0, drops arg1.
    unsafe {
        let a = *args;
        fai_drop(*args.add(1));
        a
    }
}

unsafe extern "C" fn code_addenv(env: *const i64, args: *const i64) -> Value {
    // SAFETY: env[0] is borrowed; args[0] is owned (consumed).
    unsafe {
        let x = *env;
        let y = *args;
        let r = fai_int_add(x, y);
        fai_drop(y);
        r
    }
}

unsafe extern "C" fn code_make_adder(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: arg0 is owned; its ownership moves into the returned closure.
    unsafe {
        let x = *args;
        let env = [x];
        fai_make_closure(code_addenv as *const u8, 1, 1, env.as_ptr())
    }
}

fn closure(code: unsafe extern "C" fn(*const i64, *const i64) -> Value, arity: u64) -> Value {
    // SAFETY: no captures, so the null env pointer is never read.
    unsafe { fai_make_closure(code as *const u8, arity, 0, std::ptr::null()) }
}

// --- Integers ------------------------------------------------------------

#[test]
fn immediate_int_arithmetic() {
    let _g = lock();
    let base = live_count();
    assert!(int_eq(fai_int_add(imm_int(2), imm_int(3)), 5));
    assert!(int_eq(fai_int_sub(imm_int(10), imm_int(4)), 6));
    assert!(int_eq(fai_int_mul(imm_int(6), imm_int(7)), 42));
    assert!(int_eq(fai_int_div(imm_int(20), imm_int(5)), 4));
    assert!(int_eq(fai_int_rem(imm_int(20), imm_int(7)), 6));
    assert_eq!(live_count(), base);
}

#[test]
fn overflow_is_boxed_and_preserves_value() {
    let _g = lock();
    let base = live_count();
    let big = fai_box_int(BIG);
    assert!(is_boxed(big), "value past 63 bits must be boxed");
    assert!(int_eq(big, BIG));
    fai_drop(big);

    // An immediate that overflows under multiplication boxes its result.
    let prod = fai_int_mul(imm_int(BIG / 2), imm_int(4));
    assert!(is_boxed(prod));
    assert!(int_eq(prod, (BIG / 2).wrapping_mul(4)));
    fai_drop(prod);
    assert_eq!(live_count(), base);
}

#[test]
fn comparisons() {
    let _g = lock();
    assert_eq!(fai_int_lt(imm_int(1), imm_int(2)), TRUE);
    assert_eq!(fai_int_lt(imm_int(2), imm_int(2)), FALSE);
    assert_eq!(fai_int_le(imm_int(2), imm_int(2)), TRUE);
    assert_eq!(fai_int_gt(imm_int(3), imm_int(2)), TRUE);
    assert_eq!(fai_int_ge(imm_int(2), imm_int(3)), FALSE);
}

// Division/remainder by zero aborts the process (`fai_panic`), which a unit test
// cannot catch; it is exercised by the driver's end-to-end error tests instead.

// --- Equality ------------------------------------------------------------

#[test]
fn equality_over_kinds() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_equal(imm_int(7), imm_int(7)), TRUE);
    assert_eq!(fai_equal(imm_int(7), imm_int(8)), FALSE);
    assert_eq!(fai_equal(FAI_UNIT, FAI_UNIT), TRUE);

    let a = fai_int_to_string(imm_int(123));
    let b = fai_int_to_string(imm_int(123));
    let c = fai_int_to_string(imm_int(124));
    assert_eq!(fai_equal(a, b), TRUE);
    assert_eq!(fai_equal(a, c), FALSE);
    fai_drop(a);
    fai_drop(b);
    fai_drop(c);

    // A boxed Int never equals a small immediate.
    let big = fai_box_int(BIG);
    assert_eq!(fai_equal(big, imm_int(0)), FALSE);
    fai_drop(big);
    assert_eq!(live_count(), base);
}

// --- Strings & the console ----------------------------------------------

#[test]
fn string_concat_and_console_capture() {
    let _g = lock();
    let base = live_count();
    let a = fai_int_to_string(imm_int(1));
    let b = fai_string_concat(a, a); // "11"
    capture_start();
    fai_console_write_line(FAI_UNIT, b);
    let out = capture_take();
    assert_eq!(out, "11\n");
    fai_drop(a);
    fai_drop(b);
    assert_eq!(live_count(), base);
}

// --- Closures & application ---------------------------------------------

#[test]
fn apply_exact() {
    let _g = lock();
    let base = live_count();
    let add = closure(code_add, 2);
    let args = [imm_int(2), imm_int(3)];
    // SAFETY: `add` is a closure of arity 2; `args` holds two owned values.
    let r = unsafe { fai_apply_n(add, 2, args.as_ptr()) };
    assert!(int_eq(r, 5));
    assert_eq!(live_count(), base, "closure consumed by exact application");
}

#[test]
fn apply_partial_then_complete() {
    let _g = lock();
    let base = live_count();
    let add = closure(code_add, 2);
    let one = [imm_int(1)];
    // SAFETY: under-application yields a partial application.
    let pap = unsafe { fai_apply_n(add, 1, one.as_ptr()) };
    assert!(is_boxed(pap));
    let four = [imm_int(4)];
    // SAFETY: completing the partial application.
    let r = unsafe { fai_apply_n(pap, 1, four.as_ptr()) };
    assert!(int_eq(r, 5));
    assert_eq!(live_count(), base, "closure + pap fully released");
}

#[test]
fn apply_over() {
    let _g = lock();
    let base = live_count();
    // make_adder : Int -> (Int -> Int); applying it to two args over-applies.
    let make_adder = closure(code_make_adder, 1);
    let args = [imm_int(3), imm_int(4)];
    // SAFETY: over-application calls make_adder then applies the result.
    let r = unsafe { fai_apply_n(make_adder, 2, args.as_ptr()) };
    assert!(int_eq(r, 7));
    assert_eq!(live_count(), base);
}

#[test]
fn closure_releases_captured_environment() {
    let _g = lock();
    let base = live_count();
    // Capture a boxed (heap) Int, then drop the closure: the env must be freed.
    let captured = fai_box_int(BIG);
    let env = [captured];
    // SAFETY: env holds one owned value transferred into the closure.
    let clos = unsafe { fai_make_closure(code_addenv as *const u8, 1, 1, env.as_ptr()) };
    assert_eq!(live_count(), base + 2, "closure + boxed Int are live");
    fai_drop(clos);
    assert_eq!(live_count(), base, "dropping the closure released its capture");
}

#[test]
fn const_drops_second_argument() {
    let _g = lock();
    let base = live_count();
    let k = closure(code_const, 2);
    // const x y = x, where y is a heap value that must be dropped.
    let y = fai_box_int(BIG);
    let args = [imm_int(9), y];
    // SAFETY: applying `const`; it returns arg0 and drops arg1 (the boxed Int).
    let r = unsafe { fai_apply_n(k, 2, args.as_ptr()) };
    assert!(int_eq(r, 9));
    assert_eq!(live_count(), base, "the dropped second argument was freed");
}

#[test]
fn run_entry_reports_clean_exit() {
    let _g = lock();
    // A trivial entry: `main runtime = runtime`-like; returns its (immediate) arg.
    let entry = closure(code_id, 1);
    let code = run_entry(entry);
    assert_eq!(code, 0);
}
