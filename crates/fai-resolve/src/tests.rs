//! Unit tests for resolution: pairing, visibility, scope, qualified references,
//! duplicate modules, and SCCs.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::Severity;
use fai_syntax::Symbol;
use fai_syntax::ast::Visibility;

use crate::ids::{DefId, Res};
use crate::{module_defs, module_interface, module_sccs, resolve};

/// Builds a database from `(path, text)` files and returns them in order.
fn db_with(files: &[(&str, &str)]) -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    let mut handles = Vec::new();
    for (path, text) in files {
        let id = db.add_source((*path).into(), (*text).to_owned());
        handles.push(db.source_file(id).unwrap());
    }
    (db, handles)
}

/// Collects the resolution diagnostics emitted for `file`.
fn resolve_diags(db: &dyn Db, file: SourceFile) -> Vec<fai_diagnostics::Diagnostic> {
    resolve::accumulated::<Diag>(db, file).into_iter().map(|d| d.0.clone()).collect()
}

fn codes(diags: &[fai_diagnostics::Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_str()).collect()
}

#[test]
fn pairs_signature_with_binding() {
    let (db, files) = db_with(&[("M.fai", "module M\n\npublic f : Int -> Int\nlet f x = x\n")]);
    let defs = module_defs(&db, files[0]);
    assert_eq!(defs.defs.len(), 1);
    let d = &defs.defs[0];
    assert_eq!(d.name.as_str(), "f");
    assert_eq!(d.visibility, Visibility::Public);
    assert!(d.signature.is_some());
}

#[test]
fn private_binding_without_signature_is_ok() {
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet x = 3\n")]);
    let defs = module_defs(&db, files[0]);
    assert_eq!(defs.defs.len(), 1);
    assert_eq!(defs.defs[0].visibility, Visibility::Private);
    assert!(defs.defs[0].signature.is_none());
    assert!(resolve_diags(&db, files[0]).is_empty());
}

#[test]
fn orphan_signature_is_an_error() {
    let (db, files) = db_with(&[("M.fai", "module M\n\npublic f : Int\n")]);
    let _ = module_defs(&db, files[0]);
    let diags = module_defs::accumulated::<Diag>(&db, files[0]);
    let cs: Vec<&str> = diags.iter().map(|d| d.0.code.as_str()).collect();
    assert!(cs.contains(&"FAI2005"), "expected orphan-signature, got {cs:?}");
}

#[test]
fn module_interface_excludes_private() {
    let (db, files) =
        db_with(&[("M.fai", "module M\n\npublic f : Int -> Int\nlet f x = x\n\nlet g = 3\n")]);
    let iface = module_interface(&db, files[0]);
    assert_eq!(iface.exports.len(), 1);
    assert_eq!(iface.exports[0].name.as_str(), "f");
}

#[test]
fn unbound_name_reported() {
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet f = nope\n")]);
    let diags = resolve_diags(&db, files[0]);
    assert!(codes(&diags).contains(&"FAI2001"), "got {:?}", codes(&diags));
}

#[test]
fn local_params_resolve() {
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet f x = x\n")]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "unexpected: {:?}", codes(&diags));
}

#[test]
fn qualified_reference_resolves_public() {
    let (db, files) = db_with(&[
        ("A.fai", "module A\n\npublic g : Int -> Int\nlet g x = x\n"),
        ("B.fai", "module B\n\nlet h = A.g 1\n"),
    ]);
    let diags = resolve_diags(&db, files[1]);
    assert!(diags.is_empty(), "unexpected: {:?}", codes(&diags));
    let resolved = resolve(&db, files[1]);
    let want = DefId::new(files[0].source(&db), Symbol::intern("g"));
    assert!(resolved.deps.contains(&want), "B should depend on A.g");
}

#[test]
fn qualified_reference_to_private_errors() {
    let (db, files) = db_with(&[
        ("A.fai", "module A\n\nlet g x = x\n"),
        ("B.fai", "module B\n\nlet h = A.g 1\n"),
    ]);
    let diags = resolve_diags(&db, files[1]);
    assert!(codes(&diags).contains(&"FAI2003"), "got {:?}", codes(&diags));
}

#[test]
fn qualified_reference_to_unknown_module_errors() {
    let (db, files) = db_with(&[("B.fai", "module B\n\nlet h = Zzz.g 1\n")]);
    let diags = resolve_diags(&db, files[0]);
    assert!(codes(&diags).contains(&"FAI2008"), "got {:?}", codes(&diags));
}

#[test]
fn duplicate_module_name_errors_on_each_file() {
    let (db, files) =
        db_with(&[("A.fai", "module Dup\n\nlet a = 1\n"), ("B.fai", "module Dup\n\nlet b = 2\n")]);
    let a = resolve_diags(&db, files[0]);
    let b = resolve_diags(&db, files[1]);
    assert!(codes(&a).contains(&"FAI2007"), "file A: {:?}", codes(&a));
    assert!(codes(&b).contains(&"FAI2007"), "file B: {:?}", codes(&b));
}

#[test]
fn duplicate_definition_errors() {
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet f = 1\nlet f = 2\n")]);
    let _ = module_defs(&db, files[0]);
    let diags = module_defs::accumulated::<Diag>(&db, files[0]);
    let cs: Vec<&str> = diags.iter().map(|d| d.0.code.as_str()).collect();
    assert!(cs.contains(&"FAI2004"), "got {cs:?}");
}

#[test]
fn mutually_recursive_sigless_defs_share_one_scc() {
    let (db, files) =
        db_with(&[("M.fai", "module M\n\nlet isEven n = isOdd n\nlet isOdd n = isEven n\n")]);
    let sccs = module_sccs(&db, files[0]);
    // One SCC containing both isEven and isOdd.
    let big = sccs.sccs.iter().find(|s| s.members.len() == 2);
    assert!(big.is_some(), "expected a 2-member SCC, got {:?}", sccs.sccs);
}

#[test]
fn signatured_def_is_singleton_scc() {
    let (db, files) =
        db_with(&[("M.fai", "module M\n\npublic f : Int -> Int\nlet f x = g x\nlet g y = y\n")]);
    let sccs = module_sccs(&db, files[0]);
    // f has a signature => its own singleton; g is sig-less singleton too.
    assert!(sccs.sccs.iter().all(|s| s.members.len() == 1), "got {:?}", sccs.sccs);
}

#[test]
fn shadowing_prelude_warns() {
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet length x = x\n")]);
    let diags = resolve_diags(&db, files[0]);
    let warn = diags.iter().find(|d| d.code.as_str() == "FAI2010");
    assert!(warn.is_some(), "expected shadow warning, got {:?}", codes(&diags));
    assert_eq!(warn.unwrap().severity, Severity::Warning);
}

#[test]
fn resolves_to_local_over_def() {
    // A parameter named like a top-level def resolves to the local.
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet g = 1\nlet f g = g\n")]);
    let resolved = resolve(&db, files[0]);
    // The `g` in `f`'s body is the parameter (Local), not the top-level def.
    let has_local = resolved.by_expr.values().any(|r| matches!(r, Res::Local(_)));
    assert!(has_local, "expected a local resolution");
}
