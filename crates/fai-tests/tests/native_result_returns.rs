//! Native tag/payload results avoid wrappers while preserving ownership boundaries.
use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const FUNCTIONS: &str = r#"module Main
public consume : String -> Int -> Int -> Int
let consume text start finish =
  match RegexEngine.slice text start finish with
  | Ok piece -> String.length piece
  | Err (RegexEngine.EngineError code message first last) -> code + String.length message + first + last
public stored : String -> Result String RegexEngine.Error
let stored text = RegexEngine.slice text 0 1
public storedPair : String -> (Result String RegexEngine.Error * Result String RegexEngine.Error)
let storedPair text =
  let result = RegexEngine.slice text 0 1
  (result, result)
public ignore : String -> Bool
let ignore text = match RegexEngine.slice text 0 1 with | Ok _ -> true | Err _ -> false
public shared : String -> Int
let shared text =
  let result = RegexEngine.slice text 0 1
  let alias = result
  let first = match result with | Ok piece -> String.length piece | Err _ -> 0
  let second = match alias with | Ok piece -> String.length piece | Err _ -> 0
  first + second
"#;

const SIMPLE_MAIN: &str = "\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (consume \"abc\" 0 1))\n";
const CONCURRENT_MAIN: &str = r#"
public main : Runtime -> Unit / { Concurrency, Console }
let main r =
  let text = String.join "" ["Aé", "🙂B"]
  let (first, second) = Async.parallel2 r.concurrency (fun _ -> consume text 1 3) (fun _ -> consume text 3 7)
  let invalid = consume text 0 9223372036854775807
  r.console.writeLine (if first = 1 && second = 1 && invalid = 22 && text = "Aé🙂B" then "ok" else "bad")
"#;

fn source(main: &str, control: fai_db::AllocationControl) -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    db.set_allocation_control(control);
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), format!("{FUNCTIONS}{main}"));
    let file = db.source_file(id).unwrap();
    (db, file)
}

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new(control: fai_db::AllocationControl) -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (db, file) = source(SIMPLE_MAIN, control);
        let program = fai_driver::jit_compile(&db, file).unwrap();
        Self { program, _guard: guard }
    }

    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
}

#[test]
fn matched_native_success_allocates_only_the_copied_payload() {
    let mut h = Harness::new(fai_db::AllocationControl::Enabled);
    let baseline = (rt::live_count(), rt::live_bytes());
    let text = rt::make_str("Aé🙂B");
    rt::reset_allocations();
    let value = h.call("consume", &[text, rt::make_int(1), rt::make_int(7)]);
    assert_eq!(rt::read_int(value), 2);
    assert_eq!(rt::allocations(), 1);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn native_success_control_keeps_the_result_wrapper() {
    let mut h = Harness::new(fai_db::AllocationControl::NoScalarReplacement);
    let baseline = (rt::live_count(), rt::live_bytes());
    let text = rt::make_str("Aé🙂B");
    rt::reset_allocations();
    let value = h.call("consume", &[text, rt::make_int(1), rt::make_int(7)]);
    assert_eq!(rt::read_int(value), 2);
    assert_eq!(rt::allocations(), 2);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn matched_native_error_keeps_its_fields_without_the_wrapper() {
    let mut h = Harness::new(fai_db::AllocationControl::Enabled);
    let baseline = (rt::live_count(), rt::live_bytes());
    let text = rt::make_str("é");
    rt::reset_allocations();
    let value = h.call("consume", &[text, rt::make_int(0), rt::make_int(1)]);
    assert_eq!(rt::read_int(value), 22);
    assert_eq!(rt::allocations(), 2);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn stored_native_results_keep_the_ordinary_value_representation() {
    let mut h = Harness::new(fai_db::AllocationControl::Enabled);
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("stored", &[rt::make_str("abc")]);
    assert_eq!(rt::read_int(rt::fai_data_tag(value)), 0);
    let payload = rt::fai_data_field(value, 0);
    assert_eq!(rt::read_string(payload), b"a");
    rt::fai_drop(payload);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn stored_native_result_aliases_reconstitute_one_shared_value() {
    let mut h = Harness::new(fai_db::AllocationControl::Enabled);
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("storedPair", &[rt::make_str("abc")]);
    let left = rt::fai_data_field(value, 0);
    let right = rt::fai_data_field(value, 1);
    assert_eq!(left, right, "one materialization should serve both stored aliases");
    assert_eq!(rt::read_int(rt::fai_data_tag(left)), 0);
    rt::fai_drop(left);
    rt::fai_drop(right);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn unused_native_payloads_are_released() {
    let mut h = Harness::new(fai_db::AllocationControl::Enabled);
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("ignore", &[rt::make_str("abc")]);
    assert_eq!(rt::read_int(value), 1);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn native_result_aliases_preserve_the_payload_owner() {
    let mut h = Harness::new(fai_db::AllocationControl::Enabled);
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("shared", &[rt::make_str("abc")]);
    assert_eq!(rt::read_int(value), 2);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn native_result_components_survive_worker_serialization() {
    let (db, file) = source(CONCURRENT_MAIN, fai_db::AllocationControl::Enabled);
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    rt::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = rt::capture_take();
    assert_eq!((exit, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_result_components_link_and_run_in_aot() {
    let (db, file) = source(CONCURRENT_MAIN, fai_db::AllocationControl::Enabled);
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("native-results")).unwrap();
    let build = fai_driver::build_native(&db, file, &path);
    assert!(build.ok, "{:?}", build.diagnostics);
    let output = std::process::Command::new(build.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}
