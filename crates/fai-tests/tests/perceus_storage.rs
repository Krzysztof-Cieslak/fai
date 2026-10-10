//! Allocator-retention space guards, isolated from other runtime test binaries.

use std::sync::{Barrier, Mutex};

use fai_db::{Db, FaiDatabase};
use fai_runtime::{self as rt, allocation_stats as storage};
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module Storage
type State = { x : Int, y : Int }
let records n state =
  if n <= 0 then state.x else records (n - 1) { state with x = state.x + 1 }
public recordLoop : Int -> Int
let recordLoop n = records n { x = 0, y = 1 }
let shared n original total =
  if n <= 0 then total else
    let changed = Array.unsafeSet 0 n original
    shared (n - 1) original (total + Array.unsafeGet 0 changed + Array.unsafeGet 0 original)
public sharedLoop : Int -> Int
let sharedLoop n = shared n (Array.repeat 32 1) 0
let buffers n total =
  if n <= 0 then total else
    let buffer = Array.repeat 4096 n
    buffers (n - 1) (total + Array.unsafeGet 4095 buffer)
public bufferLoop : Int -> Int
let bufferLoop n = buffers n 0
let churn n total =
  if n <= 0 then total else
    let size = if n % 3 = 0 then 8 else if n % 3 = 1 then 128 else 4096
    let buffer = Array.repeat size n
    churn (n - 1) (total + Array.unsafeGet (size - 1) buffer)
public churnLoop : Int -> Int
let churnLoop n = churn n 0
public main : Runtime -> Unit
let main runtime = ()
"#;

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    peak_live: i64,
    peak_storage: i64,
    retained: storage::Storage,
}

// Every observing thread is joined while the global test lock is held. Its TLS
// pool therefore finishes releasing slab references before another observation.
fn observe(name: &'static str, iterations: i64, expected: i64) -> Observation {
    assert_eq!(storage::current(), storage::Storage::default());
    let result = std::thread::spawn(move || {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Storage.fai".into(), SOURCE.into());
        let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        let function = program.function(Symbol::intern(name)).unwrap();
        let baseline = (rt::live_count(), rt::live_bytes());
        rt::reset_allocations();
        let value = rt::apply(function, &[rt::make_int(iterations)]);
        assert_eq!(rt::read_int(value), expected);
        rt::fai_drop(value);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
        let retained = storage::current();
        assert_eq!(retained.mapped_bytes, 0, "large mappings release at the final drop");
        assert_eq!(retained.system_bytes, 0, "unpooled allocations release at the final drop");
        Observation {
            peak_live: rt::peak_live_bytes() - baseline.1,
            peak_storage: storage::peak_bytes(),
            retained,
        }
    })
    .join()
    .unwrap();
    assert_eq!(storage::current(), storage::Storage::default(), "thread exit releases slabs");
    result
}

#[track_caller]
fn bounded(name: &'static str, expected: impl Fn(i64) -> i64) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let short = observe(name, 12, expected(12));
    let long = observe(name, 12_000, expected(12_000));
    assert!(short.peak_live > 0 && short.peak_storage >= short.peak_live, "{short:?}");
    assert_eq!(short, long, "fixed state must not accumulate allocator-owned storage");
}

#[test]
fn record_reuse_has_bounded_slab_retention() {
    bounded("recordLoop", |n| n);
}

#[test]
fn shared_array_copies_have_bounded_slab_retention() {
    bounded("sharedLoop", |n| n * (n + 1) / 2 + n);
}

#[test]
fn large_buffers_release_their_mapping_each_iteration() {
    bounded("bufferLoop", |n| n * (n + 1) / 2);
}

#[test]
fn bounded_size_churn_does_not_accumulate_storage() {
    bounded("churnLoop", |n| n * (n + 1) / 2);
}

fn concurrent(iterations: usize) -> i64 {
    const WORKERS: usize = 4;
    let start = Barrier::new(WORKERS);
    rt::reset_allocations();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for _ in 0..iterations {
                    let buffer = rt::fai_array_repeat(rt::make_int(4096), rt::make_int(1));
                    rt::fai_drop(buffer);
                }
            });
        }
    });
    assert_eq!(storage::current(), storage::Storage::default());
    assert_eq!(rt::live_count(), 0);
    let peak = storage::peak_bytes();
    assert!(peak > 0 && peak <= WORKERS as i64 * (4096 * 8 + 64), "{peak}");
    peak
}

#[test]
fn fixed_concurrency_keeps_mapping_retention_bounded() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    concurrent(12);
    concurrent(12_000);
}

fn transfer(length: i64) -> storage::Storage {
    assert_eq!(storage::current(), storage::Storage::default());
    let value = std::thread::spawn(move || {
        let value = rt::fai_array_repeat(rt::make_int(length), rt::make_int(7));
        rt::fai_mark_shared(value)
    })
    .join()
    .unwrap();
    let retained = storage::current();
    assert!(retained.total() > 0, "a live transferred value retains its storage");
    std::thread::spawn(move || rt::fai_drop(value)).join().unwrap();
    assert_eq!(storage::current(), storage::Storage::default());
    assert_eq!(rt::live_count(), 0);
    retained
}

#[test]
fn a_transferred_cell_retains_its_slab_until_the_receiver_releases_it() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let retained = transfer(32);
    assert_eq!(retained.slab_bytes, 64 * 1024);
    assert_eq!(retained.mapped_bytes, 0);
}

#[test]
fn a_transferred_large_array_retains_only_its_mapping() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let retained = transfer(4096);
    assert_eq!(retained.slab_bytes, 0);
    assert_eq!(retained.mapped_bytes, 4096 * 8 + 64);
}
