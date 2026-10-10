//! Materialized list concatenation preserves order, sharing and the final suffix.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
public fromArray : Array 'a -> List 'a
let fromArray values = Array.toList values
public toArray : List 'a -> Array 'a
let toArray values = Array.fromList values
public flatten : List (List 'a) -> List 'a
let flatten chunks = List.concat chunks
public dropTwo : List Int -> List Int
let dropTwo values = match values with | _ :: _ :: rest -> rest | _ -> []
public main : Runtime -> Unit
let main runtime = ()
"#;

fn array(values: impl IntoIterator<Item = rt::Value>) -> rt::Value {
    values.into_iter().fold(rt::fai_array_with_capacity(rt::make_int(0)), |array, value| {
        rt::fai_array_push(array, value)
    })
}

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
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), &[value])
    }

    fn list(&mut self, values: &[i64]) -> rt::Value {
        self.call("fromArray", array(values.iter().copied().map(rt::make_int)))
    }

    fn read(&mut self, list: rt::Value) -> Vec<i64> {
        let array = self.call("toArray", list);
        let size = rt::read_int(rt::fai_array_length_borrowed(array));
        let result = (0..size)
            .map(|i| {
                let value = rt::fai_array_get_borrowed(array, rt::make_int(i));
                let n = rt::read_int(value);
                rt::fai_drop(value);
                n
            })
            .collect();
        rt::fai_drop(array);
        result
    }
}

#[test]
fn a_shared_final_chunk_is_transferred_intact() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let first = h.list(&[1, 2]);
    let suffix = h.list(&[3, 4]);
    let chunks = h.call("fromArray", array([first, rt::fai_dup(suffix)]));
    let flattened = h.call("flatten", chunks);
    let tail = h.call("dropTwo", rt::fai_dup(flattened));
    assert_eq!(tail, suffix, "the final shared suffix keeps its original spine");
    rt::fai_drop(tail);
    assert_eq!(h.read(flattened), [1, 2, 3, 4]);
    assert_eq!(h.read(suffix), [3, 4]);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn shared_prefixes_keep_their_original_contents() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let first = h.list(&[1, 2]);
    let suffix = h.list(&[3, 4]);
    let chunks = h.call("fromArray", array([rt::fai_dup(first), suffix]));
    let flattened = h.call("flatten", chunks);
    assert_eq!(h.read(flattened), [1, 2, 3, 4]);
    assert_eq!(h.read(first), [1, 2]);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn many_empty_chunks_use_constant_native_stack() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let empty = h.list(&[]);
    let values = rt::fai_array_repeat(rt::make_int(100_000), empty);
    let chunks = h.call("fromArray", values);
    let flattened = h.call("flatten", chunks);
    assert!(h.read(flattened).is_empty());
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn a_long_unique_prefix_reuses_its_spine_without_recursive_frames() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let expected: Vec<_> = (0..100_000).collect();
    let first = h.list(&expected);
    let suffix = h.list(&[100_000]);
    let chunks = h.call("fromArray", array([first, suffix]));
    rt::reset_allocations();
    let output = h.call("flatten", chunks);
    assert_eq!(rt::allocations(), 0, "unique spines are threaded into the output");
    assert_eq!(h.read(output), (0..100_001).collect::<Vec<_>>());
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;
    use std::cell::RefCell;

    #[test]
    fn concatenation_preserves_materialized_chunk_order_and_full_width_values() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(
                &(
                    prop::collection::vec(prop::collection::vec(any::<i64>(), 0..16), 0..16),
                    any::<bool>(),
                ),
                |(values, shared)| {
                    let mut h = harness.borrow_mut();
                    let baseline = (rt::live_count(), rt::live_bytes());
                    let lists: Vec<_> = values.iter().map(|part| h.list(part)).collect();
                    let original = shared.then(|| {
                        lists.iter().copied().map(|value| rt::fai_dup(value)).collect::<Vec<_>>()
                    });
                    let chunks = h.call("fromArray", array(lists));
                    let output = h.call("flatten", chunks);
                    prop_assert_eq!(
                        h.read(output),
                        values.iter().flatten().copied().collect::<Vec<_>>()
                    );
                    if let Some(original) = original {
                        let actual: Vec<_> =
                            original.into_iter().map(|list| h.read(list)).collect();
                        prop_assert_eq!(actual, values);
                    }
                    prop_assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
                    Ok(())
                },
            )
            .unwrap();
    }
}
