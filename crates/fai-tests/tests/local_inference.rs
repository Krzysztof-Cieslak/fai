//! Tests for **local** type inference: the types the checker infers for
//! parameters, `let`-bound locals, and lambda binders *inside* a function body.
//!
//! These complement the public-signature assertions elsewhere. Because a
//! signatured top-level binding reports its *declared* scheme (not the inferred
//! one), the cases here deliberately inspect locals and signature-less bindings
//! so they exercise inference itself.

use std::collections::BTreeMap;

use fai_tests::{check_source, local_type, local_types, type_of};

/// Wrap a body in a module with a single function `f` taking the given params.
fn func(params: &str, body: &str) -> String {
    format!("module M\n\nlet f {params} =\n{body}\n")
}

fn locals(src: &str) -> BTreeMap<String, String> {
    local_types(src, "f")
}

// ── Parameter types inferred from use ────────────────────────────────────────

#[test]
fn param_used_arithmetically_is_int() {
    let src = func("x", "  x + 1");
    assert_eq!(local_type(&src, "f", "x"), "Int");
}

#[test]
fn param_used_as_bool_condition() {
    let src = func("b", "  if b then 1 else 2");
    assert_eq!(local_type(&src, "f", "b"), "Bool");
}

#[test]
fn param_unconstrained_is_polymorphic() {
    let src = func("x", "  x");
    assert_eq!(local_type(&src, "f", "x"), "'a");
}

#[test]
fn param_used_as_function() {
    let src = func("g", "  g 1");
    // g is applied to an Int, returning a fresh result type.
    assert_eq!(local_type(&src, "f", "g"), "Int -> 'a");
}

#[test]
fn two_params_one_constrained() {
    let src = func("x y", "  if y then x + 1 else x");
    let ls = locals(&src);
    assert_eq!(ls.get("x").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("y").map(String::as_str), Some("Bool"));
}

#[test]
fn param_used_in_string_concat_is_string() {
    let src = func("s", "  s ++ \"!\"");
    assert_eq!(local_type(&src, "f", "s"), "String");
}

#[test]
fn param_used_in_comparison_is_orderable() {
    // `<` forces an Ord type; with no other constraint and a signature-less
    // binding, it stays a variable (Ord is not displayed in M2).
    let src = func("a b", "  a < b");
    let ls = locals(&src);
    // Both operands share a type.
    assert_eq!(ls.get("a"), ls.get("b"));
}

// ── let-bound locals ─────────────────────────────────────────────────────────

#[test]
fn let_local_inferred_int() {
    let src = func("x", "  let y = x + 1\n  y");
    let ls = locals(&src);
    assert_eq!(ls.get("x").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("y").map(String::as_str), Some("Int"));
}

#[test]
fn let_chain_propagates_types() {
    let src = func("x", "  let a = x + 1\n  let b = a > 0\n  let c = if b then a else 0\n  c");
    let ls = locals(&src);
    assert_eq!(ls.get("a").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("b").map(String::as_str), Some("Bool"));
    assert_eq!(ls.get("c").map(String::as_str), Some("Int"));
}

#[test]
fn let_local_string() {
    let src = func("n", "  let label = \"n=\" ++ intToString n\n  label");
    let ls = locals(&src);
    assert_eq!(ls.get("n").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("label").map(String::as_str), Some("String"));
}

#[test]
fn let_local_tuple() {
    let src = func("x", "  let pair = (x, x + 1)\n  pair");
    assert_eq!(local_type(&src, "f", "pair"), "Int * Int");
}

#[test]
fn let_local_list() {
    // `x` is unconstrained, so the list is polymorphic in the element type.
    let src = func("x", "  let xs = [x, x, x]\n  xs");
    assert_eq!(local_type(&src, "f", "xs"), "List 'a");
}

#[test]
fn let_local_list_of_ints_is_concrete() {
    let src = func("x", "  let xs = [x + 0, 1, 2]\n  xs");
    assert_eq!(local_type(&src, "f", "xs"), "List Int");
}

// ── tuple-pattern destructuring locals ───────────────────────────────────────

#[test]
fn tuple_destructure_components() {
    let src = func("p", "  let (a, b) = p\n  a");
    let ls = locals(&src);
    assert_eq!(ls.get("p").map(String::as_str), Some("'a * 'b"));
    assert_eq!(ls.get("a").map(String::as_str), Some("'a"));
    assert_eq!(ls.get("b").map(String::as_str), Some("'b"));
}

#[test]
fn tuple_destructure_used_constrains_components() {
    let src = func("p", "  let (a, b) = p\n  a + b");
    let ls = locals(&src);
    assert_eq!(ls.get("a").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("b").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("p").map(String::as_str), Some("Int * Int"));
}

#[test]
fn nested_tuple_destructure() {
    let src = func("p", "  let (a, rest) = p\n  let (b, c) = rest\n  a + b + c");
    let ls = locals(&src);
    assert_eq!(ls.get("a").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("b").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("c").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("rest").map(String::as_str), Some("Int * Int"));
    assert_eq!(ls.get("p").map(String::as_str), Some("Int * (Int * Int)"));
}

// ── local lambdas ────────────────────────────────────────────────────────────

#[test]
fn local_lambda_identity() {
    let src = func("x", "  let id = fun a -> a\n  id x");
    assert_eq!(local_type(&src, "f", "id"), "'a -> 'a");
}

#[test]
fn local_lambda_with_arithmetic() {
    let src = func("x", "  let inc = fun a -> a + 1\n  inc x");
    let ls = locals(&src);
    assert_eq!(ls.get("inc").map(String::as_str), Some("Int -> Int"));
    assert_eq!(ls.get("x").map(String::as_str), Some("Int"));
}

#[test]
fn local_lambda_binder_type() {
    let src = func("", "  let g = fun a -> a && true\n  g");
    assert_eq!(local_type(&src, "f", "a"), "Bool");
}

// ── local function bindings (let with params) ────────────────────────────────

#[test]
fn local_function_binding() {
    let src = func("x", "  let double n = n * 2\n  double x");
    let ls = locals(&src);
    assert_eq!(ls.get("double").map(String::as_str), Some("Int -> Int"));
    assert_eq!(ls.get("n").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("x").map(String::as_str), Some("Int"));
}

// ── local let-generalization (polymorphism) ──────────────────────────────────

#[test]
fn local_let_is_generalized_and_used_at_two_types() {
    // `id` is generalized, then used at Int and Bool — the whole thing checks.
    let src = "module M\n\nlet f =\n  let id = fun x -> x\n  let a = id 1\n  let b = id true\n  if b then a else a\n";
    let outcome = check_source(src);
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
    // The generalized local renders polymorphically.
    assert_eq!(local_type(src, "f", "id"), "'a -> 'a");
    // The two instantiations resolved to concrete types.
    let ls = locals(src);
    assert_eq!(ls.get("a").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("b").map(String::as_str), Some("Bool"));
}

#[test]
fn monomorphic_local_not_generalized_through_application() {
    // `y = g x` is not a syntactic value, so it is not generalized.
    let src = func("g x", "  let y = g x\n  y");
    let ls = locals(&src);
    // y has g's result type (a plain variable, not generalized away).
    assert!(ls.contains_key("y"), "have {:?}", ls.keys());
}

// ── shadowing ────────────────────────────────────────────────────────────────

#[test]
fn inner_shadow_changes_type() {
    // The inner `x` (a String) shadows the parameter `x` within the block; the
    // body returns the inner one.
    let src = "module M\n\nlet f x =\n  let x = \"s\"\n  x\n";
    // The top-level inferred type follows the shadowing binding.
    assert_eq!(type_of(src, "f"), "'a -> String");
}

// ── interaction with the prelude ─────────────────────────────────────────────

#[test]
fn local_uses_prelude_length() {
    let src = func("xs", "  let n = length xs\n  n");
    let ls = locals(&src);
    assert_eq!(ls.get("n").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("xs").map(String::as_str), Some("List 'a"));
}

#[test]
fn local_uses_prelude_append() {
    let src = func("xs ys", "  let zs = append xs ys\n  zs");
    let ls = locals(&src);
    assert_eq!(ls.get("zs").map(String::as_str), Some("List 'a"));
    assert_eq!(ls.get("xs").map(String::as_str), Some("List 'a"));
}

// ── deeper nesting ───────────────────────────────────────────────────────────

#[test]
fn let_inside_lambda_body() {
    // The lambda's binder and the let inside it are both inferred.
    let src = func("", "  let g = fun a ->\n    let b = a + 1\n    b\n  g");
    let ls = locals(&src);
    assert_eq!(ls.get("a").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("b").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("g").map(String::as_str), Some("Int -> Int"));
}

#[test]
fn deeply_nested_blocks() {
    let src = func("x", "  let a = x + 1\n  let b =\n    let inner = a * 2\n    inner + 1\n  b");
    let ls = locals(&src);
    assert_eq!(ls.get("a").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("b").map(String::as_str), Some("Int"));
    assert_eq!(ls.get("inner").map(String::as_str), Some("Int"));
}

#[test]
fn local_shadows_prelude_name() {
    // A local named `length` shadows the prelude function within the body.
    let src = func("n", "  let length = n + 1\n  length");
    assert_eq!(local_type(&src, "f", "length"), "Int");
}

#[test]
fn result_type_follows_locals() {
    // The signature-less function's inferred type is driven entirely by its
    // locals' inference.
    let src = func("x y", "  let sum = x + y\n  let ok = sum > 0\n  ok");
    assert_eq!(type_of(&src, "f"), "Int -> Int -> Bool");
}

#[test]
fn unused_local_still_inferred() {
    let src = func("x", "  let unused = x ++ \"!\"\n  x");
    // `unused` forces `x : String` even though the result is `x`.
    assert_eq!(local_type(&src, "f", "x"), "String");
    assert_eq!(local_type(&src, "f", "unused"), "String");
}

// ── signature-less top-level inference (no declared scheme to fall back on) ───

#[test]
fn signatureless_inferred_int_chain() {
    // No signature: def_type returns the *inferred* type.
    assert_eq!(type_of("module M\n\nlet f a b = a * b + a\n", "f"), "Int -> Int -> Int");
}

#[test]
fn signatureless_inferred_higher_order() {
    assert_eq!(type_of("module M\n\nlet f g x = g (g x)\n", "f"), "('a -> 'a) -> 'a -> 'a");
}

#[test]
fn signatureless_inferred_polymorphic_pair() {
    assert_eq!(type_of("module M\n\nlet f a b = (b, a)\n", "f"), "'a -> 'b -> 'b * 'a");
}

#[test]
fn signatureless_inferred_bool_logic() {
    assert_eq!(
        type_of("module M\n\nlet f a b c = a && b || not c\n", "f"),
        "Bool -> Bool -> Bool -> Bool"
    );
}

#[test]
fn signatureless_inferred_from_prelude_use() {
    assert_eq!(type_of("module M\n\nlet f xs = length xs + 1\n", "f"), "List 'a -> Int");
}
