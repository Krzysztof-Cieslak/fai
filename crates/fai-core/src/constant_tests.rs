//! Constant-value expansion, its barriers, and incremental dependencies.

use fai_db::{Db, FaiDatabase, SourceOrigin};
use fai_syntax::Symbol;

use crate::{pretty_def, simplified};

fn database(source: &str) -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn reduced(source: &str) -> String {
    let (db, file) = database(source);
    pretty_def(&simplified(&db, file, Symbol::intern("run")))
}

#[test]
fn scalar_value_reaches_the_callers_arithmetic() {
    let result = reduced("module M\nlet dt = 0.01\nlet run x = x * dt\n");
    assert!(!result.contains("@dt"), "{result}");
    assert!(result.contains("0.01"), "{result}");
}

#[test]
fn nested_literal_aggregate_and_alias_expand() {
    let result = reduced(
        "module M\ntype Point = { x : Float, y : Float }\npoint : Point\nlet point = { x = 1.0, y = 0.0 - 2.0 }\nlet pair = (point, 7)\nlet run u = pair\n",
    );
    assert!(!result.contains("@pair") && !result.contains("@point"), "{result}");
}

#[test]
fn capture_free_dictionary_is_relocated() {
    let result = reduced(
        "module M\ninterface F = apply : Int -> Int\nlet inst = { F with apply x = x + 1 }\nlet run u = inst\n",
    );
    assert!(!result.contains("@inst"), "{result}");
    assert!(result.contains("fn1") && result.contains("closure/static"), "{result}");
}

#[test]
fn scalar_expansion_also_runs_inside_std() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source_with_origin(
        "<std>/M.fai".into(),
        "module M\nlet buckets = 256\nlet run x = x % buckets\n".into(),
        SourceOrigin::StandardLibrary,
    );
    let result = pretty_def(&simplified(&db, db.source_file(id).unwrap(), Symbol::intern("run")));
    assert!(!result.contains("@buckets") && result.contains("256"), "{result}");
}

#[test]
fn recursive_values_keep_their_forcing_path() {
    let result = reduced("module M\nlet a = b\nlet b = a\nlet run u = a\n");
    assert!(result.contains("@a"), "{result}");
}

#[test]
fn potentially_trapping_values_keep_their_forcing_path() {
    let result = reduced("module M\nlet bad = 1 / 0\nlet run take = if take then bad else 42\n");
    assert!(result.contains("@bad"), "{result}");
}

#[test]
fn effectful_initializers_keep_their_forcing_path() {
    let result = reduced("module M\nlet value = stdClock.now ()\nlet run u = value\n");
    assert!(result.contains("@value"), "{result}");
}

#[test]
fn generic_values_keep_their_representation_boundary() {
    let result = reduced("module M\nlet values = []\nlet run x = (x :: values, true :: values)\n");
    assert!(result.contains("@values"), "{result}");
}

#[test]
fn oversized_values_are_not_duplicated() {
    let values = (0..80).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let result = reduced(&format!("module M\nlet values = [{values}]\nlet run u = values\n"));
    assert!(result.contains("@values"), "{result}");
}

#[test]
fn constant_edits_match_clean_reduction() {
    let before = "module M\nlet value = 2\nlet run x = x * value\n";
    let after = before.replace("value = 2", "value = 3");
    let (mut db, file) = database(before);
    let old = simplified(&db, file, Symbol::intern("run"));
    db.add_source("M.fai".into(), after.clone());
    let new = simplified(&db, file, Symbol::intern("run"));
    assert_ne!(old, new);
    assert_eq!(pretty_def(&new), reduced(&after));
}

#[test]
fn unrelated_constant_edits_cut_off_before_caller_reduction() {
    let before = "module M\nlet value = 2\nlet unused = 1\nlet run x = x * value\n";
    let (mut db, file) = database(before);
    let old = simplified(&db, file, Symbol::intern("run"));
    db.enable_event_log();
    db.add_source("M.fai".into(), before.replace("unused = 1", "unused = 3"));
    assert_eq!(old, simplified(&db, file, Symbol::intern("run")));
    let events = db.take_events();
    assert!(!events.iter().any(|e| e.contains("simplified")), "{events:?}");
}

#[test]
fn ineligible_initializer_edits_cut_off_before_caller_reduction() {
    let before = "module M\nlet value = 2 / 1\nlet run x = x * value\n";
    let (mut db, file) = database(before);
    let old = simplified(&db, file, Symbol::intern("run"));
    db.enable_event_log();
    db.add_source("M.fai".into(), before.replace("value = 2", "value = 3"));
    assert_eq!(old, simplified(&db, file, Symbol::intern("run")));
    let events = db.take_events();
    // Only the initializer is reduced again; its ineligible summary cuts off run.
    assert_eq!(events.iter().filter(|e| e.contains("simplified")).count(), 1, "{events:?}");
}

#[test]
fn cross_file_constant_edits_preserve_the_body_firewall() {
    let (mut db, file) = database("module M\npublic run : Int -> Int\nlet run x = x + A.value\n");
    db.add_source("A.fai".into(), "module A\npublic value : Int\nlet value = 2\n".into());
    let old = simplified(&db, file, Symbol::intern("run"));
    assert!(pretty_def(&old).contains("@value"));
    db.enable_event_log();
    db.add_source("A.fai".into(), "module A\npublic value : Int\nlet value = 3\n".into());
    assert_eq!(old, simplified(&db, file, Symbol::intern("run")));
    let events = db.take_events();
    assert!(!events.iter().any(|e| e.contains("simplified")), "{events:?}");
}
