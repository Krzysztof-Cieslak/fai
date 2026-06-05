//! The headline backend property: JIT-compiled programs agree with a Rust
//! reference evaluator.
//!
//! Random `Int`-typed Fai expressions over a parameter `x` are rendered to
//! source, compiled and run through the full pipeline (lower → reference-count →
//! Cranelift → JIT), and their printed result is compared to a wrapping-integer
//! evaluator. Each run also asserts a leak-free exit, so this exercises codegen,
//! the calling convention, and reference counting together.

use proptest::prelude::*;

use crate::tests::run;

/// An `Int`-typed expression over the parameter `x`.
#[derive(Debug, Clone)]
enum E {
    X,
    Lit(i64),
    Bin(char, Box<E>, Box<E>),
    If(&'static str, Box<E>, Box<E>, Box<E>, Box<E>),
}

fn eval(e: &E, x: i64) -> i64 {
    match e {
        E::X => x,
        E::Lit(n) => *n,
        E::Bin(op, a, b) => {
            let (a, b) = (eval(a, x), eval(b, x));
            match op {
                '+' => a.wrapping_add(b),
                '-' => a.wrapping_sub(b),
                _ => a.wrapping_mul(b),
            }
        }
        E::If(cmp, cl, cr, t, f) => {
            let (l, r) = (eval(cl, x), eval(cr, x));
            let take = match *cmp {
                "<" => l < r,
                "<=" => l <= r,
                ">" => l > r,
                ">=" => l >= r,
                "=" => l == r,
                _ => l != r,
            };
            if take { eval(t, x) } else { eval(f, x) }
        }
    }
}

fn render(e: &E) -> String {
    match e {
        E::X => "x".to_owned(),
        E::Lit(n) => n.to_string(),
        E::Bin(op, a, b) => format!("({} {op} {})", render(a), render(b)),
        E::If(cmp, cl, cr, t, f) => {
            format!(
                "(if {} {cmp} {} then {} else {})",
                render(cl),
                render(cr),
                render(t),
                render(f)
            )
        }
    }
}

/// Renders an integer argument, parenthesizing negatives (so `f (0 - 5)` rather
/// than the subtraction `f - 5`).
fn render_arg(x: i64) -> String {
    if x >= 0 { x.to_string() } else { format!("(0 - {})", -x) }
}

fn expr() -> impl Strategy<Value = E> {
    let leaf = prop_oneof![Just(E::X), (0i64..1000).prop_map(E::Lit)];
    leaf.prop_recursive(4, 48, 4, |inner| {
        let arith = (prop_oneof![Just('+'), Just('-'), Just('*')], inner.clone(), inner.clone())
            .prop_map(|(op, a, b)| E::Bin(op, Box::new(a), Box::new(b)));
        let cmp = prop_oneof![Just("<"), Just("<="), Just(">"), Just(">="), Just("="), Just("<>"),];
        let conditional = (cmp, inner.clone(), inner.clone(), inner.clone(), inner.clone())
            .prop_map(|(c, cl, cr, t, f)| {
                E::If(c, Box::new(cl), Box::new(cr), Box::new(t), Box::new(f))
            });
        prop_oneof![arith, conditional]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn jit_matches_reference_evaluator(
        e in expr(),
        x in any::<i64>().prop_filter("avoid i64::MIN rendering", |v| *v != i64::MIN),
    ) {
        let expected = eval(&e, x);
        let src = format!(
            "module M\n\nlet f x = {}\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (intToString (f {}))\n",
            render(&e),
            render_arg(x),
        );
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "program leaked or failed: {}", out);
        let got: i64 = out.trim().parse().expect("integer output");
        prop_assert_eq!(got, expected);
    }
}
