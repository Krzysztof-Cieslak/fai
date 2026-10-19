//! Byte-class scanning across borrowed, first-class and serialized native entries.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase, Setter, SourceFile};
use fai_syntax::Symbol;

static RUN: Mutex<()> = Mutex::new(());

const SPAN: &str = r#"
public testSpan : Bytes -> Int -> Int -> (Int * Int * Int * Int) -> Int
let testSpan bytes start finish words = Prim.bytesSpan bytes start finish words
"#;

const MAIN: &str = r#"module Main
public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  let bytes = Bytes.fromList [63, 64, 126, 127]
  let words = (-9223372036854775807 - 1, 9223372036854775807, 0, 0)
  let (first, second) = Async.parallel2 runtime.concurrency
    (fun _ -> Bytes.testSpan bytes 0 4 words)
    (fun _ -> Callback.call (Bytes.testSpan bytes 1) 4 words)
  let complete = Bytes.testSpan bytes (-9223372036854775807 - 1) 9223372036854775807 (-1, -1, -1, -1)
  let uniform = Uniform.scan { enabled = true, extra = 42 } bytes (-9223372036854775807 - 1) 9223372036854775807 (-1, -1, -1, -1)
  runtime.console.writeLine (if first = 3 && second = 3 && complete = 4 && uniform && Bytes.toList bytes = [63, 64, 126, 127] && words = (-9223372036854775807 - 1, 9223372036854775807, 0, 0) then "ok" else "bad")
"#;

fn source() -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let bytes = db
        .all_source_files()
        .into_iter()
        .find(|file| file.path(&db).ends_with("/Bytes.fai"))
        .unwrap();
    let text = format!("{}{SPAN}", bytes.text(&db));
    bytes.set_text(&mut db).to(text);
    db.add_source(
        "Callback.fai".into(),
        "module Callback\npublic call : ('a -> 'b) -> 'a -> 'b\nlet call f x = f x\n".into(),
    );
    db.add_source(
        "Uniform.fai".into(),
        r#"module Uniform
public scan : { enabled : Bool | 'r } -> Bytes -> Int -> Int -> (Int * Int * Int * Int) -> Bool
let scan settings bytes start finish words =
  let found = Bytes.testSpan bytes start finish words
  settings.enabled && found = 4 && start = -9223372036854775807 - 1 && finish = 9223372036854775807
"#
        .into(),
    );
    let id = db.add_source("Main.fai".into(), MAIN.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn byte_spans_preserve_shared_buffers_classes_and_uniform_indices_in_jit() {
    let _guard = RUN.lock().unwrap();
    let (db, file) = source();
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let output = fai_runtime::capture_take();
    assert_eq!((result.exit_code, output.as_str()), (0, "ok\n"), "{:?}", result.diagnostics);
}

#[test]
fn byte_span_borrowing_survives_worker_serialization() {
    let (db, file) = source();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let span = bundle
        .defs
        .iter()
        .find(|def| def.id.module == "Bytes" && def.id.name == "testSpan")
        .unwrap();
    assert_eq!(span.entry_borrowed, [true, false, false, true]);
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN.lock().unwrap();
    fai_runtime::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((exit, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_byte_spans_preserve_owned_callback_boundaries() {
    let (db, file) = source();
    let directory = tempfile::tempdir().unwrap();
    let output = camino::Utf8PathBuf::from_path_buf(directory.path().join("byte-spans")).unwrap();
    let built = fai_driver::build_native(&db, file, &output);
    assert!(built.ok, "{:?}", built.diagnostics);
    let run = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(run.stdout, b"ok\n");
}

#[test]
fn byte_span_reader_edits_preserve_caller_types_and_match_clean_compilation() {
    let initial = "module Lookup\npublic scan : Bytes -> Int\nlet scan bytes = Bytes.testSpan bytes 0 (Bytes.length bytes) (-1, -1, -1, -1)\n";
    let edited = initial.replace("bytes 0 (", "bytes 1 (");
    let consumer =
        "module Consumer\npublic scan : Bytes -> Int\nlet scan bytes = Lookup.scan bytes\n";
    let setup = |lookup: &str| {
        let (mut db, _) = source();
        db.add_source("Lookup.fai".into(), lookup.into());
        let id = db.add_source("Consumer.fai".into(), consumer.into());
        let file = db.source_file(id).unwrap();
        (db, file)
    };
    let (mut db, file) = setup(initial);
    let name = Symbol::intern("scan");
    let before = fai_types::body_types(&db, file, name);
    db.enable_event_log();
    db.add_source("Lookup.fai".into(), edited.clone());
    let after = fai_types::body_types(&db, file, name);
    assert_eq!(before, after);
    assert!(!db.take_events().iter().any(|event| event.contains("infer_scc_query")));
    let (clean, clean_file) = setup(&edited);
    assert_eq!(after, fai_types::body_types(&clean, clean_file, name));
    assert_eq!(
        fai_core::pretty_def(&fai_rc::rc(&db, file, name)),
        fai_core::pretty_def(&fai_rc::rc(&clean, clean_file, name))
    );
}
