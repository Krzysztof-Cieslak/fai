//! Inferred loop bounds survive every native transport and emission path.

use std::collections::BTreeMap;
use std::sync::Mutex;

use fai_core::wire::WireDef;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

static RUN_LOCK: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module Main
public run : Int -> Int
let run n =
  let values = Array.range 0 n
  Array.sum (Array.map (fun n -> n * 2) values) + Array.sum values
public main : Runtime -> Unit / { Console }
let main runtime = runtime.console.writeLine (Int.toString (run 64 + run 0 + run (-1)))
"#;

const OTHER_ARRAY: &str = r#"module Main
let total n other =
  let values = Array.range 0 n
  Array.sum (Array.map (fun i -> Array.unsafeGet i other) values)
public main : Runtime -> Unit / { Console }
let main runtime = runtime.console.writeLine (Int.toString (total 3 [| 7 |]))
"#;

fn source(text: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), text.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[track_caller]
fn assert_facts(db: &FaiDatabase, file: SourceFile, defs: &[WireDef]) {
    let actual: BTreeMap<_, _> = defs
        .iter()
        .filter(|def| def.id.module == "Main" && def.id.name.starts_with("fuse#"))
        .map(|def| (def.id.name.clone(), (def.bounds_entry.clone(), def.bounds_result.clone())))
        .collect();
    let expected: BTreeMap<_, _> = actual
        .keys()
        .map(|name| {
            let symbol = Symbol::intern(name);
            (
                name.clone(),
                (
                    (*fai_rc::entry_bounds(db, file, symbol)).clone(),
                    (*fai_rc::result_facts(db, file, symbol)).clone(),
                ),
            )
        })
        .collect();
    assert!(
        expected.values().any(|(entry, _)| !entry.is_empty()),
        "fixture must infer loop entry facts"
    );
    assert!(
        expected.values().any(|(_, result)| !result.edges.is_empty()),
        "fixture must infer builder result facts"
    );
    assert_eq!(actual, expected, "transport must retain the exact inferred facts");
}

#[test]
fn run_workers_receive_generated_entry_and_result_facts() {
    let (db, file) = source(SOURCE);
    let result = fai_driver::build_run_bundle(&db, file);
    let bundle = result.bundle.unwrap_or_else(|| panic!("{:?}", result.diagnostics));
    let rebuilt = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    assert_facts(&db, file, &rebuilt.defs);
}

#[test]
fn contract_workers_receive_generated_entry_and_result_facts() {
    let (db, file) = source(&format!("{SOURCE}\nexample: run 6 = 45\n"));
    let plan = fai_driver::build_test_plan(&db, &[file], None, fai_driver::TestConfig::default());
    assert!(!plan.blocked, "{:?}", plan.pre_diagnostics);
    assert_facts(&db, file, &plan.bundle.defs);
    let _guard = RUN_LOCK.lock().unwrap();
    assert_eq!(fai_driver::jit_test_bundle(&plan.bundle, 0, &mut Vec::new()), 0);
}

#[test]
fn generated_loop_fact_edits_match_clean_bundles() {
    let edited = SOURCE.replace("Array.range 0 n", "Array.range 1 n");
    let comment = format!("// shifted source\n{edited}");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("Main.fai", SOURCE)], &[("Main.fai", &edited)], &[("Main.fai", &comment)]],
        |db, ids| {
            serde_json::to_value(
                fai_driver::build_run_bundle(db, db.source_file(ids[0]).unwrap()).bundle.unwrap(),
            )
            .unwrap()
        },
    );
}

#[test]
fn comment_edits_keep_generated_fact_queries_unchanged() {
    let (mut db, file) = source(SOURCE);
    let name = fai_core::fuse_def(&db, file, Symbol::intern("run")).loops[0].lowered.def.name;
    let before = fai_rc::entry_bounds(&db, file, name);
    assert!(!before.is_empty());
    db.add_source("Main.fai".into(), format!("// same program\n{SOURCE}"));
    let after = fai_rc::entry_bounds(&db, file, name);
    assert!(std::sync::Arc::ptr_eq(&before, &after));
}

#[test]
fn native_generated_loops_preserve_empty_and_negative_counts() {
    let output = native(SOURCE);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"6048\n");
}

#[test]
fn worker_generated_loops_preserve_empty_and_negative_counts() {
    let (db, file) = source(SOURCE);
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN_LOCK.lock().unwrap();
    fai_runtime::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((exit, output.as_str()), (0, "6048\n"));
}

#[test]
fn native_generated_bounds_do_not_validate_a_different_array() {
    let output = native(OTHER_ARRAY);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("array index out of bounds"));
}

#[test]
fn worker_generated_bounds_do_not_validate_a_different_array() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "invalid_generated_worker_child", "--nocapture"])
        .env("FAI_INVALID_GENERATED_WORKER", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("array index out of bounds"));
}

#[test]
fn native_range_facts_preserve_wrapping_full_width_elements() {
    let output = native(
        "module Main\npublic main : Runtime -> Unit / { Console }\nlet main runtime =\n  let values = Array.range 9223372036854775806 (-9223372036854775807)\n  runtime.console.writeLine (if Array.toList values = [9223372036854775806, 9223372036854775807, -9223372036854775808] then \"ok\" else \"wrong\")\n",
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn invalid_generated_worker_child() {
    if std::env::var_os("FAI_INVALID_GENERATED_WORKER").is_none() {
        return;
    }
    let (db, file) = source(OTHER_ARRAY);
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let _ = fai_driver::jit_run_bundle(&bundle);
}

fn native(text: &str) -> std::process::Output {
    let (db, file) = source(text);
    let temporary = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(temporary.path().join("program")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    std::process::Command::new(built.artifact.unwrap()).output().unwrap()
}
