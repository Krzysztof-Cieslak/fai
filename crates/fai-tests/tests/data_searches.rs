//! Resource-free search cursors retain one owner and preserve scalar results.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module Main
public type Tree = | End | Node Tree Int Int Tree
let balanced lo hi =
  if lo >= hi then End else
    let mid = lo + (hi - lo) / 2
    Node (balanced lo mid) mid (mid * 3) (balanced (mid + 1) hi)
public make : Int -> Tree
let make n = balanced 0 n
let chain n tree = if n <= 0 then tree else chain (n - 1) (Node End n (n * 3) tree)
public deep : Int -> Tree
let deep n = chain n End
public single : Int -> Tree
let single value = Node End 0 value End
public find : Int -> Tree -> Option Int
let find key tree =
  match tree with
  | End -> None
  | Node l k v r -> if key < k then find key l else if key > k then find key r else Some v
public lookup : Int -> Tree -> Int
let lookup key tree = match find key tree with | None -> -1 | Some value -> value
public main : Runtime -> Unit / { Console }
let main r = r.console.writeLine (Int.toString (lookup 777 (make 1024)))
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
}

#[test]
fn deep_search_allocates_nothing_and_releases_the_input() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let tree = h.call("deep", &[rt::make_int(100_000)]);
    let input_bytes = rt::live_bytes();
    rt::reset_allocations();
    let found = h.call("lookup", &[rt::make_int(100_000), tree]);
    assert_eq!(rt::read_int(found), 300_000);
    assert_eq!(rt::allocations(), 0);
    assert_eq!(rt::peak_live_bytes(), input_bytes);
    rt::fai_drop(found);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn early_return_preserves_a_shared_tree() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let tree = h.call("make", &[rt::make_int(1024)]);
    let first = h.call("lookup", &[rt::make_int(512), rt::fai_dup(tree)]);
    assert_eq!(rt::read_int(first), 1536);
    rt::fai_drop(first);
    let second = h.call("lookup", &[rt::make_int(1023), tree]);
    assert_eq!(rt::read_int(second), 3069);
    rt::fai_drop(second);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn missing_key_releases_every_branch() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let tree = h.call("make", &[rt::make_int(1024)]);
    let result = h.call("lookup", &[rt::make_int(-1), tree]);
    assert_eq!(rt::read_int(result), -1);
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn full_width_payload_outlives_the_retained_root() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let tree = h.call("single", &[rt::make_int(i64::MIN)]);
    let result = h.call("lookup", &[rt::make_int(0), tree]);
    assert_eq!(rt::read_int(result), i64::MIN);
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn resource_free_search_metadata_survives_worker_transport() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    let bundle = fai_driver::build_run_bundle(&db, db.source_file(id).unwrap()).bundle.unwrap();
    let json = serde_json::to_string(&bundle).unwrap();
    assert!(json.contains("\"resource_free\":true"));
    let bundle = serde_json::from_str(&json).unwrap();
    rt::capture_start();
    assert_eq!(fai_driver::jit_run_bundle(&bundle), 0);
    assert_eq!(rt::capture_take(), "2331\n");
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn balanced_searches_match_the_key_range() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(&(0i64..128, -10i64..140, any::<bool>()), |(size, key, shared)| {
                let mut h = harness.borrow_mut();
                let baseline = (rt::live_count(), rt::live_bytes());
                let tree = h.call("make", &[rt::make_int(size)]);
                let original = shared.then(|| rt::fai_dup(tree));
                rt::reset_allocations();
                let result = h.call("lookup", &[rt::make_int(key), tree]);
                prop_assert_eq!(
                    rt::read_int(result),
                    if (0..size).contains(&key) { key * 3 } else { -1 }
                );
                prop_assert_eq!(rt::allocations(), 0);
                rt::fai_drop(result);
                if let Some(original) = original {
                    rt::fai_drop(original);
                }
                prop_assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
                Ok(())
            })
            .unwrap();
    }
}
