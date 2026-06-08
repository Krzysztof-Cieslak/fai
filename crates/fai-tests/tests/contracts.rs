//! End-to-end `fai test`: contracts are collected, synthesized into a harness,
//! JIT-compiled, and run. Covers the `samples/` and `std/` corpora (which must
//! all pass), failure reporting with shrunk counterexamples, determinism, the
//! not-runnable path, and the runtime leak guard.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_driver::{TestConfig, TestOutcome, test};

/// Contract execution allocates through the runtime's process-global object
/// counter, so the leak guard is only meaningful when one run is in flight.
/// Serialize the e2e runs (each `fai test` invocation is its own process in
/// production, where this is automatic).
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Builds a database with the embedded standard library plus the given files,
/// returning the handles for those files (not the std ones).
fn db_with(files: &[(&str, &str)]) -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let handles = files
        .iter()
        .map(|(name, src)| {
            let id = db.add_source(Utf8PathBuf::from(*name), (*src).to_owned());
            db.source_file(id).expect("source file")
        })
        .collect();
    (db, handles)
}

/// Runs `fai test` over `files` with deterministic defaults (serialized).
fn run(files: &[(&str, &str)]) -> TestOutcome {
    let _g = lock();
    let (db, handles) = db_with(files);
    test(&db, &handles, None, TestConfig::default())
}

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn read_sample(name: &str) -> String {
    std::fs::read_to_string(samples_dir().join(name)).expect("sample exists")
}

#[test]
fn sample_contracts_all_pass() {
    // The typecheck-clean samples that carry contracts.
    let names = [
        "Math.fai",
        "Lists.fai",
        "Algebra.fai",
        "Tuples.fai",
        "Geometry.fai",
        "Properties.fai",
        "Cart.fai",
        "Patterns.fai",
    ];
    let sources: Vec<(String, String)> =
        names.iter().map(|n| ((*n).to_owned(), read_sample(n))).collect();
    let files: Vec<(&str, &str)> = sources.iter().map(|(n, s)| (n.as_str(), s.as_str())).collect();

    let outcome = run(&files);
    assert!(
        outcome.ok,
        "samples should pass; diagnostics: {:?}",
        outcome.diagnostics.iter().map(|d| (d.code.as_str(), &d.message)).collect::<Vec<_>>()
    );
    assert!(outcome.total >= 8, "expected the sample contracts, got {}", outcome.total);
    assert_eq!(outcome.passed, outcome.total);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0, "running contracts must not leak");
}

#[test]
fn prelude_contracts_pass() {
    // `Prelude` is embedded; run its own example/forall contracts.
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let prelude = fai_resolve::prelude_module_file(&db).expect("Prelude is embedded");
    let outcome = test(&db, &[prelude], None, TestConfig::default());
    assert!(
        outcome.ok,
        "Prelude contracts should pass; diagnostics: {:?}",
        outcome.diagnostics.iter().map(|d| (d.code.as_str(), &d.message)).collect::<Vec<_>>()
    );
    assert!(outcome.total > 0);
    assert_eq!(outcome.passed, outcome.total);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn wrong_example_fails_located() {
    let outcome = run(&[("Bad.fai", "module Bad\nexample: 1 = 2\n")]);
    assert!(!outcome.ok);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    assert!(d.message.contains("example does not hold"));
    // Located at the contract (line 2).
    assert_eq!(d.primary.start().raw(), "module Bad\n".len() as u32);
}

#[test]
fn wrong_forall_reports_shrunk_counterexample() {
    let outcome = run(&[("Bad.fai", "module Bad\nforall n: n = n + 1\n")]);
    assert!(!outcome.ok);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    let help = d.help.as_deref().unwrap_or("");
    assert_eq!(help, "counterexample: n = 0", "expected the minimal counterexample");
}

#[test]
fn multi_binder_counterexample_names_each_binder() {
    let outcome = run(&[("Bad.fai", "module Bad\nforall a b: a + b = a\n")]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    let help = d.help.as_deref().unwrap_or("");
    assert_eq!(help, "counterexample: (a, b) = (0, 1)");
}

#[test]
fn list_counterexample_shrinks_length_and_elements() {
    let outcome = run(&[("Bad.fai", "module Bad\nforall xs: List.length xs = 0\n")]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    assert_eq!(d.help.as_deref(), Some("counterexample: xs = [0]"));
}

#[test]
fn contract_results_are_deterministic() {
    let prog = "module Bad\nforall n: n * 2 = n\n";
    let first = run(&[("Bad.fai", prog)]);
    let second = run(&[("Bad.fai", prog)]);
    let help = |o: &TestOutcome| {
        o.diagnostics
            .iter()
            .find(|d| d.code.as_str() == "FAI6001")
            .and_then(|d| d.help.clone())
            .unwrap_or_default()
    };
    assert_eq!(help(&first), help(&second), "the same seed yields the same counterexample");
}

#[test]
fn function_typed_binder_is_not_runnable() {
    let src = "module F\npublic apply0 : (Int -> Int) -> Int\nlet apply0 f = f 0\n\
               forall f: apply0 f = apply0 f\n";
    let outcome = run(&[("F.fai", src)]);
    assert!(!outcome.ok);
    assert_eq!(outcome.not_run, 1);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6002").expect("FAI6002");
    assert!(d.message.contains("cannot be run"), "got: {}", d.message);
}

/// Runs every standard-library module's and every sample's contracts, printing a
/// per-module report and asserting nothing fails or is un-runnable.
#[test]
fn full_corpus_report() {
    let _g = lock();
    let mut db = FaiDatabase::new();
    let std_ids = fai_types::std_lib::load_std(&mut db);

    // Load the samples too (so cross-file refs resolve).
    let mut sample_files: Vec<SourceFile> = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(samples_dir())
        .expect("samples/")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    entries.sort();
    for path in &entries {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(path).unwrap();
        let id = db.add_source(Utf8PathBuf::from(name), src);
        sample_files.push(db.source_file(id).expect("sample"));
    }

    let std_files: Vec<SourceFile> = std_ids.iter().filter_map(|&id| db.source_file(id)).collect();

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failures = 0usize; // genuine FAI6001 (or other errors)
    let mut not_run = 0usize; // FAI6002 (Stage-2 generators)
    let mut report = |label: &str, file: SourceFile, db: &dyn Db| {
        let outcome = test(db, &[file], None, TestConfig::default());
        if outcome.total == 0 {
            return;
        }
        let failed = outcome.total - outcome.passed - outcome.not_run;
        total += outcome.total;
        passed += outcome.passed;
        not_run += outcome.not_run;
        failures += failed;
        let leak =
            if outcome.leaked != 0 { format!("  LEAK {}", outcome.leaked) } else { String::new() };
        println!(
            "{label:<10} {:>3} passed, {failed:>2} failed, {:>2} not-run  (of {}){leak}",
            outcome.passed, outcome.not_run, outcome.total
        );
        for d in &outcome.diagnostics {
            if d.code.as_str().starts_with("FAI6") {
                let help = d.help.as_deref().map_or(String::new(), |h| format!(" ({h})"));
                println!("    [{}] {}{help}", d.code, d.message);
            }
        }
        assert_eq!(outcome.leaked, 0, "{label} leaked objects");
    };

    println!("\n--- standard library ---");
    for &file in &std_files {
        let label = fai_resolve::module_name(&db, file)
            .map_or_else(|| "?".to_owned(), |m| m.0.as_str().to_owned());
        report(&label, file, &db);
    }
    println!("\n--- samples ---");
    for &file in &sample_files {
        let label = fai_resolve::module_name(&db, file)
            .map_or_else(|| "?".to_owned(), |m| m.0.as_str().to_owned());
        report(&label, file, &db);
    }
    println!("\n=== {passed} passed, {failures} failed, {not_run} not-run (of {total}) ===");

    // Every std + sample contract runs and passes (records and ADTs now have
    // synthesized generators).
    assert_eq!(failures, 0, "no contract should fail across std + samples");
    assert_eq!(not_run, 0, "every std + sample contract should be runnable");
}

#[test]
fn record_binder_contract_runs() {
    let src = "module R\npublic type Point = { x : Int, y : Int }\n\
               public swap : Point -> Point\nlet swap p = { x = p.y, y = p.x }\n\
               forall p: swap (swap p) = p\n";
    let outcome = run(&[("R.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn record_counterexample_is_labeled() {
    let src = "module R\npublic type Point = { x : Int, y : Int }\n\
               public total : Point -> Int\nlet total p = p.x + p.y\n\
               forall p: total p = p.x\n";
    let outcome = run(&[("R.fai", src)]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    assert_eq!(d.help.as_deref(), Some("counterexample: p = { x = 0, y = 1 }"));
}

/// A recursive ADT carrier (multi-line union form, matching the sample/fixture
/// style).
const TREE: &str =
    "module T\n\npublic type Tree 'a =\n  | Leaf\n  | Node (Tree 'a) 'a (Tree 'a)\n\n";

#[test]
fn recursive_adt_contract_runs() {
    let src = format!(
        "{TREE}public count : Tree Int -> Int\n\
         let count t =\n  match t with\n  | Leaf -> 0\n  | Node l x r -> 1 + count l + count r\n\
         forall t: count t >= 0\n"
    );
    let outcome = run(&[("T.fai", &src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn recursive_adt_counterexample_shrinks_to_minimal_node() {
    // A false property (every tree is a leaf) shrinks to the smallest Node.
    let src = format!(
        "{TREE}public isLeaf : Tree Int -> Bool\n\
         let isLeaf t =\n  match t with\n  | Leaf -> true\n  | Node l x r -> false\n\
         forall t: isLeaf t\n"
    );
    let outcome = run(&[("T.fai", &src)]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    assert_eq!(d.help.as_deref(), Some("counterexample: t = Node Leaf 0 Leaf"));
}

#[test]
fn passing_contract_run_is_clean() {
    let outcome = run(&[("Ok.fai", "module Ok\nforall xs: List.reverse (List.reverse xs) = xs\n")]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.leaked, 0);
}
