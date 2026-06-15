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
use fai_tests::algorithms::{ALGORITHMS, Oracle, by_module, expr_eval};
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

#[test]
fn dict_histogram_sample_is_valid() {
    validate("DictHistogram");
}

#[test]
fn word_count_sample_is_valid() {
    validate("WordCount");
}

#[test]
fn map_sum_shared_sample_is_valid() {
    validate("MapSumShared");
}

#[test]
fn set_dedup_sample_is_valid() {
    validate("SetDedup");
}

#[test]
fn fold_pipeline_sample_is_valid() {
    validate("FoldPipeline");
}

#[test]
fn interface_dispatch_sample_is_valid() {
    validate("InterfaceDispatch");
}

#[test]
fn particles_sample_is_valid() {
    validate("Particles");
}

#[test]
fn vec_mat_sample_is_valid() {
    validate("VecMat");
}

#[test]
fn nqueens_sample_is_valid() {
    validate("NQueens");
}

#[test]
fn matrix_multiply_sample_is_valid() {
    validate("MatrixMultiply");
}

#[test]
fn float_matrix_multiply_sample_is_valid() {
    validate("FloatMatrixMultiply");
}

#[test]
fn levenshtein_sample_is_valid() {
    validate("Levenshtein");
}

#[test]
fn game_of_life_sample_is_valid() {
    validate("GameOfLife");
}

#[test]
fn spectral_norm_sample_is_valid() {
    validate("SpectralNorm");
}

#[test]
fn mandelbrot_sample_is_valid() {
    validate("Mandelbrot");
}

#[test]
fn ackermann_sample_is_valid() {
    validate("Ackermann");
}

#[test]
fn prng_xorshift_sample_is_valid() {
    validate("PrngXorshift");
}

#[test]
fn expr_eval_sample_is_valid() {
    validate("ExprEval");
}

/// The `expr_eval` oracle parses a thousands-long token list into a deep `Expr`
/// tree; building, evaluating, or freeing either one with native recursion would
/// overflow a small stack (a Windows test process runs on a ~1 MiB main-thread
/// stack, where an earlier recursive version did overflow). Run it on a
/// deliberately tiny stack to guard against reintroducing that recursion — it must
/// keep its deep work on the heap.
#[test]
fn expr_eval_oracle_uses_no_deep_recursion() {
    let on_small_stack = std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(|| expr_eval(4_000))
        .expect("spawn worker")
        .join()
        .expect("expr_eval must not overflow a 128 KiB stack");
    assert_eq!(on_small_stack, expr_eval(4_000));
}

#[test]
fn graph_bfs_sample_is_valid() {
    validate("GraphBFS");
}

#[test]
fn coin_change_sample_is_valid() {
    validate("CoinChange");
}

#[test]
fn fib_memo_sample_is_valid() {
    validate("FibMemo");
}

#[test]
fn quicksort_sample_is_valid() {
    validate("QuickSort");
}

#[test]
fn sieve_sample_is_valid() {
    validate("Sieve");
}

#[test]
fn nbody_sample_is_valid() {
    validate("NBody");
}

#[test]
fn fannkuch_sample_is_valid() {
    validate("Fannkuch");
}

#[test]
fn union_find_sample_is_valid() {
    validate("UnionFind");
}

#[test]
fn json_serialize_sample_is_valid() {
    validate("JsonSerialize");
}

#[test]
fn string_build_sample_is_valid() {
    validate("StringBuild");
}

#[test]
fn string_slice_sample_is_valid() {
    validate("StringSlice");
}

#[test]
fn option_eval_sample_is_valid() {
    validate("OptionEval");
}

#[test]
fn int_eval_sample_is_valid() {
    validate("IntEval");
}

#[test]
fn option_path_sample_is_valid() {
    validate("OptionPath");
}

#[test]
fn option_tree_find_sample_is_valid() {
    validate("OptionTreeFind");
}

#[test]
fn list_sort_sample_is_valid() {
    validate("ListSort");
}

/// The four hand-maintained algorithm lists must not drift from the registry: the
/// two runtime benches (`algorithms_jit`/`algorithms_aot`) name each module in a
/// `algorithm_benches!` row, and this file declares a `validate` test per module.
/// `algorithms_mem` and `algo-baseline` iterate the registry directly, so they
/// need no guard. Reading the sources keeps a future registry addition from
/// silently skipping a bench or its validation.
#[test]
fn registry_is_fully_covered() {
    let jit = include_str!("../benches/algorithms_jit.rs");
    let aot = include_str!("../benches/algorithms_aot.rs");
    let here = include_str!("algorithms.rs");
    for algo in ALGORITHMS {
        let benched = format!("\"{}\"", algo.module);
        assert!(
            jit.contains(&benched),
            "{} is registered but not benched in algorithms_jit.rs",
            algo.module
        );
        assert!(
            aot.contains(&benched),
            "{} is registered but not benched in algorithms_aot.rs",
            algo.module
        );
        let validated = format!("validate(\"{}\")", algo.module);
        assert!(
            here.contains(&validated),
            "{} is registered but has no validate(\"{}\") test",
            algo.module,
            algo.module
        );
    }
}
