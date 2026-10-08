//! Local function shorthand and explicit lambdas carry the same latent effects.

use fai_db::{Db, Diag, FaiDatabase};

const WRITE: &str =
    "public write : Console -> String -> Unit / { Console }\nlet write c s = c.writeLine s\n";

#[track_caller]
fn equivalent(shorthand: &str, explicit: &str) {
    let source = format!("module M\n{WRITE}\nlet shorthand {shorthand}\nlet explicit {explicit}\n");
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let diagnostics = crate::check_file::accumulated::<Diag>(&db, file);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let short = crate::def_type(&db, file, fai_syntax::Symbol::intern("shorthand"));
    let long = crate::def_type(&db, file, fai_syntax::Symbol::intern("explicit"));
    assert_eq!(short, long);
    assert_eq!(
        crate::def_effect(&db, file, fai_syntax::Symbol::intern("shorthand")),
        crate::def_effect(&db, file, fai_syntax::Symbol::intern("explicit")),
    );
}

#[test]
fn unused_local_function_does_not_incur_its_effect() {
    equivalent("c =\n  let log s = write c s\n  42", "c =\n  let log = fun s -> write c s\n  42");
}

#[test]
fn returned_local_function_keeps_its_effect() {
    equivalent("c =\n  let log s = write c s\n  log", "c =\n  let log = fun s -> write c s\n  log");
}

#[test]
fn calling_a_local_function_incurs_its_effect() {
    equivalent(
        "c =\n  let log s = write c s\n  log \"x\"",
        "c =\n  let log = fun s -> write c s\n  log \"x\"",
    );
}

#[test]
fn multi_parameter_local_function_keeps_the_saturating_effect() {
    equivalent(
        "c =\n  let log prefix s = write c (prefix ++ s)\n  log",
        "c =\n  let log = fun prefix s -> write c (prefix ++ s)\n  log",
    );
}

#[test]
fn nested_local_functions_keep_each_creation_pure() {
    equivalent(
        "c =\n  let outer prefix =\n    let inner s = write c (prefix ++ s)\n    inner\n  outer",
        "c =\n  let outer = fun prefix ->\n    let inner = fun s -> write c (prefix ++ s)\n    inner\n  outer",
    );
}

#[test]
fn local_function_forwards_a_polymorphic_effect() {
    equivalent(
        "action =\n  let call x = action x\n  call",
        "action =\n  let call = fun x -> action x\n  call",
    );
}

#[test]
fn eager_local_value_still_incurs_its_effect() {
    let source = format!("module M\n{WRITE}\nlet run c =\n  let value = write c \"x\"\n  value\n");
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let scheme = crate::def_type(&db, file, fai_syntax::Symbol::intern("run"));
    assert_eq!(crate::render_scheme(&scheme), "Console -> () / { Console }");
}

#[test]
fn pure_signature_cannot_hide_a_returned_local_functions_effect() {
    let source = "module M\n// π\npublic make : Console -> (String -> Unit)\nlet make c =\n  let log s = c.writeLine s\n  log\n";
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let diagnostics = crate::check_file::accumulated::<Diag>(&db, file);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let diagnostic = &diagnostics[0].0;
    assert_eq!(diagnostic.code, crate::SIGNATURE_MISMATCH);
    assert_eq!(diagnostic.primary.range().start().raw() as usize, source.find("let make").unwrap());
    assert_eq!(diagnostic.primary.range().end().raw() as usize, source.len());
    assert_eq!(
        diagnostic.message,
        "the body of `make` does not match its declared type `Console -> String -> ()`"
    );
}

#[test]
fn local_body_types_preserve_the_effect_and_update_incrementally() {
    let source = format!("module M\n{WRITE}\nlet make c =\n  let log s = write c s\n  log\n");
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.clone());
    let file = db.source_file(id).unwrap();
    let name = fai_syntax::Symbol::intern("make");
    let before = crate::def_local_types(&db, file, name);
    let log = before.iter().find(|(name, _)| name == "log").unwrap();
    assert_eq!(crate::render_canonical(&log.1), "String -> () / { Console }");
    let changed = source.replace("let log s = write c s", "let log s = ()");
    db.add_source("M.fai".into(), changed.clone());
    let after = crate::body_types(&db, file, name);
    let mut clean = FaiDatabase::new();
    crate::std_lib::load_std(&mut clean);
    let clean_id = clean.add_source("M.fai".into(), changed);
    assert_eq!(after, crate::body_types(&clean, clean.source_file(clean_id).unwrap(), name));
}
