//! Strict chunk production, fold order and sharing through concatenation removal.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const HELPERS: &str = r#"module Main
produce : Console -> Int -> List Int / { Console }
let produce console n =
  let _ = console.writeLine ("p" ++ Int.toString n)
  [n, n + 10]
consume : Console -> Int -> Int -> Int / { Console }
let consume console acc n =
  let _ = console.writeLine ("c" ++ Int.toString n)
  acc + n
factory : Console -> (Int -> Int -> Int / { Console }) / { Console }
let factory console =
  let _ = console.writeLine "folder"
  consume console
initial : Console -> Int / { Console }
let initial console =
  let _ = console.writeLine "initial"
  0
"#;

fn run(body: &str, native: bool) -> (String, i32) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source =
        format!("{HELPERS}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n{body}");
    let id = db.add_source("Main.fai".into(), source);
    let file = db.source_file(id).unwrap();
    if native {
        let directory = tempfile::tempdir().unwrap();
        let output =
            camino::Utf8PathBuf::from_path_buf(directory.path().join("concat-fold")).unwrap();
        let built = fai_driver::build_native(&db, file, &output);
        assert!(built.ok, "{:?}", built.diagnostics);
        let result = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
        (String::from_utf8(result.stdout).unwrap(), result.status.code().unwrap_or(-1))
    } else {
        let wire = fai_driver::build_run_bundle(&db, file);
        let bundle = wire.bundle.unwrap_or_else(|| panic!("{:?}", wire.diagnostics));
        let encoded = serde_json::to_vec(&bundle).unwrap();
        let bundle = serde_json::from_slice(&encoded).unwrap();
        fai_runtime::capture_start();
        let exit = fai_driver::jit_run_bundle(&bundle);
        (fai_runtime::capture_take(), exit)
    }
}

#[test]
fn direct_concat_map_keeps_argument_and_callback_order() {
    let body = "let total = List.foldl (factory r.console) (initial r.console) (List.concatMap (produce r.console) [1, 2])\nr.console.writeLine (Int.toString total)";
    assert_eq!(run(body, false), ("folder\ninitial\np1\np2\nc1\nc11\nc2\nc12\n26\n".into(), 0));
}

#[test]
fn let_bound_concat_map_finishes_production_before_folder_construction() {
    let body = "let flat = List.concatMap (produce r.console) [1, 2]\nlet total = List.foldl (factory r.console) (initial r.console) flat\nr.console.writeLine (Int.toString total)";
    assert_eq!(run(body, true), ("p1\np2\nfolder\ninitial\nc1\nc11\nc2\nc12\n26\n".into(), 0));
}

#[test]
fn producer_trap_occurs_before_any_consumer_callback() {
    let body = "let flat = List.concatMap (fun n -> [10 / n]) [1, 0]\nlet total = List.foldl (consume r.console) 0 flat\nr.console.writeLine (Int.toString total)";
    let (output, exit) = run(body, true);
    assert_ne!(exit, 0);
    assert!(output.is_empty(), "consumer must not run before strict production fails: {output}");
}

#[test]
fn a_skipped_fold_still_runs_every_producer() {
    let body = "let flat = List.concatMap (produce r.console) [1, 2]\nlet total = if false then List.foldl (consume r.console) 0 flat else 0\nr.console.writeLine (Int.toString total)";
    assert_eq!(run(body, false), ("p1\np2\n0\n".into(), 0));
}

#[test]
fn empty_and_uneven_chunks_keep_fold_order() {
    let body = "let chunks = [[], [1, 2], [], [3], []]\nlet total = List.foldl (fun acc x -> acc * 10 + x) 0 (List.concat chunks)\nr.console.writeLine (Int.toString total)";
    assert_eq!(run(body, false), ("123\n".into(), 0));
}

#[test]
fn shared_flattened_lists_remain_materialized() {
    let body = "let flat = List.concatMap (produce r.console) [1, 2]\nlet first = List.foldl (consume r.console) 0 flat\nlet second = List.length flat\nr.console.writeLine (Int.toString (first + second))";
    assert_eq!(run(body, false), ("p1\np2\nc1\nc11\nc2\nc12\n30\n".into(), 0));
}

#[test]
fn fold_accumulator_float_and_boxed_elements_keep_their_representation() {
    let body = "let chunks = [[\"a\", \"bb\"], [], [\"ccc\"]]\nlet total = List.foldl (fun acc s -> acc + Int.toFloat (String.length s)) 0.5 (List.concat chunks)\nr.console.writeLine (Float.toString total)";
    assert_eq!(run(body, true), ("6.5\n".into(), 0));
}

#[test]
fn concat_body_edits_match_clean_fusion() {
    let before = "module M\nlet produce x = [x, x + 1]\nlet run xs =\n  let flat = List.concatMap produce xs\n  List.foldl (fun acc x -> acc + x) 0 flat\n";
    let after = before.replace("x + 1", "x + 2");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", before)], &[("M.fai", &after)]],
        |db, files| {
            (*fai_core::fuse_def(db, db.source_file(files[0]).unwrap(), Symbol::intern("run")))
                .clone()
        },
    );
}
