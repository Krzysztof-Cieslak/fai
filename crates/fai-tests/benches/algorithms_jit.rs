//! Runtime comparison: well-known algorithms executed in process, Fai vs Rust.
//!
//! For each algorithm this times the *execution* of the compiled code, not its
//! compilation: the Fai side compiles the sample's reachable closure once (via
//! [`fai_driver::jit_compile`], in untimed setup) and then applies the benched
//! function in the timed loop; the Rust side calls the idiomatic reference
//! ([`fai_tests::algorithms`]). The companion `algorithms_aot` bench compares the
//! delivered binaries (`fai build` vs a Rust release binary) end to end.
//!
//! Read the ratios as a progress metric, not a fair fight: Fai runs with a
//! uniform **boxed** value representation and **reference counting**, generated
//! by Cranelift at optimization level "speed" (with the host's native CPU
//! features, since this is the JIT), while Rust is unboxed and optimized by LLVM
//! at the bench profile's `-O3`. Representation gaps are intentional and
//! idiomatic — e.g. `MapSum`'s Rust iterator allocates nothing where Fai builds a
//! `List`, and `MergeSort` sorts a `Vec` where Fai sorts a linked `List`. The
//! number to watch is whether the gap shrinks as the backend improves.
//!
//! Run with `cargo bench -p fai-tests --bench algorithms_jit`.

use divan::Bencher;
use fai_db::{Db, FaiDatabase};
use fai_driver::{CompiledProgram, jit_compile};
use fai_runtime as rt;
use fai_syntax::Symbol;
use fai_tests::algorithms::{Algorithm, Oracle, by_module};

fn main() {
    // The benched functions return values rather than printing, but compiling the
    // sample's `main` brings the console capability in; capture guards any stray
    // output during setup.
    rt::capture_start();
    divan::main();
}

/// A float result is compared to the Rust oracle within this absolute tolerance,
/// loose enough to tolerate Cranelift-vs-LLVM rounding (e.g. fused multiply-add)
/// yet far tighter than any change a wrong workload size would produce.
const FLOAT_TOLERANCE: f64 = 1e-6;

/// Builds a database with the standard library and the algorithm's sample, then
/// compiles the closure reachable from its `main` into a retained JIT image.
fn compile(algo: &Algorithm) -> (FaiDatabase, CompiledProgram) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(format!("{}.fai", algo.module).into(), algo.source().to_owned());
    let file = db.source_file(id).expect("sample source registered");
    let program = jit_compile(&db, file).unwrap_or_else(|diags| {
        panic!("{} failed to compile ({} diagnostics)", algo.module, diags.len())
    });
    (db, program)
}

/// Asserts the compiled Fai function agrees with the Rust oracle at `size`, so a
/// miscompiled benchmark cannot quietly report meaningless timings.
fn verify(algo: &Algorithm, closure: i64) {
    let got = rt::apply(rt::fai_dup(closure), &[rt::make_int(algo.jit_size)]);
    match algo.oracle {
        Oracle::Int(f) => {
            let value = rt::read_int(got);
            assert_eq!(value, f(algo.jit_size), "{} disagrees with the Rust oracle", algo.module);
        }
        Oracle::Float(f) => {
            let value = rt::read_float(got);
            assert!(
                (value - f(algo.jit_size)).abs() < FLOAT_TOLERANCE,
                "{} disagrees with the Rust oracle: {value} vs {}",
                algo.module,
                f(algo.jit_size)
            );
        }
    }
    rt::fai_drop(got);
}

/// Times the idiomatic Rust reference at the algorithm's JIT size.
fn bench_rust(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let n = algo.jit_size;
    match algo.oracle {
        Oracle::Int(f) => bencher.bench(|| divan::black_box(f(divan::black_box(n)))),
        // Bench requires one output type, so map the float result to its bits.
        Oracle::Float(f) => bencher.bench(|| divan::black_box(f(divan::black_box(n)).to_bits())),
    }
}

/// Times applying the compiled Fai function at the algorithm's JIT size. The
/// image is built once (untimed); each iteration dups the immortal static closure
/// (so the repeated application stays reference-count balanced), applies it, and
/// drops the result.
fn bench_fai(bencher: Bencher, module: &str) {
    let algo = by_module(module).expect("registered algorithm");
    let (_db, mut program) = compile(algo);
    let closure = program.function(Symbol::intern(algo.entry)).expect("entry binding compiled");
    verify(algo, closure);
    let n = algo.jit_size;
    bencher.bench(|| {
        let result = rt::apply(rt::fai_dup(closure), &[rt::make_int(divan::black_box(n))]);
        divan::black_box(result);
        rt::fai_drop(result);
    });
}

/// Declares a `mod <name> { rust; fai }` for each algorithm so divan renders the
/// rows as `<name> / rust` and `<name> / fai`, ready for the summary's pairing.
macro_rules! algorithm_benches {
    ($($name:ident => $module:literal),* $(,)?) => {
        $(
            mod $name {
                use divan::Bencher;

                #[divan::bench]
                fn rust(bencher: Bencher) {
                    super::bench_rust(bencher, $module);
                }

                #[divan::bench]
                fn fai(bencher: Bencher) {
                    super::bench_fai(bencher, $module);
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
