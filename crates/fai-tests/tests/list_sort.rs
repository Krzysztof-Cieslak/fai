//! Stable list sorting, sharing, and the observable comparator call schedule.

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
public sorted : List Int -> List Int
let sorted xs = List.sortBy compare xs
public floats : List Float -> List Float
let floats xs = List.sortBy compare xs
let indexed i xs = match xs with | [] -> [] | x :: rest -> (x, i) :: indexed (i + 1) rest
let key pair = match pair with | (k, _) -> k
let ordinal pair = match pair with | (_, i) -> i
public stable : List Int -> List Int
let stable xs = List.map ordinal (List.sortBy (fun a b -> compare (key a) (key b)) (indexed 0 xs))
cmp : Int -> Int -> Int / { Console }
let cmp a b =
  let _ = stdConsole.writeLine (Int.toString a ++ ":" ++ Int.toString b)
  compare a b
public traced : List Int -> List Int / { Console }
let traced xs = List.sortBy cmp xs
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

    fn call(&mut self, name: &str, value: rt::Value) -> rt::Value {
        let f = self.program.function(Symbol::intern(name)).unwrap();
        rt::apply(rt::fai_dup(f), &[value])
    }

    fn array(&mut self, values: impl IntoIterator<Item = rt::Value>) -> rt::Value {
        values
            .into_iter()
            .fold(rt::fai_array_with_capacity(rt::make_int(0)), |a, x| rt::fai_array_push(a, x))
    }

    fn read_ints(&mut self, list: rt::Value) -> Vec<i64> {
        let array = self.call("toArray", list);
        let n = rt::read_int(rt::fai_array_length_borrowed(array));
        let result = (0..n)
            .map(|i| {
                let value = rt::fai_array_get_borrowed(array, rt::make_int(i));
                let number = rt::read_int(value);
                rt::fai_drop(value);
                number
            })
            .collect();
        rt::fai_drop(array);
        result
    }

    fn sort(&mut self, name: &str, values: &[i64], shared: bool) -> (Vec<i64>, String) {
        let baseline = rt::live_count();
        let array = self.array(values.iter().copied().map(rt::make_int));
        let list = self.call("fromArray", array);
        let original = shared.then(|| rt::fai_dup(list));
        rt::capture_start();
        let result = self.call(name, list);
        let trace = rt::capture_take();
        let result = self.read_ints(result);
        if let Some(original) = original {
            assert_eq!(self.read_ints(original), values);
        }
        assert_eq!(rt::live_count(), baseline);
        (result, trace)
    }
}

/// The linked bottom-up algorithm's schedule, independent of buffer indexing.
fn reference(values: &[i64]) -> (Vec<i64>, String) {
    let mut runs: Vec<_> = values.iter().map(|&v| vec![v]).collect();
    let mut trace = String::new();
    while runs.len() > 1 {
        runs = runs
            .chunks(2)
            .map(|pair| {
                let a = &pair[0];
                let Some(b) = pair.get(1) else { return a.clone() };
                let (mut i, mut j) = (0, 0);
                let mut merged = Vec::new();
                while i < a.len() && j < b.len() {
                    trace.push_str(&format!("{}:{}\n", a[i], b[j]));
                    if a[i] <= b[j] {
                        merged.push(a[i]);
                        i += 1;
                    } else {
                        merged.push(b[j]);
                        j += 1;
                    }
                }
                merged.extend_from_slice(&a[i..]);
                merged.extend_from_slice(&b[j..]);
                merged
            })
            .collect();
    }
    (runs.pop().unwrap_or_default(), trace)
}

#[track_caller]
fn check_trace(values: Vec<i64>) {
    assert_eq!(Harness::new().sort("traced", &values, true), reference(&values));
}

#[test]
fn empty_list_has_no_comparisons() {
    check_trace(Vec::new());
}

#[test]
fn singleton_has_no_comparisons() {
    check_trace(vec![42]);
}

#[test]
fn linked_cutoff_keeps_the_comparator_schedule() {
    check_trace((0..32).map(|i| i * 17 % 32).collect());
}

#[test]
fn first_buffered_size_keeps_the_comparator_schedule() {
    check_trace((0..33).map(|i| i * 17 % 33).collect());
}

#[test]
fn odd_runs_keep_the_comparator_schedule() {
    check_trace((0..65).map(|i| i * 17 % 65).collect());
}

#[test]
fn descending_runs_keep_the_comparator_schedule() {
    check_trace((0..128).rev().collect());
}

#[test]
fn duplicate_keys_keep_their_original_order() {
    let values: Vec<_> = (0..129).map(|i| i * 7 % 5).collect();
    let mut expected: Vec<_> = (0..values.len()).collect();
    expected.sort_by_key(|&i| values[i]);
    let expected: Vec<_> = expected.into_iter().map(|i| i as i64).collect();
    assert_eq!(Harness::new().sort("stable", &values, true).0, expected);
}

#[test]
fn float_buffers_keep_total_order_and_exact_bits() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let mut values: Vec<_> = (0..70).map(|i| (i * 17 % 71) as f64).collect();
    values.extend([
        f64::from_bits(0xfff8_0000_0000_0012),
        -0.0,
        0.0,
        f64::INFINITY,
        f64::from_bits(0x7ff8_0000_0000_1234),
    ]);
    let input = h.array(values.iter().map(|v| rt::fai_box_float(v.to_bits() as i64)));
    let list = h.call("fromArray", input);
    let result = h.call("floats", list);
    let array = h.call("toArray", result);
    values.sort_by(f64::total_cmp);
    let actual: Vec<_> = (0..values.len())
        .map(|i| {
            let value = rt::fai_array_get_borrowed(array, rt::make_int(i as i64));
            let bits = rt::read_float(value).to_bits();
            rt::fai_drop(value);
            bits
        })
        .collect();
    assert_eq!(actual, values.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
    rt::fai_drop(array);
    assert_eq!(rt::live_count(), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn sorting_and_comparator_traces_match_the_linked_reference() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(proptest::collection::vec(any::<i64>(), 0..160), any::<bool>()),
                |(values, shared)| {
                    prop_assert_eq!(
                        harness.borrow_mut().sort("traced", &values, shared),
                        reference(&values)
                    );
                    Ok(())
                },
            )
            .unwrap();
    }
}
