//! Sorting benchmark fixtures must observe order and match every language peer.

use fai_db::Db;
use fai_tests::sorting::{self, Case};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

#[test]
fn distribution_source_is_canonical() {
    let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), sorting::SOURCE);
    assert!(parsed.diagnostics.is_empty());
    assert_eq!(fai_fmt::format(&parsed.module, &parsed.comments, sorting::SOURCE), sorting::SOURCE);
}

#[test]
fn compiled_no_sort_negative_control_disagrees_with_oracle() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id =
        db.add_source("SortPatterns.fai".into(), sorting::SOURCE.replace("Array.sort", "identity"));
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
        .unwrap_or_else(|d| panic!("{d:?}"));
    let run = program.function(fai_syntax::Symbol::intern("run")).unwrap();
    let result = fai_runtime::apply(run, &[fai_runtime::make_int(1), fai_runtime::make_int(6)]);
    assert_ne!(fai_runtime::read_int(result), sorting::run(1, 6));
    fai_runtime::fai_drop(result);
}

#[track_caller]
fn check(pattern: usize, n: usize) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("SortPatterns.fai".into(), sorting::SOURCE.into());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
        .unwrap_or_else(|d| panic!("{d:?}"));
    let args = [fai_runtime::make_int(pattern as i64), fai_runtime::make_int(n as i64)];
    let input = sorting::input(pattern, n);
    let mut sorted = input.clone();
    sorted.sort();
    let array = |value| {
        let len = fai_runtime::read_int(fai_runtime::fai_array_length_borrowed(value));
        let result: Vec<_> = (0..len)
            .map(|i| {
                let field = fai_runtime::fai_array_get_borrowed(value, fai_runtime::make_int(i));
                let result = fai_runtime::read_int(field);
                fai_runtime::fai_drop(field);
                result
            })
            .collect();
        fai_runtime::fai_drop(value);
        result
    };
    let generate = program.function(fai_syntax::Symbol::intern("generate")).unwrap();
    assert_eq!(array(fai_runtime::apply(generate, &args)), input);
    let sort = program.function(fai_syntax::Symbol::intern("sortedValues")).unwrap();
    assert_eq!(array(fai_runtime::apply(sort, &args)), sorted);
    if let Some(binary) = fai_tests::ocaml::baseline() {
        let output = std::process::Command::new(binary)
            .args([sorting::PATTERNS[pattern], &n.to_string()])
            .output()
            .unwrap();
        fai_tests::benchmark_process::ExpectedAnswer::Int(sorting::checksum(&sorted))
            .verify("OCaml sort distribution", &output)
            .unwrap();
    }
}

#[test]
fn ascending_matches_peers() {
    check(0, 37);
}
#[test]
fn descending_matches_peers() {
    check(1, 37);
}
#[test]
fn shuffled_matches_peers() {
    check(2, 37);
}
#[test]
fn equal_matches_peers() {
    check(3, 37);
}
#[test]
fn few_keys_match_peers() {
    check(4, 37);
}
#[test]
fn partial_runs_match_peers() {
    check(5, 37);
}
#[test]
fn empty_input_matches_peers() {
    check(2, 0);
}
#[test]
fn singleton_matches_peers() {
    check(5, 1);
}

#[test]
fn a_no_sort_negative_control_has_the_wrong_answer() {
    let descending = sorting::input(1, 6);
    assert_ne!(sorting::checksum(&descending), sorting::run(1, 6));
}

#[test]
fn an_adjacent_inversion_changes_the_checksum() {
    let mut values = sorting::input(0, 6);
    values.swap(2, 3);
    assert_ne!(sorting::checksum(&values), sorting::run(0, 6));
}

#[test]
fn missing_or_duplicated_elements_change_the_checksum() {
    let values = vec![0, 1, 2, 3, 4, 4];
    assert_ne!(sorting::checksum(&values), sorting::run(0, 6));
}

#[test]
fn every_distribution_and_size_has_an_ocaml_dispatch_and_case() {
    let baseline = include_str!("../ocaml/baseline.ml");
    for (pattern, name) in sorting::PATTERNS.iter().enumerate() {
        assert!(baseline.contains(&format!("\"{name}\"")));
        assert!(sorting::CASES.iter().any(|c| c.pattern == pattern && c.n == 6000));
        assert!(sorting::CASES.iter().any(|c| c.pattern == pattern && c.n == 80000));
    }
    assert_eq!(sorting::CASES.len(), sorting::PATTERNS.len() * 2);
    assert!(sorting::program(Case { pattern: 2, n: 6000 }).contains("run 2 6000"));
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn shuffled_and_runs_are_permutations(n in 0usize..1000) {
            let mut shuffled = sorting::input(2,n);
            let mut runs = sorting::input(5,n);
            shuffled.sort(); runs.sort();
            prop_assert_eq!(shuffled,sorting::input(0,n));
            prop_assert_eq!(runs,sorting::input(0,n));
        }
    }
}
