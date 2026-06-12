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

/// Runs one typecheck-clean sample's contracts on their own and asserts they all
/// pass cleanly.
#[track_caller]
fn sample_contracts_pass(name: &str) {
    let src = read_sample(name);
    let outcome = run(&[(name, src.as_str())]);
    assert!(
        outcome.ok,
        "{name} contracts should pass; diagnostics: {:?}",
        outcome.diagnostics.iter().map(|d| (d.code.as_str(), &d.message)).collect::<Vec<_>>()
    );
    assert!(outcome.total > 0, "{name}: expected contracts, got {}", outcome.total);
    assert_eq!(outcome.passed, outcome.total, "{name}: every contract passes");
    assert_eq!(outcome.not_run, 0, "{name}: no contracts skipped");
    assert_eq!(outcome.leaked, 0, "{name}: running contracts must not leak");
}

#[test]
fn math_contracts_pass() {
    sample_contracts_pass("Math.fai");
}

#[test]
fn lists_contracts_pass() {
    sample_contracts_pass("Lists.fai");
}

#[test]
fn tail_loops_contracts_pass() {
    sample_contracts_pass("TailLoops.fai");
}

#[test]
fn algebra_contracts_pass() {
    sample_contracts_pass("Algebra.fai");
}

#[test]
fn tuples_contracts_pass() {
    sample_contracts_pass("Tuples.fai");
}

#[test]
fn properties_contracts_pass() {
    sample_contracts_pass("Properties.fai");
}

#[test]
fn cart_contracts_pass() {
    sample_contracts_pass("Cart.fai");
}

#[test]
fn chars_contracts_pass() {
    sample_contracts_pass("Chars.fai");
}

#[test]
fn patterns_contracts_pass() {
    sample_contracts_pass("Patterns.fai");
}

#[test]
fn geometry_contracts_pass() {
    // Float `forall` laws (magnitude is non-negative) over the finite generator.
    sample_contracts_pass("Geometry.fai");
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
fn std_modules_contracts_pass() {
    // Every embedded standard-library module (not just `Prelude`) runs its own
    // `example`/`forall` contracts — so e.g. `String.substring`/`take`/`drop`'s
    // examples are executed, not merely required to exist.
    let _g = lock();
    let mut db = FaiDatabase::new();
    let std_ids = fai_types::std_lib::load_std(&mut db);
    let files: Vec<SourceFile> = std_ids.iter().filter_map(|&id| db.source_file(id)).collect();
    let outcome = test(&db, &files, None, TestConfig::default());
    assert!(
        outcome.ok,
        "std contracts should pass; diagnostics: {:?}",
        outcome.diagnostics.iter().map(|d| (d.code.as_str(), &d.message)).collect::<Vec<_>>()
    );
    assert!(outcome.total > 0);
    assert_eq!(outcome.passed, outcome.total, "every std contract passes");
    assert_eq!(outcome.leaked, 0, "running std contracts must not leak");
}

#[test]
fn store_app_contracts_pass() {
    // The real-world sample app (the language-server bench fixture) carries
    // `example`/`forall` contracts across its modules; run them together (the
    // modules reference each other) and assert they all hold.
    let _g = lock();
    let (db, files) = fai_corpus::realworld::load_app();
    let handles: Vec<SourceFile> = files.values().copied().collect();
    let outcome = test(&db, &handles, None, TestConfig::default());
    assert!(
        outcome.ok,
        "store app contracts should pass; diagnostics: {:?}",
        outcome.diagnostics.iter().map(|d| (d.code.as_str(), &d.message)).collect::<Vec<_>>()
    );
    assert!(outcome.total > 0, "expected contracts in the store app");
    assert_eq!(outcome.passed, outcome.total, "every store-app contract passes");
    assert_eq!(outcome.not_run, 0, "no store-app contract skipped");
    assert_eq!(outcome.leaked, 0, "running contracts must not leak");
}

#[test]
fn generated_corpus_contracts_pass() {
    // The synthetic corpus the `fai test` benchmarks edit and re-run: its
    // generated `example`/`forall` contracts must all hold (a green corpus, so the
    // benches measure a passing run). Fewer trials keeps the test quick.
    let _g = lock();
    let spec = fai_corpus::CorpusSpec::with_modules_and_contracts(3);
    let (db, files) = fai_corpus::build_db(&spec);
    let config = TestConfig { trials: 16, ..TestConfig::default() };
    let outcome = test(&db, &files, None, config);
    assert!(
        outcome.ok,
        "generated corpus contracts should pass; diagnostics: {:?}",
        outcome.diagnostics.iter().map(|d| (d.code.as_str(), &d.message)).collect::<Vec<_>>()
    );
    assert!(outcome.total > 0, "expected generated contracts");
    assert_eq!(outcome.passed, outcome.total, "every generated contract passes");
    assert_eq!(outcome.leaked, 0, "running contracts must not leak");
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

#[test]
fn impure_contract_blocks_the_test_run() {
    // A contract that references a capability is impure: `fai test` surfaces the
    // located purity diagnostic and runs nothing (a file with errors cannot be
    // compiled soundly), exactly like any other type error.
    let src = "module M\npublic greet : Console -> Unit\nlet greet c = c.writeLine \"hi\"\n\
               example: greet\n";
    let outcome = run(&[("M.fai", src)]);
    assert!(!outcome.ok);
    assert_eq!(outcome.passed, 0, "nothing runs when blocked");
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6004").expect("FAI6004");
    assert!(d.message.contains("must be pure"), "got: {}", d.message);
}

#[test]
fn cross_file_opaque_record_binder_is_generated() {
    // A `forall` in another file over an opaque record type: the synthesized
    // generator peeks past opacity to build values, so the contract runs (it does
    // not become a not-runnable binder).
    let lib = indoc::indoc! {r#"
        module Lib

        public opaque type Stats = { hits : Int, misses : Int }

        public swap : Stats -> Stats
        let swap s = { hits = s.misses, misses = s.hits }

        public total : Stats -> Int
        let total s = s.hits + s.misses
    "#};
    let use_mod = indoc::indoc! {r#"
        module Use

        forall s: Lib.total (Lib.swap s) = Lib.total s
    "#};
    let outcome = run(&[("Lib.fai", lib), ("Use.fai", use_mod)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, outcome.total);
    assert_eq!(outcome.not_run, 0, "an opaque record binder must be generatable");
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn char_binder_contract_runs() {
    // A `forall` over a `Char` binder is generatable (the inverse of the
    // function-typed binder above): it runs and passes, with nothing skipped.
    let src = "module C\npublic dup : Char -> (Char * Char)\nlet dup c = (c, c)\n\
               forall c: dup c = (c, c)\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn char_and_int_binders_run() {
    // Two binders compose via tuple2 (Char, Int).
    let src = "module C\npublic f : Char -> Int -> Int\nlet f c n = Char.toCode c + n\n\
               forall c n: f c n = n + Char.toCode c\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
}

#[test]
fn list_of_char_binder_runs() {
    let src = "module C\npublic len : List Char -> Int\nlet len cs = List.length cs\n\
               forall cs: len cs >= 0\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn option_char_binder_runs() {
    let src = "module C\npublic f : Option Char -> Bool\nlet f o = Option.isSome o || Option.isNone o\n\
               forall o: f o\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
}

#[test]
fn record_with_char_field_binder_runs() {
    // A synthesized record generator with a Char field.
    let src = "module C\npublic type Tagged = { c : Char, n : Int }\n\
               public flip : Tagged -> Tagged\nlet flip t = { c = t.c, n = 0 - t.n }\n\
               forall t: flip (flip t) = t\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn adt_with_char_field_binder_runs() {
    // A synthesized ADT generator carrying a Char.
    let src = "module C\npublic type Keyed =\n  | Empty\n  | One Char\n\
               public same : Keyed -> Bool\nlet same k = k = k\n\
               forall k: same k\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
}

#[test]
fn char_counterexample_is_rendered_as_a_char_literal() {
    // A property that only holds for one char fails; the shrunk counterexample is
    // rendered by the Char generator's `show` as a valid char literal.
    let src = "module C\npublic isA : Char -> Bool\nlet isA c = c = 'a'\nforall c: isA c\n";
    let outcome = run(&[("C.fai", src)]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    let help = d.help.as_deref().unwrap_or("");
    assert!(help.starts_with("counterexample: c = '"), "got: {help}");
    assert!(help.ends_with('\''), "got: {help}");
}

#[test]
fn float_law_holds_without_nan_failures() {
    // A square is non-negative for every *finite* float, but `NaN >= 0.0` is
    // false. The default generator is finite, so the law passes — a regression
    // guard against the earlier full-domain (NaN-emitting) generator. (`>=` is the
    // IEEE comparison, so signed zero is not an issue, unlike bitwise `=`.)
    let src = "module M\nforall x: x * x >= 0.0\n";
    let outcome = run(&[("M.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn false_float_law_shrinks_to_zero() {
    // `x * 0.0 = 1.0` is false for every x, so a counterexample is found on the
    // first trial and shrinks all the way to the simplest float, 0.0.
    let src = "module M\nforall x: x * 0.0 = 1.0\n";
    let outcome = run(&[("M.fai", src)]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    assert_eq!(d.help.as_deref(), Some("counterexample: x = 0.0"));
}

#[test]
fn float_counterexample_shrinks_to_whole_number() {
    // `x < 1.0` fails for any x >= 1.0; the shrinker (toward 0.0, then whole-number
    // truncation, then halving) converges on the simplest failing value, 1.0.
    let src = "module M\nforall x: x < 1.0\n";
    let outcome = run(&[("M.fai", src)]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    assert_eq!(d.help.as_deref(), Some("counterexample: x = 1.0"));
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
fn array_binder_contract_runs() {
    // A `forall` over `Array Int` generates arrays via the synthesized `array`
    // combinator and runs cleanly.
    let src = "module A\npublic rev2 : Array Int -> Array Int\n\
               let rev2 xs = Array.reverse (Array.reverse xs)\nforall xs: rev2 xs = xs\n";
    let outcome = run(&[("A.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

#[test]
fn recursive_array_field_type_generates() {
    // A type that recurses through an `Array` field generates via the synthesized
    // `recArray` (budget-splitting), with the empty array as the base case.
    let src = "module R\npublic type Rose =\n  | Rose Int (Array Rose)\n\
               public count : Rose -> Int\n\
               let count r =\n  match r with\n  | Rose _ kids -> 1 + Array.sum (Array.map count kids)\n\
               forall r: count r >= 1\n";
    let outcome = run(&[("R.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
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

// --- Fuel-guarded recursion (collection-wrapped and mutual) ------------------

/// A rose tree recurses only through a `List` field, which the old size-only
/// guard did not shrink — so generation would explode. The fuel split bounds the
/// total size, so it generates, runs, and passes (rather than timing out).
#[test]
fn rose_tree_collection_recursion_runs() {
    let src = "module R\n\
               public type Rose =\n  | Node Int (List Rose)\n\
               public count : Rose -> Int\n\
               let count r =\n  match r with\n  | Node n kids -> 1 + sumCounts kids\n\
               let sumCounts kids =\n  match kids with\n  | [] -> 0\n\
               \u{20}\u{20}| k :: rest -> count k + sumCounts rest\n\
               forall r: count r >= 1\n";
    let outcome = run(&[("R.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

/// A false property over a rose tree still shrinks to a small counterexample.
#[test]
fn rose_tree_counterexample_shrinks() {
    let src = "module R\n\
               public type Rose =\n  | Node Int (List Rose)\n\
               public count : Rose -> Int\n\
               let count r =\n  match r with\n  | Node n kids -> 1 + sumCounts kids\n\
               let sumCounts kids =\n  match kids with\n  | [] -> 0\n\
               \u{20}\u{20}| k :: rest -> count k + sumCounts rest\n\
               forall r: count r = 1\n";
    let outcome = run(&[("R.fai", src)]);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
    let help = d.help.as_deref().unwrap_or("");
    assert!(help.starts_with("counterexample: r = Node "), "got: {help}");
}

/// Mutually-recursive types (the recursion crosses `Tree`↔`Forest`, never a
/// direct self-field) generate and run without diverging.
#[test]
fn mutually_recursive_adts_run() {
    let src = "module M\n\
               public type Tree =\n  | Leaf Int\n  | Branch Forest\n\
               public type Forest =\n  | Empty\n  | Cons Tree Forest\n\
               public treeSize : Tree -> Int\n\
               let treeSize t =\n  match t with\n  | Leaf n -> 1\n  | Branch f -> 1 + forestSize f\n\
               let forestSize f =\n  match f with\n  | Empty -> 0\n\
               \u{20}\u{20}| Cons t rest -> treeSize t + forestSize rest\n\
               forall t: treeSize t >= 1\n";
    let outcome = run(&[("M.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
    assert_eq!(outcome.leaked, 0);
}

/// Recursion reachable only through an `Option` field bottoms out (the wrapper
/// grounds to `None` at the budget floor).
#[test]
fn option_wrapped_recursion_runs() {
    let src = "module O\n\
               public type Chain =\n  | End\n  | Link (Option Chain)\n\
               public len : Chain -> Int\n\
               let len c =\n  match c with\n  | End -> 0\n\
               \u{20}\u{20}| Link o ->\n    match o with\n    | None -> 1\n    | Some c2 -> 1 + len c2\n\
               forall c: len c >= 0\n";
    let outcome = run(&[("O.fai", src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
}

/// A type with no finite value (every constructor recurses, no base case) cannot
/// be generated: a clean `FAI6005`, not a hang or panic.
#[test]
fn non_groundable_type_is_reported() {
    let src = "module S\n\
               public type Stream =\n  | More Int Stream\n\
               public head : Stream -> Int\n\
               let head s =\n  match s with\n  | More n rest -> n\n\
               forall s: head s = head s\n";
    let outcome = run(&[("S.fai", src)]);
    assert!(!outcome.ok);
    assert_eq!(outcome.not_run, 1);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6005").expect("FAI6005");
    assert!(d.message.contains("no finite value"), "got: {}", d.message);
}

// --- Custom `Arbitrary` overrides --------------------------------------------

/// The invariant generator: every `Even` carries an even number.
const EVEN: &str = "module C\n\
                    public type Even =\n  | Even Int\n\
                    public value : Even -> Int\n\
                    let value e =\n  match e with\n  | Even n -> n\n\
                    public arbEven : Test.Arbitrary Even\n\
                    let arbEven =\n\
                    \u{20}\u{20}let g size seed =\n    let (n, s2) = Test.int.gen size seed\n\
                    \u{20}\u{20}\u{20}\u{20}(Even (n + n), s2)\n\
                    \u{20}\u{20}{ gen = g, show = fun e -> Int.toString (value e), shrink = fun e -> [] }\n";

/// A user-supplied `Arbitrary Even` overrides synthesis: the property holds only
/// because every generated `Even` is even (the synthesized generator would draw
/// odd values and fail).
#[test]
fn custom_generator_overrides_synthesis() {
    let src = format!("{EVEN}forall e: value e % 2 = 0\n");
    let outcome = run(&[("C.fai", &src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
    assert_eq!(outcome.not_run, 0);
}

/// The override also applies where the type appears nested (here, as the element
/// of a `List Even` binder).
#[test]
fn custom_generator_applies_to_nested_use() {
    let src = format!(
        "{EVEN}public allEven : List Even -> Bool\n\
         let allEven es =\n  match es with\n  | [] -> true\n\
         \u{20}\u{20}| e :: rest -> value e % 2 = 0 && allEven rest\n\
         forall es: allEven es\n"
    );
    let outcome = run(&[("C.fai", &src)]);
    assert!(outcome.ok, "diagnostics: {:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
}

/// Two `Arbitrary Even` definitions make the override ambiguous: `FAI6006`.
#[test]
fn ambiguous_custom_generator_is_reported() {
    let src = format!(
        "{EVEN}public arbEven2 : Test.Arbitrary Even\n\
         let arbEven2 = arbEven\n\
         forall e: value e % 2 = 0\n"
    );
    let outcome = run(&[("C.fai", &src)]);
    assert!(!outcome.ok);
    assert_eq!(outcome.not_run, 1);
    let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6006").expect("FAI6006");
    assert!(d.message.contains("more than one"), "got: {}", d.message);
}

/// Overrides apply to user records/ADTs only — a user `Arbitrary Int` does not
/// replace the built-in `Int` generator, so a property that holds only for one
/// fixed value still fails.
#[test]
fn custom_generator_does_not_override_builtins() {
    let src = "module C\n\
               public arbInt : Test.Arbitrary Int\n\
               let arbInt =\n\
               \u{20}\u{20}{ gen = fun size seed -> (42, seed), show = Int.toString, shrink = fun n -> [] }\n\
               forall n: n = 42\n";
    let outcome = run(&[("C.fai", src)]);
    assert!(!outcome.ok, "built-in Int must not be overridden");
    assert!(outcome.diagnostics.iter().any(|d| d.code.as_str() == "FAI6001"));
}

// --- `fai check`'s eager closed-`example` evaluation (in-process) -------------

/// Evaluates the closed `example` contracts in `files` the way `fai check` does,
/// but in this process (no worker isolation — the examples here never trap).
fn check_example_failures(files: &[(&str, &str)]) -> Vec<fai_diagnostics::Diagnostic> {
    let _g = lock();
    let (db, handles) = db_with(files);
    fai_driver::check_examples_in_process(&db, &handles)
}

#[test]
fn check_reports_a_wrong_example_at_its_span() {
    let src = "module Bad\nexample: 1 = 2\n";
    let diags = check_example_failures(&[("Bad.fai", src)]);
    assert_eq!(diags.len(), 1, "one failing example: {diags:?}");
    assert_eq!(diags[0].code.as_str(), "FAI6001");
    assert!(diags[0].message.contains("example does not hold"), "got: {}", diags[0].message);
    // Located at the `example:` declaration, not the whole file.
    let start = diags[0].primary.start().raw() as usize;
    assert_eq!(start, src.find("example").expect("the example keyword"));
}

#[test]
fn check_passes_a_correct_example() {
    let diags = check_example_failures(&[("Ok.fai", "module Ok\nexample: 1 + 1 = 2\n")]);
    assert!(diags.is_empty(), "a true example yields no diagnostic: {diags:?}");
}

#[test]
fn check_evaluates_an_example_over_callees() {
    // A correct example calling into the embedded standard library passes; the
    // wrong one is reported. Exercises the reachable-callee gathering.
    let ok = "module M\nexample: List.map (fun x -> x + 1) [1, 2, 3] = [2, 3, 4]\n";
    assert!(check_example_failures(&[("M.fai", ok)]).is_empty());
    let bad = "module M\nexample: List.map (fun x -> x + 1) [1, 2, 3] = [1, 2, 3]\n";
    let diags = check_example_failures(&[("M.fai", bad)]);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.as_str(), "FAI6001");
}

#[test]
fn check_ignores_forall_contracts() {
    // `forall`s need generated inputs; `fai check` leaves them to `fai test`,
    // even a plainly false one.
    let diags = check_example_failures(&[("F.fai", "module F\nforall n: n = n + 1\n")]);
    assert!(diags.is_empty(), "forall is not evaluated by check: {diags:?}");
}

#[test]
fn check_skips_a_file_with_a_type_error() {
    // The file has a wrong example *and* an unrelated type error: the example is
    // not run (a broken module cannot be compiled soundly), so check reports no
    // FAI6001 — the type error is surfaced by the type-check itself.
    let src = "module M\nexample: 1 = 2\npublic bad : Int\nlet bad = true\n";
    let diags = check_example_failures(&[("M.fai", src)]);
    assert!(diags.is_empty(), "examples are skipped when the file does not type-check: {diags:?}");
}

#[test]
fn check_ignores_an_example_free_file() {
    let diags = check_example_failures(&[("M.fai", "module M\n\nlet x = 1\n")]);
    assert!(diags.is_empty());
}

#[test]
fn check_example_plan_is_empty_without_a_closed_example() {
    // The fast path: a file with no `example` (here, only a `forall`) builds an
    // empty plan, so `fai check` spawns no worker and pays nothing extra.
    let _g = lock();
    let (db, handles) = db_with(&[("M.fai", "module M\n\nlet x = 1\nforall n: n = n\n")]);
    let plan = fai_driver::build_example_plan(&db, &handles, fai_driver::TestConfig::default());
    assert!(plan.bundle.contracts.is_empty(), "no closed example ⇒ empty plan, no worker");
}
