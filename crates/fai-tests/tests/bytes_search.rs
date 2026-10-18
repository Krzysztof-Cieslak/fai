//! Binary search preserves ownership across direct, first-class and worker ABIs.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

fn bytes(text: &str) -> rt::Value {
    rt::fai_bytes_from_string(rt::make_str(text))
}
const SOURCE: &str = r#"module Main
public search : Int -> Int -> Bytes -> Bytes -> Int
let search start finish needle bytes =
  match Bytes.findIn start finish needle bytes with | None -> -1 | Some i -> i
public held : Bytes -> (Bytes -> Option Int)
let held needle = Bytes.find needle
public asInt : Option Int -> Int
let asInt value = match value with | None -> -1 | Some i -> i
public main : Runtime -> Unit / { Console, Concurrency }
let main r =
  let bytes = Bytes.fromList [0, 255, 128, 0, 255]
  let needle = Bytes.fromList [0, 255]
  let (a, b) = Async.parallel2 r.concurrency (fun _ -> search 0 5 needle bytes) (fun _ -> search 1 5 needle bytes)
  let same = Bytes.find bytes bytes
  let empty = Bytes.findIn 9223372036854775807 9223372036854775807 Bytes.empty bytes
  let missing = Bytes.findIn 2 1 Bytes.empty bytes
  let firstClass = held needle
  let good = a = 0 && b = 3 && same = Some 0 && empty = Some 5 && missing = None && firstClass bytes = Some 0
  r.console.writeLine (if good then "ok" else "bad")
"#;

fn database() -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (db, file) = database();
        Self { program: fai_driver::jit_compile(&db, file).unwrap(), _guard: guard }
    }
    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
}

#[test]
fn full_width_bounds_and_owned_buffers_are_released() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call(
        "search",
        &[rt::make_int(i64::MIN), rt::make_int(i64::MAX), bytes("\0b"), bytes("c\0b")],
    );
    assert_eq!(rt::read_int(value), 1);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn partial_application_retains_a_pattern_for_repeated_searches() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let search = h.call("held", &[bytes("b")]);
    let first = rt::apply(rt::fai_dup(search), &[bytes("ab")]);
    let first = h.call("asInt", &[first]);
    assert_eq!(rt::read_int(first), 1);
    rt::fai_drop(first);
    let second = rt::apply(search, &[bytes("a")]);
    let second = h.call("asInt", &[second]);
    assert_eq!(rt::read_int(second), -1);
    rt::fai_drop(second);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn binary_search_survives_worker_serialization() {
    let (db, file) = database();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    rt::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = rt::capture_take();
    assert_eq!((exit, output.as_str()), (0, "ok\n"));
}

#[test]
fn binary_search_links_and_runs_in_aot() {
    let (db, file) = database();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("bytes-search")).unwrap();
    let build = fai_driver::build_native(&db, file, &path);
    assert!(build.ok, "{:?}", build.diagnostics);
    let output = std::process::Command::new(build.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}
