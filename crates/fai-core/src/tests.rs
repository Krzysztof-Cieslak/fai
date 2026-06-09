//! Lowering tests: surface programs to compact Core renderings.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_syntax::Symbol;
use indoc::indoc;

use crate::{core, pretty_def};

fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn lower(src: &str, name: &str) -> String {
    let (db, file) = db_with(src);
    pretty_def(&core(&db, file, Symbol::intern(name)))
}

fn codes(src: &str, name: &str) -> Vec<String> {
    let (db, file) = db_with(src);
    core::accumulated::<Diag>(&db, file, Symbol::intern(name))
        .into_iter()
        .map(|d| d.0.code.as_str().to_owned())
        .collect()
}

#[test]
fn lowers_arithmetic() {
    let src = indoc! {r#"
        module M

        public add : Int -> Int -> Int
        let add x y = x + y
    "#};
    let got = lower(src, "add");
    assert_eq!(got, "fn0(%0, %1) = (+ %0 %1)\n");
}

#[test]
fn lowers_if_and_negation() {
    let src = indoc! {r#"
        module M

        let f n = if n < 0 then 0 - n else n
    "#};
    let got = lower(src, "f");
    assert_eq!(got, "fn0(%0) = (if (< %0 0) (- 0 %0) %0)\n");
}

// `++`, `|>`, and `>>` are ordinary `Prelude` operator functions now, so their
// lowering is plain application of a global (covered by the application tests and
// the standard-library/e2e suites) rather than a dedicated Core form.

#[test]
fn lowers_console_capability_access() {
    // Console output goes through the `Runtime` record: `runtime.console` is a
    // constant-offset projection (`console` is field 1 of the sorted
    // `{clock, console, random}`), then `.writeLine` projects method 0 of the
    // `Console` dictionary, applied to the string.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine "Hi"
    "#};
    assert_eq!(lower(src, "main"), "fn0(%0) = (app (field 0 (field 1 %0)) \"Hi\")\n");
}

#[test]
fn lowers_let_block() {
    let src = indoc! {r#"
        module M

        let f a =
          let b = a + 1
          b + b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %1 = (+ %0 1); (+ %1 %1))\n");
}

#[test]
fn lowers_partial_application_as_general_app() {
    let src = indoc! {r#"
        module M

        let add x y = x + y

        let inc = add 1
    "#};
    assert_eq!(lower(src, "inc"), "fn0() = (app @add 1)\n");
}

#[test]
fn lowers_lambda_with_capture() {
    let src = indoc! {r#"
        module M

        let adder x = fun y -> x + y
    "#};
    let got = lower(src, "adder");
    assert_eq!(got, "fn0(%0) = (closure fn1 [%0])\nfn1(%1) [caps %0] = (+ %0 %1)\n");
}

#[test]
fn lowers_not_equal_to_not_of_eq() {
    let src = indoc! {r#"
        module M

        let f a b = a <> b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (not (= %0 %1))\n");
}

#[test]
fn lowers_short_circuit_booleans() {
    let src = indoc! {r#"
        module M

        let f a b = a && b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (if %0 %1 false)\n");
}

#[test]
fn references_prelude_helper_as_global() {
    let src = indoc! {r#"
        module M

        let f a b = compare a b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (app @compare %0 %1)\n");
}

#[test]
fn float_lowers_to_a_boxed_literal() {
    let src = indoc! {r#"
        module M

        let x = 3.0
    "#};
    assert_eq!(lower(src, "x"), "fn0() = 3\n");
    assert!(codes(src, "x").is_empty());
}

#[test]
fn tuples_lower_to_data() {
    let src = indoc! {r#"
        module M

        let pair a b = (a, b)
    "#};
    assert_eq!(lower(src, "pair"), "fn0(%0, %1) = (data 0 %0 %1)\n");
    assert!(codes(src, "pair").is_empty());
}

#[test]
fn integer_literals_are_decoded() {
    let src = indoc! {r#"
        module M

        let x = 0xFF + 1_000
    "#};
    assert_eq!(lower(src, "x"), "fn0() = (+ 255 1000)\n");
}

#[test]
fn char_literal_lowers_to_an_immediate() {
    let src = indoc! {r#"
        module M

        let c = 'a'
    "#};
    assert_eq!(lower(src, "c"), "fn0() = 'a'\n");
    assert!(codes(src, "c").is_empty());
}

#[test]
fn char_pattern_lowers_to_an_equality_test() {
    let src = indoc! {r#"
        module M

        let f c =
          match c with
          | 'a' -> 1
          | _ -> 0
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %2 = %0; (if (= %2 'a') 1 0))\n");
    assert!(codes(src, "f").is_empty());
}

#[test]
fn list_literal_lowers_to_cons_and_nil() {
    let src = indoc! {r#"
        module M

        let xs = [1, 2, 3]
    "#};
    assert_eq!(lower(src, "xs"), "fn0() = (data 1 1 (data 1 2 (data 1 3 (data 0))))\n");
    assert!(codes(src, "xs").is_empty());
}

#[test]
fn cons_lowers_to_data() {
    let src = indoc! {r#"
        module M

        let f x = x :: []
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (data 1 %0 (data 0))\n");
    assert!(codes(src, "f").is_empty());
}

#[test]
fn float_in_argument_position_lowers() {
    let src = indoc! {r#"
        module M

        let f = Float.toString 3.0
    "#};
    assert!(codes(src, "f").is_empty());
}

#[test]
fn list_prelude_helper_lowers_to_a_global() {
    // `List.length` is an ordinary standard-library definition, reached as a global.
    let src = indoc! {r#"
        module M

        let n = List.length [1]
    "#};
    assert!(codes(src, "n").is_empty());
}

#[test]
fn tuple_let_binding_destructures() {
    let src = indoc! {r#"
        module M

        let f p =
          let (x, y) = p
          x
    "#};
    assert!(codes(src, "f").is_empty());
}

#[test]
fn monomorphic_record_access_lowers_to_a_projection() {
    let src = indoc! {r#"
        module M

        public dist : { x : Int, y : Int } -> Int
        let dist v = v.x + v.y
    "#};
    // `x` is field 0, `y` is field 1 in canonical (sorted) order.
    assert_eq!(lower(src, "dist"), "fn0(%0) = (+ (field 0 %0) (field 1 %0))\n");
    assert!(codes(src, "dist").is_empty());
}

#[test]
fn row_polymorphic_field_access_uses_offset_evidence() {
    // A field access through a row variable cannot pick a constant slot; the
    // function takes a leading offset-evidence parameter (here `%1`) and the slot
    // is `base + evidence`.
    let src = indoc! {r#"
        module M

        let getX r = r.x
    "#};
    assert!(codes(src, "getX").is_empty(), "got {:?}", codes(src, "getX"));
    assert_eq!(lower(src, "getX"), "fn0(%1, %0) = (field 0+%1 %0)\n");
}

#[test]
fn record_literal_lowers_to_sorted_data() {
    // Fields are stored in canonical (sorted-by-label) order, so `x` precedes
    // `y` regardless of how the literal is written.
    let src = indoc! {r#"
        module M

        let p = { y = 2, x = 1 }
    "#};
    assert_eq!(lower(src, "p"), "fn0() = (data 0 1 2)\n");
    assert!(codes(src, "p").is_empty());
}

#[test]
fn nullary_constructor_lowers_to_tagged_data() {
    let src = indoc! {r#"
        module M

        type T =
          | A
          | B Int

        let mkA = A
    "#};
    assert_eq!(lower(src, "mkA"), "fn0() = (data 0)\n");
    assert!(codes(src, "mkA").is_empty());
}

#[test]
fn constructor_with_field_lowers_to_tagged_data() {
    // `B` is the second constructor, so it carries tag 1 and its field.
    let src = indoc! {r#"
        module M

        type T =
          | A
          | B Int

        let mkB n = B n
    "#};
    assert_eq!(lower(src, "mkB"), "fn0(%0) = (data 1 %0)\n");
    assert!(codes(src, "mkB").is_empty());
}

#[test]
fn match_lowers_to_tag_tests_and_field_projections() {
    // The scrutinee is bound once, then a chain of tag tests selects an arm and
    // projects the matched constructor's field; the impossible final fallthrough
    // of this exhaustive match is an unreachable `<error>` leaf.
    let src = indoc! {r#"
        module M

        type T =
          | A Int
          | B Int

        let f t =
          match t with
          | A x -> x
          | B y -> y
    "#};
    assert_eq!(
        lower(src, "f"),
        "fn0(%0) = (let %3 = %0; (if (= (tag %3) 0) (let %1 = (field 0 %3); %1) (if (= (tag %3) 1) (let %2 = (field 0 %3); %2) <error>)))\n"
    );
    assert!(codes(src, "f").is_empty());
}

#[test]
fn float_comparison_selects_the_float_primitive() {
    let src = indoc! {r#"
        module M

        let lt = 1.0 < 2.0
    "#};
    assert_eq!(lower(src, "lt"), "fn0() = (<. 1 2)\n");
    assert!(codes(src, "lt").is_empty());
}

#[test]
fn structural_compare_lowers_to_the_compare_primitive() {
    // `<` on a non-numeric type (a tuple here) becomes the structural `compare`.
    let src = indoc! {r#"
        module M

        let before a b = (a, 1) < (b, 2)
    "#};
    assert!(codes(src, "before").is_empty());
    assert!(lower(src, "before").contains("compare"), "got {}", lower(src, "before"));
}

/// The lowered entry's arity matches the binding's parameter count, and every
/// global the body references resolves to a real binding somewhere.
#[track_caller]
fn assert_lowering_invariants(src: &str, name: &str) {
    use fai_syntax::ast::ItemKind;

    let (db, file) = db_with(src);
    let lowered = core(&db, file, fai_syntax::Symbol::intern(name));

    let params = fai_syntax::parse(&db, file)
        .module
        .items
        .iter()
        .find_map(|it| match &it.kind {
            ItemKind::Binding { name: n, params, .. } if n.as_str() == name => Some(params.len()),
            _ => None,
        })
        .unwrap();
    assert_eq!(lowered.entry().params.len(), params, "{name}: entry arity");

    for def in lowered.referenced_globals() {
        let target = db.source_file(def.file).expect("global's file is registered");
        assert!(
            fai_resolve::module_defs(&db, target).get(def.name).is_some(),
            "{name}: dangling global {}",
            def.name
        );
    }
}

#[test]
fn lowering_invariants_simple_binding() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let add x y = x + y
        "#},
        "add",
    );
}

#[test]
fn lowering_invariants_composition() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let twice f = f >> f
        "#},
        "twice",
    );
}

#[test]
fn lowering_invariants_returns_closure() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let adder x = fun y -> x + y
        "#},
        "adder",
    );
}

#[test]
fn lowering_invariants_calls_helper_and_capability() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let helper x = x + 1

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (helper 1))
        "#},
        "main",
    );
}
