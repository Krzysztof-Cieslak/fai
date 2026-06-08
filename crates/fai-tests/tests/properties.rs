//! Property-based tests for the type checker.
//!
//! Strategy: generate *well-typed* expressions by construction (an AST tagged
//! with its type), render them to Fai source, and assert the checker agrees —
//! clean check and the expected inferred type. Plus general invariants:
//! inference is deterministic and never panics on arbitrary (parseable) input.

use fai_tests::{check_source, local_type};
use indoc::formatdoc;
use proptest::prelude::*;

/// Whether `name` is a reserved keyword (so it cannot be a binding name).
///
/// Delegates to the lexer's keyword table — the single source of truth — so this
/// can never drift from the set the lexer actually reserves (a new keyword is
/// honored here automatically).
fn is_reserved(name: &str) -> bool {
    fai_syntax::TokenKind::keyword(name).is_some()
}

/// A generated, well-typed expression of a known type.
#[derive(Debug, Clone)]
enum Expr {
    Int(IntExpr),
    Bool(BoolExpr),
}

#[derive(Debug, Clone)]
enum IntExpr {
    Lit(i64),
    Add(Box<IntExpr>, Box<IntExpr>),
    Sub(Box<IntExpr>, Box<IntExpr>),
    Mul(Box<IntExpr>, Box<IntExpr>),
    IfThen(Box<BoolExpr>, Box<IntExpr>, Box<IntExpr>),
}

#[derive(Debug, Clone)]
enum BoolExpr {
    Lit(bool),
    Not(Box<BoolExpr>),
    And(Box<BoolExpr>, Box<BoolExpr>),
    Or(Box<BoolExpr>, Box<BoolExpr>),
    Lt(Box<IntExpr>, Box<IntExpr>),
    EqI(Box<IntExpr>, Box<IntExpr>),
}

fn render_int(e: &IntExpr) -> String {
    match e {
        // Render negatives via subtraction so we never emit a bare `-5` literal
        // ambiguously; keep everything parenthesized for unambiguous structure.
        IntExpr::Lit(n) => {
            if *n < 0 {
                format!("(0 - {})", n.unsigned_abs())
            } else {
                n.to_string()
            }
        }
        IntExpr::Add(a, b) => format!("({} + {})", render_int(a), render_int(b)),
        IntExpr::Sub(a, b) => format!("({} - {})", render_int(a), render_int(b)),
        IntExpr::Mul(a, b) => format!("({} * {})", render_int(a), render_int(b)),
        IntExpr::IfThen(c, t, e) => {
            format!("(if {} then {} else {})", render_bool(c), render_int(t), render_int(e))
        }
    }
}

fn render_bool(e: &BoolExpr) -> String {
    match e {
        BoolExpr::Lit(b) => b.to_string(),
        BoolExpr::Not(a) => format!("(not {})", render_bool(a)),
        BoolExpr::And(a, b) => format!("({} && {})", render_bool(a), render_bool(b)),
        BoolExpr::Or(a, b) => format!("({} || {})", render_bool(a), render_bool(b)),
        BoolExpr::Lt(a, b) => format!("({} < {})", render_int(a), render_int(b)),
        BoolExpr::EqI(a, b) => format!("({} = {})", render_int(a), render_int(b)),
    }
}

fn int_strategy() -> impl Strategy<Value = IntExpr> {
    let leaf = (-1000i64..1000).prop_map(IntExpr::Lit);
    leaf.prop_recursive(5, 40, 4, |inner| {
        let bool_inner = bool_strategy_with(inner.clone().boxed());
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| IntExpr::Add(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| IntExpr::Sub(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| IntExpr::Mul(Box::new(a), Box::new(b))),
            (bool_inner, inner.clone(), inner.clone()).prop_map(|(c, t, e)| IntExpr::IfThen(
                Box::new(c),
                Box::new(t),
                Box::new(e)
            )),
        ]
    })
}

fn bool_strategy_with(int: BoxedStrategy<IntExpr>) -> BoxedStrategy<BoolExpr> {
    let leaf = any::<bool>().prop_map(BoolExpr::Lit);
    leaf.prop_recursive(4, 24, 3, move |inner| {
        let int = int.clone();
        prop_oneof![
            inner.clone().prop_map(|a| BoolExpr::Not(Box::new(a))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| BoolExpr::And(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| BoolExpr::Or(Box::new(a), Box::new(b))),
            (int.clone(), int.clone()).prop_map(|(a, b)| BoolExpr::Lt(Box::new(a), Box::new(b))),
            (int.clone(), int.clone()).prop_map(|(a, b)| BoolExpr::EqI(Box::new(a), Box::new(b))),
        ]
    })
    .boxed()
}

fn expr_strategy() -> impl Strategy<Value = Expr> {
    prop_oneof![
        int_strategy().prop_map(Expr::Int),
        bool_strategy_with(int_strategy().boxed()).prop_map(Expr::Bool),
    ]
}

/// A generated, well-typed `Float` expression.
#[derive(Debug, Clone)]
enum FloatExpr {
    Lit(u32),
    Add(Box<FloatExpr>, Box<FloatExpr>),
    Sub(Box<FloatExpr>, Box<FloatExpr>),
    Mul(Box<FloatExpr>, Box<FloatExpr>),
    Div(Box<FloatExpr>, Box<FloatExpr>),
}

fn render_float(e: &FloatExpr) -> String {
    match e {
        // Always emit a decimal point so the literal lexes as a `Float`.
        FloatExpr::Lit(n) => format!("{n}.0"),
        FloatExpr::Add(a, b) => format!("({} + {})", render_float(a), render_float(b)),
        FloatExpr::Sub(a, b) => format!("({} - {})", render_float(a), render_float(b)),
        FloatExpr::Mul(a, b) => format!("({} * {})", render_float(a), render_float(b)),
        FloatExpr::Div(a, b) => format!("({} / {})", render_float(a), render_float(b)),
    }
}

fn float_strategy() -> impl Strategy<Value = FloatExpr> {
    let leaf = (0u32..1000).prop_map(FloatExpr::Lit);
    leaf.prop_recursive(5, 40, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| FloatExpr::Add(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| FloatExpr::Sub(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| FloatExpr::Mul(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| FloatExpr::Div(Box::new(a), Box::new(b))),
        ]
    })
}

proptest! {
    // A well-typed Int expression typechecks clean and infers to `Int`.
    #[test]
    fn well_typed_int_expressions_infer_int(e in int_strategy()) {
        let src = formatdoc! {r#"
            module P

            let result = {}
        "#, render_int(&e)};
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Int"));
    }

    // A well-typed Int expression bound to a *local* infers to `Int`, and the
    // local that returns it does too — exercises local inference, not just the
    // public signature.
    #[test]
    fn local_binding_of_int_expr_infers_int(e in int_strategy()) {
        let src = formatdoc! {r#"
            module P

            let f =
              let value = {}
              value
        "#, render_int(&e)};
        prop_assert_eq!(local_type(&src, "f", "value"), "Int");
    }

    // A well-typed Bool expression typechecks clean and infers to `Bool`.
    #[test]
    fn well_typed_bool_expressions_infer_bool(e in bool_strategy_with(int_strategy().boxed())) {
        let src = formatdoc! {r#"
            module P

            let result = {}
        "#, render_bool(&e)};
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Bool"));
    }

    // Adding the correct signature to a well-typed expression keeps it clean.
    #[test]
    fn correct_signature_keeps_it_clean(e in int_strategy()) {
        let src = formatdoc! {r#"
            module P

            public result : Int
            let result = {}
        "#, render_int(&e)};
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
    }

    // Inference is deterministic: checking the same source twice agrees on both
    // the inferred types and the diagnostics.
    #[test]
    fn inference_is_deterministic(e in expr_strategy()) {
        let body = match &e {
            Expr::Int(i) => render_int(i),
            Expr::Bool(b) => render_bool(b),
        };
        let src = formatdoc! {r#"
            module P

            let result = {body}
        "#};
        let a = check_source(&src);
        let b = check_source(&src);
        let (a_codes, b_codes) = (a.codes(), b.codes());
        prop_assert_eq!(a.types, b.types);
        prop_assert_eq!(a_codes, b_codes);
    }

    // The checker never panics on arbitrary identifier-shaped programs (it may
    // report errors, but must terminate cleanly).
    #[test]
    fn never_panics_on_arbitrary_bindings(
        name in "[a-z][a-z0-9]{0,8}".prop_filter("reserved keyword", |s| !is_reserved(s)),
        n in 0i64..100,
    ) {
        let src = formatdoc! {r#"
            module P

            let {name} = {n}
        "#};
        let outcome = check_source(&src);
        // A lone integer binding (with a non-keyword name) is always clean `Int`.
        prop_assert!(!outcome.has_errors(), "{:?}", outcome.codes());
    }

    // A well-typed Int used in a comparison yields a clean Bool binding,
    // exercising mixed Int/Bool subexpressions.
    #[test]
    fn comparison_of_ints_is_bool(a in int_strategy(), b in int_strategy()) {
        let src = formatdoc! {r#"
            module P

            let result = {} < {}
        "#, render_int(&a), render_int(&b)};
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Bool"));
    }

    // A well-typed Float expression typechecks clean and infers to `Float` — the
    // overloaded operators resolve to the Float type with no Int defaulting.
    #[test]
    fn well_typed_float_expressions_infer_float(e in float_strategy()) {
        let src = formatdoc! {r#"
            module P

            let result = {}
        "#, render_float(&e)};
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Float"));
    }

    // A record of distinct `Int` fields infers a closed record whose labels are
    // rendered in sorted order, and accessing any field yields its `Int` type.
    #[test]
    fn record_field_access_infers_sorted_closed_record(
        labels in prop::collection::hash_set("[a-z]{1,4}", 1..5),
    ) {
        prop_assume!(labels.iter().all(|l| !is_reserved(l)));
        let mut sorted: Vec<String> = labels.into_iter().collect();
        sorted.sort();

        let lits = sorted
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{l} = {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let first = &sorted[0];
        let src = formatdoc! {r#"
            module P

            let rec0 = {{ {lits} }}

            let got = rec0.{first}
        "#};

        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("got").map(String::as_str), Some("Int"));

        let expected = format!(
            "{{ {} }}",
            sorted.iter().map(|l| format!("{l} : Int")).collect::<Vec<_>>().join(", ")
        );
        prop_assert_eq!(outcome.types.get("rec0"), Some(&expected));
    }

    // For a union of any size: a `match` covering every constructor is clean,
    // and dropping one arm is reported as a non-exhaustive match (FAI4001).
    #[test]
    fn union_match_exhaustiveness_tracks_constructor_count(n in 1usize..6) {
        let variants = (0..n).map(|i| format!("  | C{i}")).collect::<Vec<_>>().join("\n");
        let arms = |count: usize| {
            (0..count).map(|i| format!("  | C{i} -> {i}")).collect::<Vec<_>>().join("\n")
        };
        let header = formatdoc! {r#"
            module P

            public type T =
            {variants}

            public f : T -> Int
            let f t =
              match t with
        "#};

        // Covering all `n` constructors is exhaustive and clean.
        let complete = check_source(&format!("{header}{}\n", arms(n)));
        prop_assert!(!complete.has_errors(), "complete match errored: {:?}", complete.codes());

        // Dropping the last arm makes it non-exhaustive (with at least one arm left).
        if n > 1 {
            let partial = check_source(&format!("{header}{}\n", arms(n - 1)));
            prop_assert!(
                partial.codes().contains(&"FAI4001".to_owned()),
                "missing arm should be non-exhaustive: {:?}",
                partial.codes()
            );
        }
    }
}
