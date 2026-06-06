//! Unit tests for resolution: pairing, visibility, scope, qualified references,
//! duplicate modules, and SCCs.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::Severity;
use fai_syntax::Symbol;
use fai_syntax::ast::Visibility;
use indoc::indoc;

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
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public f : Int -> Int
            let f x = x
        "#},
    )]);
    let defs = module_defs(&db, files[0]);
    assert_eq!(defs.defs.len(), 1);
    let d = &defs.defs[0];
    assert_eq!(d.name.as_str(), "f");
    assert_eq!(d.visibility, Visibility::Public);
    assert!(d.signature.is_some());
}

#[test]
fn private_binding_without_signature_is_ok() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let x = 3
        "#},
    )]);
    let defs = module_defs(&db, files[0]);
    assert_eq!(defs.defs.len(), 1);
    assert_eq!(defs.defs[0].visibility, Visibility::Private);
    assert!(defs.defs[0].signature.is_none());
    assert!(resolve_diags(&db, files[0]).is_empty());
}

#[test]
fn orphan_signature_is_an_error() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public f : Int
        "#},
    )]);
    let _ = module_defs(&db, files[0]);
    let diags = module_defs::accumulated::<Diag>(&db, files[0]);
    let cs: Vec<&str> = diags.iter().map(|d| d.0.code.as_str()).collect();
    assert!(cs.contains(&"FAI2005"), "expected orphan-signature, got {cs:?}");
}

#[test]
fn module_interface_excludes_private() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public f : Int -> Int
            let f x = x

            let g = 3
        "#},
    )]);
    let iface = module_interface(&db, files[0]);
    assert_eq!(iface.exports.len(), 1);
    assert_eq!(iface.exports[0].name.as_str(), "f");
}

#[test]
fn unbound_name_reported() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let f = nope
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(codes(&diags).contains(&"FAI2001"), "got {:?}", codes(&diags));
}

#[test]
fn local_params_resolve() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let f x = x
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "unexpected: {:?}", codes(&diags));
}

#[test]
fn qualified_reference_resolves_public() {
    let (db, files) = db_with(&[
        (
            "A.fai",
            indoc! {r#"
                module A

                public g : Int -> Int
                let g x = x
            "#},
        ),
        (
            "B.fai",
            indoc! {r#"
                module B

                let h = A.g 1
            "#},
        ),
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
        (
            "A.fai",
            indoc! {r#"
                module A

                let g x = x
            "#},
        ),
        (
            "B.fai",
            indoc! {r#"
                module B

                let h = A.g 1
            "#},
        ),
    ]);
    let diags = resolve_diags(&db, files[1]);
    assert!(codes(&diags).contains(&"FAI2003"), "got {:?}", codes(&diags));
}

#[test]
fn qualified_reference_to_unknown_module_errors() {
    let (db, files) = db_with(&[(
        "B.fai",
        indoc! {r#"
            module B

            let h = Zzz.g 1
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(codes(&diags).contains(&"FAI2008"), "got {:?}", codes(&diags));
}

#[test]
fn duplicate_module_name_errors_on_each_file() {
    let (db, files) = db_with(&[
        (
            "A.fai",
            indoc! {r#"
                module Dup

                let a = 1
            "#},
        ),
        (
            "B.fai",
            indoc! {r#"
                module Dup

                let b = 2
            "#},
        ),
    ]);
    let a = resolve_diags(&db, files[0]);
    let b = resolve_diags(&db, files[1]);
    assert!(codes(&a).contains(&"FAI2007"), "file A: {:?}", codes(&a));
    assert!(codes(&b).contains(&"FAI2007"), "file B: {:?}", codes(&b));
}

#[test]
fn duplicate_definition_errors() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let f = 1
            let f = 2
        "#},
    )]);
    let _ = module_defs(&db, files[0]);
    let diags = module_defs::accumulated::<Diag>(&db, files[0]);
    let cs: Vec<&str> = diags.iter().map(|d| d.0.code.as_str()).collect();
    assert!(cs.contains(&"FAI2004"), "got {cs:?}");
}

#[test]
fn mutually_recursive_sigless_defs_share_one_scc() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let isEven n = isOdd n
            let isOdd n = isEven n
        "#},
    )]);
    let sccs = module_sccs(&db, files[0]);
    // One SCC containing both isEven and isOdd.
    let big = sccs.sccs.iter().find(|s| s.members.len() == 2);
    assert!(big.is_some(), "expected a 2-member SCC, got {:?}", sccs.sccs);
}

#[test]
fn signatured_def_is_singleton_scc() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public f : Int -> Int
            let f x = g x
            let g y = y
        "#},
    )]);
    let sccs = module_sccs(&db, files[0]);
    // f has a signature => its own singleton; g is sig-less singleton too.
    assert!(sccs.sccs.iter().all(|s| s.members.len() == 1), "got {:?}", sccs.sccs);
}

#[test]
fn shadowing_prelude_warns() {
    // Shadow an auto-imported name: the warning needs the Prelude module present
    // (as a standard-library file) so its exports are known.
    let (db, files) = db_with(&[
        (
            "<std>/Prelude.fai",
            indoc! {r#"
                module Prelude

                public not : Bool -> Bool
                let not b = b
            "#},
        ),
        (
            "M.fai",
            indoc! {r#"
                module M

                let not x = x
            "#},
        ),
    ]);
    let diags = resolve_diags(&db, files[1]);
    let warn = diags.iter().find(|d| d.code.as_str() == "FAI2010");
    assert!(warn.is_some(), "expected shadow warning, got {:?}", codes(&diags));
    assert_eq!(warn.unwrap().severity, Severity::Warning);
}

#[test]
fn standard_library_module_may_use_prim() {
    let (db, files) = db_with(&[(
        "<std>/Bool.fai",
        indoc! {r#"
            module Bool

            public neg : Bool -> Bool
            let neg b = Prim.not b
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "a std module may use Prim, got {:?}", codes(&diags));
}

#[test]
fn prim_outside_standard_library_is_rejected() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let neg b = Prim.not b
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(
        diags.iter().any(|d| d.code.as_str() == "FAI2014"),
        "expected FAI2014, got {:?}",
        codes(&diags)
    );
}

#[test]
fn prim_unknown_intrinsic_is_unbound() {
    let (db, files) = db_with(&[(
        "<std>/M.fai",
        indoc! {r#"
            module M

            public f : Int -> Int
            let f x = Prim.nope x
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.iter().any(|d| d.code.as_str() == "FAI2001"), "got {:?}", codes(&diags));
}

#[test]
fn duplicate_auto_imported_export_is_detected() {
    // Two auto-imported modules exporting the same name are recorded as a
    // duplicate by the merge (FAI2013 is emitted per offending file from there).
    let (db, files) = db_with(&[
        (
            "<std>/A.fai",
            indoc! {r#"
                module A

                public dup : Int
                let dup = 1
            "#},
        ),
        (
            "<std>/B.fai",
            indoc! {r#"
                module B

                public dup : Int
                let dup = 2
            "#},
        ),
    ]);
    let exports = crate::merge_auto_imports(&db, &files);
    assert!(
        exports.duplicates.iter().any(|d| d.name.as_str() == "dup"),
        "expected `dup` recorded as a duplicate export"
    );
}

#[test]
fn resolves_to_local_over_def() {
    // A parameter named like a top-level def resolves to the local.
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let g = 1
            let f g = g
        "#},
    )]);
    let resolved = resolve(&db, files[0]);
    // The `g` in `f`'s body is the parameter (Local), not the top-level def.
    let has_local = resolved.by_expr.values().any(|r| matches!(r, Res::Local(_)));
    assert!(has_local, "expected a local resolution");
}

// ── Privacy-leak check (FAI2015) ─────────────────────────────────────────────

/// The source slice a diagnostic's primary span covers (for exact-span asserts).
fn primary_text<'a>(src: &'a str, diag: &fai_diagnostics::Diagnostic) -> &'a str {
    let range = diag.primary.range();
    &src[range.start().to_usize()..range.end().to_usize()]
}

#[test]
fn public_signature_exposing_private_type_is_a_leak() {
    let src = indoc! {r#"
        module M

        type Secret = Int

        public f : Secret -> Int
        let f x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert_eq!(codes(&diags), vec!["FAI2015"], "expected one privacy leak");
    assert!(diags[0].message.contains("Secret"), "message: {}", diags[0].message);
    // The span points precisely at the leaked type reference.
    assert_eq!(primary_text(src, &diags[0]), "Secret");
}

#[test]
fn public_alias_body_exposing_private_type_is_a_leak() {
    let src = indoc! {r#"
        module M

        type Inner = Int

        public type Outer = { value : Inner }
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert_eq!(codes(&diags), vec!["FAI2015"]);
    assert_eq!(primary_text(src, &diags[0]), "Inner");
}

#[test]
fn public_constructor_field_exposing_private_type_is_a_leak() {
    let src = indoc! {r#"
        module M

        type Inner = Int

        public type Wrap =
          | Wrap Inner
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert_eq!(codes(&diags), vec!["FAI2015"]);
    assert_eq!(primary_text(src, &diags[0]), "Inner");
}

#[test]
fn private_surface_referencing_private_type_is_clean() {
    // A private signature may freely name a private type — nothing is exposed.
    let src = indoc! {r#"
        module M

        type Secret = Int

        f : Secret -> Int
        let f x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "got {:?}", codes(&diags));
}

#[test]
fn user_operator_resolves_and_unknown_operator_is_unbound() {
    // A user-defined operator resolves to its definition; a built-in operator is
    // a builtin; an undefined operator is unbound.
    let src = indoc! {r#"
        module M

        let (+++) a b = a

        let usesUser x = x +++ x
        let usesBuiltin x = x + x
        let usesUnknown x = x >?> x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let resolved = resolve(&db, files[0]);
    // The user operator is a definition reference (so it is a dependency).
    assert!(resolved.deps.iter().any(|d| d.name.as_str() == "+++"), "expected `+++` in deps");
    // The unknown operator `>?>` is reported unbound.
    let diags = resolve_diags(&db, files[0]);
    let cs = codes(&diags);
    assert!(cs.contains(&"FAI2001"), "expected unbound operator, got {cs:?}");
}

#[test]
fn public_signature_referencing_public_or_builtin_type_is_clean() {
    let src = indoc! {r#"
        module M

        public type Visible = Int

        public f : Visible -> Int
        let f x = x

        public g : Int -> Int
        let g x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "got {:?}", codes(&diags));
}
