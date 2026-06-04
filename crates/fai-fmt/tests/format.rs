//! Formatter golden snapshots, idempotence, and property tests.

use fai_span::SourceId;
use fai_syntax::{ItemTree, TokenKind, build_item_tree, parse_module};
use proptest::prelude::*;

fn fmt(src: &str) -> String {
    let parsed = parse_module(SourceId::new(0), src);
    fai_fmt::format(&parsed.module, &parsed.comments, src)
}

/// Formatting an already-formatted program is a no-op.
fn assert_idempotent(src: &str) {
    let once = fmt(src);
    let twice = fmt(&once);
    assert_eq!(once, twice, "fmt is not idempotent\n=== once ===\n{once}\n=== twice ===\n{twice}");
}

#[test]
fn hello() {
    let src = "module Hello\npublic main : Runtime -> Unit\nlet main runtime =\n  runtime.console.writeLine \"Hello, Fai!\"";
    insta::assert_snapshot!("hello", fmt(src));
    assert_idempotent(src);
}

#[test]
fn signatures_and_operators() {
    let src = "module Basics\npublic add : Int -> Int -> Int\nlet add x y = x + y\nlet ratio = 3.0 / 2.0\nlet isEven = count % 2 = 0";
    insta::assert_snapshot!("basics", fmt(src));
    assert_idempotent(src);
}

#[test]
fn pipes_collapse_when_they_fit() {
    let src = "module Funcs\npublic describe : Int -> String\nlet describe n =\n  n\n  |> inc\n  |> intToString";
    insta::assert_snapshot!("pipes", fmt(src));
    assert_idempotent(src);
}

#[test]
fn local_let_block() {
    let src = "module Locals\npublic hypotenuse : Float -> Float -> Float\nlet hypotenuse a b =\n  let a2 = a * a\n  let b2 = b * b\n  sqrt (a2 + b2)";
    insta::assert_snapshot!("locals", fmt(src));
    assert_idempotent(src);
}

#[test]
fn if_else_chain_collapses_when_it_fits() {
    let src = "module Locals\npublic classify : Int -> String\nlet classify n =\n  if n < 0 then \"negative\"\n  else if n = 0 then \"zero\"\n  else \"positive\"";
    insta::assert_snapshot!("classify", fmt(src));
    assert_idempotent(src);
}

#[test]
fn multiline_if_when_it_does_not_fit() {
    let src = "module M\nlet f x =\n  if someVeryLongCondition x then theFirstRatherLongBranchResult x else theSecondEquallyLongBranchResultValue x";
    insta::assert_snapshot!("multiline_if", fmt(src));
    assert_idempotent(src);
}

#[test]
fn tuples_lists_and_contracts() {
    let src = "module Tuples\npublic divMod : Int -> Int -> Int * Int\nlet divMod a b = (a / b, a % b)\nexample: divMod 7 3 = (2, 1)\nlet xs = [1, 2, 3]\npublic swap : 'a * 'b -> 'b * 'a\nlet swap pair =\n  let (x, y) = pair\n  (y, x)";
    insta::assert_snapshot!("tuples", fmt(src));
    assert_idempotent(src);
}

#[test]
fn comments_doc_leading_and_trailing() {
    let src = "module Comments\n// a standalone note\n/// Doc for answer.\npublic answer : Int\nlet answer = 42 // the trailing answer";
    insta::assert_snapshot!("comments", fmt(src));
    assert_idempotent(src);
}

#[test]
fn trailing_comment_on_local_let_survives() {
    // Exercises the expression-trailing attachment path end to end.
    let src = "module M\nlet f =\n  let a = 1 // keep me\n  a";
    let out = fmt(src);
    assert!(out.contains("let a = 1 // keep me"), "comment dropped:\n{out}");
    assert_idempotent(src);
}

#[test]
fn messy_input_is_canonicalized() {
    // Extra blank lines and odd spacing collapse to the canonical layout.
    let src = "module M\n\n\n\nlet    x=1\n\n\n\nlet y   =   2";
    insta::assert_snapshot!("messy", fmt(src));
    assert_idempotent(src);
}

proptest! {
    /// Formatting arbitrary input never panics.
    #[test]
    fn format_never_panics(input in any::<String>()) {
        let parsed = parse_module(SourceId::new(0), &input);
        let _ = fai_fmt::format(&parsed.module, &parsed.comments, &input);
    }

    /// Formatting is idempotent on generated bindings.
    #[test]
    fn idempotent_on_generated_bindings(name in "[a-z][a-zA-Z0-9_]*", value in 0u32..100_000) {
        prop_assume!(TokenKind::keyword(&name).is_none());
        let src = format!("module M\nlet {name} = {value}");
        let once = fmt(&src);
        let twice = fmt(&once);
        prop_assert_eq!(once, twice);
    }
}

// --- broader coverage -------------------------------------------------------

fn item_tree_of(src: &str) -> ItemTree {
    build_item_tree(&parse_module(SourceId::new(0), src).module)
}

/// Formatting must be idempotent, reparse cleanly, and preserve the item tree.
fn assert_canonical(src: &str) -> String {
    let once = fmt(src);
    let reparsed = parse_module(SourceId::new(0), &once);
    assert!(reparsed.diagnostics.is_empty(), "fmt output did not reparse cleanly:\n{once}");
    assert_eq!(fmt(&once), once, "fmt is not idempotent:\n{once}");
    assert_eq!(
        item_tree_of(src),
        build_item_tree(&reparsed.module),
        "fmt changed the item tree:\n{once}"
    );
    once
}

#[test]
fn all_binary_operators_format_with_spaces() {
    let src = "module M\nlet a = w - x * y / z % p\nlet b = c ++ d :: e\nlet c = p && q || r\nlet d = a = b\nlet e = a <> b\nlet f = a < b\nlet g = a <= b\nlet h = a > b\nlet i = a >= b\nlet j = f >> g\nlet k = x |> f";
    let out = assert_canonical(src);
    for needle in [
        "w - x * y / z % p",
        "c ++ d :: e",
        "p && q || r",
        "a = b",
        "a <> b",
        "a <= b",
        "a >= b",
        "f >> g",
        "x |> f",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
}

#[test]
fn parens_are_preserved_and_not_invented() {
    assert!(assert_canonical("module M\nlet x = a + b * c").contains("let x = a + b * c"));
    assert!(assert_canonical("module M\nlet x = (a + b) * c").contains("let x = (a + b) * c"));
    assert!(assert_canonical("module M\nlet x = a - (b - c)").contains("let x = a - (b - c)"));
    assert!(assert_canonical("module M\nlet x = ((a))").contains("let x = ((a))"));
}

#[test]
fn unary_minus_and_negatives() {
    assert!(assert_canonical("module M\nlet x = -a * b").contains("-a * b"));
    assert!(assert_canonical("module M\nlet y = f (-3)").contains("f (-3)"));
    assert!(assert_canonical("module M\nlet z = 0 - n").contains("0 - n"));
}

#[test]
fn literals_are_reproduced_verbatim() {
    let out = assert_canonical(
        "module M\nlet a = 0xFF\nlet b = 1_000\nlet c = 'a'\nlet d = 3.0\nlet e = \"hi\"",
    );
    for needle in ["= 0xFF", "= 1_000", "= 'a'", "= 3.0", "= \"hi\""] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
}

#[test]
fn string_escapes_are_preserved() {
    let out = assert_canonical("module M\nlet s = \"a\\nb\"");
    assert!(out.contains("let s = \"a\\nb\""), "out:\n{out}");
}

#[test]
fn type_signatures_format() {
    assert!(
        assert_canonical("module M\npublic f : Int -> Int -> Int\nlet f a b = a")
            .contains("public f : Int -> Int -> Int")
    );
    assert!(
        assert_canonical("module M\npublic g : 'a * 'b -> 'b * 'a\nlet g p = p")
            .contains("public g : 'a * 'b -> 'b * 'a")
    );
    assert!(
        assert_canonical("module M\npublic h : ('a -> 'b) -> List 'a -> List 'b\nlet h f = f")
            .contains("public h : ('a -> 'b) -> List 'a -> List 'b")
    );
}

#[test]
fn lambda_forms() {
    assert!(assert_canonical("module M\nlet a = fun x -> x").contains("fun x -> x"));
    assert!(
        assert_canonical("module M\nlet b = fun acc x -> acc + x").contains("fun acc x -> acc + x")
    );
    assert!(assert_canonical("module M\nlet c = fun (x, y) -> x").contains("fun (x, y) -> x"));
}

#[test]
fn field_access_and_application() {
    assert!(assert_canonical("module M\nlet a = r.x.y").contains("r.x.y"));
    assert!(assert_canonical("module M\nlet b = f (g x) y").contains("f (g x) y"));
}

#[test]
fn collections_and_unit() {
    assert!(assert_canonical("module M\nlet a = [(1, 2), (3, 4)]").contains("[(1, 2), (3, 4)]"));
    assert!(assert_canonical("module M\nlet b = ()").contains("let b = ()"));
    assert!(assert_canonical("module M\nlet c = []").contains("let c = []"));
}

#[test]
fn block_comment_leads_an_item() {
    assert!(assert_canonical("module M\n(* a note *)\nlet x = 1").contains("(* a note *)"));
}

#[test]
fn trailing_comment_on_a_signature() {
    let out = assert_canonical("module M\npublic f : Int // sig note\nlet f = 1");
    assert!(out.contains("public f : Int // sig note"), "out:\n{out}");
}

#[test]
fn aligned_trailing_comment_collapses_to_one_space() {
    let out = assert_canonical("module M\nlet x = 3        // aligned");
    assert!(out.contains("let x = 3 // aligned"), "out:\n{out}");
}

#[test]
fn comment_only_module_keeps_the_comment() {
    assert!(assert_canonical("module M\n// lonely").contains("// lonely"));
}

#[test]
fn contracts_stay_in_the_binding_group() {
    let out =
        assert_canonical("module M\npublic f : Int\nlet f = 1\nexample: f = 1\nforall x: f = x");
    assert!(
        out.contains("public f : Int\nlet f = 1\nexample: f = 1\nforall x: f = x"),
        "contracts were split from the binding:\n{out}",
    );
}

#[test]
fn distinct_bindings_get_a_blank_line() {
    assert!(assert_canonical("module M\nlet a = 1\nlet b = 2").contains("let a = 1\n\nlet b = 2"));
}

#[test]
fn equivalent_inputs_format_identically() {
    assert_eq!(fmt("module M\nlet x = a + b"), fmt("module M\n\n\nlet   x   =   a+b"));
}

proptest! {
    /// fmt output of a generated program reparses cleanly and is idempotent.
    #[test]
    fn generated_program_is_canonical(name in "[a-z][a-zA-Z0-9_]*", a in 0u32..1000, b in 0u32..1000) {
        prop_assume!(TokenKind::keyword(&name).is_none());
        let src = format!("module M\nlet {name} = {a} + {b} * {a}");
        let once = fmt(&src);
        let reparsed = parse_module(SourceId::new(0), &once);
        prop_assert!(reparsed.diagnostics.is_empty());
        prop_assert_eq!(fmt(&once), once);
        prop_assert_eq!(item_tree_of(&src), build_item_tree(&reparsed.module));
    }
}
