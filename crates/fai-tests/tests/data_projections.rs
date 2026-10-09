//! Inline tag and field operations preserve physical layouts and owned results.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

fn object(source: &str, concurrent: bool) -> Vec<u8> {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    assert!(fai_tests::check_source_diagnostics(&db, file).is_empty());
    (*fai_driver::object_code(&db, file, Symbol::intern("read"), concurrent)).clone()
}

fn has_symbol(bytes: &[u8], symbol: &str) -> bool {
    bytes.windows(symbol.len()).any(|w| w == symbol.as_bytes())
}

#[test]
fn list_tag_and_tail_do_not_import_runtime_projections() {
    let code = object(
        "module M\npublic read : List String -> List String\nlet read xs = match xs with\n  | [] -> []\n  | x :: rest -> rest\n",
        false,
    );
    assert!(!code.is_empty());
    assert!(!has_symbol(&code, "fai_data_tag"));
    assert!(!has_symbol(&code, "fai_data_field"));
}

#[test]
fn generic_dynamic_field_keeps_float_boxing_without_projection_call() {
    let code =
        object("module M\npublic read : { value : 'a | _ } -> 'a\nlet read r = r.value\n", false);
    assert!(has_symbol(&code, "fai_box_float"));
    assert!(!has_symbol(&code, "fai_data_field"));
}

#[test]
fn known_array_slot_needs_no_float_boxing_or_projection_call() {
    let code = object(
        "module M\npublic read : { items : Array Int } -> Array Int\nlet read r = r.items\n",
        false,
    );
    assert!(!has_symbol(&code, "fai_box_float"));
    assert!(!has_symbol(&code, "fai_data_field"));
}

#[test]
fn concurrent_field_dup_uses_the_biased_runtime() {
    let code = object(
        "module M\npublic read : { value : String } -> String\nlet read r = r.value\n",
        true,
    );
    assert!(has_symbol(&code, "fai_dup"));
    assert!(!has_symbol(&code, "fai_data_field"));
}

#[track_caller]
fn round_trip(values: &[String]) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Reader.fai".into(), "module Reader\npublic pick : ('a * Bool) -> 'a\nlet pick pair =\n  let (x, flag) = pair\n  x\n".into());
    let literal = values.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ");
    let source = format!(
        "module M\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n  let result = Reader.pick ([| {literal} |], true)\n  r.console.writeLine (Array.foldl (fun a x -> a ++ x) \"\" result)\n"
    );
    let id = db.add_source("M.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    assert_eq!(result.exit_code, 0);
    assert_eq!(fai_runtime::capture_take(), format!("{}\n", values.concat()));
}

#[test]
fn owned_projected_children_outlive_their_parent() {
    round_trip(&["owned".into(), "λ".into(), "".into()]);
}

mod proptests {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]
        #[test]
        fn generic_children_match_the_rust_contents(values in prop::collection::vec("[a-z]{0,8}", 0..10)) {
            super::round_trip(&values);
        }
    }
}
