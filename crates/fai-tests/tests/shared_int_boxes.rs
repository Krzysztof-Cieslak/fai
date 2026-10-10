//! Full-width Int values share a box across repeated uniform fields.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module Main\npublic pair : Int -> (Int * Int)\nlet pair x = (x + 1, x + 1)\npublic choose : Bool -> Int -> (Int * Int)\nlet choose yes x = if yes then (x + 1, x + 1) else (0, 0)\npublic captured : Int -> Int\nlet captured x =\n  let first = x + 1\n  let second = x + 1\n  let read _ = second\n  first + read ()\npublic main : Runtime -> Unit\nlet main _ = ()\n";

fn program() -> fai_driver::CompiledProgram {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap()
}

#[test]
fn repeated_full_width_fields_share_one_owned_box() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut program = program();
    let baseline = rt::live_count();
    let argument = rt::make_int(i64::MAX - 1);
    rt::reset_allocations();
    let result = rt::apply(program.function(Symbol::intern("pair")).unwrap(), &[argument]);
    assert_eq!(rt::allocations(), 2, "one integer box and one tuple");
    let first = rt::fai_data_field(result, 0);
    let second = rt::fai_data_field(result, 1);
    assert_eq!(rt::read_int(first), i64::MAX);
    assert_eq!(first, second, "the same immutable box is retained twice");
    rt::fai_drop(first);
    rt::fai_drop(second);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn an_untaken_branch_does_not_allocate_its_integer_box() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut program = program();
    let argument = rt::make_int(i64::MAX - 1);
    rt::reset_allocations();
    let result = rt::apply(
        program.function(Symbol::intern("choose")).unwrap(),
        &[rt::make_int(0), argument],
    );
    assert_eq!(rt::allocations(), 1);
    rt::fai_drop(result);
}

#[test]
fn captured_arithmetic_values_keep_valid_bindings() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut program = program();
    let result =
        rt::apply(program.function(Symbol::intern("captured")).unwrap(), &[rt::make_int(7)]);
    assert_eq!(rt::read_int(result), 16);
    rt::fai_drop(result);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn every_integer_value_keeps_its_two_wrapping_results() {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut program = program();
        let pair = program.function(Symbol::intern("pair")).unwrap();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                let result = rt::apply(rt::fai_dup(pair), &[rt::make_int(value)]);
                let first = rt::fai_data_field(result, 0);
                let second = rt::fai_data_field(result, 1);
                prop_assert_eq!(rt::read_int(first), value.wrapping_add(1));
                prop_assert_eq!(rt::read_int(second), value.wrapping_add(1));
                rt::fai_drop(first);
                rt::fai_drop(second);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
