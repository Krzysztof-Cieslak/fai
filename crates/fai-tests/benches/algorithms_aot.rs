//! Runtime comparison of the *delivered binaries*: a `fai build` native
//! executable vs a Rust release binary, end to end (process startup, linking, the
//! workload, and exit).
//!
//! Each Fai binary is built once with [`fai_driver::build_native`] in untimed
//! setup, then spawned in the timed loop; the Rust side spawns the
//! `algo-baseline` binary (built by Cargo at the bench profile's `-O3`). Both run
//! a single, large workload with the size supplied through argv on every side.
//! The Fai wrapper replaces only the sample's entry point. See the warm AOT
//! comparison for repeated execution without process startup.
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
use fai_tests::benchmark_aot::{Entry, fai_program};
use fai_tests::benchmark_process::{ExpectedAnswer, spawn_checked};

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

/// Links the workload with an argv-reading native entry point.
fn build_fai_binary(algo: &Algorithm) -> Utf8PathBuf {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(format!("{}.fai", algo.module).into(), fai_program(algo, Entry::Once));
    let file = db.source_file(id).expect("sample source registered");
    let outcome = build_native(&db, file, &unique_exe(algo.module));
    outcome
        .artifact
        .unwrap_or_else(|| panic!("{} failed to build a native executable", algo.module))
}

/// Spawns `command` to completion, capturing its output (so nothing reaches the
/// bench's own stdout) and returning it for `black_box`ing.
fn spawn(command: &mut Command) -> std::process::Output {
    spawn_checked(command).unwrap_or_else(|error| panic!("{error}"))
}

/// Times the delivered Fai binary running its runtime-supplied workload.
fn bench_fai_binary(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let original = build_fai_binary(algo);
    let exe = fai_tests::native_peer::retain_once(
        algo,
        "fai",
        original.as_std_path(),
        serde_json::json!({"compiler": fai_driver::build_info()}),
    );
    if exe != original {
        let _ = std::fs::remove_file(&original);
    }
    let size = algo.aot_size.to_string();
    // Validate the exact delivered binary and workload, with oracle computation
    // outside timing. Every later timed process still has its exit checked.
    let first = spawn(Command::new(&exe).arg(&size));
    verify(algo, "fai", &first);
    bencher.bench(|| divan::black_box(spawn(Command::new(&exe).arg(&size))));
    if std::env::var_os("FAI_BENCH_ARTIFACT_DIR").is_none() {
        let _ = std::fs::remove_file(&exe);
    }
}

/// Times the Rust release binary running the same workload.
fn bench_rust_binary(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let baseline = fai_tests::native_peer::rust(algo, Entry::Once);
    let details = serde_json::from_slice(
        &std::fs::read(baseline.with_file_name("compiler-info.json")).unwrap(),
    )
    .unwrap();
    let baseline =
        fai_tests::native_peer::retain_once(algo, "rust", baseline.as_std_path(), details);
    let size = algo.aot_size.to_string();
    let first = spawn(Command::new(&baseline).arg(&size));
    verify(algo, "rust", &first);
    bencher.bench(|| divan::black_box(spawn(Command::new(&baseline).arg(&size))));
}

/// Times the OCaml release binary running the same workload, or skips when the
/// `ocamlopt` toolchain is unavailable. When OCaml is absent the function returns
/// without benching, so divan renders an empty leaf (the summary skips it) and the
/// run still works without OCaml installed; the Benchmarks workflow installs it.
fn bench_ocaml_binary(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let Some(exe) = fai_tests::ocaml::isolated(algo, Entry::Once) else {
        return;
    };
    let details = serde_json::json!({"compilerInfo": std::fs::read_to_string(exe.with_file_name("compiler-info.txt")).unwrap()});
    let exe = fai_tests::native_peer::retain_once(algo, "ocaml", exe.as_std_path(), details);
    let size = algo.aot_size.to_string();
    // Confirm it runs cleanly and agrees with the Rust oracle once (untimed), so a
    // wrong OCaml implementation fails the bench rather than reporting a
    // meaningless timing.
    let first = spawn(Command::new(&exe).arg(&size));
    verify(algo, "ocaml", &first);
    bencher.bench(|| divan::black_box(spawn(Command::new(&exe).arg(&size))));
}

/// Uses the same answer check for every language and the memory suite.
fn verify(algo: &Algorithm, language: &str, output: &std::process::Output) {
    ExpectedAnswer::for_algorithm(algo)
        .verify(&format!("{} {language}", algo.module), output)
        .unwrap_or_else(|error| panic!("{error}"));
}

/// Declares a `mod <name> { rust; fai; ocaml }` per algorithm, so the summary
/// pairs the Fai row against its Rust and OCaml baselines.
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

                #[divan::bench]
                fn ocaml(bencher: Bencher) {
                    super::bench_ocaml_binary(bencher, $module);
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
    vec_mat => "VecMat",
    nqueens => "NQueens",
    matrix_multiply => "MatrixMultiply",
    float_matrix_multiply => "FloatMatrixMultiply",
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
    option_eval => "OptionEval",
    int_eval => "IntEval",
    option_path => "OptionPath",
    option_tree_find => "OptionTreeFind",
    list_sort => "ListSort",
}
