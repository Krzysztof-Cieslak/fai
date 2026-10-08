//! Abstract aliases keep their identity across signatures and re-exports.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::Diagnostic;
use fai_syntax::Symbol;

const LIBRARY: &str = "module Lib\npublic opaque type Secret = { answer : Int }\npublic make : Int -> Secret\nlet make n = { answer = n }\npublic read : Secret -> Int\nlet read x = x.answer\n";

fn workspace(library: &str, client: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    db.add_source("Lib.fai".into(), library.into());
    let id = db.add_source("Main.fai".into(), format!("module Main\n{client}"));
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn diagnostics(db: &FaiDatabase, file: SourceFile) -> Vec<Diagnostic> {
    fai_resolve::resolve::accumulated::<Diag>(db, file)
        .into_iter()
        .chain(crate::check_file::accumulated::<Diag>(db, file))
        .map(|d| d.0.clone())
        .collect()
}

#[track_caller]
fn clean(db: &FaiDatabase, file: SourceFile) {
    let errors = diagnostics(db, file);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn nominal_clients_construct_pass_and_consume_values() {
    let (db, file) = workspace(
        LIBRARY,
        "public inspect : Lib.Secret -> Int\nlet inspect secret = Lib.read secret\nlet value = inspect (Lib.make 42)\n",
    );
    clean(&db, file);
}

#[test]
fn returned_aliases_do_not_leak_fields() {
    let (db, file) = workspace(LIBRARY, "let leak = (Lib.make 42).answer\n");
    let errors = diagnostics(&db, file);
    let error =
        errors.iter().find(|d| d.code == crate::OPAQUE_ACCESS).expect("opaque field access");
    let range = error.primary.range();
    assert_eq!(
        &file.text(&db)[range.start().raw() as usize..range.end().raw() as usize],
        "(Lib.make 42).answer"
    );
}

#[test]
fn fabricated_alias_arguments_are_rejected() {
    let (db, file) = workspace(LIBRARY, "let invalid = Lib.read { answer = 42 }\n");
    assert!(diagnostics(&db, file).iter().any(|d| d.code == crate::OPAQUE_ACCESS));
}

#[test]
fn returned_aliases_cannot_be_record_updated() {
    let (db, file) = workspace(LIBRARY, "let invalid = { (Lib.make 42) with answer = 0 }\n");
    assert!(diagnostics(&db, file).iter().any(|d| d.code == crate::OPAQUE_ACCESS));
}

#[test]
fn same_file_transparent_reexports_preserve_external_opacity() {
    let source = format!(
        "{LIBRARY}public type Alias = Secret\npublic alias : Int -> Alias\nlet alias n = make n\n"
    );
    let (db, file) = workspace(&source, "let leak = (Lib.alias 42).answer\n");
    assert!(diagnostics(&db, file).iter().any(|d| d.code == crate::OPAQUE_ACCESS));
}

#[test]
fn third_file_reexports_preserve_the_original_observer() {
    let (mut db, file) = workspace(
        LIBRARY,
        "public inspect : Facade.Secret -> Int\nlet inspect secret = Lib.read secret\nlet value = inspect (Facade.make 42)\n",
    );
    db.add_source("Facade.fai".into(), "module Facade\npublic type Secret = Lib.Secret\npublic make : Int -> Secret\nlet make n = Lib.make n\n".into());
    clean(&db, file);
}

#[test]
fn an_alias_returned_to_its_own_file_is_transparent() {
    let source = format!(
        "{LIBRARY}public inspect : Facade.Secret -> Int\nlet inspect value = value.answer\n"
    );
    let (mut db, _) = workspace(&source, "let unused = 0\n");
    db.add_source("Facade.fai".into(), "module Facade\npublic type Secret = Lib.Secret\n".into());
    let lib = db.source_file(db.id_for_path("Lib.fai".into()).unwrap()).unwrap();
    clean(&db, lib);
}

#[test]
fn public_constructor_fields_remain_nominal_in_clients() {
    let source = format!("{LIBRARY}public type Wrapped = | Wrapped Secret\n");
    let (db, file) = workspace(
        &source,
        "public inspect : Lib.Wrapped -> Int\nlet inspect wrapped =\n  match wrapped with\n  | Lib.Wrapped value -> Lib.read value\nlet value = inspect (Lib.Wrapped (Lib.make 42))\n",
    );
    clean(&db, file);
}

#[test]
fn constructor_patterns_do_not_expose_alias_representations() {
    let source = format!("{LIBRARY}public type Wrapped = | Wrapped Secret\n");
    let (db, file) = workspace(
        &source,
        "let leak wrapped =\n  match wrapped with\n  | Lib.Wrapped value -> value.answer\n",
    );
    assert!(diagnostics(&db, file).iter().any(|d| d.code == crate::OPAQUE_ACCESS));
}

#[test]
fn interface_method_signatures_preserve_alias_identity() {
    let source = format!(
        "{LIBRARY}public interface Provider =\n  provide : Unit -> Secret\npublic provider : Provider\nlet provider = {{ Provider with provide u = make 42 }}\n"
    );
    let (db, file) = workspace(
        &source,
        "public value : Lib.Secret\nlet value = Lib.provider.provide ()\nlet answer = Lib.read value\n",
    );
    clean(&db, file);
}

#[test]
fn interface_methods_do_not_expose_alias_representations() {
    let source = format!(
        "{LIBRARY}public interface Provider =\n  provide : Unit -> Secret\npublic provider : Provider\nlet provider = {{ Provider with provide u = make 42 }}\n"
    );
    let (db, file) = workspace(&source, "let leak = (Lib.provider.provide ()).answer\n");
    assert!(diagnostics(&db, file).iter().any(|d| d.code == crate::OPAQUE_ACCESS));
}

#[test]
fn opaque_functions_cannot_be_applied_directly() {
    let (db, file) = workspace(
        "module Lib\npublic opaque type Function = Int -> Int\npublic function : Function\nlet function = fun n -> n + 1\n",
        "let invalid = Lib.function 41\n",
    );
    assert!(diagnostics(&db, file).iter().any(|d| d.code == crate::TYPE_MISMATCH));
}

#[test]
fn generic_alias_parameters_substitute_in_nominal_clients() {
    let (db, file) = workspace(
        "module Lib\npublic opaque type Box 'a = { value : 'a }\npublic box : 'a -> Box 'a\nlet box x = { value = x }\npublic get : Box 'a -> 'a\nlet get x = x.value\n",
        "public inspect : Lib.Box String -> String\nlet inspect box = Lib.get box\nlet result = inspect (Lib.box \"ok\")\n",
    );
    clean(&db, file);
}

#[test]
fn effect_alias_parameters_substitute_in_nominal_clients() {
    let (db, file) = workspace(
        "module Lib\npublic opaque type Deferred 'a 'e = Unit -> 'a / 'e\npublic wrap : (Unit -> 'a / 'e) -> Deferred 'a 'e\nlet wrap f = f\npublic force : Deferred 'a 'e -> 'a / 'e\nlet force f = f ()\n",
        "public action : Lib.Deferred Unit { Console }\nlet action = Lib.wrap (fun u -> stdConsole.writeLine \"called\")\npublic run : Unit -> Unit / { Console }\nlet run u = Lib.force action\n",
    );
    clean(&db, file);
}

#[test]
fn abstract_signature_queries_cut_off_private_representation_edits() {
    let (mut db, file) = workspace(LIBRARY, "let value = Lib.make 42\n");
    let lib = db.source_file(db.id_for_path("Lib.fai".into()).unwrap()).unwrap();
    let before = crate::signature_scheme_observed(&db, lib, Symbol::intern("make"), Some(file));
    db.add_source(
        "Lib.fai".into(),
        LIBRARY
            .replace("{ answer : Int }", "Int")
            .replace("{ answer = n }", "n")
            .replace("x.answer", "x"),
    );
    let after = crate::signature_scheme_observed(&db, lib, Symbol::intern("make"), Some(file));
    assert_eq!(before, after);
    clean(&db, file);
}

#[test]
fn client_inference_does_not_repeat_for_a_hidden_representation_edit() {
    let (mut db, file) = workspace(LIBRARY, "let value = Lib.make 42\n");
    let name = Symbol::intern("value");
    let before = crate::def_type(&db, file, name);
    db.enable_event_log();
    db.add_source(
        "Lib.fai".into(),
        LIBRARY
            .replace("{ answer : Int }", "Int")
            .replace("{ answer = n }", "n")
            .replace("x.answer", "x"),
    );
    assert_eq!(before, crate::def_type(&db, file, name));
    let events = db.take_events();
    assert!(!events.iter().any(|event| event.contains("infer_scc_query")), "{events:?}");
}
