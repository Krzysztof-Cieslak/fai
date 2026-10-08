//! Value forcing retains execution effects separately from the value's type.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

const INITIALIZER: &str = "let answer =\n  let ignored = stdConsole.writeLine \"forced\"\n  42\n";

fn workspace(source: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), format!("module Main\n{source}"));
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn effects(db: &FaiDatabase, file: SourceFile, name: &str) -> Vec<String> {
    crate::def_effect(db, file, Symbol::intern(name))
        .labels
        .iter()
        .map(|label| label.name.as_str().to_owned())
        .collect()
}

fn codes(db: &FaiDatabase, file: SourceFile) -> Vec<String> {
    crate::check_file::accumulated::<Diag>(db, file)
        .into_iter()
        .map(|diagnostic| diagnostic.0.code.as_str().to_owned())
        .collect()
}

#[test]
fn private_value_effects_flow_to_its_reader() {
    let (db, file) = workspace(&format!(
        "{INITIALIZER}public read : Unit -> Int / {{ Console }}\nlet read u = answer\n"
    ));
    assert_eq!(effects(&db, file, "answer"), ["Console"]);
    assert_eq!(effects(&db, file, "read"), ["Console"]);
    assert!(codes(&db, file).is_empty());
}

#[test]
fn a_private_value_signature_does_not_hide_forcing() {
    let (db, file) = workspace(&format!(
        "answer : Int\n{INITIALIZER}public read : Unit -> Int\nlet read u = answer\n"
    ));
    assert!(codes(&db, file).contains(&"FAI5001".to_owned()));
}

#[test]
fn exported_values_cannot_have_unwritten_forcing_effects() {
    let (db, file) = workspace(&format!("public answer : Int\n{INITIALIZER}"));
    let diagnostics = crate::check_file::accumulated::<Diag>(&db, file);
    let error =
        diagnostics.iter().find(|diagnostic| diagnostic.0.code.as_str() == "FAI5001").unwrap();
    assert!(error.0.message.contains("exported value `answer` must initialize purely"));
}

#[test]
fn internal_values_also_require_pure_initializers() {
    let (db, file) = workspace(&format!("internal answer : Int\n{INITIALIZER}"));
    assert!(codes(&db, file).contains(&"FAI5001".to_owned()));
}

#[test]
fn constructing_an_effectful_closure_is_pure() {
    let (db, file) = workspace(
        "public action : Int -> Int / { Console }\nlet action = fun n ->\n  let ignored = stdConsole.writeLine \"called\"\n  n\n",
    );
    assert!(effects(&db, file, "action").is_empty());
    assert!(codes(&db, file).is_empty());
}

#[test]
fn effectful_closure_construction_is_charged_when_read() {
    let (db, file) = workspace(
        "let action =\n  let ignored = stdConsole.writeLine \"created\"\n  fun n -> n\npublic use : Unit -> Int / { Console }\nlet use u = action 1\n",
    );
    assert_eq!(effects(&db, file, "use"), ["Console"]);
    assert!(codes(&db, file).is_empty());
}

#[test]
fn recursive_value_and_declared_function_reach_an_effect_fixpoint() {
    let (db, file) = workspace(
        "let answer = produce 0\npublic produce : Int -> Int / { Console }\nlet produce n =\n  if n = 0 then\n    let ignored = stdConsole.writeLine \"forced\"\n    42\n  else answer\n",
    );
    assert_eq!(effects(&db, file, "answer"), ["Console"]);
    assert_eq!(effects(&db, file, "produce"), ["Console"]);
    assert!(codes(&db, file).is_empty(), "{:?}", codes(&db, file));
}

#[test]
fn recursive_value_and_inferred_function_reach_an_effect_fixpoint() {
    let (db, file) = workspace(
        "let answer = produce 0\nlet produce n =\n  if n = 0 then\n    let ignored = stdConsole.writeLine \"forced\"\n    42\n  else answer\n",
    );
    assert_eq!(effects(&db, file, "answer"), ["Console"]);
    assert_eq!(effects(&db, file, "produce"), ["Console"]);
    assert!(codes(&db, file).is_empty(), "{:?}", codes(&db, file));
}

#[test]
fn a_recursive_function_value_does_not_force_its_body() {
    let (db, file) = workspace(
        "let action = callback\nlet callback n =\n  let ignored = stdConsole.writeLine \"called\"\n  if n = 0 then 42 else action (n - 1)\n",
    );
    assert!(effects(&db, file, "action").is_empty());
    assert_eq!(effects(&db, file, "callback"), ["Console"]);
    assert!(codes(&db, file).is_empty());
}

#[test]
fn contracts_cannot_force_an_effectful_value() {
    let (db, file) = workspace(&format!("{INITIALIZER}example: answer = 42\n"));
    assert!(codes(&db, file).contains(&"FAI6004".to_owned()));
}

#[test]
fn private_runtime_builders_keep_their_initializer_effects() {
    let (db, file) = workspace(
        "let runtime =\n  let ignored = stdConsole.writeLine \"initialize\"\n  defaultRuntime\npublic main : Runtime -> Unit\nlet main r = ()\n",
    );
    assert_eq!(effects(&db, file, "runtime"), ["Console"]);
    assert!(codes(&db, file).is_empty());
}

#[test]
fn a_returned_closure_carries_the_effect_of_forcing_a_private_value() {
    let (db, file) = workspace(&format!(
        "{INITIALIZER}public make : Unit -> (Unit -> Int / {{ Console }})\nlet make u = fun v -> answer\n"
    ));
    assert!(effects(&db, file, "make").is_empty());
    assert!(codes(&db, file).is_empty(), "{:?}", codes(&db, file));
}

#[test]
fn transitive_private_initializers_propagate_forcing_effects() {
    let (db, file) = workspace(&format!(
        "{INITIALIZER}let copied = answer\nlet wrapped = {{ value = copied }}\npublic read : Unit -> Int / {{ Console }}\nlet read u = wrapped.value\n"
    ));
    assert_eq!(effects(&db, file, "read"), ["Console"]);
    assert!(codes(&db, file).is_empty());
}

#[test]
fn pure_recursive_value_effect_analysis_terminates() {
    let (db, file) = workspace("let forever = forever\n");
    assert!(effects(&db, file, "forever").is_empty());
}

#[test]
fn mutually_recursive_values_union_their_forcing_capabilities() {
    let (db, file) = workspace(
        "let first =\n  if true then\n    let ignored = stdConsole.writeLine \"forced\"\n    1\n  else second\nlet second = if true then stdClock.now () else first\n",
    );
    assert_eq!(effects(&db, file, "first"), ["Clock", "Console"]);
    assert_eq!(effects(&db, file, "second"), ["Clock", "Console"]);
    assert!(codes(&db, file).is_empty(), "{:?}", codes(&db, file));
}

#[test]
fn holding_a_function_still_forces_its_effectful_initializer() {
    let (db, file) = workspace(
        "let action =\n  let ignored = stdConsole.writeLine \"created\"\n  fun n -> n\nexample: const true action\n",
    );
    assert!(codes(&db, file).contains(&"FAI6004".to_owned()));
}

#[test]
fn unchanged_forcing_effects_cut_off_dependent_queries() {
    let (mut db, file) = workspace("let value = 1\n");
    let name = Symbol::intern("value");
    assert!(crate::query::forcing_test_summary(&db, file, name));
    db.enable_event_log();
    db.add_source("Main.fai".into(), "module Main\nlet value = 2\n".into());
    assert!(crate::query::forcing_test_summary(&db, file, name));
    let events = db.take_events();
    assert!(events.iter().any(|event| event.contains("infer_scc_query")), "{events:?}");
    assert!(!events.iter().any(|event| event.contains("forcing_test_summary")), "{events:?}");
    db.add_source("Main.fai".into(), "module Main\nlet value = stdClock.now ()\n".into());
    assert!(!crate::query::forcing_test_summary(&db, file, name));
    assert!(db.take_events().iter().any(|event| event.contains("forcing_test_summary")));
}
