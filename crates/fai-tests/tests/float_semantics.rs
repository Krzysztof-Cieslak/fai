//! IEEE total ordering and sign-bit negation across native calling boundaries.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source("Generic.fai".into(), r#"module Generic
public flags : 'a -> 'a -> Int
let flags a b = (if a < b then 1 else 0) + (if a <= b then 2 else 0) + (if a > b then 4 else 0) + (if a >= b then 8 else 0)
public apply : (Float -> Float -> Int) -> Float -> Float -> Int
let apply f a b = f a b
public applyUnary : (Float -> Float) -> Float -> Float
let applyUnary f a = f a
"#.into());
        let id = db.add_source("Main.fai".into(), r#"module Main
public flags : Float -> Float -> Int
let flags a b = (if a < b then 1 else 0) + (if a <= b then 2 else 0) + (if a > b then 4 else 0) + (if a >= b then 8 else 0)
public compareBits : Int -> Int -> Int
let compareBits a b =
  let x = Float.fromBits a
  let y = Float.fromBits b
  flags x y + 16 * Generic.flags x y + 256 * Generic.apply flags x y + 4096 * Generic.flags { value = x } { value = y }
public negate : Float -> Float
let negate x = -x
public negateBits : Int -> Int
let negateBits a = Float.toBits (negate (Float.fromBits a))
public negateFirstClass : Int -> Int
let negateFirstClass a = Float.toBits (Generic.applyUnary negate (Float.fromBits a))
public equalBits : Int -> Int -> Int
let equalBits a b = if Float.fromBits a = Float.fromBits b then 1 else 0
public main : Runtime -> Unit
let main runtime = ()
"#.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
            .unwrap_or_else(|diagnostics| panic!("{diagnostics:?}"));
        Self { program, _guard: guard }
    }

    fn integer_call(&mut self, name: &str, args: &[u64]) -> i64 {
        let baseline = rt::live_count();
        let function = self.program.function(Symbol::intern(name)).unwrap();
        let args: Vec<_> = args.iter().map(|&bits| rt::make_int(bits as i64)).collect();
        let result = rt::apply(rt::fai_dup(function), &args);
        let value = rt::read_int(result);
        rt::fai_drop(result);
        assert_eq!(rt::live_count(), baseline);
        value
    }

    fn check_order(&mut self, a: u64, b: u64) {
        let flags = match f64::from_bits(a).total_cmp(&f64::from_bits(b)) {
            std::cmp::Ordering::Less => 3,
            std::cmp::Ordering::Equal => 10,
            std::cmp::Ordering::Greater => 12,
        };
        assert_eq!(
            self.integer_call("compareBits", &[a, b]),
            flags * 0x1111,
            "order of {a:016x} and {b:016x}"
        );
        assert_eq!(self.integer_call("equalBits", &[a, b]), i64::from(a == b));
        let operands = || (rt::fai_box_float(a as i64), rt::fai_box_float(b as i64));
        let (x, y) = operands();
        let lt = rt::read_int(rt::fai_float_lt(x, y));
        let (x, y) = operands();
        let le = rt::read_int(rt::fai_float_le(x, y));
        let (x, y) = operands();
        let gt = rt::read_int(rt::fai_float_gt(x, y));
        let (x, y) = operands();
        let ge = rt::read_int(rt::fai_float_ge(x, y));
        assert_eq!(lt + 2 * le + 4 * gt + 8 * ge, flags);
    }
}

#[track_caller]
fn order(a: u64, b: u64) {
    Harness::new().check_order(a, b);
}

#[track_caller]
fn negation(bits: u64) {
    let mut harness = Harness::new();
    let expected = (bits ^ (1u64 << 63)) as i64;
    assert_eq!(harness.integer_call("negateBits", &[bits]), expected);
    assert_eq!(harness.integer_call("negateFirstClass", &[bits]), expected);
    let boxed = rt::fai_float_neg(rt::fai_box_float(bits as i64));
    let boxed_bits = rt::fai_float_to_bits(boxed);
    assert_eq!(rt::read_int(boxed_bits), expected);
    rt::fai_drop(boxed_bits);
}

#[test]
fn signed_zeros_have_the_same_order_in_every_context() {
    order(1u64 << 63, 0);
}
#[test]
fn positive_nan_orders_after_infinity() {
    order(0x7ff8000000000001, f64::INFINITY.to_bits());
}
#[test]
fn negative_nan_orders_before_negative_infinity() {
    order(0xfff8000000000001, f64::NEG_INFINITY.to_bits());
}
#[test]
fn distinct_nan_payloads_have_a_total_order() {
    order(0x7ff8000000000001, 0x7ff8000000000002);
}
#[test]
fn identical_nan_payloads_compare_equal() {
    order(0x7ff8000000000001, 0x7ff8000000000001);
}
#[test]
fn signaling_and_quiet_nans_have_a_total_order() {
    order(0x7ff0000000000001, 0x7ff8000000000001);
}
#[test]
fn positive_zero_negates_to_negative_zero() {
    negation(0);
}
#[test]
fn negative_zero_negates_to_positive_zero() {
    negation(1u64 << 63);
}
#[test]
fn negation_preserves_a_quiet_nan_payload() {
    negation(0x7ff8000000001234);
}
#[test]
fn negation_preserves_a_signaling_nan_payload() {
    negation(0x7ff0000000001234);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;
    use std::cell::RefCell;

    #[test]
    fn every_bit_pattern_uses_the_structural_total_order() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&(any::<u64>(), any::<u64>()), |(a, b)| {
                harness.borrow_mut().check_order(a, b);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn every_bit_pattern_negates_by_toggling_only_the_sign() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&any::<u64>(), |bits| {
                let expected = (bits ^ (1u64 << 63)) as i64;
                let mut harness = harness.borrow_mut();
                prop_assert_eq!(harness.integer_call("negateBits", &[bits]), expected);
                prop_assert_eq!(harness.integer_call("negateFirstClass", &[bits]), expected);
                Ok(())
            })
            .unwrap();
    }
}
