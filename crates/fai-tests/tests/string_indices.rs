//! String-index ownership through the native and database-free worker paths.

use fai_db::{Db, FaiDatabase};

const SOURCE: &str = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let a = String.take 9223372036854775807 \"abc\"\n  let b = String.drop (-9223372036854775808) \"abc\"\n  let c = String.substring 0 9223372036854775807 \"abc\"\n  let d = String.substring 9223372036854775807 1 \"abc\"\n  r.console.writeLine (if a = \"abc\" && b = \"abc\" && c = \"abc\" && d = \"\" then \"ok\" else \"wrong\")\n";

#[test]
fn native_string_indices_leave_no_live_boxes() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    let temporary = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(temporary.path().join("indices")).unwrap();
    let built = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn worker_string_indices_leave_no_live_boxes() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    let bundle = fai_driver::build_run_bundle(&db, db.source_file(id).unwrap()).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let bundle = fai_driver::bundle_from_slice(&bytes).unwrap();
    fai_runtime::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((code, output.as_str()), (0, "ok\n"));
}
