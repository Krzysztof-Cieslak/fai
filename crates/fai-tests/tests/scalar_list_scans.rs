//! Scalar list cursors retain one owner through full scans and early returns.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
public fromArray : Array 'a -> List 'a
let fromArray xs = Array.toList xs
public toArray : List 'a -> Array 'a
let toArray xs = Array.fromList xs
public sum : Int -> List Int -> Int
let sum acc xs = match xs with | [] -> acc | x :: rest -> sum (acc + x) rest
public floats : Float -> List Float -> Float
let floats acc xs = match xs with | [] -> acc | x :: rest -> floats (acc + x) rest
public find : Int -> List Int -> Bool
let find needle xs = match xs with | [] -> false | x :: rest -> if x = needle then true else find needle rest
public all : List Bool -> Bool
let all xs = match xs with | [] -> true | x :: rest -> if x then all rest else false
public chars : Int -> List Char -> Int
let chars acc xs = match xs with | [] -> acc | x :: rest -> chars (acc + Char.toCode x) rest
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
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }
    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
    fn list(&mut self, values: impl IntoIterator<Item = rt::Value>) -> rt::Value {
        let array = values
            .into_iter()
            .fold(rt::fai_array_with_capacity(rt::make_int(0)), |a, x| rt::fai_array_push(a, x));
        self.call("fromArray", &[array])
    }
}

#[test]
fn unique_long_scan_allocates_no_cells_and_releases_the_root() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let list = h.list((0..100_000).map(rt::make_int));
    let input_bytes = rt::live_bytes();
    rt::reset_allocations();
    let result = h.call("sum", &[rt::make_int(0), list]);
    assert_eq!(rt::read_int(result), 4_999_950_000);
    assert_eq!(rt::allocations(), 0);
    assert_eq!(rt::peak_live_bytes(), input_bytes);
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn early_return_keeps_a_shared_input_unchanged() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let list = h.list([1, 2, 3, 4].map(rt::make_int));
    let found = h.call("find", &[rt::make_int(2), rt::fai_dup(list)]);
    assert_eq!(rt::read_int(found), 1);
    rt::fai_drop(found);
    let sum = h.call("sum", &[rt::make_int(0), list]);
    assert_eq!(rt::read_int(sum), 10);
    rt::fai_drop(sum);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn full_width_integer_heads_stay_live_until_read() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let list = h.list([i64::MAX, i64::MIN, 42].map(rt::make_int));
    let sum = h.call("sum", &[rt::make_int(0), list]);
    assert_eq!(rt::read_int(sum), 41);
    rt::fai_drop(sum);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn boxed_float_heads_preserve_scalar_results() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let list = h.list([1.25f64, 2.5, 4.0].map(|v| rt::fai_box_float(v.to_bits() as i64)));
    let sum = h.call("floats", &[rt::fai_box_float(0), list]);
    assert_eq!(rt::read_float(sum), 7.75);
    rt::fai_drop(sum);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn bool_and_empty_scans_keep_early_exit_values() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let list = h.list([1, 1, 0, 1].map(rt::make_int));
    let result = h.call("all", &[list]);
    assert_eq!(rt::read_int(result), 0);
    rt::fai_drop(result);
    let empty = h.list([]);
    let result = h.call("all", &[empty]);
    assert_eq!(rt::read_int(result), 1);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn unicode_char_heads_keep_their_scalar_values() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let list = h.list([65, 0x1f642].map(rt::make_int));
    let result = h.call("chars", &[rt::make_int(0), list]);
    assert_eq!(rt::read_int(result), 65 + 0x1f642);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn scalar_list_shape_survives_the_worker_wire() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = "module Main\nlet sum acc xs = match xs with | [] -> acc | x :: rest -> sum (acc + x) rest\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (sum 0 (List.range 0 1000)))\n";
    let id = db.add_source("Main.fai".into(), source.into());
    let bundle = fai_driver::build_run_bundle(&db, db.source_file(id).unwrap()).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let bundle = serde_json::from_slice(&bytes).unwrap();
    rt::capture_start();
    assert_eq!(fai_driver::jit_run_bundle(&bundle), 0);
    assert_eq!(rt::capture_take(), "499500\n");
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn scalar_scans_preserve_full_width_sums_and_shared_roots() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(proptest::collection::vec(any::<i64>(), 0..64), any::<bool>()),
                |(values, shared)| {
                    let mut h = harness.borrow_mut();
                    let baseline = (rt::live_count(), rt::live_bytes());
                    let input = h.list(values.iter().copied().map(rt::make_int));
                    let original = shared.then(|| rt::fai_dup(input));
                    rt::reset_allocations();
                    let result = h.call("sum", &[rt::make_int(0), input]);
                    prop_assert_eq!(
                        rt::read_int(result),
                        values.iter().copied().fold(0i64, i64::wrapping_add)
                    );
                    prop_assert!(rt::allocations() <= 1, "only an overflowed final result may box");
                    rt::fai_drop(result);
                    if let Some(mut cursor) = original {
                        for expected in &values {
                            let value = rt::fai_data_field(cursor, 0);
                            prop_assert_eq!(rt::read_int(value), *expected);
                            rt::fai_drop(value);
                            let tail = rt::fai_data_field(cursor, 1);
                            rt::fai_drop(cursor);
                            cursor = tail;
                        }
                        rt::fai_drop(cursor);
                    }
                    prop_assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
                    Ok(())
                },
            )
            .unwrap();
    }
}
