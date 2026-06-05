//! Lowering tests: surface programs to compact Core renderings.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

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
    let got = lower("module M\n\npublic add : Int -> Int -> Int\nlet add x y = x + y\n", "add");
    assert_eq!(got, "fn0(%0, %1) = (+ %0 %1)\n");
}

#[test]
fn lowers_if_and_negation() {
    let got = lower("module M\n\nlet f n = if n < 0 then 0 - n else n\n", "f");
    assert_eq!(got, "fn0(%0) = (if (< %0 0) (- 0 %0) %0)\n");
}

#[test]
fn lowers_string_concat() {
    let got = lower("module M\n\nlet greet name = \"Hi \" ++ name\n", "greet");
    assert_eq!(got, "fn0(%0) = (++ \"Hi \" %0)\n");
}

#[test]
fn lowers_console_write_line() {
    let src = "module M\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime \"Hi\"\n";
    assert_eq!(lower(src, "main"), "fn0(%0) = (writeLine %0 \"Hi\")\n");
}

#[test]
fn lowers_let_block() {
    let src = "module M\n\nlet f a =\n  let b = a + 1\n  b + b\n";
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %1 = (+ %0 1); (+ %1 %1))\n");
}

#[test]
fn lowers_partial_application_as_general_app() {
    let src = "module M\n\nlet add x y = x + y\n\nlet inc = add 1\n";
    assert_eq!(lower(src, "inc"), "fn0() = (app @add 1)\n");
}

#[test]
fn lowers_pipe_to_application() {
    let src = "module M\n\nlet f x = x\n\nlet g n = n |> f\n";
    assert_eq!(lower(src, "g"), "fn0(%1) = (app @f %1)\n");
}

#[test]
fn lowers_pipe_into_global_wrapper() {
    // `Int.toString` is an ordinary standard-library wrapper, so the pipe lowers
    // to a call of the global (whose body is the intrinsic).
    let src = "module M\n\nlet describe n = n |> Int.toString\n";
    assert_eq!(lower(src, "describe"), "fn0(%0) = (app @toString %0)\n");
}

#[test]
fn lowers_compose_with_capture() {
    let src = "module M\n\npublic twice : ('a -> 'a) -> 'a -> 'a\nlet twice f = f >> f\n";
    let got = lower(src, "twice");
    assert_eq!(got, "fn0(%0) = (closure fn1 [%0])\nfn1(%1) [caps %0] = (app %0 (app %0 %1))\n");
}

#[test]
fn lowers_lambda_with_capture() {
    let src = "module M\n\nlet adder x = fun y -> x + y\n";
    let got = lower(src, "adder");
    assert_eq!(got, "fn0(%0) = (closure fn1 [%0])\nfn1(%1) [caps %0] = (+ %0 %1)\n");
}

#[test]
fn lowers_not_equal_to_not_of_eq() {
    let src = "module M\n\nlet f a b = a <> b\n";
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (not (= %0 %1))\n");
}

#[test]
fn lowers_short_circuit_booleans() {
    let src = "module M\n\nlet f a b = a && b\n";
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (if %0 %1 false)\n");
}

#[test]
fn references_prelude_helper_as_global() {
    let src = "module M\n\nlet f a b = compare a b\n";
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (app @compare %0 %1)\n");
}

#[test]
fn float_lowers_to_a_boxed_literal() {
    let src = "module M\n\nlet x = 3.0\n";
    assert_eq!(lower(src, "x"), "fn0() = 3\n");
    assert!(codes(src, "x").is_empty());
}

#[test]
fn tuples_lower_to_data() {
    let src = "module M\n\nlet pair a b = (a, b)\n";
    assert_eq!(lower(src, "pair"), "fn0(%0, %1) = (data 0 %0 %1)\n");
    assert!(codes(src, "pair").is_empty());
}

#[test]
fn integer_literals_are_decoded() {
    let src = "module M\n\nlet x = 0xFF + 1_000\n";
    assert_eq!(lower(src, "x"), "fn0() = (+ 255 1000)\n");
}

#[test]
fn char_literal_is_unsupported() {
    let src = "module M\n\nlet c = 'a'\n";
    assert!(codes(src, "c").contains(&"FAI7001".to_owned()));
}

#[test]
fn list_literal_lowers_to_cons_and_nil() {
    let src = "module M\n\nlet xs = [1, 2, 3]\n";
    assert_eq!(lower(src, "xs"), "fn0() = (data 1 1 (data 1 2 (data 1 3 (data 0))))\n");
    assert!(codes(src, "xs").is_empty());
}

#[test]
fn cons_lowers_to_data() {
    let src = "module M\n\nlet f x = x :: []\n";
    assert_eq!(lower(src, "f"), "fn0(%0) = (data 1 %0 (data 0))\n");
    assert!(codes(src, "f").is_empty());
}

#[test]
fn float_in_argument_position_lowers() {
    let src = "module M\n\nlet f = Float.toString 3.0\n";
    assert!(codes(src, "f").is_empty());
}

#[test]
fn list_prelude_helper_lowers_to_a_global() {
    // `List.length` is an ordinary standard-library definition, reached as a global.
    let src = "module M\n\nlet n = List.length [1]\n";
    assert!(codes(src, "n").is_empty());
}

#[test]
fn tuple_let_binding_destructures() {
    let src = "module M\n\nlet f p =\n  let (x, y) = p\n  x\n";
    assert!(codes(src, "f").is_empty());
}

#[test]
fn monomorphic_record_access_lowers_to_a_projection() {
    let src = "module M\n\npublic dist : { x : Int, y : Int } -> Int\nlet dist v = v.x + v.y\n";
    // `x` is field 0, `y` is field 1 in canonical (sorted) order.
    assert_eq!(lower(src, "dist"), "fn0(%0) = (+ (field 0 %0) (field 1 %0))\n");
    assert!(codes(src, "dist").is_empty());
}

#[test]
fn row_polymorphic_field_access_is_deferred() {
    // A reachable field access through a row variable cannot pick a constant
    // offset yet; it defers to the offset-evidence work with FAI7002.
    let src = "module M\n\nlet getX r = r.x\n";
    assert!(codes(src, "getX").contains(&"FAI7002".to_owned()));
}

#[test]
fn record_literal_lowers_to_sorted_data() {
    // Fields are stored in canonical (sorted-by-label) order, so `x` precedes
    // `y` regardless of how the literal is written.
    let src = "module M\n\nlet p = { y = 2, x = 1 }\n";
    assert_eq!(lower(src, "p"), "fn0() = (data 0 1 2)\n");
    assert!(codes(src, "p").is_empty());
}

#[test]
fn nullary_constructor_lowers_to_tagged_data() {
    let src = "module M\n\ntype T =\n  | A\n  | B Int\n\nlet mkA = A\n";
    assert_eq!(lower(src, "mkA"), "fn0() = (data 0)\n");
    assert!(codes(src, "mkA").is_empty());
}

#[test]
fn constructor_with_field_lowers_to_tagged_data() {
    // `B` is the second constructor, so it carries tag 1 and its field.
    let src = "module M\n\ntype T =\n  | A\n  | B Int\n\nlet mkB n = B n\n";
    assert_eq!(lower(src, "mkB"), "fn0(%0) = (data 1 %0)\n");
    assert!(codes(src, "mkB").is_empty());
}

#[test]
fn match_lowers_to_tag_tests_and_field_projections() {
    // The scrutinee is bound once, then a chain of tag tests selects an arm and
    // projects the matched constructor's field; the impossible final fallthrough
    // of this exhaustive match is an unreachable `<error>` leaf.
    let src = "module M\n\ntype T =\n  | A Int\n  | B Int\n\nlet f t =\n  match t with\n  | A x -> x\n  | B y -> y\n";
    assert_eq!(
        lower(src, "f"),
        "fn0(%0) = (let %3 = %0; (if (= (tag %3) 0) (let %1 = (field 0 %3); %1) (if (= (tag %3) 1) (let %2 = (field 0 %3); %2) <error>)))\n"
    );
    assert!(codes(src, "f").is_empty());
}

#[test]
fn float_comparison_selects_the_float_primitive() {
    let src = "module M\n\nlet lt = 1.0 < 2.0\n";
    assert_eq!(lower(src, "lt"), "fn0() = (<. 1 2)\n");
    assert!(codes(src, "lt").is_empty());
}

#[test]
fn structural_compare_lowers_to_the_compare_primitive() {
    // `<` on a non-numeric type (a tuple here) becomes the structural `compare`.
    let src = "module M\n\nlet before a b = (a, 1) < (b, 2)\n";
    assert!(codes(src, "before").is_empty());
    assert!(lower(src, "before").contains("compare"), "got {}", lower(src, "before"));
}

#[test]
fn lowering_invariants_hold_across_programs() {
    use fai_syntax::ast::ItemKind;

    let programs: &[(&str, &str)] = &[
        ("module M\n\nlet add x y = x + y\n", "add"),
        ("module M\n\nlet twice f = f >> f\n", "twice"),
        ("module M\n\nlet adder x = fun y -> x + y\n", "adder"),
        (
            "module M\n\nlet helper x = x + 1\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (Int.toString (helper 1))\n",
            "main",
        ),
    ];
    for (src, name) in programs {
        let (db, file) = db_with(src);
        let lowered = core(&db, file, fai_syntax::Symbol::intern(name));

        // The entry function's arity matches the binding's parameter count.
        let params = fai_syntax::parse(&db, file)
            .module
            .items
            .iter()
            .find_map(|it| match &it.kind {
                ItemKind::Binding { name: n, params, .. } if n.as_str() == *name => {
                    Some(params.len())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(lowered.entry().params.len(), params, "{name}: entry arity");

        // Every referenced global resolves to a real binding somewhere.
        for def in lowered.referenced_globals() {
            let target = db.source_file(def.file).expect("global's file is registered");
            assert!(
                fai_resolve::module_defs(&db, target).get(def.name).is_some(),
                "{name}: dangling global {}",
                def.name
            );
        }
    }
}
