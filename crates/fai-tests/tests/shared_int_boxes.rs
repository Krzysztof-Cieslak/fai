//! Full-width Int values share a box across repeated uniform fields.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module Main\npublic pair : Int -> (Int * Int)\nlet pair x = (x + 1, x + 1)\npublic choose : Bool -> Int -> (Int * Int)\nlet choose yes x = if yes then (x + 1, x + 1) else (0, 0)\npublic captured : Int -> Int\nlet captured x =\n  let first = x + 1\n  let second = x + 1\n  let read _ = second\n  first + read ()\npublic makeInput : Int -> (Int * String)\nlet makeInput value = (value, \"label\")\npublic forward : (Int * String) -> (Int * String * Bool)\nlet forward pair =\n  let (value, label) = pair\n  (value, label, true)\npublic forwardPair : (Int * String) -> (Int * String)\nlet forwardPair pair = pair\npublic makeSome : Int -> Option Int\nlet makeSome value = Some value\npublic forwardSome : Option Int -> (Int * Int)\nlet forwardSome value = match value with | None -> (0, 0) | Some number -> (number, number)\npublic main : Runtime -> Unit\nlet main _ = ()\n";

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

#[test]
fn forwarding_a_tuple_field_keeps_its_original_integer_box() {
    // A triple stays boxed, so this remains a uniform-field forwarding test.
    // A two-field Int/state result instead crosses the raw scalar return ABI.
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut program = program();
    let baseline = rt::live_count();
    let input = rt::apply(
        program.function(Symbol::intern("makeInput")).unwrap(),
        &[rt::make_int(i64::MAX)],
    );
    let integer = rt::fai_data_field(input, 0);
    let retained = rt::fai_dup(input);
    rt::reset_allocations();
    let result = rt::apply(program.function(Symbol::intern("forward")).unwrap(), &[input]);
    assert_eq!(rt::allocations(), 1, "only the shared tuple shell is copied");
    let output = rt::fai_data_field(result, 0);
    assert_eq!(integer, output);
    assert_eq!(rt::read_int(output), i64::MAX);
    rt::fai_drop(output);
    rt::fai_drop(integer);
    rt::fai_drop(retained);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn first_class_integer_state_results_bridge_the_raw_return() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut program = program();
    let baseline = rt::live_count();
    let input = rt::apply(
        program.function(Symbol::intern("makeInput")).unwrap(),
        &[rt::make_int(i64::MAX)],
    );
    let retained = rt::fai_dup(input);
    rt::reset_allocations();
    let result = rt::apply(program.function(Symbol::intern("forwardPair")).unwrap(), &[input]);
    let split =
        cfg!(any(target_arch = "aarch64", all(target_arch = "x86_64", not(target_os = "windows"))));
    assert_eq!(
        rt::allocations(),
        if split { 2 } else { 0 },
        "the split return rebuilds its uniform pair and full-width Int box"
    );
    let value = rt::fai_data_field(result, 0);
    assert_eq!(rt::read_int(value), i64::MAX);
    rt::fai_drop(value);
    rt::fai_drop(result);
    rt::fai_drop(retained);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn forwarding_a_niche_payload_keeps_its_original_integer_box() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut program = program();
    let baseline = rt::live_count();
    let input =
        rt::apply(program.function(Symbol::intern("makeSome")).unwrap(), &[rt::make_int(i64::MIN)]);
    let integer = rt::fai_data_field(input, 0);
    rt::reset_allocations();
    let result = rt::apply(program.function(Symbol::intern("forwardSome")).unwrap(), &[input]);
    assert_eq!(rt::allocations(), 1, "only the tuple shell is constructed");
    let output = rt::fai_data_field(result, 0);
    assert_eq!(integer, output);
    assert_eq!(rt::read_int(output), i64::MIN);
    rt::fai_drop(output);
    rt::fai_drop(integer);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn forwarded_fields_preserve_unique_and_shared_values() {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut program = program();
        let make = program.function(Symbol::intern("makeInput")).unwrap();
        let forward = program.function(Symbol::intern("forward")).unwrap();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), any::<bool>()), |(value, shared)| {
                let baseline = rt::live_count();
                let input = rt::apply(rt::fai_dup(make), &[rt::make_int(value)]);
                let original = rt::fai_data_field(input, 0);
                let retained = shared.then(|| rt::fai_dup(input));
                let output = rt::apply(rt::fai_dup(forward), &[input]);
                let actual = rt::fai_data_field(output, 0);
                prop_assert_eq!(actual, original);
                prop_assert_eq!(rt::read_int(actual), value);
                rt::fai_drop(actual);
                rt::fai_drop(original);
                rt::fai_drop(output);
                if let Some(retained) = retained {
                    rt::fai_drop(retained);
                }
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }

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
