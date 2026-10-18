//! Boolean control flow preserves short-circuiting through native and worker ABIs.
use std::sync::Mutex;

use fai_db::{Db, FaiDatabase, SourceFile};

static RUN: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module Main
public choose : Int -> Bool
let choose x = (x >= 3 && x <= 5) || x = 9 || x = 12
let safe values index = index >= 0 && index < Array.length values && Array.unsafeGet index values = 7
let collect n previous out =
  if n = 0 then out
  else
    let selected = if n % 2 = 0 then previous else not previous
    collect (n - 1) selected (selected :: out)
say : Runtime -> String -> Bool -> Bool / { Console }
let say r message value =
  let _ = r.console.writeLine message
  value
public main : Runtime -> Unit / { Console }
let main r =
  let values = Array.map (fun x -> Callback.apply choose x) (Array.range 0 16)
  let count = Array.length (Array.filter identity values)
  let slots = [| 7, 8 |]
  let valid = not (safe slots (-1)) && safe slots 0 && not (safe slots 1) && not (safe slots 2)
  let selected = (say r "a" false || say r "b" true) || say r "skipped" false
  let source = String.join "" ["retained", " text"]
  let stored = String.length source > 0 || String.contains "unused" source
  let shared = if count = 5 then valid else false
  let uniform = if count < 0 then true else Callback.keep selected
  let slotsOk = [shared, not shared, uniform] = [true, false, true]
  let loopOk = collect 5 false [] = [true, false, false, true, true]
  r.console.writeLine (if count = 5 && valid && selected && stored && stored && slotsOk && loopOk && source = "retained text" then "ok" else "bad")
"#;

fn source() -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source(
        "Callback.fai".into(),
        "module Callback\npublic apply : (Int -> Bool) -> Int -> Bool\nlet apply f x = f x\npublic keep : Bool -> Bool\nlet keep value = value\n"
            .into(),
    );
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn native_boolean_decisions_preserve_effects_bounds_and_values() {
    let (db, file) = source();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("booleans")).unwrap();
    let build = fai_driver::build_native(&db, file, &path);
    assert!(build.ok, "{:?}", build.diagnostics);
    let output = std::process::Command::new(build.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"a\nb\nok\n");
}

#[test]
fn serialized_boolean_decisions_preserve_effects_bounds_and_values() {
    let (db, file) = source();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN.lock().unwrap();
    fai_runtime::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((exit, output.as_str()), (0, "a\nb\nok\n"));
}
