//! Matched storage fixtures and synchronized process-RSS observation boundaries.

#![cfg(target_os = "linux")]

use std::process::Command;

use fai_db::Db;
use fai_tests::benchmark_aot::{self, Entry};
use fai_tests::benchmark_process::{ExpectedAnswer, Worker};
use fai_tests::{algorithms, footprint::Rss, memory_workloads, native_peer, ocaml};

#[track_caller]
fn exchange(command: Command, expected: ExpectedAnswer) {
    let mut worker = Worker::start(command, "prepare").unwrap();
    let before = Rss::observe(worker.id()).unwrap();
    expected.verify_line("footprint result", &worker.request("run").unwrap()).unwrap();
    let after = Rss::observe(worker.id()).unwrap();
    assert!(after.peak_kib >= before.current_kib);
    worker.finish().unwrap();
}

#[track_caller]
fn with_peers(module: &str, mut test: impl FnMut(&str)) {
    let algorithm =
        memory_workloads::by_module(module).or_else(|| algorithms::by_module(module)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = benchmark_aot::fai_program(algorithm, Entry::Footprint);
    let id = db.add_source(format!("{module}.fai").into(), source);
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("fai")).unwrap();
    let built = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
    assert!(built.ok, "{module}: {:?}", built.diagnostics);
    test(path.as_str());
    let rust = native_peer::rust(algorithm, Entry::Footprint);
    test(rust.as_str());
    if let Some(ocaml) = ocaml::isolated(algorithm, Entry::Footprint) {
        test(ocaml.as_str());
    }
}

#[track_caller]
fn check(module: &str, size: i64, count: usize, floor: bool, value: ExpectedAnswer) {
    let expected = benchmark_aot::checksum(&[value], count);
    with_peers(module, |path| {
        let mut command = Command::new(path);
        command.args([
            size.to_string(),
            count.to_string(),
            if floor { "floor" } else { "run" }.into(),
        ]);
        exchange(command, expected);
    });
}

macro_rules! storage {
    ($($name:ident, $empty:ident => $module:literal, $size:literal, $kind:ident($expected:expr), $zero:expr);* $(;)?) => {
        $(
            #[test]
            fn $name() { check($module, $size, 2, false, ExpectedAnswer::$kind($expected)); }
            #[test]
            fn $empty() { check($module, 0, 2, false, ExpectedAnswer::$kind($zero)); }
        )*
    };
}

storage! {
    integer_array, empty_integer_array => "MemoryArrayInt", 16, Int(136), 0;
    float_array, empty_float_array => "MemoryArrayFloat", 16, Float(140.0), 0.0;
    shared_arrays, empty_shared_arrays => "MemorySharedArrays", 16, Int(288), 0;
    linked_list, empty_linked_list => "MemoryList", 16, Int(136), 0;
    binary_tree, empty_binary_tree => "MemoryTree", 4, Int(41), 0;
    utf8_buffer, empty_utf8_buffer => "MemoryUtf8", 16, Int(64), 0;
    distinct_histogram_keys, empty_histogram => "MemoryHistogram", 16, Int(24), 0;
    distinct_set_keys, empty_set => "MemorySet", 16, Int(18), 0;
    materialized_graph, empty_materialized_graph => "MemoryGraph", 16, Int(16), 0;
    float_bodies, empty_float_bodies => "MemoryBodies", 16, Float(168.0), 0.0;
}

#[test]
fn process_floor_does_not_execute_an_infeasible_workload() {
    check("Fib", 1000000, 3, true, ExpectedAnswer::Int(1000000));
}

#[test]
fn zero_repetitions_do_not_execute_an_infeasible_workload() {
    check("Fib", 1000000, 0, false, ExpectedAnswer::Int(0));
}

#[test]
fn invalid_preparation_is_rejected_before_measurement() {
    with_peers("MemoryArrayInt", |path| {
        let mut command = Command::new(path);
        command.args(["16", "1", "run"]);
        assert!(Worker::start(command, "wrong").is_err());
    });
}

#[test]
fn extra_requests_after_the_result_are_rejected() {
    with_peers("MemoryArrayInt", |path| {
        let mut command = Command::new(path);
        command.args(["16", "1", "run"]);
        let mut worker = Worker::start(command, "prepare").unwrap();
        assert_eq!(worker.request("run").unwrap(), "136");
        assert!(worker.request("again").is_err());
    });
}

#[test]
fn out_of_range_arguments_never_become_observations() {
    with_peers("MemoryArrayInt", |path| {
        let mut command = Command::new(path);
        command.args(["-1", "1", "run"]);
        assert!(Worker::start(command, "prepare").is_err());
    });
}

#[test]
fn memory_fixture_sources_have_canonical_layout() {
    for case in memory_workloads::FIXTURES {
        let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), case.source());
        assert!(parsed.diagnostics.is_empty(), "{}: {:?}", case.module, parsed.diagnostics);
        assert_eq!(
            fai_fmt::format(&parsed.module, &parsed.comments, case.source()),
            case.source(),
            "{} formatting",
            case.module
        );
    }
}
