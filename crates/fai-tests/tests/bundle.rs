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
fn native_capability_sample_recipe_runs() {
    // Verify the `samples/NativeCapability.fai` recipe end to end: build the
    // native C function it documents into the shared library its `fai.toml`
    // names, then load and run the real sample through the JIT. Keeps the
    // sample's documented C/`fai.toml`/Fai code from drifting out of sync.
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let sample =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/NativeCapability.fai");
    let src = std::fs::read_to_string(&sample).expect("read the sample");

    let dir = workspace(&[("Main.fai", &src)]);
    std::fs::write(
        dir.join("fai.toml"),
        "[native]\nlibrary-dirs = [\"native\"]\nlibraries = [\"process\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("native")).unwrap();
    // The exact C source the sample's comments document.
    std::fs::write(
        dir.join("native/process.c"),
        "#include <stdint.h>\n#include <unistd.h>\nint64_t app_getpid(void){return (int64_t)getpid();}\n",
    )
    .unwrap();
    let lib = dir.join(format!("native/libprocess.{ext}"));
    let built = std::process::Command::new(&cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(dir.join("native/process.c").as_std_path())
        .arg("-o")
        .arg(lib.as_std_path())
        .status();
    match built {
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
            .expect("the sample compiles to a bundle");

    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!(exit, 0, "the sample runs cleanly");
    // `getpid` varies; assert the documented shape (`pid = <digits>`).
    let pid = output.strip_prefix("pid = ").and_then(|s| s.strip_suffix('\n'));
    assert!(
        pid.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        "expected `pid = <number>`, got {output:?}"
    );
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
fn wide_float_return_survives_the_json_transport_hop() {
    let library = "module Wide\npublic type V = { x : Float, y : Float, z : Float }\npublic shift : V -> V\nlet shift v = { x = v.x + 1.0, y = v.y + 2.0, z = v.z + 3.0 }\n";
    let main = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let v = Wide.shift { x = 1.0, y = 2.0, z = 3.0 }\n  r.console.writeLine (Int.toString (Float.toInt (v.x + v.y + v.z)))\n";
    let dir = workspace(&[("Main.fai", main), ("Wide.fai", library)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let json = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&json).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "12\n");
}

#[test]
fn owned_map_slots_survive_the_json_transport_hop() {
    let source = indoc! {r#"
        module Main
        let bump xs = Array.map (fun x -> x + 0.5) xs
        let same xs = Array.map identity xs
        public main : Runtime -> Unit / { Console }
        let main r =
          let xs = Array.init 3 (fun i -> Int.toFloat i)
          let ys = same (bump xs)
          r.console.writeLine (if Array.toList ys = [0.5, 1.5, 2.5] then "yes" else "no")
    "#};
    let dir = workspace(&[("Main.fai", source)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let json = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&json).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&decoded);
    assert_eq!(exit, 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn owned_float_record_layout_guards_survive_transport() {
    let source = "module Main\ntype Quad = { a : Float, b : Float, c : Float, d : Float }\nlet make a b c d = { a = a, b = b, c = c, d = d }\nmove : Quad -> Quad\nlet move p = { p with b = p.b + 1.0 }\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let xs = [| make 1.0 2.0 3.0 4.0 |]\n  let ys = Array.map move xs\n  let zs = Array.map move ys\n  r.console.writeLine (Float.toString (Array.unsafeGet 0 zs).b)\n";
    let dir = workspace(&[("Main.fai", source)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let encoded = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&encoded).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "4.0\n");
}

#[test]
fn invariant_callback_entries_survive_transport() {
    let source = "module Main\nlet loop f n x = if n <= 0 then x else loop f (n - 1) (f x)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (loop (fun x -> x + 1) 1000 0))\n";
    let dir = workspace(&[("Main.fai", source)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "1000\n");
}

#[test]
fn unique_constructor_tag_evidence_survives_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/UniqueTags.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "39\n");
}

#[test]
fn borrowed_uniform_fields_survive_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/BorrowedFields.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "6\n");
}

#[test]
fn spread_self_loops_survive_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/SpreadLoop.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "1000000\n");
}

#[test]
fn canonical_callbacks_survive_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/CanonicalCallback.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "9223372036854775805\n");
}

#[test]
fn numeric_array_loop_versions_survive_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/NumericArrayLoop.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn borrowed_scalar_list_scans_survive_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/BorrowedListScan.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn exact_scalar_fixed_points_survive_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/ScalarFixedPoint.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "9223372036854775807\n");
}

#[test]
fn bulk_repeat_survives_worker_transport() {
    let source = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let values = Array.repeat 3 1.25\n  let total = Array.foldl (fun acc value -> acc + value) 0.0 values\n  r.console.writeLine (Float.toString total ++ \" \" ++ Int.toString (Array.length values))\n";
    let dir = workspace(&[("Main.fai", source)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "3.75 3\n");
}

#[test]
fn shared_integer_boxes_survive_worker_transport() {
    let dir = workspace(&[
        ("Main.fai", include_str!("fixtures/shared_int/Main.fai")),
        ("Boxed.fai", include_str!("fixtures/shared_int/Boxed.fai")),
    ]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn scalar_specializations_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/ScalarSpecialization.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    let decoded: fai_driver::WireBundle = serde_json::from_slice(&bytes).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "9 -9223372036854775807 16\n");
}

#[test]
fn repeated_scalar_maps_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/RepeatedScalarMaps.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded: fai_driver::WireBundle =
        serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn dying_field_transfers_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/DyingFields.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded: fai_driver::WireBundle =
        serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn canonical_comparator_predicates_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/CanonicalComparator.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded: fai_driver::WireBundle =
        serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn integer_state_returns_survive_worker_transport() {
    let dir = workspace(&[
        ("Pairs.fai", include_str!("fixtures/integer_state/Pairs.fai")),
        ("Main.fai", include_str!("fixtures/integer_state/Extra.fai")),
    ]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded: fai_driver::WireBundle =
        serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn reuse_entry_option_results_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/ReuseOptionReturn.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded: fai_driver::WireBundle =
        serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn local_constructor_branches_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/LocalConstructors.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded: fai_driver::WireBundle =
        serde_json::from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "first\nsecond\nyes\n");
}

#[test]
fn affine_predicates_survive_worker_transport() {
    let dir = workspace(&[("Main.fai", include_str!("fixtures/AffinePredicates.fai"))]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();
    let decoded = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    assert_eq!(jit_run_bundle(&decoded), 0);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
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
fn deeply_nested_bundle_survives_the_json_hop() {
    // Helper inlining folds the `Async` combinators (and the std functions they call)
    // into `main`, producing a lowered expression that nests deeper than serde_json's
    // default recursion limit. The bundle must still round-trip the JSON transport
    // hop: `bundle_from_slice` disables that guard for our own trusted output, where
    // the plain `serde_json::from_slice` rejects it. (Regression: a program composing
    // a few concurrency combinators previously failed `fai run` with "malformed run
    // bundle: recursion limit exceeded".)
    let src = indoc! {r#"
        module Main

        sumDoubled : Concurrency -> Int -> Int -> Int / { Concurrency }
        let sumDoubled c a b =
          let (ra, rb) = Async.parallel2 c (fun u -> a * 2) (fun u -> b * 2)
          ra + rb

        sumSquares : Concurrency -> List Int -> Int / { Concurrency }
        let sumSquares c xs = List.sum (Async.mapConcurrent c (fun x -> x * x) xs)

        consume : Concurrency -> Channel Int -> Int / { Concurrency }
        let consume c ch = List.sum (Async.collect c ch)

        sumChannel : Concurrency -> List Int -> Int / { Concurrency }
        let sumChannel c items = Async.pipe c 4 (Async.produceList c items) (consume c)

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          let a = sumDoubled runtime.concurrency 3 4
          let b = sumSquares runtime.concurrency [1, 2, 3, 4, 5]
          let c = sumChannel runtime.concurrency [1, 2, 3, 4, 5]
          let d = sumDoubled runtime.concurrency 5 6
          let e = sumSquares runtime.concurrency [6, 7, 8, 9]
          let f = sumChannel runtime.concurrency [6, 7, 8, 9]
          let total = a + b + c + d + e + f
          runtime.console.writeLine ("a=" ++ Int.toString a ++ " total=" ++ Int.toString total)
    "#};
    let dir = workspace(&[("Main.fai", src)]);
    let session = Session::open(dir).unwrap();
    let bundle = build_run_bundle(session.db(), entry(&session, "Main.fai")).bundle.unwrap();

    let json = serde_json::to_vec(&bundle).unwrap();
    // The default guard rejects the deeply nested bundle (so the regression is real)...
    assert!(
        serde_json::from_slice::<fai_driver::WireBundle>(&json).is_err(),
        "this program is meant to nest past serde_json's default recursion limit"
    );
    // ...but the recursion-limit-disabled reader (used by the run/test workers) accepts it.
    let decoded = fai_driver::bundle_from_slice(&json).expect("trusted bundle round-trips");

    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = jit_run_bundle(&decoded);
    let output = fai_runtime::capture_take();
    assert_eq!(exit, 0, "the reconstructed concurrent program runs cleanly");
    // a=14; total = 14 + 55 + 15 + 22 + 230 + 30 = 366.
    assert_eq!(output, "a=14 total=366\n");
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
