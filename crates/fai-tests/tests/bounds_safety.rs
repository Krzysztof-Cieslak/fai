//! Wrapping arithmetic must never justify removing a required array check.

use fai_db::{Db, FaiDatabase};

const ADD: &str = include_str!("fixtures/bounds/WrappingAdd.fai");
const SUB: &str = include_str!("fixtures/bounds/WrappingSub.fai");
const CROSS_CALL: &str = include_str!("fixtures/bounds/CrossCall.fai");

#[track_caller]
fn assert_bounds_trap(case: &str, shadow: bool) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "bounds_worker", "--nocapture"])
        .env("FAI_BOUNDS_CASE", case)
        .env("FAI_BOUNDS_SHADOW", if shadow { "1" } else { "0" })
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "invalid index was accepted");
    assert!(stderr.contains("array index out of bounds"), "{stderr}");
    assert!(!stderr.contains("bounds-check elimination unsound"), "{stderr}");
}

#[test]
fn wrapping_addition_keeps_the_jit_bounds_check() {
    assert_bounds_trap("add", false);
}

#[test]
fn wrapping_subtraction_keeps_the_jit_bounds_check() {
    assert_bounds_trap("sub", false);
}

#[test]
fn wrapping_addition_does_not_trigger_a_shadow_violation() {
    assert_bounds_trap("add", true);
}

#[test]
fn wrapping_subtraction_does_not_trigger_a_shadow_violation() {
    assert_bounds_trap("sub", true);
}

#[test]
fn wrapping_facts_do_not_cross_call_boundaries() {
    assert_bounds_trap("cross-call", true);
}

#[test]
fn bounds_worker() {
    let Ok(case) = std::env::var("FAI_BOUNDS_CASE") else { return };
    let source = match case.as_str() {
        "add" => ADD,
        "sub" => SUB,
        "cross-call" => CROSS_CALL,
        _ => panic!("unknown bounds case"),
    };
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    fai_driver::set_bce_shadow(std::env::var("FAI_BOUNDS_SHADOW").as_deref() == Ok("1"));
    let outcome = fai_driver::jit_run_program(&db, file);
    assert_eq!(outcome.exit_code, 0);
}

#[test]
fn wrapping_fact_edits_match_clean_inference() {
    let safe = CROSS_CALL.replace("let j = i + 1", "let j = 0");
    let original = [("Main.fai", CROSS_CALL)];
    let revised = [("Main.fai", safe.as_str())];
    let revisions = [original.as_slice(), revised.as_slice(), original.as_slice()];
    fai_tests::assert_incremental_with_std_matches_clean(&revisions, |db, ids| {
        let file = db.source_file(*ids.last().unwrap()).unwrap();
        let name = fai_syntax::Symbol::intern("at");
        (fai_rc::entry_bounds(db, file, name), fai_rc::result_facts(db, file, name))
    });
}
