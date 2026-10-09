//! Interface literals use the lexical and qualified lookup of interface types.

use fai_db::{Db, Diag, FaiDatabase, SourceOrigin};

fn errors(source: &str, library: Option<(&str, SourceOrigin)>) -> Vec<String> {
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    if let Some((text, origin)) = library {
        db.add_source_with_origin("Library.fai".into(), text.into(), origin);
    }
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    fai_resolve::resolve::accumulated::<Diag>(&db, file)
        .into_iter()
        .chain(crate::check_file::accumulated::<Diag>(&db, file))
        .map(|d| d.0.code.as_str().to_owned())
        .collect()
}

#[test]
fn nested_interface_is_visible_by_its_local_name() {
    assert!(errors("module Main\nmodule Inner =\n  interface Service = run : Int -> Int\n  let service = { Service with run x = x }\n", None).is_empty());
}

#[test]
fn nested_interface_shadows_a_different_outer_interface() {
    assert!(errors("module Main\ninterface Service = outer : String -> String\nmodule Inner =\n  interface Service = inner : Int -> Int\n  let service = { Service with inner x = x }\n", None).is_empty());
}

#[test]
fn nested_code_can_build_an_enclosing_interface() {
    assert!(errors("module Main\ninterface Service = run : Int -> Int\nmodule Inner =\n  let service = { Service with run x = x }\n", None).is_empty());
}

#[test]
fn same_file_qualified_instance_uses_private_interface() {
    assert!(errors("module Main\nmodule Inner =\n  interface Service = run : Int -> Int\nlet service = { Inner.Service with run x = x }\n", None).is_empty());
}

#[test]
fn self_module_qualified_instance_uses_private_interface() {
    assert!(errors("module Main\ninterface Service = run : Int -> Int\nlet service = { Main.Service with run x = x }\n", None).is_empty());
}

#[test]
fn cross_file_nested_public_interface_is_constructible() {
    assert!(
        errors(
            "module Main\nlet service = { Library.Inner.Service with run x = x }\n",
            Some((
                "module Library\nmodule Inner =\n  public interface Service = run : Int -> Int\n",
                SourceOrigin::User
            ))
        )
        .is_empty()
    );
}

#[test]
fn cross_file_private_interface_is_rejected() {
    assert!(
        errors(
            "module Main\nlet service = { Library.Service with run x = x }\n",
            Some(("module Library\ninterface Service = run : Int -> Int\n", SourceOrigin::User))
        )
        .contains(&"FAI2003".into())
    );
}

#[test]
fn cross_origin_internal_interface_is_rejected() {
    assert!(
        errors(
            "module Main\nlet service = { Library.Service with run x = x }\n",
            Some((
                "module Library\ninternal interface Service = run : Int -> Int\n",
                SourceOrigin::StandardLibrary
            ))
        )
        .contains(&"FAI2020".into())
    );
}

#[test]
fn same_origin_internal_interface_is_constructible() {
    assert!(
        errors(
            "module Main\nlet service = { Library.Service with run x = x }\n",
            Some((
                "module Library\ninternal interface Service = run : Int -> Int\n",
                SourceOrigin::User
            ))
        )
        .is_empty()
    );
}

#[test]
fn nested_contract_uses_its_interface_scope() {
    assert!(errors("module Main\nmodule Inner =\n  interface Service = run : Int -> Int\n  example: ({ Service with run x = x }).run 1 = 1\n", None).is_empty());
}
