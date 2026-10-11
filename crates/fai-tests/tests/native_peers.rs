//! Native peers must remain compiler-independent and answer-compatible.

#![cfg(not(windows))]

use std::process::Command;

use fai_tests::algorithms::{ALGORITHMS, by_module};
use fai_tests::benchmark_aot::{self as aot, Entry};
use fai_tests::benchmark_process::{ExpectedAnswer, Worker, spawn_checked};

#[test]
fn every_isolated_peer_matches_its_registered_answer() {
    // Registry-wide coverage is one invariant: adding a case must also provide
    // a buildable, isolated native entry on each available peer.
    for case in ALGORITHMS
        .iter()
        .chain(fai_tests::tail_components::COMPONENTS)
        .chain(fai_tests::memory_workloads::FIXTURES)
    {
        let expected = ExpectedAnswer::at_size(case, case.jit_size);
        let rust = fai_tests::native_peer::rust(case, Entry::Once);
        let output = spawn_checked(Command::new(&rust).arg(case.jit_size.to_string())).unwrap();
        expected.verify(case.module, &output).unwrap();
        if let Some(ocaml) = fai_tests::ocaml::isolated(case, Entry::Once) {
            let output = spawn_checked(Command::new(ocaml).arg(case.jit_size.to_string())).unwrap();
            expected.verify(case.module, &output).unwrap();
        }
    }
}

#[test]
fn an_isolated_worker_accepts_repeated_runtime_inputs() {
    let case = by_module("Fib").unwrap();
    let path = fai_tests::native_peer::rust(case, Entry::Worker);
    let mut worker = Worker::start(Command::new(path), "8 9").unwrap();
    assert_eq!(worker.request("run 3").unwrap(), "76");
    assert_eq!(worker.request("floor 3").unwrap(), "25");
    assert_eq!(worker.request("value 1").unwrap(), "34");
    worker.finish().unwrap();
}

#[test]
fn process_floor_does_not_execute_the_selected_workload() {
    // A Fibonacci computation at this input cannot complete; floor mode must
    // inspect only the input and checksum, never call the workload.
    let case = by_module("Fib").unwrap();
    let rust = fai_tests::native_peer::rust(case, Entry::Memory);
    let output = spawn_checked(Command::new(rust).args(["1000000", "3", "floor"])).unwrap();
    ExpectedAnswer::Int(3_000_000).verify("floor", &output).unwrap();
}

#[test]
fn repeated_memory_workloads_preserve_full_width_checksums() {
    let case = by_module("PrngXorshift").unwrap();
    let expected = aot::checksum(&[ExpectedAnswer::at_size(case, 17)], 3);
    let rust = fai_tests::native_peer::rust(case, Entry::Memory);
    expected
        .verify("rust", &spawn_checked(Command::new(rust).args(["17", "3", "run"])).unwrap())
        .unwrap();
    if let Some(ocaml) = fai_tests::ocaml::isolated(case, Entry::Memory) {
        expected
            .verify("ocaml", &spawn_checked(Command::new(ocaml).args(["17", "3", "run"])).unwrap())
            .unwrap();
    }
}

#[test]
fn a_compiler_linked_executable_fails_isolation_validation() {
    assert!(fai_tests::native_peer::verify_isolated(&std::env::current_exe().unwrap()).is_err());
}

#[test]
fn a_selected_rust_peer_excludes_unrelated_workload_symbols() {
    use object::{Object, ObjectSymbol};
    let path = fai_tests::native_peer::rust(by_module("Fib").unwrap(), Entry::Once);
    let bytes = std::fs::read(path).unwrap();
    let image = object::File::parse(bytes.as_slice()).unwrap();
    assert!(!image.symbols().filter_map(|symbol| symbol.name().ok()).any(|name| {
        name.contains("graph_bfs") || name.contains("json_serialize") || name.contains("fannkuch")
    }));
}

#[test]
fn selected_ocaml_source_excludes_unrelated_workloads_and_dispatch() {
    let source = fai_tests::ocaml::isolated_source(by_module("Fib").unwrap(), Entry::Worker);
    assert!(source.contains("let rec fib "));
    assert!(!source.contains("let graph_bfs "));
    assert!(!source.contains("let workload ="));
}
