//! Pair encoding across borrowed, first-class, worker and native call boundaries.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase, Setter, SourceFile};
use fai_syntax::Symbol;

static RUN: Mutex<()> = Mutex::new(());

const ENCODE: &str = r#"
public testRenderPairs : String -> String -> String -> String -> List (String * String) -> Bytes
let testRenderPairs prefix separator ending suffix fields =
  Prim.textPairsRenderList prefix separator ending suffix fields

public testRenderRows : String -> String -> String -> String -> Int -> Bool -> List (Int * String * String) -> Bytes
let testRenderRows prefix separator ending suffix selected reverse fields =
  Prim.textPairsRenderRows prefix separator ending suffix selected reverse fields
"#;

const MAIN: &str = r#"module Main
public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  let base = "prefix" ++ String.joinArray "" (Array.repeat 64 "é🙂") ++ "suffix"
  let view = String.substring 6 128 base
  let fields = [("é", "one"), ("é", "🙂"), ("", ""), ("z", view)]
  let expected = Bytes.fromString ("[é=one;é=🙂;=;z=" ++ view ++ ";]")
  let (first, second) = Async.parallel2 runtime.concurrency
    (fun _ -> String.testRenderPairs "[" "=" ";" "]" fields)
    (fun _ -> Callback.call (String.testRenderPairs "[" "=" ";" "]") fields)
  let empty = String.testRenderPairs "left" "unused" "unused" "right" []
  let rows = [(63, "first", "é"), (0, "skip", ""), (63, "last", view)]
  let mask = -9223372036854775807 - 1
  let (selected, indirect) = Async.parallel2 runtime.concurrency
    (fun _ -> String.testRenderRows "[" "=" ";" "]" mask true rows)
    (fun _ -> Callback.call (String.testRenderRows "[" "=" ";" "]" mask true) rows)
  let ordered = Bytes.fromString ("[last=" ++ view ++ ";first=é;]")
  runtime.console.writeLine (if first = expected && second = expected && empty = Bytes.fromString "leftright" && List.length fields = 4 && String.length view = 128 && selected = ordered && indirect = ordered && mask = -9223372036854775807 - 1 then "ok" else "bad")
"#;

fn source() -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let string = db
        .all_source_files()
        .into_iter()
        .find(|file| file.path(&db).ends_with("/String.fai"))
        .unwrap();
    let text = format!("{}{ENCODE}", string.text(&db));
    string.set_text(&mut db).to(text);
    db.add_source(
        "Callback.fai".into(),
        "module Callback\npublic call : ('a -> 'b) -> 'a -> 'b\nlet call f x = f x\n".into(),
    );
    let id = db.add_source("Main.fai".into(), MAIN.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn encoding_preserves_shared_fields_and_views_in_jit() {
    let _guard = RUN.lock().unwrap();
    let (db, file) = source();
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let output = fai_runtime::capture_take();
    assert_eq!((result.exit_code, output.as_str()), (0, "ok\n"), "{:?}", result.diagnostics);
}

#[test]
fn encoding_preserves_per_argument_borrows_across_worker_serialization() {
    let (db, file) = source();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let encode = bundle
        .defs
        .iter()
        .find(|def| def.id.module == "String" && def.id.name == "testRenderPairs")
        .unwrap();
    assert_eq!(encode.entry_borrowed, [true; 5]);
    let selected = bundle
        .defs
        .iter()
        .find(|def| def.id.module == "String" && def.id.name == "testRenderRows")
        .unwrap();
    assert_eq!(selected.entry_borrowed, [true, true, true, true, false, false, true]);
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN.lock().unwrap();
    fai_runtime::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((exit, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_encoding_preserves_owned_callback_arguments() {
    let (db, file) = source();
    let directory = tempfile::tempdir().unwrap();
    let output =
        camino::Utf8PathBuf::from_path_buf(directory.path().join("pair-encoding")).unwrap();
    let built = fai_driver::build_native(&db, file, &output);
    assert!(built.ok, "{:?}", built.diagnostics);
    let run = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(run.stdout, b"ok\n");
}

#[test]
fn encoder_body_edits_preserve_caller_types_and_match_clean_compilation() {
    let initial = "module Encode\npublic fields : List (String * String) -> Bytes\nlet fields pairs = String.testRenderPairs \"[\" \"=\" \";\" \"]\" pairs\n";
    let edited = initial.replace("\";\"", "\"!\"");
    let setup = |encoder: &str| {
        let (mut db, _) = source();
        db.add_source("Encode.fai".into(), encoder.into());
        let id = db.add_source(
            "Consumer.fai".into(),
            "module Consumer\npublic fields : List (String * String) -> Bytes\nlet fields pairs = Encode.fields pairs\n".into(),
        );
        let file = db.source_file(id).unwrap();
        (db, file)
    };
    let (mut db, file) = setup(initial);
    let name = Symbol::intern("fields");
    let before = fai_types::body_types(&db, file, name);
    db.enable_event_log();
    db.add_source("Encode.fai".into(), edited.clone());
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
