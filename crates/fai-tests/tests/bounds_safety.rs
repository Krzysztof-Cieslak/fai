//! Wrapping arithmetic must never justify removing a required array check.

use fai_db::{Db, FaiDatabase};

const ADD: &str = include_str!("fixtures/bounds/WrappingAdd.fai");
const SUB: &str = include_str!("fixtures/bounds/WrappingSub.fai");
const CROSS_CALL: &str = include_str!("fixtures/bounds/CrossCall.fai");
const FUSED_CALL: &str = include_str!("fixtures/bounds/FusedCaller.fai");
const FUSED_FUNCTION: &str = include_str!("fixtures/bounds/FusedFunction.fai");
const SPREAD_LOOP: &str = include_str!("fixtures/bounds/SpreadLoop.fai");
const NUMERIC_LOOP: &str = include_str!("fixtures/bounds/NumericArrayLoop.fai");

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
fn fused_callers_do_not_lose_their_bounds_checks() {
    assert_bounds_trap("fused-call", true);
}

#[test]
fn fused_callers_report_the_normal_bounds_diagnostic() {
    assert_bounds_trap("fused-call", false);
}

#[test]
fn generated_callers_supply_entry_facts() {
    let source = FUSED_CALL
        .replace("let first = at 0 0 xs", "let first = 0")
        .replace("at 0 (-1) xs", "at 0 0 xs");
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let facts = fai_rc::entry_bounds(&db, file, fai_syntax::Symbol::intern("at"));
    assert!(
        facts.edges.contains(&(
            fai_core::bounds::PTerm::Zero,
            fai_core::bounds::PTerm::Param(1),
            0,
        )),
        "the generated caller establishes a nonnegative index: {facts:?}"
    );
}

#[test]
fn generated_first_class_uses_keep_unknown_callers_conservative() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), FUSED_FUNCTION.into());
    let file = db.source_file(id).unwrap();
    let facts = fai_rc::entry_bounds(&db, file, fai_syntax::Symbol::intern("at"));
    assert!(facts.is_empty(), "a first-class use has unknown callers: {facts:?}");
}

#[test]
fn generated_first_class_uses_report_the_normal_bounds_diagnostic() {
    assert_bounds_trap("fused-function", true);
}

#[test]
fn generated_callers_keep_checks_after_worker_transport() {
    assert_bounds_trap("fused-wire", true);
}

#[test]
fn spread_self_loops_keep_checks_for_later_iterations() {
    assert_bounds_trap("spread-loop", true);
}

#[test]
fn unique_numeric_array_loops_keep_update_bounds_checks() {
    assert_bounds_trap("numeric-loop", true);
}

#[test]
fn bounds_worker() {
    let Ok(case) = std::env::var("FAI_BOUNDS_CASE") else { return };
    let source = match case.as_str() {
        "add" => ADD,
        "sub" => SUB,
        "cross-call" => CROSS_CALL,
        "fused-call" | "fused-wire" => FUSED_CALL,
        "fused-function" => FUSED_FUNCTION,
        "spread-loop" => SPREAD_LOOP,
        "numeric-loop" => NUMERIC_LOOP,
        _ => panic!("unknown bounds case"),
    };
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    fai_driver::set_bce_shadow(std::env::var("FAI_BOUNDS_SHADOW").as_deref() == Ok("1"));
    if case == "fused-wire" {
        let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
        let bytes = serde_json::to_vec(&bundle).unwrap();
        let bundle = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(fai_driver::jit_run_bundle(&bundle), 0);
        return;
    }
    let outcome = fai_driver::jit_run_program(&db, file);
    assert_eq!(outcome.exit_code, 0, "{outcome:?}");
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

#[test]
fn generated_caller_edits_match_clean_inference() {
    let safe = FUSED_CALL.replace("at 0 (-1) xs", "at 0 0 xs");
    let revisions =
        [[("Main.fai", safe.as_str())], [("Main.fai", FUSED_CALL)], [("Main.fai", FUSED_FUNCTION)]];
    let revisions: Vec<_> = revisions.iter().map(|revision| revision.as_slice()).collect();
    fai_tests::assert_incremental_with_std_matches_clean(&revisions, |db, ids| {
        let file = db.source_file(ids[0]).unwrap();
        let name = fai_syntax::Symbol::intern("at");
        (fai_rc::entry_bounds(db, file, name), fai_rc::result_facts(db, file, name))
    });
}

#[test]
fn contract_dependencies_remove_private_entry_assumptions() {
    let source = "module Main\nlet at depth i xs = if depth <= 0 then Array.unsafeGet i xs else at (depth - 1) i xs\nlet ordinary () = at 0 0 [| 42 |]\nexample: at 0 0 [| 42 |] = 42\n";
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source.into());
    let facts =
        fai_rc::entry_bounds(&db, db.source_file(id).unwrap(), fai_syntax::Symbol::intern("at"));
    assert!(facts.is_empty(), "contract inputs are independent of ordinary callers: {facts:?}");
}

#[test]
fn contract_dependency_edits_match_clean_bounds() {
    let base = "module Main\nlet at depth i xs = if depth <= 0 then Array.unsafeGet i xs else at (depth - 1) i xs\nlet ordinary () = at 0 0 [| 42 |]\n";
    let contract = format!("{base}example: at 0 0 [| 42 |] = 42\n");
    let revisions = [[("Main.fai", base)], [("Main.fai", contract.as_str())], [("Main.fai", base)]];
    let revisions: Vec<_> = revisions.iter().map(|revision| revision.as_slice()).collect();
    fai_tests::assert_incremental_with_std_matches_clean(&revisions, |db, ids| {
        let file = db.source_file(ids[0]).unwrap();
        fai_rc::entry_bounds(db, file, fai_syntax::Symbol::intern("at"))
    });
}
