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

    public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

#[cfg(unix)]
#[test]
fn jit_run_bundle_calls_a_user_foreign_via_a_loaded_library() {
    // A user `foreign` resolved through the JIT: its native code is built as a
    // shared library, declared in `fai.toml`, and loaded by the worker so the
    // symbol resolves; the marshalled call runs through the in-process JIT.
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let src = indoc! {r#"
        module Main

        foreign "fai_jit_triple" triple : Int -> Int / { Console }

        public main : Runtime -> Unit / { Console }
        let main r = r.console.writeLine (Int.toString (triple 14))
    "#};
    let manifest = "[native]\nlibrary-dirs = [\".\"]\nlibraries = [\"jitlib\"]\n";
    let dir = workspace(&[("Main.fai", src), ("fai.toml", manifest)]);

    // Build the shared library `libjitlib.<ext>` in the workspace root.
    let c_path = dir.join("jitlib.c");
    std::fs::write(
        &c_path,
        "#include <stdint.h>\nint64_t fai_jit_triple(int64_t x){return x*3;}\n",
    )
    .unwrap();
    let lib_path = dir.join(format!("libjitlib.{ext}"));
    let status = std::process::Command::new(&cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(c_path.as_std_path())
        .arg("-o")
        .arg(lib_path.as_std_path())
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("skipping: could not build a shared library with `{cc}`");
            return;
        }
    }

    let session = Session::open(dir.clone()).unwrap();
    let native = fai_driver::read_native_manifest(&dir).unwrap();
    let bundle =
        fai_driver::build_run_bundle_with_deps(session.db(), entry(&session, "Main.fai"), &native)
            .bundle
            .expect("a clean program yields a bundle");
    assert!(!bundle.libraries.is_empty(), "the bundle records the native library to load");

    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!(exit, 0, "the foreign-calling program runs cleanly");
    assert_eq!(output, "42\n");
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

        public main : Runtime -> Unit / { Console }
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
fn jit_run_bundle_drops_data_cleanly_with_reconstructed_types() {
    // The worker compiles definitions whose node types are marker types rebuilt
    // from the bundle's `WireTy` projection. This program discards a list of
    // strings, an ADT value, a record, and a float (each through the inlined
    // drop), and frees a deep list (the iterative dead path). A clean (exit 0) run
    // is the runtime's end-of-run leak check, so the reconstructed-type drops
    // released — and freed the children of — every value exactly once, and the
    // deep list drained without overflowing the native stack.
    let src = indoc! {r#"
        module Main

        type Box = { label : String, n : Int }

        compute : Int -> Int
        let compute n =
          let names = ["a", "b", "c"]
          let opt = Some "wrapped"
          let b = { label = "k", n = n }
          let f = 1.5
          let deep = List.range 0 50000
          List.length names + List.length deep + b.n

        public main : Runtime -> Unit / { Console }
        let main r = r.console.writeLine (Int.toString (compute 7))
    "#};
    let dir = workspace(&[("Main.fai", src)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();

    // Through the JSON transport hop, as the daemon ships it to the worker.
    let json = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&json).unwrap();

    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&decoded);
    let output = fai_runtime::capture_take();
    assert_eq!(exit, 0, "the reconstructed-type drops must run leak-free");
    assert_eq!(output, "50010\n");
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
    // A reachable comparison operator used as a first-class value is outside the
    // native subset (FAI7001): no bundle.
    let src = indoc! {r#"
        module Main

        public lt : Int -> Int -> Bool
        let lt = (<)

        public main : Runtime -> Unit / { Console }
        let main r = r.console.writeLine (if lt 1 2 then "lt" else "ge")
    "#};
    let dir = workspace(&[("Main.fai", src)]);
    let session = Session::open(dir).unwrap();
    let result = build_run_bundle(session.db(), entry(&session, "Main.fai"));
    assert!(result.bundle.is_none(), "an unsupported construct must block the bundle");
    assert!(result.diagnostics.iter().any(|d| d.code.as_str() == "FAI7001"));
}
