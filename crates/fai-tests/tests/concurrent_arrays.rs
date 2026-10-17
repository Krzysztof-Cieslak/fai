//! Concurrent native array reads retain generic representations and owners.
use fai_db::{Db, FaiDatabase};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn program() -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Access.fai".into(), "module Access\npublic at : Int -> Array 'a -> 'a\nlet at i values = Array.unsafeGet i values\n".into());
    let id = db.add_source("Main.fai".into(), r#"module Main
public main : Runtime -> Unit / { Console, Concurrency }
let main r =
  let nan = Float.fromBits 9221120237041090626
  let numbers = [| nan, 3.5 |]
  let words = [| "kept", "second" |]
  let (first, second) = Async.parallel2 r.concurrency (fun _ -> Access.at 0 numbers) (fun _ -> Access.at 0 words)
  let updated = Array.unsafeSet 0 "new" words
  let wide = [| 9223372036854775807 |]
  let result = Async.parallel r.concurrency [fun _ -> Access.at 0 wide, fun _ -> Array.length numbers]
  r.console.writeLine (if Float.toBits first = Float.toBits nan && second = "kept" && Access.at 0 words = "kept" && Access.at 0 updated = "new" && result = [9223372036854775807, 2] then "ok" else "wrong")
"#.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn worker_generic_reads_preserve_aliases_and_float_bits() {
    let _guard = LOCK.lock().unwrap();
    let (db, file) = program();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let bundle = fai_driver::bundle_from_slice(&bytes).unwrap();
    fai_runtime::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((code, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_generic_reads_preserve_aliases_and_float_bits() {
    let (db, file) = program();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("arrays")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let result = std::process::Command::new(built.artifact.unwrap())
        .env("FAI_WORKERS", "2")
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(result.stdout, b"ok\n");
}
