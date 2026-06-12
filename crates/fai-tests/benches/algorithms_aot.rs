//! Runtime comparison of the *delivered binaries*: a `fai build` native
//! executable vs a Rust release binary, end to end (process startup, linking, the
//! workload, and exit).
//!
//! Each Fai binary is built once with [`fai_driver::build_native`] in untimed
//! setup, then spawned in the timed loop; the Rust side spawns the
//! `algo-baseline` binary (built by Cargo at the bench profile's `-O3`). Both run
//! a single, large baked workload so process startup is amortized — the Fai
//! sample's `main` passes the size to `run`/`runF`, and the Rust binary takes the
//! matching size as an argument (kept in lockstep by the sample validation
//! tests). See the `algorithms_jit` bench for the in-process compute comparison
//! and the fairness caveats.
//!
//! Not run on Windows (the build/link + spawn path mirrors the daemon e2e benches
//! and would need the MSVC environment); the bench still compiles there, so
//! `build --all-targets` keeps it from bitrotting, and the Benchmarks workflow
//! runs on Linux. Run with `cargo bench -p fai-tests --bench algorithms_aot`.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use divan::Bencher;
use fai_db::{Db, FaiDatabase};
use fai_driver::build_native;
use fai_tests::algorithms::{Algorithm, by_module};

fn main() {
    // The build/link + spawn path is skipped on Windows (see the module docs); on
    // every other platform this runs the full divan suite.
    #[cfg(not(windows))]
    divan::main();
}

/// A unique temporary path for a built executable.
fn unique_exe(module: &str) -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8");
    dir.join(format!(
        "fai-algo-{module}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Links the algorithm's sample (with its baked workload size) into a native
/// executable, returning the path actually produced.
fn build_fai_binary(algo: &Algorithm) -> Utf8PathBuf {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(format!("{}.fai", algo.module).into(), algo.source().to_owned());
    let file = db.source_file(id).expect("sample source registered");
    let outcome = build_native(&db, file, &unique_exe(algo.module));
    outcome
        .artifact
        .unwrap_or_else(|| panic!("{} failed to build a native executable", algo.module))
}

/// Spawns `command` to completion, capturing its output (so nothing reaches the
/// bench's own stdout) and returning it for `black_box`ing.
fn spawn(command: &mut Command) -> std::process::Output {
    command.output().expect("spawn benchmark binary")
}

/// Times the delivered Fai binary running its baked workload.
fn bench_fai_binary(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let exe = build_fai_binary(algo);
    // Confirm it runs cleanly once (untimed); exit 0 also means leak-free.
    let first = spawn(&mut Command::new(&exe));
    assert!(first.status.success(), "{module} fai binary exited with {:?}", first.status);
    bencher.bench(|| divan::black_box(spawn(&mut Command::new(&exe))));
    let _ = std::fs::remove_file(&exe);
}

/// Times the Rust release binary running the same workload.
fn bench_rust_binary(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let baseline = env!("CARGO_BIN_EXE_algo-baseline");
    let size = algo.aot_size.to_string();
    bencher.bench(|| divan::black_box(spawn(Command::new(baseline).args([module, size.as_str()]))));
}

/// Declares a `mod <name> { rust; fai }` per algorithm, matching the
/// `algorithms_jit` layout so the summary pairs the two families' rows.
macro_rules! algorithm_benches {
    ($($name:ident => $module:literal),* $(,)?) => {
        $(
            mod $name {
                use divan::Bencher;

                #[divan::bench]
                fn rust(bencher: Bencher) {
                    super::bench_rust_binary(bencher, $module);
                }

                #[divan::bench]
                fn fai(bencher: Bencher) {
                    super::bench_fai_binary(bencher, $module);
                }
            }
        )*
    };
}

algorithm_benches! {
    fib => "Fib",
    collatz => "Collatz",
    map_sum => "MapSum",
    merge_sort => "MergeSort",
    binary_trees => "BinaryTrees",
    pi => "Pi",
    dict_histogram => "DictHistogram",
    word_count => "WordCount",
    map_sum_shared => "MapSumShared",
    set_dedup => "SetDedup",
    fold_pipeline => "FoldPipeline",
    interface_dispatch => "InterfaceDispatch",
    particles => "Particles",
    nqueens => "NQueens",
    matrix_multiply => "MatrixMultiply",
    levenshtein => "Levenshtein",
    game_of_life => "GameOfLife",
    spectral_norm => "SpectralNorm",
    mandelbrot => "Mandelbrot",
    ackermann => "Ackermann",
    prng_xorshift => "PrngXorshift",
    expr_eval => "ExprEval",
    graph_bfs => "GraphBFS",
    coin_change => "CoinChange",
    fib_memo => "FibMemo",
    quicksort => "QuickSort",
    sieve => "Sieve",
    nbody => "NBody",
    fannkuch => "Fannkuch",
    union_find => "UnionFind",
    json_serialize => "JsonSerialize",
    string_build => "StringBuild",
    string_slice => "StringSlice",
}
