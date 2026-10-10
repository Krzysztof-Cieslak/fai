//! Cross-language native benchmark entries consume runtime inputs and checked batches.

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use fai_db::Db;
use fai_tests::algorithms::{Algorithm, Oracle, by_module};
use fai_tests::benchmark_aot::{self as aot, Entry};
use fai_tests::benchmark_process::ExpectedAnswer;
use wait_timeout::ChildExt;

struct Native {
    _directory: tempfile::TempDir,
    path: camino::Utf8PathBuf,
}

fn build(algorithm: &Algorithm, entry: Entry) -> Native {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db
        .add_source(format!("{}.fai", algorithm.module).into(), aot::fai_program(algorithm, entry));
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("program")).unwrap();
    let result = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
    assert!(result.ok, "{}: {:?}", algorithm.module, result.diagnostics);
    Native { _directory: directory, path: result.artifact.unwrap() }
}

#[track_caller]
fn exchange(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let completed = child.wait_timeout(Duration::from_secs(30)).unwrap().is_some();
    if !completed {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(completed, "worker timed out: {}", String::from_utf8_lossy(&output.stderr));
    output
}

#[track_caller]
fn verify(output: &Output, expected: &[ExpectedAnswer]) {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("ready"));
    for answer in expected {
        answer.verify_line("native worker", lines.next().expect("response")).unwrap();
    }
    assert_eq!(lines.next(), None, "extra worker output");
}

#[track_caller]
fn check_worker(module: &str) {
    let algorithm = by_module(module).unwrap();
    let expected = [ExpectedAnswer::at_size(algorithm, 2), ExpectedAnswer::at_size(algorithm, 3)];
    let floor = match algorithm.oracle {
        Oracle::Int(_) => [ExpectedAnswer::Int(2), ExpectedAnswer::Int(3)],
        Oracle::Float(_) => [ExpectedAnswer::Float(2.0), ExpectedAnswer::Float(3.0)],
    };
    let replies = [
        expected[0],
        expected[1],
        aot::checksum(&expected, 5),
        aot::checksum(&floor, 5),
        aot::checksum(&expected, 0),
    ];
    let input = "2 3\nvalue 0\nvalue 1\nrun 5\nfloor 5\nrun 0\n";
    let program = build(algorithm, Entry::Worker);
    verify(&exchange(Command::new(program.path), input), &replies);
    let mut rust = Command::new(env!("CARGO_BIN_EXE_algo-worker"));
    rust.arg(module);
    verify(&exchange(rust, input), &replies);
    if let Some(ocaml) = fai_tests::ocaml::worker_baseline(algorithm) {
        verify(&exchange(Command::new(ocaml), input), &replies);
    }
}

macro_rules! workers {
    ($($test:ident => $module:literal),* $(,)?) => {
        $(#[test] fn $test() { check_worker($module); })*
    };
}

workers! {
    fib => "Fib", collatz => "Collatz", map_sum => "MapSum", merge_sort => "MergeSort",
    binary_trees => "BinaryTrees", pi => "Pi", dict_histogram => "DictHistogram",
    word_count => "WordCount", map_sum_shared => "MapSumShared", set_dedup => "SetDedup",
    fold_pipeline => "FoldPipeline", interface_dispatch => "InterfaceDispatch",
    particles => "Particles", vec_mat => "VecMat", nqueens => "NQueens",
    matrix_multiply => "MatrixMultiply", float_matrix_multiply => "FloatMatrixMultiply",
    levenshtein => "Levenshtein", game_of_life => "GameOfLife", spectral_norm => "SpectralNorm",
    mandelbrot => "Mandelbrot", ackermann => "Ackermann", prng_xorshift => "PrngXorshift",
    expr_eval => "ExprEval", graph_bfs => "GraphBFS", coin_change => "CoinChange",
    fib_memo => "FibMemo", quicksort => "QuickSort", sieve => "Sieve", nbody => "NBody",
    fannkuch => "Fannkuch", union_find => "UnionFind", json_serialize => "JsonSerialize",
    string_build => "StringBuild", string_slice => "StringSlice", option_eval => "OptionEval",
    int_eval => "IntEval", option_path => "OptionPath", option_tree_find => "OptionTreeFind",
    list_sort => "ListSort",
}

#[test]
fn one_native_binary_observes_changed_argv_sizes() {
    let algorithm = by_module("MapSum").unwrap();
    let program = build(algorithm, Entry::Once);
    let first = Command::new(&program.path).arg("4").output().unwrap();
    ExpectedAnswer::Int(12).verify("first size", &first).unwrap();
    let second = Command::new(&program.path).arg("9").output().unwrap();
    ExpectedAnswer::Int(72).verify("changed size", &second).unwrap();
}

#[test]
fn full_width_results_survive_raw_and_batched_protocols() {
    let algorithm = by_module("FibMemo").unwrap();
    let value = ExpectedAnswer::at_size(algorithm, 93);
    let expected = [value, aot::checksum(&[value], 3)];
    let input = "93\nvalue 0\nrun 3\n";
    let program = build(algorithm, Entry::Worker);
    verify(&exchange(Command::new(program.path), input), &expected);
    let mut rust = Command::new(env!("CARGO_BIN_EXE_algo-worker"));
    rust.arg("FibMemo");
    verify(&exchange(rust, input), &expected);
    if let Some(ocaml) = fai_tests::ocaml::worker_baseline(algorithm) {
        verify(&exchange(Command::new(ocaml), input), &expected);
    }
}

#[test]
fn malformed_worker_inputs_produce_an_error_and_terminate() {
    let algorithm = by_module("MapSum").unwrap();
    let program = build(algorithm, Entry::Worker);
    let output = exchange(Command::new(program.path), "1 bad 2\n");
    assert_eq!(output.stdout, b"error: inputs\n");
    if let Some(ocaml) = fai_tests::ocaml::worker_baseline(algorithm) {
        assert_eq!(exchange(Command::new(ocaml), "1 bad 2\n").stdout, b"error: inputs\n");
    }
}

#[test]
fn negative_worker_batch_counts_are_rejected() {
    let algorithm = by_module("MapSum").unwrap();
    let program = build(algorithm, Entry::Worker);
    let output = exchange(Command::new(program.path), "2\nrun -1\n");
    assert_eq!(output.stdout, b"ready\nerror: request\n");
    if let Some(ocaml) = fai_tests::ocaml::worker_baseline(algorithm) {
        assert_eq!(exchange(Command::new(ocaml), "2\nrun -1\n").stdout, b"ready\nerror: request\n");
    }
}
