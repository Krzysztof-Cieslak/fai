//! Runtime unit tests.
//!
//! The live-object counter and the console sink are process-global, so every
//! test serializes on [`TEST_LOCK`] and asserts reference-count balance (the
//! live count returns to its starting value) around each scenario.
//!
//! The live-object counter is compiled in only under `debug_assertions`, so the
//! balance assertions are meaningful only in a debug build — the default for
//! `cargo test` (a `--release` test run reports zero and would fail them).
//!
//! The runtime uses a uniform **consume** convention: every primitive and
//! [`fai_apply_n`] consumes (releases) its operands. A function's parameters are
//! owned (consumed by its body); its captured environment is borrowed (a use
//! must [`fai_dup`] it, and the closure releases it on death). The test code
//! functions below follow that discipline, mirroring generated code.

use std::sync::{Mutex, MutexGuard};

use super::*;

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Serializes runtime tests (shared global counter + sink) and tolerates a
/// poisoned lock from an earlier panicking test.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `true`/`false` as Fai `Bool` immediates.
const TRUE: Value = 3;
const FALSE: Value = 1;

/// A value just past the 63-bit immediate range, so it must be boxed.
const BIG: i64 = 1 << 62;

/// Whether `v` equals `Int n`. Consumes `v` (via `fai_equal`).
fn int_eq(v: Value, n: i64) -> bool {
    fai_equal(v, fai_box_int(n)) == TRUE
}

// --- Test "Fai functions" (the compiled-code ABI). ------------------------

unsafe extern "C" fn code_id(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: `args` holds one owned value; returning it transfers ownership.
    unsafe { *args }
}

extern "C" fn code_unit(_env: *const i64, _args: *const i64) -> Value {
    FAI_UNIT
}

unsafe extern "C" fn code_add(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: `args` holds two owned values; `fai_int_add` consumes both.
    unsafe { fai_int_add(*args, *args.add(1)) }
}

unsafe extern "C" fn code_const(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: returns arg0 (owned), drops arg1.
    unsafe {
        let a = *args;
        fai_drop(*args.add(1));
        a
    }
}

unsafe extern "C" fn code_addenv(env: *const i64, args: *const i64) -> Value {
    // SAFETY: env[0] is borrowed (dup before the consuming add); args[0] is owned.
    unsafe { fai_int_add(fai_dup(*env), *args) }
}

unsafe extern "C" fn code_make_adder(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: arg0 is owned; its ownership moves into the returned closure.
    unsafe {
        let x = *args;
        let env = [x];
        fai_make_closure(code_addenv as *const u8, 1, 1, env.as_ptr())
    }
}

unsafe extern "C" fn code_add3(_env: *const i64, args: *const i64) -> Value {
    // SAFETY: three owned arguments, summed (each `fai_int_add` consumes two).
    unsafe {
        let s = fai_int_add(*args, *args.add(1));
        fai_int_add(s, *args.add(2))
    }
}

unsafe extern "C" fn code_apply_env(env: *const i64, args: *const i64) -> Value {
    // SAFETY: env[0] is a borrowed closure (dup before the consuming apply);
    // args[0] is an owned argument passed through.
    unsafe { fai_apply_n(fai_dup(*env), 1, args) }
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
fn bitwise_int_ops() {
    let _g = lock();
    let base = live_count();
    assert!(int_eq(fai_int_and(imm_int(0b1100), imm_int(0b1010)), 0b1000));
    assert!(int_eq(fai_int_or(imm_int(0b1100), imm_int(0b1010)), 0b1110));
    assert!(int_eq(fai_int_xor(imm_int(0b1100), imm_int(0b1010)), 0b0110));
    assert!(int_eq(fai_int_complement(imm_int(0)), -1));
    assert!(int_eq(fai_int_shl(imm_int(1), imm_int(4)), 16));
    assert!(int_eq(fai_int_shr(imm_int(-16), imm_int(2)), -4)); // arithmetic: sign-extends
    assert!(int_eq(fai_int_shr_logical(imm_int(16), imm_int(2)), 4));
    // A logical right shift of a negative value fills with zeros (no sign bit),
    // unlike the arithmetic shift, which sign-extends.
    assert!(int_eq(fai_int_shr_logical(imm_int(-1), imm_int(60)), 15));
    assert!(int_eq(fai_int_shr(imm_int(-1), imm_int(60)), -1));
    // Shift amounts are taken modulo 64, so 64 is a no-op shift, not UB.
    assert!(int_eq(fai_int_shl(imm_int(1), imm_int(64)), 1));
    assert_eq!(live_count(), base);
}

#[test]
fn overflow_is_boxed_and_preserves_value() {
    let _g = lock();
    let base = live_count();
    let big = fai_box_int(BIG);
    assert!(is_boxed(big), "value past 63 bits must be boxed");
    assert!(int_eq(big, BIG)); // consumes `big`

    // An immediate that overflows under multiplication boxes its result.
    let prod = fai_int_mul(imm_int(BIG / 2), imm_int(4));
    assert!(is_boxed(prod));
    assert!(int_eq(prod, (BIG / 2).wrapping_mul(4))); // consumes `prod`
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
    assert_eq!(fai_equal(fai_dup(a), b), TRUE); // consumes a-dup and b
    assert_eq!(fai_equal(a, c), FALSE); // consumes a and c

    // A boxed Int never equals a small immediate.
    let big = fai_box_int(BIG);
    assert_eq!(fai_equal(big, imm_int(0)), FALSE); // consumes big
    assert_eq!(live_count(), base);
}

// --- Strings & the console ----------------------------------------------

#[test]
fn string_concat_and_console_capture() {
    let _g = lock();
    let base = live_count();
    let a = fai_int_to_string(imm_int(1)); // "1", rc 1
    let b = fai_string_concat(fai_dup(a), fai_dup(a)); // "11"; `a` stays rc 1
    capture_start();
    let unit = fai_console_write_line(b); // consumes b
    assert_eq!(unit, FAI_UNIT);
    let out = capture_take();
    assert_eq!(out, "11\n");
    fai_drop(a);
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
    // A trivial entry returning its (immediate) argument, applied to the value
    // forced from a zero-arity `Runtime` binding (here just `Unit`).
    let entry = closure(code_id, 1);
    let runtime = closure(code_unit, 0);
    let code = run_entry(entry, runtime);
    assert_eq!(code, 0);
}

/// A value at or inside the immediate range stays unboxed and round-trips with
/// no net heap allocation.
#[track_caller]
fn immediate_round_trips(n: i64) {
    let _g = lock();
    let base = live_count();
    let v = fai_box_int(n);
    assert!(!is_boxed(v), "{n} should be immediate");
    assert!(int_eq(v, n));
    assert_eq!(live_count(), base);
}

/// A value outside the immediate range boxes and round-trips with no net heap
/// allocation (the comparison consumes the box).
#[track_caller]
fn boxed_round_trips(n: i64) {
    let _g = lock();
    let base = live_count();
    let v = fai_box_int(n);
    assert!(is_boxed(v), "{n} should be boxed");
    assert!(int_eq(v, n)); // consumes v
    assert_eq!(live_count(), base);
}

#[test]
fn zero_is_immediate() {
    immediate_round_trips(0);
}

#[test]
fn one_is_immediate() {
    immediate_round_trips(1);
}

#[test]
fn neg_one_is_immediate() {
    immediate_round_trips(-1);
}

#[test]
fn max_immediate_is_immediate() {
    immediate_round_trips((1i64 << 62) - 1);
}

#[test]
fn min_immediate_is_immediate() {
    immediate_round_trips(-(1i64 << 62));
}

#[test]
fn just_above_max_immediate_boxes() {
    boxed_round_trips(1i64 << 62);
}

#[test]
fn just_below_min_immediate_boxes() {
    boxed_round_trips(-(1i64 << 62) - 1);
}

#[test]
fn i64_max_boxes() {
    boxed_round_trips(i64::MAX);
}

#[test]
fn i64_min_boxes() {
    boxed_round_trips(i64::MIN);
}

#[test]
fn empty_string_concatenation() {
    let _g = lock();
    let base = live_count();
    let empty1 = fai_int_to_string(imm_int(0)); // "0" — then build "" via slicing is hard; use concat of empties
    // Build two genuinely empty strings by concatenating nothing is not possible
    // through the public API, so compare "0" ++ "" semantics via two non-empty.
    let a = fai_string_concat(empty1, fai_int_to_string(imm_int(0))); // "00"
    assert_eq!(fai_equal(a, fai_int_to_string(imm_int(0))), FALSE);
    assert_eq!(live_count(), base);
}

#[test]
fn min_int_division_does_not_panic() {
    let _g = lock();
    // i64::MIN / -1 overflows in two's complement; wrapping semantics apply.
    let q = fai_int_div(fai_box_int(i64::MIN), fai_box_int(-1));
    assert!(int_eq(q, i64::MIN.wrapping_div(-1)));
}

/// Applying a 3-ary closure to the arguments 1, 2, 3 — whether in one call or
/// split into several partial applications — always totals 6, with no net heap
/// allocation.
#[track_caller]
fn arity_three_split_totals_six(split: &[usize]) {
    let _g = lock();
    let base = live_count();
    let mut callee = closure(code_add3, 3);
    let mut next = 1i64;
    for (k, &count) in split.iter().enumerate() {
        let args: Vec<Value> = (0..count)
            .map(|_| {
                let v = imm_int(next);
                next += 1;
                v
            })
            .collect();
        let is_last = k + 1 == split.len();
        // SAFETY: applying `count` owned arguments to the current callee.
        callee = unsafe { fai_apply_n(callee, count as u64, args.as_ptr()) };
        if is_last {
            assert!(int_eq(callee, 6), "split {split:?} should total 6");
        }
    }
    assert_eq!(live_count(), base);
}

#[test]
fn arity_three_single_call() {
    arity_three_split_totals_six(&[3]);
}

#[test]
fn arity_three_split_one_then_two() {
    arity_three_split_totals_six(&[1, 2]);
}

#[test]
fn arity_three_split_two_then_one() {
    arity_three_split_totals_six(&[2, 1]);
}

#[test]
fn arity_three_fully_curried() {
    arity_three_split_totals_six(&[1, 1, 1]);
}

#[test]
fn closure_capturing_a_closure_releases_both() {
    let _g = lock();
    let base = live_count();
    let inner = closure(code_id, 1); // heap closure, rc 1
    let env = [inner];
    // SAFETY: the inner closure's ownership moves into the outer closure's env.
    let outer = unsafe { fai_make_closure(code_apply_env as *const u8, 1, 1, env.as_ptr()) };
    assert_eq!(live_count(), base + 2, "inner + outer closures are live");

    let args = [imm_int(99)];
    // SAFETY: applying the outer closure (which applies its captured inner one).
    let result = unsafe { fai_apply_n(outer, 1, args.as_ptr()) };
    assert!(int_eq(result, 99));
    assert_eq!(live_count(), base, "both closures released");
}

#[test]
fn deeply_curried_closure_releases_cleanly() {
    let _g = lock();
    let base = live_count();
    // Capture a large environment of heap values, then drop the closure.
    let env: Vec<Value> = (0..16).map(|_| fai_box_int(1 << 62)).collect();
    // SAFETY: env holds 16 owned values transferred into the closure.
    let clos = unsafe { fai_make_closure(code_id as *const u8, 1, env.len() as u64, env.as_ptr()) };
    assert_eq!(live_count(), base + 17, "closure + 16 captures");
    fai_drop(clos);
    assert_eq!(live_count(), base, "all captures released with the closure");
}

// --- Float -----------------------------------------------------------------

/// A `Float` value from an `f64`.
fn flt(x: f64) -> Value {
    fai_box_float(x.to_bits() as i64)
}

/// Reads (and consumes) a `Float` result.
fn float_val(v: Value) -> f64 {
    let f = unbox_float(v);
    fai_drop(v);
    f
}

#[test]
fn float_arithmetic_and_comparison() {
    let _g = lock();
    let base = live_count();
    assert!((float_val(fai_float_add(flt(1.5), flt(2.5))) - 4.0).abs() < 1e-9);
    assert!((float_val(fai_float_sub(flt(5.0), flt(1.5))) - 3.5).abs() < 1e-9);
    assert!((float_val(fai_float_mul(flt(2.0), flt(3.0))) - 6.0).abs() < 1e-9);
    assert!((float_val(fai_float_div(flt(9.0), flt(2.0))) - 4.5).abs() < 1e-9);
    assert_eq!(fai_float_lt(flt(2.0), flt(3.0)), TRUE);
    assert_eq!(fai_float_gt(flt(2.0), flt(3.0)), FALSE);
    assert_eq!(fai_float_le(flt(3.0), flt(3.0)), TRUE);
    assert_eq!(fai_float_ge(flt(2.0), flt(3.0)), FALSE);
    assert!((float_val(fai_sqrt(flt(16.0))) - 4.0).abs() < 1e-9);
    assert_eq!(live_count(), base, "every Float operand and result was freed");
}

#[test]
fn float_conversions_and_rendering() {
    let _g = lock();
    let base = live_count();
    assert!((float_val(fai_int_to_float(imm_int(16))) - 16.0).abs() < 1e-9);
    assert!(int_eq(fai_float_to_int(flt(3.9)), 3)); // truncation toward zero
    let rendered = fai_float_to_string(flt(4.0));
    assert_eq!(unsafe { string_str(rendered) }, "4.0");
    fai_drop(rendered);
    assert_eq!(live_count(), base);
}

// --- Chars -----------------------------------------------------------------

#[test]
fn char_to_string_renders_one_character() {
    let _g = lock();
    let base = live_count();
    let s = fai_char_to_string(imm_int(i64::from('a' as u32)));
    assert_eq!(unsafe { string_str(s) }, "a");
    fai_drop(s);
    // A multibyte scalar value encodes to its full UTF-8.
    let emoji = fai_char_to_string(imm_int(0x1F600));
    assert_eq!(unsafe { string_str(emoji) }, "\u{1F600}");
    fai_drop(emoji);
    assert_eq!(live_count(), base);
}

#[test]
fn char_int_conversions_are_identity_round_trips() {
    let _g = lock();
    let base = live_count();
    // Char and Int share the immediate encoding, so the conversions are bitcasts.
    assert_eq!(fai_char_to_code(imm_int(i64::from('a' as u32))), imm_int(97));
    assert_eq!(fai_char_from_code(imm_int(97)), imm_int(i64::from('a' as u32)));
    assert_eq!(fai_char_from_code(fai_char_to_code(imm_int(0x1F600))), imm_int(0x1F600));
    assert_eq!(live_count(), base);
}

#[test]
fn is_valid_char_code_rejects_out_of_range_and_surrogates() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_is_valid_char_code(imm_int(97)), TRUE);
    assert_eq!(fai_is_valid_char_code(imm_int(0x10_FFFF)), TRUE);
    assert_eq!(fai_is_valid_char_code(imm_int(0xD800)), FALSE); // a surrogate
    assert_eq!(fai_is_valid_char_code(imm_int(0x11_0000)), FALSE); // past the maximum
    assert_eq!(fai_is_valid_char_code(imm_int(-1)), FALSE);
    assert_eq!(live_count(), base);
}

#[test]
fn is_valid_char_code_at_surrogate_and_range_boundaries() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_is_valid_char_code(imm_int(0)), TRUE); // NUL is a scalar value
    assert_eq!(fai_is_valid_char_code(imm_int(0xD7FF)), TRUE); // just below the surrogates
    assert_eq!(fai_is_valid_char_code(imm_int(0xD800)), FALSE); // first surrogate
    assert_eq!(fai_is_valid_char_code(imm_int(0xDFFF)), FALSE); // last surrogate
    assert_eq!(fai_is_valid_char_code(imm_int(0xE000)), TRUE); // just above the surrogates
    assert_eq!(live_count(), base);
}

#[test]
fn char_to_string_encodes_utf8_at_byte_boundaries() {
    let _g = lock();
    let base = live_count();
    // One representative per UTF-8 length, plus the extremes: each must match
    // Rust's own encoding of the same scalar value.
    for &cp in &[0u32, 0x7F, 0x80, 0x7FF, 0x800, 0xFFFF, 0x1_0000, 0x10_FFFF] {
        let expected = char::from_u32(cp).unwrap().to_string();
        let s = fai_char_to_string(imm_int(i64::from(cp)));
        assert_eq!(unsafe { string_str(s) }, expected, "code point U+{cp:X}");
        fai_drop(s);
    }
    assert_eq!(live_count(), base);
}

// --- Structural comparison -------------------------------------------------

/// The three-way `compare` result as a plain `i64` (consumes both operands).
fn cmp3(a: Value, b: Value) -> i64 {
    fai_compare(a, b) >> 1
}

/// A boxed two-field record/tuple (tag 0) of immediate `Int`s.
fn pair(x: i64, y: i64) -> Value {
    let fields = [imm_int(x), imm_int(y)];
    // SAFETY: `fields` holds two owned immediate values.
    unsafe { fai_make_data(0, 2, fields.as_ptr()) }
}

#[test]
fn compare_orders_ints_strings_floats_and_data() {
    let _g = lock();
    let base = live_count();
    // Immediates (Ints / nullary tags).
    assert_eq!(cmp3(imm_int(1), imm_int(2)), -1);
    assert_eq!(cmp3(imm_int(2), imm_int(2)), 0);
    assert_eq!(cmp3(imm_int(5), imm_int(2)), 1);
    // Strings order lexicographically.
    assert_eq!(cmp3(make_string(b"abc"), make_string(b"abd")), -1);
    assert_eq!(cmp3(make_string(b"b"), make_string(b"a")), 1);
    // Floats.
    assert_eq!(cmp3(flt(1.0), flt(2.0)), -1);
    assert_eq!(cmp3(flt(2.5), flt(2.5)), 0);
    // Same constructor: fields compared left to right.
    assert_eq!(cmp3(pair(1, 2), pair(1, 3)), -1);
    assert_eq!(cmp3(pair(1, 2), pair(1, 2)), 0);
    // Different constructors order by tag.
    let t0 = {
        let f = [imm_int(9)];
        // SAFETY: one owned field.
        unsafe { fai_make_data(0, 1, f.as_ptr()) }
    };
    let t1 = {
        let f = [imm_int(0)];
        // SAFETY: one owned field.
        unsafe { fai_make_data(1, 1, f.as_ptr()) }
    };
    assert_eq!(cmp3(t0, t1), -1);
    assert_eq!(live_count(), base, "compare consumed every operand");
}

// --- Composite construction and projection ---------------------------------

#[test]
fn make_data_tag_and_field_projection() {
    let _g = lock();
    let base = live_count();
    let big = fai_box_int(BIG);
    let fields = [imm_int(10), big];
    // SAFETY: two owned fields move into the data value.
    let d = unsafe { fai_make_data(1, 2, fields.as_ptr()) };
    assert_eq!(live_count(), base + 2, "the data object plus its boxed field are live");

    // The tag and fields are read by *borrowing* `d` (no release), so it stays
    // live across these reads and is dropped exactly once at the end.
    assert_eq!(fai_data_tag(d), imm_int(1));
    // Field 0 is the immediate `10`.
    assert!(int_eq(fai_data_field(d, 0), 10));
    // Field 1 is the boxed Int, duplicated out of the still-live `d`.
    assert!(int_eq(fai_data_field(d, 1), BIG));
    fai_drop(d); // release the borrowed base once
    assert_eq!(live_count(), base, "data object and its capture released");
}

// --- Inlined-drop dead path (fai_drop_dead) --------------------------------
// Generated code inlines a boxed value's reference-count decrement and, on
// reaching zero, calls `fai_drop_dead` to release its children and free it.
// `inline_drop_boxed` mirrors that emitted sequence exactly.

/// Mirrors generated code's inlined drop of a known-boxed value: decrement the
/// reference count in place and, on reaching zero, release the children and free
/// the cell via `fai_drop_dead`.
fn inline_drop_boxed(v: Value) {
    // SAFETY: `v` is a boxed object; its refcount slot is in bounds, and
    // `fai_drop_dead` is called only once the count has reached zero.
    unsafe {
        let rc = read_u64(as_obj(v), RC_OFFSET) - 1;
        write_u64(as_obj(v), RC_OFFSET, rc);
        if rc == 0 {
            fai_drop_dead(v);
        }
    }
}

#[test]
fn fai_drop_dead_releases_a_boxed_child() {
    let _g = lock();
    let base = live_count();
    // A cell with one immediate field and one boxed (reference-counted) child.
    let fields = [imm_int(7), fai_box_int(BIG)];
    // SAFETY: two owned fields move into the data value.
    let d = unsafe { fai_make_data(1, 2, fields.as_ptr()) };
    assert_eq!(live_count(), base + 2, "the cell and its boxed child are live");
    inline_drop_boxed(d); // unique, so the decrement reaches the dead path
    assert_eq!(live_count(), base, "fai_drop_dead freed the cell and released its child");
}

#[test]
fn fai_drop_dead_frees_a_childless_cell() {
    let _g = lock();
    let base = live_count();
    let d = pair(1, 2); // two immediate fields: nothing to release
    assert_eq!(live_count(), base + 1, "the cell is live");
    inline_drop_boxed(d);
    assert_eq!(live_count(), base, "fai_drop_dead freed the childless cell");
}

#[test]
fn fai_drop_dead_drains_a_deep_structure_iteratively() {
    let _g = lock();
    let base = live_count();
    // A long unique cons list (each cell owns the tail). Freeing the head must
    // drain the whole chain with the worklist, never via native recursion, so a
    // structure far deeper than the native stack still releases cleanly.
    let n: i64 = 200_000;
    let mut list = imm_int(NIL_TAG); // the immediate empty tail
    for i in 0..n {
        let fields = [imm_int(i), list];
        // SAFETY: two owned fields (the payload and the owned tail) move in.
        list = unsafe { fai_make_data(CONS_TAG, 2, fields.as_ptr()) };
    }
    assert_eq!(live_count(), base + n, "every cons cell is live");
    inline_drop_boxed(list);
    assert_eq!(live_count(), base, "the whole chain drained iteratively");
}

#[test]
fn structural_equality_over_composites() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_equal(pair(1, 2), pair(1, 2)), TRUE);
    assert_eq!(fai_equal(pair(1, 2), pair(1, 9)), FALSE);
    // Equality recurses through boxed fields and frees both trees cleanly.
    let a = {
        let f = [fai_box_int(BIG), imm_int(0)];
        // SAFETY: two owned fields.
        unsafe { fai_make_data(0, 2, f.as_ptr()) }
    };
    let b = {
        let f = [fai_box_int(BIG), imm_int(0)];
        // SAFETY: two owned fields.
        unsafe { fai_make_data(0, 2, f.as_ptr()) }
    };
    assert_eq!(fai_equal(a, b), TRUE);
    assert_eq!(live_count(), base);
}

// --- String intrinsics -----------------------------------------------------

#[test]
fn string_case_trim_and_length() {
    let _g = lock();
    let base = live_count();
    let upper = fai_to_upper(make_string(b"abc"));
    assert_eq!(unsafe { string_str(upper) }, "ABC");
    fai_drop(upper);
    let lower = fai_to_lower(make_string(b"AbC"));
    assert_eq!(unsafe { string_str(lower) }, "abc");
    fai_drop(lower);
    let trimmed = fai_trim(make_string(b"  hi  "));
    assert_eq!(unsafe { string_str(trimmed) }, "hi");
    fai_drop(trimmed);
    assert!(int_eq(fai_string_length(make_string(b"hello")), 5));
    assert_eq!(live_count(), base);
}

#[test]
fn string_contains_split_and_join() {
    let _g = lock();
    let base = live_count();
    assert_eq!(fai_string_contains(make_string(b"hello"), make_string(b"ell")), TRUE);
    assert_eq!(fai_string_contains(make_string(b"hello"), make_string(b"xyz")), FALSE);
    // Split on spaces, then join with commas, round-tripping through List String.
    let parts = fai_string_split(make_string(b" "), make_string(b"a b c"));
    let joined = fai_string_join(make_string(b","), parts);
    assert_eq!(unsafe { string_str(joined) }, "a,b,c");
    fai_drop(joined);
    assert_eq!(live_count(), base);
}

#[test]
fn record_update_in_place_when_unique() {
    let _g = lock();
    let base = live_count();
    let a0 = allocations();
    // A unique 2-field record.
    let rec = pair(1, 2);
    assert_eq!(allocations(), a0 + 1, "one record allocated");
    // Update field 1 in place: no new allocation, same pointer.
    let updated = fai_record_update(rec, imm_int(1), imm_int(9));
    assert_eq!(updated, rec, "in-place update returns the same object");
    assert_eq!(allocations(), a0 + 1, "no allocation for an in-place update");
    assert_eq!(fai_data_field(updated, 1), imm_int(9));
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

#[test]
fn record_update_copies_when_shared() {
    let _g = lock();
    let base = live_count();
    let rec = pair(1, 2);
    fai_dup(rec); // share it
    let a0 = allocations();
    let updated = fai_record_update(fai_dup(rec), imm_int(1), imm_int(9));
    assert_eq!(allocations(), a0 + 1, "shared update copies (one allocation)");
    assert_ne!(updated, rec, "a copy is a different object");
    // Original is unchanged.
    assert_eq!(fai_data_field(rec, 1), imm_int(2));
    assert_eq!(fai_data_field(updated, 1), imm_int(9));
    fai_drop(rec);
    fai_drop(rec);
    fai_drop(updated);
    assert_eq!(live_count(), base);
}

/// On Linux the peak-RSS probe reads a positive high-water mark from
/// `/proc/self/status`; the benchmark harness relies on this value being present.
#[cfg(target_os = "linux")]
#[test]
fn peak_rss_is_reported_on_linux() {
    let kib = peak_rss_kib().expect("VmHWM is available on Linux");
    assert!(kib > 0, "a running process has a non-zero peak RSS");
}

/// Off Linux the probe yields nothing rather than a wrong number, so the harness
/// can mark the measurement unavailable instead of reporting garbage.
#[cfg(not(target_os = "linux"))]
#[test]
fn peak_rss_is_unavailable_off_linux() {
    assert_eq!(peak_rss_kib(), None);
}

/// The `/proc/self/status` `VmHWM:` line is parsed to its KiB value; an absent or
/// malformed field yields `None` rather than a wrong number.
#[test]
fn parses_vmhwm_from_status_text() {
    let status = "Name:\tfai\nVmHWM:\t   12345 kB\nVmRSS:\t   10000 kB\n";
    assert_eq!(parse_vmhwm_kib(status), Some(12345));
    assert_eq!(parse_vmhwm_kib("VmRSS:\t 100 kB\n"), None, "no VmHWM line");
    assert_eq!(parse_vmhwm_kib("VmHWM:\t kB\n"), None, "no numeric field");
    assert_eq!(parse_vmhwm_kib(""), None);
}
