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
type FloatPoint = { w : Float, x : Float, y : Float, z : Float }
floatArrayLoop : Int -> Array FloatPoint -> Int
let floatArrayLoop n xs = if n <= 0 then Array.length xs else floatArrayLoop (n - 1) (Array.map (fun p -> { p with x = p.x + 1.0 }) xs)
public floatArrays : Int -> Int
let floatArrays n = floatArrayLoop n (Array.init 16 (fun i -> { w = 0.0, x = Int.toFloat i, y = 0.0, z = 0.0 }))
let listLoop n xs = if n <= 0 then List.length xs else listLoop (n - 1) (List.map (fun x -> x + 1) xs)
public lists : Int -> Int
let lists n = listLoop n (List.range 0 32)
let prefixLoop n xs = if n <= 0 then List.length xs else prefixLoop (n - 1) (List.append (List.reverse (List.take 7 xs)) (List.drop 7 xs))
public prefixes : Int -> Int
let prefixes n = prefixLoop n (List.range 0 32)
let scan acc xs = match xs with | [] -> acc | x :: rest -> scan (acc + x) rest
let scanLoop n xs acc = if n <= 0 then acc else scanLoop (n - 1) xs (acc + scan 0 xs)
public listScans : Int -> Int
let listScans n = scanLoop n (List.range 0 32) 0
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
let largeLoop n acc =
  if n <= 0 then acc else
    let xs = Array.range 0 4096
    let first = Array.unsafeGet 0 xs
    let last = Array.unsafeGet 4095 xs
    largeLoop (n - 1) (acc + first + last)
public largeArrays : Int -> Int
let largeArrays n = largeLoop n 0
let sumDown n = if n <= 0 then 0 else n + sumDown (n - 1)
public additionLoop : Int -> Int
let additionLoop n = sumDown n
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
fn repeated_float_record_array_maps_keep_constant_storage() {
    bounded_reuse("floatArrays", |_| 16);
}

#[test]
fn repeated_unique_list_maps_keep_constant_storage() {
    bounded_reuse("lists", |_| 32);
}

#[test]
fn repeated_prefix_reversals_keep_constant_storage() {
    bounded_reuse("prefixes", |_| 32);
}

#[test]
fn repeated_scalar_list_scans_keep_constant_storage() {
    bounded_reuse("listScans", |n| n * 496);
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

#[test]
fn repeated_large_buffers_keep_peak_storage_constant() {
    let mut harness = Harness::new();
    let short = harness.measure("largeArrays", 8, 8 * 4095);
    let long = harness.measure("largeArrays", 4096, 4096 * 4095);
    assert_eq!(
        (long.peak_bytes, long.peak_objects),
        (short.peak_bytes, short.peak_objects),
        "{short:?} -> {long:?}"
    );
    assert_eq!(long.array_copies, 0);
    assert!(long.peak_bytes >= 4096 * 8 && long.peak_bytes < 2 * 4096 * 8, "{long:?}");
}

#[test]
fn addition_tail_accumulator_uses_no_heap_storage() {
    let mut harness = Harness::new();
    let short = harness.measure("additionLoop", 8, 36);
    let long = harness.measure("additionLoop", 1_000_000, 500_000_500_000);
    assert_eq!(short, long);
    assert_eq!(long.peak_bytes, 0);
    assert_eq!(long.allocations, 0);
}
