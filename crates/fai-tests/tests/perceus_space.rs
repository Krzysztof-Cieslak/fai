//! Deterministic Perceus space guards: iteration count must not grow live memory.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module M
type Point = { x : Int, y : Int }
recordLoop : Int -> Point -> Int
let recordLoop n p = if n <= 0 then p.x else recordLoop (n - 1) { p with x = p.x + 1, y = p.y + 1 }
public records : Int -> Int
let records n = recordLoop n { x = 0, y = 0 }
let openLoop n p = if n <= 0 then p.x else openLoop (n - 1) { p with x = p.x + 1, y = p.y + 1 }
public openRecords : Int -> Int
let openRecords n = openLoop n { x = 0, y = 0, other = 3 }
let arrayLoop n xs = if n <= 0 then Array.length xs else arrayLoop (n - 1) (Array.map (fun p -> { p with x = p.x + 1 }) xs)
public arrays : Int -> Int
let arrays n = arrayLoop n (Array.init 16 (fun i -> { x = i, y = i }))
let listLoop n xs = if n <= 0 then List.length xs else listLoop (n - 1) (List.map (fun x -> x + 1) xs)
public lists : Int -> Int
let lists n = listLoop n (List.range 0 32)
type Slot = | Full Int
let readLoop n i xs acc =
  if n <= 0 then acc else
    match Array.unsafeGet i xs with
    | Full value -> readLoop (n - 1) (if i = 15 then 0 else i + 1) xs (acc + value)
public borrowedSlots : Int -> Int
let borrowedSlots n = readLoop n 0 (Array.init 16 (fun i -> Full 1)) 0
let replaceLoop n d = if n <= 0 then HashDict.getOr 0 0 d else replaceLoop (n - 1) (HashDict.updateOr 0 (fun value -> value + 1) 0 d)
public replacements : Int -> Int
let replacements n = replaceLoop n (HashDict.singleton 0 0)
let chunkLoop n chunks acc =
  if n <= 0 then acc else
    let total = List.foldl (fun sum value -> sum + value) 0 (List.concat chunks)
    chunkLoop (n - 1) chunks (acc + total)
public chunks : Int -> Int
let chunks n = chunkLoop n [[1, 2], [], [3, 4]] 0
public main : Runtime -> Unit
let main r = ()
"#;

#[derive(Debug, PartialEq, Eq)]
struct Space {
    peak_objects: i64,
    peak_bytes: i64,
    allocations: i64,
    array_copies: i64,
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
        let id = db.add_source("M.fai".into(), SOURCE.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }

    fn measure(&mut self, name: &str, iterations: i64, expected: i64) -> Space {
        let function = self.program.function(Symbol::intern(name)).unwrap();
        let baseline = (rt::live_count(), rt::live_bytes());
        rt::reset_allocations();
        let result = rt::apply(function, &[rt::make_int(iterations)]);
        assert_eq!(rt::read_int(result), expected);
        let space = Space {
            peak_objects: rt::peak_live_count() - baseline.0,
            peak_bytes: rt::peak_live_bytes() - baseline.1,
            allocations: rt::allocations(),
            array_copies: rt::array_copies(),
        };
        rt::fai_drop(result);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline, "all live storage is released");
        space
    }
}

#[track_caller]
fn bounded_reuse(name: &str, result: impl Fn(i64) -> i64) {
    let mut harness = Harness::new();
    let short = harness.measure(name, 8, result(8));
    let long = harness.measure(name, 4096, result(4096));
    eprintln!("{name}: {short:?} -> {long:?}");
    assert_eq!(long, short, "repeated unique updates keep both allocation and space costs fixed");
    assert_eq!(long.array_copies, 0);
    assert!(long.peak_bytes > 0 && long.peak_bytes < 4096, "{long:?}");
}

#[test]
fn repeated_record_updates_keep_constant_storage() {
    bounded_reuse("records", |n| n);
}

#[test]
fn repeated_owned_array_maps_keep_constant_storage() {
    bounded_reuse("arrays", |_| 16);
}

#[test]
fn repeated_unique_list_maps_keep_constant_storage() {
    bounded_reuse("lists", |_| 32);
}

#[test]
fn repeated_borrowed_slot_reads_keep_constant_storage() {
    bounded_reuse("borrowedSlots", |n| n);
}

#[test]
fn repeated_chunk_folds_keep_constant_storage() {
    bounded_reuse("chunks", |n| n * 10);
}

#[test]
fn replacing_hash_entries_keeps_peak_storage_constant() {
    let mut harness = Harness::new();
    let short = harness.measure("replacements", 8, 8);
    let long = harness.measure("replacements", 4096, 4096);
    eprintln!("replacements: {short:?} -> {long:?}");
    assert_eq!(
        (long.peak_bytes, long.peak_objects),
        (short.peak_bytes, short.peak_objects),
        "{short:?} -> {long:?}"
    );
    assert_eq!(long.array_copies, 0);
    assert!(long.peak_bytes > 0 && long.peak_bytes < 4096, "{long:?}");
}

#[test]
fn open_record_updates_keep_peak_storage_constant() {
    let mut harness = Harness::new();
    let short = harness.measure("openRecords", 8, 8);
    let long = harness.measure("openRecords", 4096, 4096);
    eprintln!("openRecords: {short:?} -> {long:?}");
    assert_eq!(
        (long.peak_bytes, long.peak_objects),
        (short.peak_bytes, short.peak_objects),
        "{short:?} -> {long:?}"
    );
    assert!(long.peak_bytes > 0 && long.peak_bytes < 4096, "{long:?}");
}
