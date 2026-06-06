//! The run-bundle pipeline at the driver level: `build_run_bundle` (the warm
//! front end) and `jit_run_bundle` (the database-free worker side), including
//! cross-module reconstruction, the JSON transport hop, and failure reporting.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use fai_core::from_wire;
use fai_db::SourceFile;
use fai_driver::{Session, build_run_bundle, jit_run_bundle};
use indoc::indoc;

/// Serializes the in-process JIT runs (the runtime's output sink is global).
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn workspace(files: &[(&str, &str)]) -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
        "fai-bundle-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).unwrap();
    }
    dir
}

fn entry(session: &Session, name: &str) -> SourceFile {
    *session.select_files(Some(Utf8Path::new(name))).first().expect("entry file")
}

const ARITH: &str = indoc! {r#"
    module Main

    public main : Runtime -> Unit
    let main r = r.console.writeLine (Int.toString (1 + 2 * 3))
"#};

#[test]
fn builds_a_bundle_for_a_single_module() {
    let dir = workspace(&[("Main.fai", ARITH)]);
    let session = Session::open(dir).unwrap();
    let result = build_run_bundle(session.db(), entry(&session, "Main.fai"));
    let bundle = result.bundle.expect("a clean program yields a bundle");
    assert_eq!(bundle.entry.module, "Main");
    assert_eq!(bundle.entry.name, "main");
    assert!(!bundle.defs.is_empty());
}

#[test]
fn cross_module_bundle_reconstructs_distinct_modules() {
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Lib.shout "hi")
    "#};
    let lib = indoc! {r#"
        module Lib

        public shout : String -> String
        let shout s = s ++ "!"
    "#};
    let dir = workspace(&[("Main.fai", main), ("Lib.fai", lib)]);
    let session = Session::open(dir).unwrap();

    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    // Both modules are present and reconstruct to distinct synthetic source ids.
    let rebuilt = from_wire(&bundle);
    let labels: std::collections::BTreeSet<&str> =
        rebuilt.module_labels.values().map(String::as_str).collect();
    assert!(labels.contains("Main") && labels.contains("Lib"), "labels: {labels:?}");
    // Main and Lib reconstruct to distinct synthetic ids (the standard library,
    // pulled in by `++`, contributes more, so there are at least two).
    let ids: std::collections::BTreeSet<_> = rebuilt.defs.iter().map(|d| d.def.file).collect();
    assert!(ids.len() >= 2, "Main and Lib must get distinct source ids");
}

#[test]
fn bundle_survives_the_json_transport_hop() {
    // The daemon writes the bundle as JSON to a temp file; the worker reads it.
    let dir = workspace(&[("Main.fai", ARITH)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();

    let json = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&json).unwrap();

    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&decoded);
    let output = fai_runtime::capture_take();
    assert_eq!(exit, 0, "the reconstructed program should run cleanly");
    assert_eq!(output, "7\n");
}

#[test]
fn jit_run_bundle_executes_a_cross_module_program() {
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Lib.shout "hi")
    "#};
    let lib = indoc! {r#"
        module Lib

        public shout : String -> String
        let shout s = s ++ "!"
    "#};
    let dir = workspace(&[("Main.fai", main), ("Lib.fai", lib)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();

    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!(exit, 0);
    assert_eq!(output, "hi!\n");
}

#[test]
fn no_main_reports_no_entry_point_and_no_bundle() {
    let dir = workspace(&[(
        "M.fai",
        indoc! {r#"
            module M

            let x = 1
        "#},
    )]);
    let session = Session::open(dir).unwrap();
    let result = build_run_bundle(session.db(), entry(&session, "M.fai"));
    assert!(result.bundle.is_none());
    assert!(
        result.diagnostics.iter().any(|d| d.code == fai_driver::NO_ENTRY_POINT),
        "expected NO_ENTRY_POINT, got {:?}",
        result.diagnostics.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn reachable_unsupported_construct_blocks_the_bundle() {
    // A reachable `Char` is outside the native subset (FAI7001): no bundle.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (if 'a' = 'b' then 0 else 1))
    "#};
    let dir = workspace(&[("Main.fai", src)]);
    let session = Session::open(dir).unwrap();
    let result = build_run_bundle(session.db(), entry(&session, "Main.fai"));
    assert!(result.bundle.is_none(), "an unsupported construct must block the bundle");
    assert!(result.diagnostics.iter().any(|d| d.code.as_str() == "FAI7001"));
}
