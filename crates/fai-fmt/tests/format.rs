//! Formatter golden snapshots, idempotence, and property tests.

use fai_span::SourceId;
use fai_syntax::{TokenKind, parse_module};
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
