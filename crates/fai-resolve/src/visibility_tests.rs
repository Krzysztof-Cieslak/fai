//! Exported surface reach includes interfaces, effects, and nested declarations.

use fai_db::{Db, Diag, FaiDatabase};

fn diagnostics(source: &str) -> Vec<fai_diagnostics::Diagnostic> {
    let mut db = FaiDatabase::new();
    let id = db.add_source("Main.fai".into(), source.into());
    crate::resolve::accumulated::<Diag>(&db, db.source_file(id).unwrap())
        .into_iter()
        .map(|d| d.0.clone())
        .collect()
}

#[track_caller]
fn leaks(source: &str, spelling: &str) {
    let errors = diagnostics(source);
    let error = errors
        .iter()
        .find(|d| d.code == crate::PRIVATE_TYPE_IN_PUBLIC_SIGNATURE)
        .expect("visibility leak");
    let start = source.rfind(spelling).unwrap();
    assert_eq!(error.primary.start().to_usize(), start);
    assert_eq!(error.primary.end().to_usize(), start + spelling.len());
    assert!(error.message.contains("surface exposes"), "{}", error.message);
}

#[test]
fn private_interface_in_a_public_signature_is_rejected() {
    leaks(
        "module Main\ninterface Hidden = value : Int -> Int\npublic expose : Hidden -> Int\nlet expose x = 0\n",
        "Hidden",
    );
}

#[test]
fn internal_interface_in_a_public_signature_is_rejected() {
    leaks(
        "module Main\ninternal interface Hidden = value : Int -> Int\npublic expose : Hidden -> Int\nlet expose x = 0\n",
        "Hidden",
    );
}

#[test]
fn private_interface_in_an_internal_signature_is_rejected() {
    leaks(
        "module Main\ninterface Hidden = value : Int -> Int\ninternal expose : Hidden -> Int\nlet expose x = 0\n",
        "Hidden",
    );
}

#[test]
fn private_nested_type_is_resolved_in_its_lexical_scope() {
    leaks(
        "module Main\nmodule Inner =\n  type Hidden = Int\n  public expose : Hidden -> Int\n  let expose x = x\n",
        "Hidden",
    );
}

#[test]
fn private_nested_type_is_checked_when_qualified_from_outside() {
    leaks(
        "module Main\nmodule Inner =\n  type Hidden = Int\npublic expose : Inner.Hidden -> Int\nlet expose x = x\n",
        "Inner.Hidden",
    );
}

#[test]
fn same_file_fully_qualified_private_type_is_rejected() {
    leaks(
        "module Main\nmodule Inner =\n  type Hidden = Int\npublic expose : Main.Inner.Hidden -> Int\nlet expose x = x\n",
        "Main.Inner.Hidden",
    );
}

#[test]
fn private_inner_type_shadows_the_public_outer_type() {
    leaks(
        "module Main\npublic type Hidden = Int\nmodule Inner =\n  type Hidden = Bool\n  public expose : Hidden -> Int\n  let expose x = 0\n",
        "Hidden",
    );
}

#[test]
fn nested_sibling_lookup_retains_private_reach() {
    leaks(
        "module Main\nmodule Outer =\n  module First =\n    type Hidden = Int\n  module Second =\n    public expose : First.Hidden -> Int\n    let expose x = x\n",
        "First.Hidden",
    );
}

#[test]
fn private_effect_atom_in_a_public_arrow_is_rejected() {
    leaks(
        "module Main\ninterface Hidden = run : Unit -> Unit / { Hidden }\npublic expose : Unit -> Unit / { Hidden }\nlet expose x = ()\n",
        "Hidden",
    );
}

#[test]
fn qualified_effect_atom_reports_its_precise_spelling() {
    leaks(
        "module Main\nmodule Inner =\n  interface Hidden = run : Unit -> Unit / { Hidden }\npublic expose : Unit -> Unit / { Inner . Hidden }\nlet expose x = ()\n",
        "Inner . Hidden",
    );
}

#[test]
fn private_effect_argument_in_a_public_alias_is_rejected() {
    leaks(
        "module Main\ninterface Hidden = run : Unit -> Unit / { Hidden }\npublic type Surface = Stream Int { Hidden }\n",
        "Hidden",
    );
}

#[test]
fn opaque_representations_may_keep_private_interfaces() {
    let errors = diagnostics(
        "module Main\ninterface Hidden = value : Int -> Int\npublic opaque type Secret = Hidden\n",
    );
    assert!(
        !errors.iter().any(|d| d.code == crate::PRIVATE_TYPE_IN_PUBLIC_SIGNATURE),
        "{errors:?}"
    );
}

#[test]
fn internal_surfaces_may_name_internal_interfaces() {
    let errors = diagnostics(
        "module Main\ninternal interface Hidden = value : Int -> Int\ninternal expose : Hidden -> Int\nlet expose x = 0\n",
    );
    assert!(
        !errors.iter().any(|d| d.code == crate::PRIVATE_TYPE_IN_PUBLIC_SIGNATURE),
        "{errors:?}"
    );
}

#[test]
fn cross_file_internal_interfaces_do_not_escape_publicly() {
    let mut db = FaiDatabase::new();
    db.add_source(
        "Library.fai".into(),
        "module Library\ninternal interface Hidden = value : Int -> Int\n".into(),
    );
    let id = db.add_source(
        "Main.fai".into(),
        "module Main\npublic expose : Library.Hidden -> Int\nlet expose x = 0\n".into(),
    );
    let errors = crate::resolve::accumulated::<Diag>(&db, db.source_file(id).unwrap());
    assert!(errors.iter().any(|d| d.0.code == crate::PRIVATE_TYPE_IN_PUBLIC_SIGNATURE));
}
