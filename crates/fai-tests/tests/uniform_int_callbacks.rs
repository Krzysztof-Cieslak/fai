//! Uniform scalar callbacks preserve full-width arithmetic and owned inputs.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module M\npublic makeAdd : Unit -> (Int -> Int -> Int)\nlet makeAdd u = fun a b -> a + b\npublic makeSub : Unit -> (Int -> Int -> Int)\nlet makeSub u = fun a b -> a - b\npublic main : Runtime -> Unit\nlet main r = ()\n";

struct Harness {
    _program: fai_driver::CompiledProgram,
    add: rt::Value,
    sub: rt::Value,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), SOURCE.into());
        let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        let add =
            rt::apply(program.function(Symbol::intern("makeAdd")).unwrap(), &[rt::make_int(0)]);
        let sub =
            rt::apply(program.function(Symbol::intern("makeSub")).unwrap(), &[rt::make_int(0)]);
        Self { _program: program, add, sub, _guard: guard }
    }

    fn check(&self, subtract: bool, a: i64, b: i64) -> i64 {
        let baseline = (rt::live_count(), rt::live_bytes());
        let inputs = [rt::make_int(a), rt::make_int(b)];
        rt::reset_allocations();
        let function = if subtract { self.sub } else { self.add };
        let result = rt::apply(rt::fai_dup(function), &inputs);
        assert_eq!(
            rt::read_int(result),
            if subtract { a.wrapping_sub(b) } else { a.wrapping_add(b) }
        );
        let allocations = rt::allocations();
        rt::fai_drop(result);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
        allocations
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        rt::fai_drop(self.add);
        rt::fai_drop(self.sub);
    }
}

#[test]
fn largest_immediate_plus_zero_needs_no_box() {
    assert_eq!(Harness::new().check(false, (1 << 62) - 1, 0), 0);
}

#[test]
fn smallest_immediate_minus_zero_needs_no_box() {
    assert_eq!(Harness::new().check(true, -(1 << 62), 0), 0);
}

#[test]
fn addition_beyond_the_immediate_boundary_boxes_once() {
    assert_eq!(Harness::new().check(false, (1 << 62) - 1, 1), 1);
}

#[test]
fn subtraction_beyond_the_immediate_boundary_boxes_once() {
    assert_eq!(Harness::new().check(true, -(1 << 62), 1), 1);
}

#[test]
fn full_width_addition_wraps_to_the_minimum() {
    assert_eq!(Harness::new().check(false, i64::MAX, 1), 1);
}

#[test]
fn full_width_subtraction_wraps_to_the_maximum() {
    assert_eq!(Harness::new().check(true, i64::MIN, 1), 1);
}

#[test]
fn boxed_operands_can_produce_an_immediate_result() {
    assert_eq!(Harness::new().check(false, i64::MAX, i64::MIN), 0);
}

#[test]
fn callback_preserves_a_shared_boxed_operand() {
    let h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let input = rt::make_int(i64::MAX);
    let result = rt::apply(rt::fai_dup(h.sub), &[rt::fai_dup(input), rt::make_int(1)]);
    assert_eq!(rt::read_int(input), i64::MAX);
    assert_eq!(rt::read_int(result), i64::MAX - 1);
    rt::fai_drop(result);
    rt::fai_drop(input);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn uniform_callbacks_match_wrapping_i64_arithmetic() {
        let h = Harness::new();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        let integer = || {
            prop_oneof![
                Just(i64::MIN),
                Just(i64::MAX),
                Just(-(1 << 62)),
                Just((1 << 62) - 1),
                any::<i64>()
            ]
        };
        runner
            .run(&(integer(), integer(), any::<bool>()), |(a, b, subtract)| {
                prop_assert!(h.check(subtract, a, b) <= 1);
                Ok(())
            })
            .unwrap();
    }
}
