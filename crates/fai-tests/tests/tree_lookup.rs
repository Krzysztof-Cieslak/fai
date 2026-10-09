//! Matched binary-tree shapes, query outcomes and observable benchmark answers.

#[path = "../benches/support/tree_fixture.rs"]
mod fixture;

use fai_db::Db;
use fai_tests::tree_lookup::{self as tree, BinaryTree, KEYS};

#[test]
fn empty_shapes_and_misses_match() {
    fixture::validate(0);
}
#[test]
fn singleton_shapes_and_hits_match() {
    fixture::validate(1);
}
#[test]
fn two_node_shapes_match() {
    fixture::validate(2);
}
#[test]
fn odd_shapes_match() {
    fixture::validate(31);
}
#[test]
fn benchmark_shapes_and_all_queries_match() {
    fixture::validate(KEYS);
}

#[test]
fn legacy_btree_sum_keeps_the_same_answers() {
    let tree = BinaryTree::build(KEYS);
    let sum: i64 = (0..5000).map(|i| tree.find(i % (KEYS * 2)).unwrap_or(0)).sum();
    assert_eq!(sum, fai_tests::algorithms::option_tree_find(5000));
}

#[test]
fn reordered_results_change_the_new_checksum() {
    let reordered = tree::checksum(5, |key| Some((4 - key) * 3));
    assert_ne!(reordered, tree::expected(5));
}

#[test]
fn a_missing_zero_value_changes_the_new_checksum() {
    assert_ne!(tree::checksum(1, |_| None), tree::expected(1));
}

#[track_caller]
fn native_worker(build: bool) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use wait_timeout::ChildExt;

    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("OptionTreeFind.fai".into(), tree::fai_worker(build));
    let dir = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("tree-worker")).unwrap();
    let result = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
    assert!(result.ok, "{:?}", result.diagnostics);
    let mut child = Command::new(result.artifact.unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"5000\n100000\n5000\n").unwrap();
    let finished = child.wait_timeout(Duration::from_secs(15)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(finished && output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "ready\n{}\n{}\n{}\n",
            tree::expected(5000),
            tree::expected(100000),
            tree::expected(5000)
        )
    );
}

#[test]
fn native_lookup_worker_keeps_its_tree_across_requests() {
    native_worker(false);
}
#[test]
fn native_build_worker_rebuilds_for_each_request() {
    native_worker(true);
}

mod proptests {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]
        #[test]
        fn generated_small_shapes_match(n in 0i64..128) {
            super::fixture::validate(n);
        }
    }
}
