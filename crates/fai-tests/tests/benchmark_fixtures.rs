//! Benchmarks must measure their named semantic path without warming cold inputs.

use fai_db::Db;
use fai_tests::benchmark_fixture::*;

#[test]
fn validation_does_not_warm_the_measured_database() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), "module M\nlet f x = x + 1\n".into());
    let file = db.source_file(id).unwrap();
    db.enable_event_log();
    validate_db(&db, &[file], &[]);
    assert!(db.take_events().is_empty());
    fai_types::check_file(&db, file);
    assert!(db.take_events().iter().any(|e| e.contains("infer_scc_query")));
}

#[test]
#[should_panic(expected = "benchmark fixture")]
fn recovered_syntax_cannot_pass_as_valid_inference() {
    validate_sources(&[("M.fai".into(), if_chain(400))], &[]);
}

#[test]
fn recovery_has_an_explicit_diagnostic_contract() {
    validate_sources(&[("M.fai".into(), if_chain(400))], &["FAI1023"]);
}

#[test]
fn largest_decision_fixture_is_valid() {
    validate_sources(&[("M.fai".into(), if_chain(*IF_DEPTHS.last().unwrap()))], &[]);
}

#[test]
fn largest_arithmetic_fixture_is_valid() {
    validate_sources(
        &[("M.fai".into(), arithmetic_chain(*ARITHMETIC_LENGTHS.last().unwrap()))],
        &[],
    );
}

#[test]
fn type_error_fixture_has_the_expected_code() {
    validate_sources(&[("M.fai".into(), "module M\nlet f = 1 + true\n".into())], &["FAI3001"]);
}

#[test]
fn small_backend_fixture_is_valid() {
    validate_sources(&[("M.fai".into(), SMALL_PROGRAM.into())], &[]);
}
#[test]
fn medium_backend_fixture_is_valid() {
    validate_sources(&[("M.fai".into(), MEDIUM_PROGRAM.into())], &[]);
}
#[test]
fn record_fixture_exports_its_signature_types() {
    validate_sources(&[("M.fai".into(), record_source(128))], &[]);
}
#[test]
fn union_fixture_exports_its_signature_types() {
    validate_sources(&[("M.fai".into(), union_source(128))], &[]);
}
#[test]
fn capability_fixture_declares_its_effects() {
    validate_sources(&[("M.fai".into(), capability_source(16))], &[]);
}
