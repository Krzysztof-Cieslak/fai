//! Property-based tests for the type checker.
//!
//! Strategy: generate *well-typed* expressions by construction (an AST tagged
//! with its type), render them to Fai source, and assert the checker agrees —
//! clean check and the expected inferred type. Plus general invariants:
//! inference is deterministic and never panics on arbitrary (parseable) input.

use fai_tests::check_source;
use proptest::prelude::*;

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

proptest! {
    // A well-typed Int expression typechecks clean and infers to `Int`.
    #[test]
    fn well_typed_int_expressions_infer_int(e in int_strategy()) {
        let src = format!("module P\n\nlet result = {}\n", render_int(&e));
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Int"));
    }

    // A well-typed Bool expression typechecks clean and infers to `Bool`.
    #[test]
    fn well_typed_bool_expressions_infer_bool(e in bool_strategy_with(int_strategy().boxed())) {
        let src = format!("module P\n\nlet result = {}\n", render_bool(&e));
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Bool"));
    }

    // Adding the correct signature to a well-typed expression keeps it clean.
    #[test]
    fn correct_signature_keeps_it_clean(e in int_strategy()) {
        let src = format!(
            "module P\n\npublic result : Int\nlet result = {}\n",
            render_int(&e)
        );
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
        let src = format!("module P\n\nlet result = {body}\n");
        let a = check_source(&src);
        let b = check_source(&src);
        let (a_codes, b_codes) = (a.codes(), b.codes());
        prop_assert_eq!(a.types, b.types);
        prop_assert_eq!(a_codes, b_codes);
    }

    // The checker never panics on arbitrary identifier-shaped programs (it may
    // report errors, but must terminate cleanly).
    #[test]
    fn never_panics_on_arbitrary_bindings(name in "[a-z][a-z0-9]{0,8}", n in 0i64..100) {
        let src = format!("module P\n\nlet {name} = {n}\n");
        let outcome = check_source(&src);
        // A lone integer binding is always clean and `Int`.
        prop_assert!(!outcome.has_errors(), "{:?}", outcome.codes());
    }

    // A well-typed Int used in a comparison yields a clean Bool binding,
    // exercising mixed Int/Bool subexpressions.
    #[test]
    fn comparison_of_ints_is_bool(a in int_strategy(), b in int_strategy()) {
        let src = format!(
            "module P\n\nlet result = {} < {}\n",
            render_int(&a),
            render_int(&b)
        );
        let outcome = check_source(&src);
        prop_assert!(!outcome.has_errors(), "errors {:?} for {src}", outcome.codes());
        prop_assert_eq!(outcome.types.get("result").map(String::as_str), Some("Bool"));
    }
}
