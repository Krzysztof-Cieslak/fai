//! Allocation decisions and incremental guarantees of arity-aware escape analysis.

use fai_db::{Db, FaiDatabase};

use super::*;

fn marked(body: &str) -> String {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), format!("module M\n{body}\n"));
    let file = db.source_file(id).unwrap();
    let mut lowered = (*core(&db, file, Symbol::intern("make"))).clone();
    mark_escaping_closures(&db, &mut lowered);
    fai_core::pretty_def(&lowered)
}

#[test]
fn under_applied_inline_lambda_stays_heap_allocated() {
    let body = marked("let make seed = (fun x y -> seed + x + y) 1");
    assert!(body.contains("closure/heap"), "{body}");
    assert!(!body.contains("closure/stack"), "{body}");
}

#[test]
fn returned_let_bound_partial_application_stays_heap_allocated() {
    let body = marked("let add a b = a + b\nlet make seed =\n  let partial = add seed\n  partial");
    assert!(body.contains("(app @add"), "{body}");
    assert!(!body.contains("app stack"), "{body}");
}

#[test]
fn returned_conditional_alias_keeps_both_closures_on_the_heap() {
    let body = marked(
        "let make seed flag =\n  let first = fun x -> seed + x\n  let second = fun x -> seed - x\n  let chosen = if flag then first else second\n  chosen",
    );
    assert_eq!(body.matches("closure/heap").count(), 2, "{body}");
    assert!(!body.contains("closure/stack"), "{body}");
}

#[test]
fn saturated_inline_lambda_still_stack_allocates() {
    let body = marked("let make seed = (fun x -> seed + x) 1");
    assert!(body.contains("closure/stack"), "{body}");
}

#[test]
fn generic_map_records_an_arity_requirement() {
    let mut db = FaiDatabase::new();
    let id = db.add_source("M.fai".into(), "module M\nlet map f xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> f x :: map f rest\n".into());
    assert_eq!(
        escape_profile(&db, db.source_file(id).unwrap(), Symbol::intern("map")),
        [EscapeUse::Applied(1), EscapeUse::Always]
    );
}

#[test]
fn under_application_through_a_generic_helper_stays_on_the_heap() {
    let body = marked("let apply f x = f x\nlet make seed = apply (fun x y -> seed + x + y) 1");
    assert!(body.contains("closure/heap"), "{body}");
    assert!(!body.contains("closure/stack"), "{body}");
}

#[test]
fn known_saturated_argument_satisfies_a_generic_helpers_requirement() {
    let body = marked("let apply f x = f x\nlet make seed = apply (fun x -> seed + x) 1");
    assert!(body.contains("closure/stack"), "{body}");
}

#[test]
fn arity_profile_changes_update_callers_and_match_clean_analysis() {
    let helper =
        "module Helper\npublic apply : ('a -> 'b / 'e) -> 'a -> 'b / 'e\nlet apply f x = f x\n";
    let caller = "module Caller\nlet make seed = Helper.apply (fun x -> seed + x) 1\n";
    let load = |helper: &str| {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source("Helper.fai".into(), helper.into());
        let id = db.add_source("Caller.fai".into(), caller.into());
        let file = db.source_file(id).unwrap();
        (db, file)
    };
    let (mut db, file) = load(helper);
    let name = Symbol::intern("make");
    let before = crate::rc(&db, file, name);
    assert!(fai_core::pretty_def(&before).contains("closure/stack"));
    db.enable_event_log();
    db.add_source("Helper.fai".into(), format!("{helper}// comment\n"));
    assert_eq!(before, crate::rc(&db, file, name));
    let events = db.take_events();
    assert!(!events.iter().any(|e| e.contains("rc(")), "{events:?}");
    let changed =
        helper.replace("let apply f x = f x", "let apply f x =\n  let stored = (f, f)\n  f x");
    db.add_source("Helper.fai".into(), changed.clone());
    let after = crate::rc(&db, file, name);
    assert!(fai_core::pretty_def(&after).contains("closure/heap"));
    let (clean, clean_file) = load(&changed);
    assert_eq!(after, crate::rc(&clean, clean_file, name));
}
