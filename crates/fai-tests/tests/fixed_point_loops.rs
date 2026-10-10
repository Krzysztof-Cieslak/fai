//! Exact-state loop shortcuts preserve scalar arithmetic and termination values.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module M\npublic iterate : Int -> Int -> Float -> Float -> Float\nlet iterate i n x acc = if i >= n then x + acc else iterate (i + 1) n (x * 0.5) (acc + x)\npublic terminal : Int -> Int -> Float -> Int\nlet terminal i n x = if i >= n then i else terminal (i + 1) n (x * 0.5)\npublic toggle : Int -> Int -> Float -> Float\nlet toggle i n x = if i >= n then x else toggle (i + 1) n (-x)\npublic counted : Int -> Int -> Float -> Float\nlet counted i n x = if i >= n then x else counted (i + 1) n (x + Int.toFloat i)\npublic main : Runtime -> Unit\nlet main _ = ()\n";
const DESCENDING: &str = "public descend : Int -> Int -> Float -> Float -> Float\nlet descend i bound x acc = if i <= bound then x + acc else descend (i - 1) bound (x * 0.5) (acc + x)\npublic terminalDown : Int -> Int -> Float -> Int\nlet terminalDown i bound x = if i <= bound then i else terminalDown (i - 1) bound (x * 0.5)\npublic toZero : Int -> Float -> Int\nlet toZero n x = if n <= 0 then n else toZero (n - 1) (x * 0.5)\npublic toggleDown : Int -> Float -> Float\nlet toggleDown n x = if n <= 0 then x else toggleDown (n - 1) (-x)\n";

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), format!("{SOURCE}\n{DESCENDING}"));
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }

    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
}

fn float(x: f64) -> rt::Value {
    rt::fai_box_float(x.to_bits() as i64)
}

#[test]
fn signed_zero_cycles_are_not_bitwise_fixed_points() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let result = h.call("toggle", &[rt::make_int(0), rt::make_int(5), float(0.0)]);
    assert_eq!(rt::read_float(result).to_bits(), (-0.0f64).to_bits());
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn counter_dependent_work_is_not_skipped_after_an_unchanged_step() {
    let mut h = Harness::new();
    let result = h.call("counted", &[rt::make_int(0), rt::make_int(4), float(0.0)]);
    assert_eq!(rt::read_float(result), 6.0);
    rt::fai_drop(result);
}

#[test]
fn already_finished_loops_keep_the_original_counter() {
    let mut h = Harness::new();
    let result = h.call("terminal", &[rt::make_int(7), rt::make_int(-5), float(0.0)]);
    assert_eq!(rt::read_int(result), 7);
    rt::fai_drop(result);
}

#[test]
fn full_width_bounds_keep_the_exact_terminal_counter() {
    let mut h = Harness::new();
    let result =
        h.call("terminal", &[rt::make_int(i64::MAX - 1), rt::make_int(i64::MAX), float(0.0)]);
    assert_eq!(rt::read_int(result), i64::MAX);
    rt::fai_drop(result);
}

#[test]
fn converged_nan_state_keeps_its_payload() {
    let mut h = Harness::new();
    let nan = f64::from_bits(0xfff8_0000_0000_4567);
    let result = h.call("iterate", &[rt::make_int(0), rt::make_int(16), float(nan), float(0.0)]);
    assert_eq!(rt::read_float(result).to_bits(), nan.to_bits());
    rt::fai_drop(result);
}

#[test]
fn descending_loops_preserve_already_finished_negative_counters() {
    let mut h = Harness::new();
    let result = h.call("toZero", &[rt::make_int(-5), float(0.0)]);
    assert_eq!(rt::read_int(result), -5);
    rt::fai_drop(result);
}

#[test]
fn descending_loops_reach_the_full_width_lower_bound() {
    let mut h = Harness::new();
    let result =
        h.call("terminalDown", &[rt::make_int(i64::MIN + 3), rt::make_int(i64::MIN), float(0.0)]);
    assert_eq!(rt::read_int(result), i64::MIN);
    rt::fai_drop(result);
}

#[test]
fn descending_signed_zero_cycles_are_not_fixed_points() {
    let mut h = Harness::new();
    let result = h.call("toggleDown", &[rt::make_int(5), float(0.0)]);
    assert_eq!(rt::read_float(result).to_bits(), (-0.0f64).to_bits());
    rt::fai_drop(result);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn descending_states_match_every_original_scalar_step() {
        let h = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<u64>(), any::<u64>(), 0i64..32), |(x, acc, count)| {
                let baseline = rt::live_count();
                let (mut a, mut b) = (f64::from_bits(x), f64::from_bits(acc));
                let result = h
                    .borrow_mut()
                    .call("descend", &[rt::make_int(count), rt::make_int(0), float(a), float(b)]);
                for _ in 0..count {
                    (a, b) = (a * 0.5, b + a);
                }
                prop_assert_eq!(rt::read_float(result).to_bits(), (a + b).to_bits());
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn arbitrary_states_match_every_original_scalar_step() {
        let h = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<u64>(), any::<u64>(), 0i64..32), |(x, acc, count)| {
                let baseline = rt::live_count();
                let (mut a, mut b) = (f64::from_bits(x), f64::from_bits(acc));
                let result = h
                    .borrow_mut()
                    .call("iterate", &[rt::make_int(0), rt::make_int(count), float(a), float(b)]);
                for _ in 0..count {
                    (a, b) = (a * 0.5, b + a);
                }
                prop_assert_eq!(rt::read_float(result).to_bits(), (a + b).to_bits());
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
