//! Checked text slicing across borrowed entries, owned callbacks and native bundles.
use std::sync::Mutex;

use fai_db::{Db, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

static RUN: Mutex<()> = Mutex::new(());

const CUT: &str = "module Cut\npublic cut : String -> Int -> Int -> Result String RegexEngine.Error\nlet cut text start finish = RegexEngine.slice text start finish\n";
const CALLBACK: &str =
    "module Callback\npublic apply : ('a -> 'b) -> 'a -> 'b\nlet apply f value = f value\n";
const MAIN: &str = r#"module Main
public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  let source = String.join "" ["Aé", "🙂B"]
  let partial = Callback.apply (RegexEngine.slice source 1) 7
  let (first, second) = Async.parallel2 runtime.concurrency
    (fun _ -> Cut.cut source 1 3)
    (fun _ -> Cut.cut source 3 7)
  let invalid = Cut.cut source 2 3
  let oversized = Callback.apply (Cut.cut source 0) 9223372036854775807
  let empty = Cut.cut source 8 8
  runtime.console.writeLine (if partial = Ok "é🙂" && first = Ok "é" && second = Ok "🙂" && Result.isErr invalid && Result.isErr oversized && empty = Ok "" && source = "Aé🙂B" then "ok" else "bad")
"#;

fn source(main: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Cut.fai".into(), CUT.into());
    db.add_source("Callback.fai".into(), CALLBACK.into());
    let id = db.add_source("Main.fai".into(), main.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn shared_slices_and_owned_callbacks_agree_in_jit() {
    let _guard = RUN.lock().unwrap();
    let (db, file) = source(MAIN);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(output, "ok\n");
}

#[test]
fn slice_borrowing_survives_worker_serialization() {
    let (db, file) = source(MAIN);
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let cut =
        bundle.defs.iter().find(|def| def.id.module == "Cut" && def.id.name == "cut").unwrap();
    assert!(cut.entry_borrowed[0]);
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN.lock().unwrap();
    fai_runtime::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((exit, output.as_str()), (0, "ok\n"));
}

#[test]
fn shared_slices_and_owned_callbacks_agree_in_aot() {
    let (db, file) = source(MAIN);
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("text-slicing")).unwrap();
    let build = fai_driver::build_native(&db, file, &path);
    assert!(build.ok, "{:?}", build.diagnostics);
    let result = std::process::Command::new(build.artifact.unwrap()).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(result.stdout, b"ok\n");
}

#[test]
fn slice_borrowing_edits_match_clean_compilation() {
    let initial = "module Cut\npublic cut : String -> Result String RegexEngine.Error\nlet cut text = RegexEngine.slice text 0 1\n";
    let edited = initial.replace("text 0 1", "text 1 2");
    let consuming = initial.replace("RegexEngine.slice text 0 1", "Ok text");
    let proxy = "module Proxy\npublic cut : String -> Result String RegexEngine.Error\nlet cut text = Cut.cut text\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[
            &[("Cut.fai", initial), ("Proxy.fai", proxy)],
            &[("Cut.fai", &edited), ("Proxy.fai", proxy)],
            &[("Cut.fai", &consuming), ("Proxy.fai", proxy)],
            &[("Cut.fai", initial), ("Proxy.fai", proxy)],
        ],
        |db, ids| {
            ids.iter()
                .map(|id| {
                    let file = db.source_file(*id).unwrap();
                    let name = Symbol::intern("cut");
                    (
                        fai_rc::borrow_signature(db, file, name).0,
                        fai_core::pretty_def(&fai_rc::rc(db, file, name)),
                    )
                })
                .collect::<Vec<_>>()
        },
    );
}

#[test]
fn a_slice_range_edit_keeps_the_callers_ownership_code() {
    let (mut db, _) = source("module Main\nlet inspect text = Cut.cut text 0 1\n");
    let proxy = db.add_source(
        "Proxy.fai".into(),
        "module Proxy\nlet inspect text = Cut.cut text 0 1\n".into(),
    );
    let file = db.source_file(proxy).unwrap();
    let name = Symbol::intern("inspect");
    let before = fai_rc::rc(&db, file, name);
    assert_eq!(fai_rc::borrow_signature(&db, file, name).0, [true]);
    db.add_source(
        "Cut.fai".into(),
        CUT.replace(
            "RegexEngine.slice text start finish",
            "RegexEngine.slice text finish start",
        ),
    );
    let after = fai_rc::rc(&db, file, name);
    assert!(std::sync::Arc::ptr_eq(&before, &after));
}
