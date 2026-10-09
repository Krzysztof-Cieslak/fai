//! Single-operation lowering preserves strict operand and callback order.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const HELPERS: &str = "module M\nemit : { console : Console | _ } -> Int -> Int / { Console }\nlet emit r x =\n  let _ = r.console.writeLine (Int.toString x)\n  x\nsource : Runtime -> Array Int / { Console }\nlet source r =\n  let _ = r.console.writeLine \"source\"\n  [| 1, 2, 3 |]\ninitial : Runtime -> Int / { Console }\nlet initial r =\n  let _ = r.console.writeLine \"initial\"\n  0\nfactory : Runtime -> (Int -> Int -> Int / { Console }) / { Console }\nlet factory r =\n  let _ = r.console.writeLine \"callback\"\n  fun acc x -> acc + emit r x\n";

#[track_caller]
fn check(body: &str, expected: &str, native: bool) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let body = body.lines().map(|l| format!("  {l}\n")).collect::<String>();
    let source = format!(
        "{HELPERS}\nunary : Runtime -> (Int -> Int / {{ Console }}) / {{ Console }}\nlet unary r =\n  let _ = r.console.writeLine \"callback\"\n  fun x -> emit r x\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n{body}"
    );
    let id = db.add_source("M.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let diagnostics = fai_tests::check_source_diagnostics(&db, file);
    assert!(
        !diagnostics.iter().any(|d| d.severity == fai_diagnostics::Severity::Error),
        "{diagnostics:?}"
    );
    if native {
        let temp = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(temp.path().join("ordered-arrays")).unwrap();
        let result = fai_driver::build_native(&db, file, &path);
        assert!(result.ok, "{:?}", result.diagnostics);
        let output = std::process::Command::new(result.artifact.unwrap()).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    } else {
        fai_runtime::capture_start();
        let result = fai_driver::jit_run_program(&db, file);
        assert_eq!(result.exit_code, 0);
        assert_eq!(fai_runtime::capture_take().trim(), expected);
    }
}

const FOLD: &str = "let sum = Array.foldl (factory r) (initial r) (source r)\nr.console.writeLine (Int.toString sum)";

#[test]
fn fold_operands_run_once_in_source_order() {
    check(FOLD, "callback\ninitial\nsource\n1\n2\n3\n6", false);
}

#[test]
fn native_fold_preserves_operand_order() {
    check(FOLD, "callback\ninitial\nsource\n1\n2\n3\n6", true);
}

#[test]
fn effectful_map_materializes_before_short_circuit_consumer() {
    check(
        "let yes = Array.any (fun x -> x = 1) (Array.map (fun x -> emit r x) (source r))\nr.console.writeLine (if yes then \"yes\" else \"no\")",
        "source\n1\n2\n3\nyes",
        false,
    );
}

#[test]
fn effectful_short_circuit_callback_stops_at_the_match() {
    check(
        "let yes = Array.any (fun x -> emit r x = 2) (source r)\nr.console.writeLine (if yes then \"yes\" else \"no\")",
        "source\n1\n2\nyes",
        false,
    );
}

#[test]
fn standalone_init_calls_in_increasing_index_order() {
    check(
        "let xs = Array.init (emit r 3) (fun i -> emit r i)\nr.console.writeLine (Int.toString (Array.sum xs))",
        "3\n0\n1\n2\n3",
        false,
    );
}

#[test]
fn empty_init_evaluates_operands_without_calling_the_callback() {
    check(
        "let xs = Array.init (emit r 0) (unary r)\nr.console.writeLine (Int.toString (Array.length xs))",
        "0\ncallback\n0",
        false,
    );
}

#[test]
fn foldr_callback_keeps_reverse_order() {
    check(
        "let sum = Array.foldr (fun x acc -> emit r x - acc) 0 (source r)\nr.console.writeLine (Int.toString sum)",
        "source\n3\n2\n1\n2",
        false,
    );
}

#[test]
fn callback_can_capture_its_shared_source() {
    check(
        "let xs = Array.range 0 4\nlet ys = Array.map (fun i -> Array.unsafeGet (3 - i) xs) xs\nr.console.writeLine (Int.toString (Array.sum xs + Array.sum ys))",
        "12",
        false,
    );
}

#[test]
fn map_callback_retains_source_without_a_later_outer_use() {
    check(
        "let xs = Array.range 0 4\nlet ys = Array.map (fun i -> Array.unsafeGet (3 - i) xs) xs\nr.console.writeLine (if Array.toList ys = [3, 2, 1, 0] then \"yes\" else \"no\")",
        "yes",
        false,
    );
}

#[test]
fn native_same_type_map_preserves_raw_float_results() {
    check(
        "let xs = Array.init 3 (fun i -> Int.toFloat i)\nlet ys = Array.map (fun x -> x + 0.5) xs\nr.console.writeLine (if Array.toList ys = [0.5, 1.5, 2.5] then \"yes\" else \"no\")",
        "yes",
        true,
    );
}

#[test]
fn map_changing_element_representation_keeps_a_fresh_buffer() {
    check(
        "let xs = Array.init 3 (fun i -> i)\nlet ys = Array.map (fun x -> Int.toFloat x + 0.5) xs\nr.console.writeLine (if Array.toList ys = [0.5, 1.5, 2.5] then \"yes\" else \"no\")",
        "yes",
        false,
    );
}

#[test]
fn same_type_map_callback_edits_match_clean_generation() {
    let source = "module M\nlet run xs = Array.map (fun x -> 100 / x) xs\n";
    let edited = source.replace("100 / x", "200 / x");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*fai_core::fuse_def(db, db.source_file(files[0]).unwrap(), Symbol::intern("run")))
                .clone()
        },
    );
}

#[test]
fn named_map_callback_edits_match_clean_generation() {
    let source = "module M\nlet step x = x + 1\nlet run xs = Array.map step xs\n";
    let edited = source.replace("x + 1", "x + 2");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*fai_core::fuse_def(db, db.source_file(files[0]).unwrap(), Symbol::intern("run")))
                .clone()
        },
    );
}

#[test]
fn nested_lifted_closures_remain_valid() {
    check(
        "let fs = Array.init 3 (fun x -> fun y -> x + y)\nlet f = Array.unsafeGet 2 fs\nr.console.writeLine (Int.toString (f 40))",
        "42",
        false,
    );
}

#[test]
fn generic_repeat_keeps_boxed_values_alive() {
    check(
        "let xs = Array.repeat 3 \"hello\"\nlet value = Array.foldl (fun acc x -> acc ++ x) \"\" xs\nr.console.writeLine value",
        "hellohellohello",
        false,
    );
}

#[test]
fn callback_edits_match_clean_loop_generation() {
    let source = "module M\nlet run xs = Array.foldl (fun acc x -> acc + 10 / x) 0 xs\n";
    let edited = source.replace("10 / x", "20 / x");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*fai_core::fuse_def(db, db.source_file(files[0]).unwrap(), Symbol::intern("run")))
                .clone()
        },
    );
}

#[test]
fn contract_harness_emits_single_operation_loops() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "M.fai".into(),
        "module M\nexample: Array.toList (Array.init 3 (fun i -> 10 / (i + 1))) = [10, 5, 3]\n"
            .into(),
    );
    let result = fai_driver::test(
        &db,
        &[db.source_file(id).unwrap()],
        None,
        fai_driver::TestConfig::default(),
    );
    assert!(result.ok, "{:?}", result.diagnostics);
    assert_eq!(result.passed, 1);
}

#[test]
fn complete_builder_retains_its_result_length_fact() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "M.fai".into(),
        "module M\nlet build n = Array.init n (fun i -> 10 / (i + 1))\n".into(),
    );
    let result = fai_rc::result_facts(&db, db.source_file(id).unwrap(), Symbol::intern("build"));
    assert!(
        result.edges.contains(&(
            fai_core::RTerm::Param(0),
            fai_core::RTerm::ResultLen(fai_core::WHOLE),
            0
        )),
        "{result:?}"
    );
}

#[test]
fn filtering_builder_does_not_claim_the_input_length() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "M.fai".into(),
        "module M\nlet build n = Array.filter (fun i -> false) (Array.range 0 n)\n".into(),
    );
    let result = fai_rc::result_facts(&db, db.source_file(id).unwrap(), Symbol::intern("build"));
    assert!(
        !result.edges.contains(&(
            fai_core::RTerm::Param(0),
            fai_core::RTerm::ResultLen(fai_core::WHOLE),
            0
        )),
        "{result:?}"
    );
}
