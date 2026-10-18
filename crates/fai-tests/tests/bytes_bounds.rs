//! Byte bounds proofs preserve faults, full-width arithmetic and transport facts.

use fai_db::{Db, FaiDatabase, Setter};
use fai_syntax::Symbol;

const SCAN: &str = "module Main\nlet scan i n acc xs = if i >= n then acc else scan (i + 1) n (acc + Bytes.unsafeGet i xs) xs\npublic sum : Bytes -> Int\nlet sum xs = scan 0 (Bytes.length xs) 0 xs\n";

const POSITIVE: &str = r#"
public safe : Int -> Bytes -> Int
let safe i xs =
  let limit = Support.size 1 xs
  if i >= 0 && i < limit then Bytes.unsafeGet i xs else -1
public main : Runtime -> Unit / { Console, Concurrency }
let main r =
  let bytes = Bytes.fromList [0, 127, 128, 255]
  let alias = bytes
  let (_, carried) = Support.carry 1 bytes
  let (left, right) = Async.parallel2 r.concurrency (fun _ -> sum bytes) (fun _ -> sum carried)
  let text = Bytes.fromString "é"
  let mapped = Array.map (fun i -> safe i alias) (Array.range 0 4)
  let ok = left = 510 && right = 510 && sum Bytes.empty = 0 &&
           Array.sum mapped = 510 && safe 0 text = 195 && safe 1 text = 169 &&
           safe 2 text = -1 && safe (-1) bytes = -1 &&
           safe 9223372036854775807 bytes = -1 && safe (-9223372036854775808) bytes = -1 &&
           bytes = alias
  r.console.writeLine (if ok then "ok" else "bad")
"#;

const SUPPORT: &str = "module Support\npublic size : Int -> Bytes -> Int\nlet size n xs = if n <= 0 then Bytes.length xs else size (n - 1) xs\npublic carry : Int -> Bytes -> (Int * Bytes)\nlet carry n xs = if n <= 0 then (0, xs) else carry (n - 1) xs\n";

fn database(source: &str) -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Support.fai".into(), SUPPORT.into());
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[track_caller]
fn worker(case: &str, transport: &str, succeeds: bool) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "bytes_worker", "--nocapture"])
        .env("FAI_BYTES_CASE", case)
        .env("FAI_BYTES_TRANSPORT", transport)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if succeeds {
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.lines().any(|line| line == "ok"), "{stdout}\n{stderr}");
    } else {
        assert!(!output.status.success(), "invalid index was accepted: {stdout}");
        assert!(stderr.contains("Bytes.unsafeGet: index out of bounds"), "{stderr}");
        assert!(!stderr.contains("bounds-check elimination unsound"), "{stderr}");
    }
}

#[test]
fn bytes_bounds_run_under_shadow_checks() {
    worker("positive", "jit", true);
}

#[test]
fn bytes_bounds_survive_worker_serialization() {
    worker("positive", "wire", true);
}

#[test]
fn bytes_bounds_run_in_aot() {
    worker("positive", "aot", true);
}

#[test]
fn bytes_negative_index_retains_its_fault() {
    worker("negative", "jit", false);
}

#[test]
fn bytes_empty_buffer_retains_its_fault() {
    worker("empty", "wire", false);
}

#[test]
fn bytes_endpoint_retains_its_fault() {
    worker("endpoint", "jit", false);
}

#[test]
fn bytes_wrapping_index_retains_its_fault() {
    worker("wrapping", "jit", false);
}

#[test]
fn bytes_other_buffer_retains_its_fault() {
    worker("other", "jit", false);
}

#[test]
fn bytes_replaced_loop_buffer_retains_its_fault() {
    worker("replacement", "wire", false);
}

#[test]
fn bytes_nondominating_guard_retains_its_fault() {
    worker("branch", "aot", false);
}

#[test]
fn bytes_generated_callers_keep_unknown_indices_checked() {
    worker("generated", "wire", false);
}

#[test]
fn bytes_worker() {
    let Ok(case) = std::env::var("FAI_BYTES_CASE") else { return };
    let (definitions, expression) = match case.as_str() {
        "positive" => ("", ""),
        "disjunction" => (
            "let read i xs = if i >= 0 || i < Bytes.length xs then Bytes.unsafeGet i xs else 0\n",
            "read (-1) (Bytes.fromString \"x\")",
        ),
        "false-conjunction" => (
            "let read i xs = if i < 0 && i >= Bytes.length xs then 0 else Bytes.unsafeGet i xs\n",
            "read (-1) (Bytes.fromString \"x\")",
        ),
        "negative" => (
            "let read i xs = if i < Bytes.length xs then Bytes.unsafeGet i xs else 0\n",
            "read (-1) (Bytes.fromString \"x\")",
        ),
        "empty" => ("", "Bytes.unsafeGet 0 Bytes.empty"),
        "endpoint" => (
            "let read i xs = if i >= 0 && i <= Bytes.length xs then Bytes.unsafeGet i xs else 0\n",
            "read 1 (Bytes.fromString \"x\")",
        ),
        "wrapping" => (
            "let read i xs =\n  let next = i + 1\n  if i >= 0 && next < Bytes.length xs then Bytes.unsafeGet next xs else 0\n",
            "read 9223372036854775807 (Bytes.fromString \"x\")",
        ),
        "other" => (
            "let read i xs ys =\n  let first = Bytes.unsafeGet i xs\n  first + Bytes.unsafeGet i ys\n",
            "read 1 (Bytes.fromString \"xy\") (Bytes.fromString \"z\")",
        ),
        "replacement" => (
            "let read i n xs = if i >= n then 0 else Bytes.unsafeGet i xs + read (i + 1) n (Bytes.slice 0 1 xs)\n",
            "read 0 2 (Bytes.fromString \"xy\")",
        ),
        "branch" => (
            "let read i xs =\n  let value = if i >= 0 && i < Bytes.length xs then Bytes.unsafeGet i xs else 0\n  value + Bytes.unsafeGet i xs\n",
            "read 2 (Bytes.fromString \"x\")",
        ),
        "generated" => (
            "let at depth i xs = if depth <= 0 then Bytes.unsafeGet i xs else at (depth - 1) i xs\nlet read xs =\n  let first = at 0 0 xs\n  first + Array.sum (Array.map (fun _ -> at 0 (-1) xs) (Array.range 0 1))\n",
            "read (Bytes.fromString \"x\")",
        ),
        _ => panic!("unknown byte bounds case"),
    };
    let source = if case == "positive" {
        format!("{SCAN}{POSITIVE}")
    } else {
        format!(
            "module Main\n{definitions}public main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (Int.toString ({expression}))\n"
        )
    };
    let (db, file) = database(&source);
    fai_driver::set_bce_shadow(true);
    match std::env::var("FAI_BYTES_TRANSPORT").unwrap().as_str() {
        "wire" => {
            let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
            let bundle =
                fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
            assert_eq!(fai_driver::jit_run_bundle(&bundle), 0);
        }
        "aot" => {
            let dir = tempfile::tempdir().unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("bytes-bounds")).unwrap();
            let build = fai_driver::build_native(&db, file, &path);
            assert!(build.ok, "{:?}", build.diagnostics);
            let status = std::process::Command::new(build.artifact.unwrap()).status().unwrap();
            std::process::exit(status.code().unwrap_or(1));
        }
        _ => {
            let outcome = fai_driver::jit_run_program(&db, file);
            assert_eq!(outcome.exit_code, 0, "{outcome:?}");
        }
    }
}

#[test]
fn bytes_bounds_edits_match_clean_inference() {
    let replaced = SCAN.replace(
        "(acc + Bytes.unsafeGet i xs) xs",
        "(acc + Bytes.unsafeGet i xs) (Bytes.slice 0 1 xs)",
    );
    let revisions = [[("Main.fai", SCAN)], [("Main.fai", replaced.as_str())], [("Main.fai", SCAN)]];
    let revisions: Vec<_> = revisions.iter().map(|revision| revision.as_slice()).collect();
    fai_tests::assert_incremental_with_std_matches_clean(&revisions, |db, ids| {
        let file = db.source_file(ids[0]).unwrap();
        let name = Symbol::intern("scan");
        (fai_rc::entry_bounds(db, file, name), fai_rc::result_facts(db, file, name))
    });
}

#[test]
fn bytes_disjunction_cannot_validate_a_negative_index() {
    worker("disjunction", "jit", false);
}

#[test]
fn bytes_false_conjunction_cannot_validate_a_negative_index() {
    worker("false-conjunction", "jit", false);
}

#[test]
fn bytes_bounds_comment_edit_retains_cached_object() {
    let (mut db, file) = database(SCAN);
    let name = Symbol::intern("scan");
    let before = fai_driver::object_code(&db, file, name, false);
    db.enable_event_log();
    file.set_text(&mut db).to(format!("// unchanged semantics\n{SCAN}"));
    let after = fai_driver::object_code(&db, file, name, false);
    assert_eq!(before, after);
    assert!(!db.take_events().iter().any(|event| event.contains("object_code")));
}

#[test]
fn bytes_result_fact_edits_match_clean_inference() {
    let changed = SUPPORT.replace("Bytes.length xs", "Bytes.length xs - 1");
    let revisions = [
        [("Support.fai", SUPPORT)],
        [("Support.fai", changed.as_str())],
        [("Support.fai", SUPPORT)],
    ];
    let revisions: Vec<_> = revisions.iter().map(|revision| revision.as_slice()).collect();
    fai_tests::assert_incremental_with_std_matches_clean(&revisions, |db, ids| {
        let file = db.source_file(ids[0]).unwrap();
        (
            fai_rc::result_facts(db, file, Symbol::intern("size")),
            fai_rc::result_facts(db, file, Symbol::intern("carry")),
        )
    });
}
