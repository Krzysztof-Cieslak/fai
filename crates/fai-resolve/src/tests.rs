//! Unit tests for resolution: pairing, visibility, scope, qualified references,
//! duplicate modules, and SCCs.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::Severity;
use fai_syntax::Symbol;
use fai_syntax::ast::Visibility;
use indoc::indoc;

use crate::ids::{DefId, Res};
use crate::{module_defs, module_interface, module_sccs, recursive_defs, resolve};

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
fn recursive_defs_finds_signatured_self_recursion() {
    // The case `module_sccs` misses: a self-recursive binding *with a signature*
    // (its edge would be cut, and its self-edge dropped, by the inference SCCs).
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public fac : Int -> Int
            let fac n = if n <= 0 then 1 else n * fac (n - 1)

            public helper : Int -> Int
            let helper x = x + 1
        "#},
    )]);
    let rec = recursive_defs(&db, files[0]);
    let src = files[0].source(&db);
    assert!(rec.contains(&DefId::new(src, Symbol::intern("fac"))), "fac is self-recursive");
    assert!(!rec.contains(&DefId::new(src, Symbol::intern("helper"))), "helper is not recursive");
}

#[test]
fn recursive_defs_finds_mutual_recursion() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public isEven : Int -> Bool
            let isEven n = if n <= 0 then true else isOdd (n - 1)

            public isOdd : Int -> Bool
            let isOdd n = if n <= 0 then false else isEven (n - 1)
        "#},
    )]);
    let rec = recursive_defs(&db, files[0]);
    let src = files[0].source(&db);
    assert!(rec.contains(&DefId::new(src, Symbol::intern("isEven"))));
    assert!(rec.contains(&DefId::new(src, Symbol::intern("isOdd"))));
}

#[test]
fn recursive_defs_excludes_a_helper_chain() {
    // A non-recursive call chain (`top` -> `mid` -> `leaf`) is not recursive, even
    // though every binding has a signature.
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public top : Int -> Int
            let top x = mid x

            mid : Int -> Int
            let mid x = leaf x

            leaf : Int -> Int
            let leaf x = x + 1
        "#},
    )]);
    let rec = recursive_defs(&db, files[0]);
    assert!(rec.is_empty(), "an acyclic chain has no recursive defs, got {rec:?}");
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
fn public_interface_method_exposing_private_type_is_a_leak() {
    let src = indoc! {r#"
        module M

        type Secret = Int

        public interface Store =
          get : Unit -> Secret
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert_eq!(codes(&diags), vec!["FAI2015"], "got {:?}", codes(&diags));
    assert_eq!(primary_text(src, &diags[0]), "Secret");
}

#[test]
fn instance_method_body_resolves_without_sibling_methods() {
    // `a` calls `b` by a bare name — methods are record fields, not siblings, so
    // the bare `b` is unbound (not the sibling method).
    let src = indoc! {r#"
        module M

        interface Two =
          a : Unit -> Int
          b : Unit -> Int

        let bad = { Two with a u = b u, b u = 1 }
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert!(codes(&diags).contains(&"FAI2001"), "expected unbound `b`, got {:?}", codes(&diags));
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

#[test]
fn nested_module_members_are_qualified_and_resolve() {
    let src = indoc! {r#"
        module M

        module Inner =
          let pi = 3
          let square x = x * x

        let area r = Inner.pi * Inner.square r
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let defs = module_defs(&db, files[0]);
    let names: Vec<&str> = defs.defs.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"Inner.pi"), "got {names:?}");
    assert!(names.contains(&"Inner.square"), "got {names:?}");
    assert!(names.contains(&"area"), "got {names:?}");
    assert!(defs.is_module(Symbol::intern("Inner")));
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "got {:?}", codes(&diags));
}

#[test]
fn nested_body_sees_enclosing_names_lexically() {
    // A nested binding may use an enclosing module's binding by its bare name.
    let src = indoc! {r#"
        module M

        let base = 10

        module Inner =
          let scaled = base + 1
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let resolved = resolve(&db, files[0]);
    // `Inner.scaled` depends on the top-level `base`.
    let inner = DefId::new(files[0].source(&db), Symbol::intern("Inner.scaled"));
    let base = DefId::new(files[0].source(&db), Symbol::intern("base"));
    assert!(resolved.deps_of(inner).contains(&base), "Inner.scaled should reference base");
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "got {:?}", codes(&diags));
}

#[test]
fn top_level_cannot_see_nested_member_by_bare_name() {
    let src = indoc! {r#"
        module M

        module Inner =
          let helper = 1

        let bad = helper
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    let cs = codes(&diags);
    assert!(cs.contains(&"FAI2001"), "bare `helper` at top level should be unbound, got {cs:?}");
}

#[test]
fn cross_file_sees_only_public_nested_members() {
    let outer = indoc! {r#"
        module Outer

        module Inner =
          public exposed : Int
          let exposed = 1

          let hidden = 2
    "#};
    let user_ok = indoc! {r#"
        module User

        let use = Outer.Inner.exposed
    "#};
    let user_bad = indoc! {r#"
        module Bad

        let use = Outer.Inner.hidden
    "#};
    let (db, files) =
        db_with(&[("Outer.fai", outer), ("User.fai", user_ok), ("Bad.fai", user_bad)]);
    let ok = resolve_diags(&db, files[1]);
    assert!(ok.is_empty(), "public nested member should resolve: {:?}", codes(&ok));
    let bad_diags = resolve_diags(&db, files[2]);
    let bad = codes(&bad_diags);
    assert!(bad.contains(&"FAI2003"), "private nested member should be FAI2003, got {bad:?}");
}

#[test]
fn module_name_conflict_is_reported() {
    let src = indoc! {r#"
        module M

        type Inner = Int

        module Inner =
          let x = 1
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    let cs = codes(&diags);
    assert!(cs.contains(&"FAI2016"), "module/type name clash should be FAI2016, got {cs:?}");
}

#[test]
fn module_used_as_value_is_reported() {
    let src = indoc! {r#"
        module M

        module Inner =
          let x = 1

        let bad = Inner
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    let cs = codes(&diags);
    assert!(cs.contains(&"FAI2017"), "a bare module name should be FAI2017, got {cs:?}");
}

#[test]
fn nested_module_shadows_a_cross_file_module() {
    // A same-file nested module named like a workspace file module wins.
    let list_file = indoc! {r#"
        module List

        public bad : Int
        let bad = 99
    "#};
    let user = indoc! {r#"
        module M

        module List =
          let good = 1

        let pick = List.good
    "#};
    let (db, files) = db_with(&[("List.fai", list_file), ("M.fai", user)]);
    // `List.good` resolves to the nested module's member (no diagnostics), not the
    // cross-file `List` (which has no `good`).
    let diags = resolve_diags(&db, files[1]);
    assert!(diags.is_empty(), "got {:?}", codes(&diags));
}

#[test]
fn opaque_type_exports_its_name_but_not_its_constructors() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public opaque type T =
              | MkT Int
        "#},
    )]);
    let iface = module_interface(&db, files[0]);
    assert!(iface.has_type(Symbol::intern("T")), "the opaque type name is exported");
    assert!(!iface.has_ctor(Symbol::intern("MkT")), "its constructor is not exported");
}

#[test]
fn same_file_opaque_constructor_resolves() {
    // Opacity is file-scoped: the declaring file builds and matches freely.
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public opaque type T =
              | MkT Int

            let make = MkT 1

            let read t =
              match t with
              | MkT n -> n
        "#},
    )]);
    let diags = resolve_diags(&db, files[0]);
    assert!(diags.is_empty(), "same-file constructor use is fine: {:?}", codes(&diags));
}

#[test]
fn cross_file_opaque_constructor_is_an_error() {
    let m = indoc! {r#"
        module M

        public opaque type T =
          | MkT Int
    "#};
    let user = indoc! {r#"
        module User

        let bad = M.MkT 1
    "#};
    let (db, files) = db_with(&[("M.fai", m), ("User.fai", user)]);
    let diags = resolve_diags(&db, files[1]);
    assert!(codes(&diags).contains(&"FAI2018"), "expected FAI2018, got {:?}", codes(&diags));
}

#[test]
fn cross_file_opaque_constructor_pattern_is_an_error() {
    let m = indoc! {r#"
        module M

        public opaque type T =
          | MkT Int
    "#};
    let user = indoc! {r#"
        module User

        let read t =
          match t with
          | M.MkT n -> n
    "#};
    let (db, files) = db_with(&[("M.fai", m), ("User.fai", user)]);
    let diags = resolve_diags(&db, files[1]);
    assert!(codes(&diags).contains(&"FAI2018"), "expected FAI2018, got {:?}", codes(&diags));
}

#[test]
fn opaque_constructor_field_does_not_leak_a_private_type() {
    // A non-opaque public type's constructor field would be an FAI2015 leak, but
    // an opaque type's fields are not cross-file-visible, so they cannot leak.
    let src = indoc! {r#"
        module M

        type Secret = Int

        public opaque type T =
          | MkT Secret
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert!(!codes(&diags).contains(&"FAI2015"), "opaque fields don't leak: {:?}", codes(&diags));
}

// --- `internal` visibility --------------------------------------------------
//
// `internal` exports a member across files only within the same *origin*. In
// these tests the origin boundary is the standard-library path prefix
// (`fai_db::STD_PATH_PREFIX`): a file registered under that prefix is std-origin,
// any other file is user-origin. Two std files are same-origin; a std file and a
// user file are cross-origin.

/// Builds a database whose files are all standard-library (synthetic `<std>/`)
/// origin, so they share an origin for the `internal` checks.
fn std_db_with(files: &[(&str, &str)]) -> (FaiDatabase, Vec<SourceFile>) {
    let prefixed: Vec<(String, &str)> = files
        .iter()
        .map(|(name, text)| (format!("{}{name}", fai_db::STD_PATH_PREFIX), *text))
        .collect();
    let refs: Vec<(&str, &str)> = prefixed.iter().map(|(p, t)| (p.as_str(), *t)).collect();
    db_with(&refs)
}

#[test]
fn internal_value_is_visible_to_a_same_origin_file() {
    let a = indoc! {r#"
        module A

        internal helper : Int -> Int
        let helper x = x
    "#};
    let b = indoc! {r#"
        module B

        public use : Int -> Int
        let use x = A.helper x
    "#};
    let (db, files) = std_db_with(&[("A.fai", a), ("B.fai", b)]);
    let diags = resolve_diags(&db, files[1]);
    assert!(diags.is_empty(), "same-origin internal use is fine: {:?}", codes(&diags));
}

#[test]
fn internal_value_is_hidden_from_another_origin() {
    let a = indoc! {r#"
        module A

        internal helper : Int -> Int
        let helper x = x
    "#};
    let user = indoc! {r#"
        module User

        let bad = A.helper 1
    "#};
    // `A` is std-origin; `User` is user-origin.
    let (mut db, _) = std_db_with(&[("A.fai", a)]);
    let user_id = db.add_source("User.fai".into(), user.to_owned());
    let user_file = db.source_file(user_id).unwrap();
    let diags = resolve_diags(&db, user_file);
    assert_eq!(codes(&diags), vec!["FAI2020"], "cross-origin internal is FAI2020");
    assert_eq!(primary_text(user, &diags[0]), "A.helper");
    assert!(diags[0].message.contains("internal"), "message: {}", diags[0].message);
    assert!(
        diags[0].message.contains("standard library"),
        "origin-accurate message: {}",
        diags[0].message
    );
}

#[test]
fn internal_value_is_visible_across_user_files() {
    // Within user code there is one origin, so `internal` reads like `public`.
    let a = indoc! {r#"
        module A

        internal helper : Int -> Int
        let helper x = x
    "#};
    let b = indoc! {r#"
        module B

        let use = A.helper 1
    "#};
    let (db, files) = db_with(&[("A.fai", a), ("B.fai", b)]);
    let diags = resolve_diags(&db, files[1]);
    assert!(diags.is_empty(), "same-origin (user) internal use is fine: {:?}", codes(&diags));
}

#[test]
fn internal_constructor_is_visible_to_a_same_origin_file() {
    let a = indoc! {r#"
        module A

        internal type T =
          | MkT Int
    "#};
    let b = indoc! {r#"
        module B

        let make = A.MkT 1
    "#};
    let (db, files) = std_db_with(&[("A.fai", a), ("B.fai", b)]);
    let diags = resolve_diags(&db, files[1]);
    assert!(diags.is_empty(), "same-origin internal ctor use is fine: {:?}", codes(&diags));
}

#[test]
fn internal_constructor_is_hidden_from_another_origin() {
    let a = indoc! {r#"
        module A

        internal type T =
          | MkT Int
    "#};
    let user = indoc! {r#"
        module User

        let bad = A.MkT 1
    "#};
    let (mut db, _) = std_db_with(&[("A.fai", a)]);
    let user_id = db.add_source("User.fai".into(), user.to_owned());
    let user_file = db.source_file(user_id).unwrap();
    let diags = resolve_diags(&db, user_file);
    assert_eq!(codes(&diags), vec!["FAI2020"], "cross-origin internal ctor is FAI2020");
}

#[test]
fn internal_opaque_constructor_is_opaque_even_same_origin() {
    // `internal opaque` hides its representation across files (even same-origin),
    // so a sibling sees FAI2018 — opacity applies within the origin.
    let a = indoc! {r#"
        module A

        internal opaque type T =
          | MkT Int
    "#};
    let b = indoc! {r#"
        module B

        let make = A.MkT 1
    "#};
    let (db, files) = std_db_with(&[("A.fai", a), ("B.fai", b)]);
    let diags = resolve_diags(&db, files[1]);
    assert_eq!(codes(&diags), vec!["FAI2018"], "same-origin internal-opaque ctor is opaque");
}

#[test]
fn internal_opaque_constructor_is_internal_first_cross_origin() {
    // Across origins the name is hidden before opacity matters: FAI2020, not FAI2018.
    let a = indoc! {r#"
        module A

        internal opaque type T =
          | MkT Int
    "#};
    let user = indoc! {r#"
        module User

        let bad = A.MkT 1
    "#};
    let (mut db, _) = std_db_with(&[("A.fai", a)]);
    let user_id = db.add_source("User.fai".into(), user.to_owned());
    let user_file = db.source_file(user_id).unwrap();
    let diags = resolve_diags(&db, user_file);
    assert_eq!(codes(&diags), vec!["FAI2020"], "origin is checked before opacity");
}

#[test]
fn public_signature_exposing_an_internal_type_is_a_leak() {
    let src = indoc! {r#"
        module M

        internal type Secret = Int

        public f : Secret -> Int
        let f x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert_eq!(codes(&diags), vec!["FAI2015"], "public exposing internal leaks");
    assert_eq!(primary_text(src, &diags[0]), "Secret");
    assert!(diags[0].message.contains("internal"), "message: {}", diags[0].message);
}

#[test]
fn internal_signature_exposing_a_private_type_is_a_leak() {
    let src = indoc! {r#"
        module M

        type Secret = Int

        internal f : Secret -> Int
        let f x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert_eq!(codes(&diags), vec!["FAI2015"], "internal exposing private leaks");
    assert!(diags[0].message.contains("private"), "message: {}", diags[0].message);
}

#[test]
fn internal_signature_exposing_a_public_or_internal_type_is_clean() {
    let src = indoc! {r#"
        module M

        public type Pub = Int

        internal type Inter = Int

        internal f : Pub -> Inter
        let f x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let diags = resolve_diags(&db, files[0]);
    assert!(
        !codes(&diags).contains(&"FAI2015"),
        "internal may name public/internal: {:?}",
        codes(&diags)
    );
}

#[test]
fn public_signature_exposing_a_cross_module_internal_type_is_a_leak() {
    // A public surface naming a same-origin `internal` type from *another* file
    // still leaks it across the origin boundary.
    let a = indoc! {r#"
        module A

        internal type Secret = Int
    "#};
    let b = indoc! {r#"
        module B

        public f : A.Secret -> Int
        let f x = x
    "#};
    let (db, files) = std_db_with(&[("A.fai", a), ("B.fai", b)]);
    let diags = resolve_diags(&db, files[1]);
    assert_eq!(codes(&diags), vec!["FAI2015"], "public exposing cross-module internal leaks");
    assert_eq!(primary_text(b, &diags[0]), "A.Secret");
}

#[test]
fn internal_marker_on_a_binding_with_a_signature_errors() {
    let src = indoc! {r#"
        module M

        public f : Int -> Int
        internal let f x = x
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let _ = module_defs(&db, files[0]);
    let diags = module_defs::accumulated::<Diag>(&db, files[0]);
    let cs: Vec<&str> = diags.iter().map(|d| d.0.code.as_str()).collect();
    assert!(cs.contains(&"FAI2009"), "expected FAI2009, got {cs:?}");
}

#[test]
fn internal_foreign_is_rejected() {
    let src = indoc! {r#"
        module M

        internal foreign "sym" f : Int -> Int
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let _ = module_defs(&db, files[0]);
    let diags = module_defs::accumulated::<Diag>(&db, files[0]);
    let cs: Vec<&str> = diags.iter().map(|d| d.0.code.as_str()).collect();
    assert!(cs.contains(&"FAI2019"), "expected FAI2019 for internal foreign, got {cs:?}");
}
