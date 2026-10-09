//! Buffer and element ownership through same-type sequential Array maps.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module Main
public ints : Array Int -> Array Int
let ints xs = Array.map (fun x -> x * 2 + 1) xs
public floats : Array Float -> Array Float
let floats xs = Array.map (fun x -> x + 1.0) xs
public same : Array 'a -> Array 'a
let same xs = Array.map identity xs
public records : Array { x : Int, y : Int } -> Array { x : Int, y : Int }
let records xs = Array.map (fun p -> { p with x = p.x + 1 }) xs
public buildRecords : Unit -> Array { x : Int, y : Int }
let buildRecords u = [| { x = 10, y = 20 }, { x = 30, y = 40 } |]
public main : Runtime -> Unit
let main r = ()
"#;

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Main.fai".into(), SOURCE.into());
        let file = db.source_file(id).unwrap();
        let program = fai_driver::jit_compile(&db, file).unwrap_or_else(|d| panic!("{d:?}"));
        Self { program, _guard: guard }
    }

    fn call(&mut self, name: &str, value: rt::Value) -> rt::Value {
        let function = self.program.function(Symbol::intern(name)).unwrap();
        rt::apply(rt::fai_dup(function), &[value])
    }
}

fn array(values: impl IntoIterator<Item = rt::Value>) -> rt::Value {
    values.into_iter().fold(rt::fai_array_with_capacity(rt::make_int(4)), |array, value| {
        rt::fai_array_push(array, value)
    })
}

fn ints(value: rt::Value) -> Vec<i64> {
    let length = rt::read_int(rt::fai_array_length_borrowed(value));
    (0..length)
        .map(|i| {
            let element = rt::fai_array_get_borrowed(value, rt::make_int(i));
            let n = rt::read_int(element);
            rt::fai_drop(element);
            n
        })
        .collect()
}

#[test]
fn unique_map_keeps_its_buffer_without_allocating() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = array([1, 2, 3].map(rt::make_int));
    rt::reset_allocations();
    let output = h.call("ints", input);
    assert_eq!(output, input);
    assert_eq!(rt::allocations(), 0);
    assert_eq!(ints(output), [3, 5, 7]);
    rt::fai_drop(output);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn shared_map_copies_once_and_preserves_the_source() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = array([1, 2, 3].map(rt::make_int));
    rt::reset_allocations();
    let output = h.call("ints", rt::fai_dup(input));
    assert_eq!(rt::array_copies(), 1);
    assert_eq!(ints(output), [3, 5, 7]);
    assert_eq!(ints(input), [1, 2, 3]);
    rt::fai_drop(output);
    rt::fai_drop(input);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn empty_map_reuses_its_empty_buffer() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = array([]);
    rt::reset_allocations();
    let output = h.call("ints", input);
    assert_eq!(output, input);
    assert_eq!(rt::allocations(), 0);
    assert!(ints(output).is_empty());
    rt::fai_drop(output);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn monomorphic_float_map_keeps_raw_slots() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = array([rt::fai_box_float(2.5f64.to_bits() as i64)]);
    rt::reset_allocations();
    let output = h.call("floats", input);
    assert_eq!(output, input);
    assert_eq!(rt::allocations(), 0);
    let element = rt::fai_array_get_borrowed(output, rt::make_int(0));
    assert_eq!(rt::read_float(element), 3.5);
    rt::fai_drop(element);
    rt::fai_drop(output);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn generic_float_map_retains_nan_payload_bits() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let bits = 0xfff8_1234_5678_9abcu64;
    let input = array([rt::fai_box_float(bits as i64)]);
    let output = h.call("same", input);
    assert_eq!(output, input);
    let element = rt::fai_array_get_borrowed(output, rt::make_int(0));
    assert_eq!(rt::read_float(element).to_bits(), bits);
    rt::fai_drop(element);
    rt::fai_drop(output);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn monomorphic_map_releases_boxed_integer_inputs() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = array([rt::make_int(i64::MIN)]);
    let output = h.call("ints", input);
    assert_eq!(ints(output), [1]);
    rt::fai_drop(output);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn record_update_reuses_uniquely_owned_elements() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = h.call("buildRecords", rt::FAI_UNIT);
    rt::reset_allocations();
    let output = h.call("records", input);
    assert_eq!(output, input);
    assert_eq!(rt::allocations(), 0, "moving the slot keeps its record unique");
    let first = rt::fai_array_get_borrowed(output, rt::make_int(0));
    let x = rt::fai_data_field(first, 0);
    let y = rt::fai_data_field(first, 1);
    assert_eq!((rt::read_int(x), rt::read_int(y)), (11, 20));
    rt::fai_drop(x);
    rt::fai_drop(y);
    rt::fai_drop(first);
    rt::fai_drop(output);
    assert_eq!(rt::live_count(), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn maps_preserve_wrapping_values_and_shared_sources() {
        let mut harness = Harness::new();
        let function = harness.program.function(Symbol::intern("ints")).unwrap();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(proptest::collection::vec(any::<i64>(), 0..128), any::<bool>()),
                |(values, shared)| {
                    let baseline = rt::live_count();
                    let input = array(values.iter().copied().map(rt::make_int));
                    let original = shared.then(|| rt::fai_dup(input));
                    let output = rt::apply(rt::fai_dup(function), &[input]);
                    let expected: Vec<_> =
                        values.iter().map(|v| v.wrapping_mul(2).wrapping_add(1)).collect();
                    prop_assert_eq!(ints(output), expected);
                    if let Some(original) = original {
                        prop_assert_eq!(ints(original), values);
                        rt::fai_drop(original);
                    } else {
                        prop_assert_eq!(output, input);
                    }
                    rt::fai_drop(output);
                    prop_assert_eq!(rt::live_count(), baseline);
                    Ok(())
                },
            )
            .unwrap();
    }
}
