//! Read-only buffer-list scans retain owners, Unicode semantics and constant stack.

use fai_db::{Db, FaiDatabase};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn program() -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source(
        "Buffers.fai".into(),
        r#"module Buffers
public total : List Bytes -> Int -> Result Int String
let total xs acc =
  match xs with
  | [] -> Ok acc
  | bytes :: rest ->
    let size = Bytes.length bytes
    if size > 100000 - acc then Err "large" else total rest (acc + size)
public text : List String -> Int -> Int
let text xs acc = match xs with | [] -> acc | value :: rest -> text rest (acc + String.length value)
"#
        .into(),
    );
    let id = db.add_source("Main.fai".into(), r#"module Main
let repeat n value acc = if n <= 0 then acc else repeat (n - 1) value (value :: acc)
public main : Runtime -> Unit / { Console, Concurrency }
let main r =
  let buffers = repeat 50000 (Bytes.fromString "ab") []
  let text = repeat 1000 "é🙂" []
  let (complete, overflow) = Async.parallel2 r.concurrency (fun _ -> Buffers.total buffers 0) (fun _ -> Buffers.total buffers 99)
  r.console.writeLine (if complete = Ok 100000 && overflow = Err "large" && List.length buffers = 50000 && Buffers.text text 0 = 2000 then "ok" else "wrong")
"#.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn worker_buffer_scans_preserve_shared_values_and_early_exit() {
    let _guard = LOCK.lock().unwrap();
    let (db, file) = program();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    fai_runtime::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((code, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_buffer_scans_keep_constant_task_stack() {
    let (db, file) = program();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("buffers")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let result = std::process::Command::new(built.artifact.unwrap())
        .env("FAI_WORKERS", "2")
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(result.stdout, b"ok\n");
}

#[test]
fn buffer_scan_edits_preserve_incremental_ownership() {
    let source = "module M\nlet scan xs acc = match xs with | [] -> acc | bytes :: rest -> scan rest (acc + Bytes.length bytes)\n";
    let changed = source.replace("Bytes.length bytes", "Bytes.length (Bytes.concat bytes bytes)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &changed)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = fai_syntax::Symbol::intern("scan");
            (fai_rc::borrow_signature(db, file, name), (*fai_rc::rc_emit(db, file, name)).clone())
        },
    );
}
