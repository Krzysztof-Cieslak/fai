//! Validation for the runtime-benchmark sample algorithms under
//! `samples/algorithms/`: each must be canonically formatted, typecheck cleanly,
//! pass its contracts, and run its `main` to the value the shared Rust reference
//! ([`fai_tests::algorithms`]) computes.
//!
//! Running `main` exercises the large workload the AOT bench builds, and
//! comparing its output to the Rust oracle at the registered `aot_size` confirms
//! both the algorithm's correctness and that the size baked into the sample
//! matches the bench registry. The samples live in a subdirectory the top-level
//! `samples` tests skip, so this is their dedicated coverage.
//!
//! The runtime's console sink and live-object counter are process-global, so the
//! cases serialize on [`LOCK`]; under nextest each test is its own process, so
//! they still run in parallel.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_diagnostics::Severity;
use fai_driver::{TestConfig, jit_run_program, test};
use fai_runtime as rt;
use fai_span::SourceId;
use fai_syntax::parse_module;
use fai_tests::algorithms::{Oracle, by_module};
use fai_tests::check_source_diagnostics;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Canonical formatting of `src`.
fn fmt(src: &str) -> String {
    let parsed = parse_module(SourceId::new(0), src);
    fai_fmt::format(&parsed.module, &parsed.comments, src)
}

/// Validates one algorithm sample end to end.
#[track_caller]
fn validate(module: &str) {
    let _g = lock();
    let algo = by_module(module).expect("registered algorithm");
    let src = algo.source();

    // Canonical formatting: already-canonical and idempotent.
    let formatted = fmt(src);
    assert_eq!(formatted, src, "{module}.fai is not canonically formatted (run `fai fmt`)");
    assert_eq!(fmt(&formatted), formatted, "{module}.fai formatting is not idempotent");

    // The size baked into `main` must match the bench registry's `aot_size`, so
    // the subprocess AOT bench compares the same workload on both sides.
    let baked = format!("({} {})", algo.entry, algo.aot_size);
    assert!(src.contains(&baked), "{module}.fai main should call `{baked}` (size out of sync)");

    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(format!("{module}.fai").into(), src.to_owned());
    let file = db.source_file(id).expect("sample source registered");

    // Typechecks with no errors.
    let errors: Vec<_> = check_source_diagnostics(&db, file)
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{module}.fai has type errors: {errors:?}");

    // Contracts pass.
    let outcome = test(&db, &[file], None, TestConfig::default());
    assert!(
        outcome.ok,
        "{module}.fai contracts failed: {} passed of {} ({} could not run)",
        outcome.passed, outcome.total, outcome.not_run
    );

    // `main` runs the baked workload to the value the Rust oracle computes.
    rt::capture_start();
    let run = jit_run_program(&db, file);
    let output = rt::capture_take();
    assert_eq!(
        run.exit_code, 0,
        "{module} main exited with {} (0 also means leak-free)",
        run.exit_code
    );
    let printed = output.trim();
    match algo.oracle {
        Oracle::Int(f) => {
            assert_eq!(
                printed,
                f(algo.aot_size).to_string(),
                "{module} output disagrees with Rust"
            );
        }
        Oracle::Float(f) => {
            let got: f64 =
                printed.parse().unwrap_or_else(|_| panic!("{module} printed a float: {printed:?}"));
            let expected = f(algo.aot_size);
            let tolerance = 1e-6 * expected.abs().max(1.0);
            assert!(
                (got - expected).abs() < tolerance,
                "{module} output {got} differs from the Rust oracle {expected}"
            );
        }
    }
}

#[test]
fn fib_sample_is_valid() {
    validate("Fib");
}

#[test]
fn collatz_sample_is_valid() {
    validate("Collatz");
}

#[test]
fn map_sum_sample_is_valid() {
    validate("MapSum");
}

#[test]
fn merge_sort_sample_is_valid() {
    validate("MergeSort");
}

#[test]
fn binary_trees_sample_is_valid() {
    validate("BinaryTrees");
}

#[test]
fn pi_sample_is_valid() {
    validate("Pi");
}
