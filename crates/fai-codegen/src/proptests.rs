//! The headline backend property: JIT-compiled programs agree with a Rust
//! reference evaluator.
//!
//! Random `Int`-typed Fai expressions over a parameter `x` are rendered to
//! source, compiled and run through the full pipeline (lower → reference-count →
//! Cranelift → JIT), and their printed result is compared to a wrapping-integer
//! evaluator. Each run also asserts a leak-free exit, so this exercises codegen,
//! the calling convention, and reference counting together.

use indoc::formatdoc;
use proptest::prelude::*;

use crate::tests::run;

/// An `Int`-typed expression over the parameter `x`.
#[derive(Debug, Clone)]
enum E {
    X,
    Lit(i64),
    Bin(char, Box<E>, Box<E>),
    /// A bitwise operation by std name: `and`/`or`/`xor`/`shiftLeft`/
    /// `shiftRight`/`shiftRightLogical`.
    Bit(&'static str, Box<E>, Box<E>),
    /// Bitwise complement (unary).
    Comp(Box<E>),
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
        // Mirrors the runtime's bitwise primitives: shifts mask the amount to
        // `0..63`; `shiftRight` is arithmetic, `shiftRightLogical` is logical.
        E::Bit(op, a, b) => {
            let (a, b) = (eval(a, x), eval(b, x));
            match *op {
                "and" => a & b,
                "or" => a | b,
                "xor" => a ^ b,
                "shiftLeft" => ((a as u64) << ((b & 63) as u32)) as i64,
                "shiftRight" => a >> ((b & 63) as u32),
                _ => ((a as u64) >> ((b & 63) as u32)) as i64,
            }
        }
        E::Comp(a) => !eval(a, x),
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
        E::Bit(op, a, b) => format!("(Int.{op} {} {})", render(a), render(b)),
        E::Comp(a) => format!("(Int.complement {})", render(a)),
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
        let bitwise = (
            prop_oneof![
                Just("and"),
                Just("or"),
                Just("xor"),
                Just("shiftLeft"),
                Just("shiftRight"),
                Just("shiftRightLogical"),
            ],
            inner.clone(),
            inner.clone(),
        )
            .prop_map(|(op, a, b)| E::Bit(op, Box::new(a), Box::new(b)));
        let complement = inner.clone().prop_map(|a| E::Comp(Box::new(a)));
        let cmp = prop_oneof![Just("<"), Just("<="), Just(">"), Just(">="), Just("="), Just("<>"),];
        let conditional = (cmp, inner.clone(), inner.clone(), inner.clone(), inner.clone())
            .prop_map(|(c, cl, cr, t, f)| {
                E::If(c, Box::new(cl), Box::new(cr), Box::new(t), Box::new(f))
            });
        prop_oneof![arith, bitwise, complement, conditional]
    })
}

/// A `Bool`-typed expression over `x` (comparisons combined with `&&`/`||`/`not`).
#[derive(Debug, Clone)]
enum B {
    Cmp(&'static str, Box<E>, Box<E>),
    And(Box<B>, Box<B>),
    Or(Box<B>, Box<B>),
    Not(Box<B>),
}

fn eval_b(b: &B, x: i64) -> bool {
    match b {
        B::Cmp(cmp, l, r) => {
            let (l, r) = (eval(l, x), eval(r, x));
            match *cmp {
                "<" => l < r,
                "<=" => l <= r,
                ">" => l > r,
                ">=" => l >= r,
                "=" => l == r,
                _ => l != r,
            }
        }
        B::And(a, b) => eval_b(a, x) && eval_b(b, x),
        B::Or(a, b) => eval_b(a, x) || eval_b(b, x),
        B::Not(a) => !eval_b(a, x),
    }
}

fn render_b(b: &B) -> String {
    match b {
        B::Cmp(cmp, l, r) => format!("({} {cmp} {})", render(l), render(r)),
        B::And(a, b) => format!("({} && {})", render_b(a), render_b(b)),
        B::Or(a, b) => format!("({} || {})", render_b(a), render_b(b)),
        B::Not(a) => format!("(not {})", render_b(a)),
    }
}

fn bool_expr() -> impl Strategy<Value = B> {
    let cmp = prop_oneof![Just("<"), Just("<="), Just(">"), Just(">="), Just("="), Just("<>"),];
    let leaf = (cmp, expr(), expr()).prop_map(|(c, l, r)| B::Cmp(c, Box::new(l), Box::new(r)));
    leaf.prop_recursive(3, 16, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| B::And(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| B::Or(Box::new(a), Box::new(b))),
            inner.prop_map(|a| B::Not(Box::new(a))),
        ]
    })
}

/// A pool of distinct field names (sorted, so a label's slot is its index among
/// the present fields — which shifts as the field set changes).
const FIELD_NAMES: &[&str] = &["a", "b", "c", "d", "e", "f", "g", "h"];

/// A non-empty record (distinct fields with `Int` values) plus the index of one
/// field to read back. Different field sets put the read field at different slots,
/// so this exercises the offset evidence rather than a baked-in offset.
fn record_spec() -> impl Strategy<Value = (Vec<(&'static str, i64)>, usize)> {
    proptest::sample::subsequence(FIELD_NAMES.to_vec(), 1..=FIELD_NAMES.len()).prop_flat_map(
        |names| {
            let count = names.len();
            let values = proptest::collection::vec(0i64..1_000, count);
            (Just(names), values, 0..count).prop_map(|(names, values, idx)| {
                (names.into_iter().zip(values).collect::<Vec<_>>(), idx)
            })
        },
    )
}

/// Renders `{ a = 1, b = 2, … }` from field specs.
fn render_record(fields: &[(&str, i64)]) -> String {
    let body = fields.iter().map(|(n, v)| format!("{n} = {v}")).collect::<Vec<_>>().join(", ");
    format!("{{ {body} }}")
}

/// A non-empty set of distinct method names with `Int` results, plus the index of
/// the method to call back. As the method set changes, the called method sits at
/// a different dictionary slot (methods are stored sorted by name).
fn interface_spec() -> impl Strategy<Value = (Vec<(&'static str, i64)>, usize)> {
    proptest::sample::subsequence(FIELD_NAMES.to_vec(), 1..=FIELD_NAMES.len()).prop_flat_map(
        |names| {
            let count = names.len();
            let values = proptest::collection::vec(0i64..1_000, count);
            (Just(names), values, 0..count).prop_map(|(names, values, idx)| {
                (names.into_iter().zip(values).collect::<Vec<_>>(), idx)
            })
        },
    )
}

/// A flat arithmetic expression: `terms` interleaved with `ops` (one fewer).
/// Operands are positive (so `/`/`%` never divide by zero) and small (so results
/// render without `i64::MIN` trouble).
fn flat_arith() -> impl Strategy<Value = (Vec<i64>, Vec<char>)> {
    (2usize..=6).prop_flat_map(|n| {
        let terms = proptest::collection::vec(1i64..50, n);
        let op = prop_oneof![Just('+'), Just('-'), Just('*'), Just('/'), Just('%')];
        let ops = proptest::collection::vec(op, n - 1);
        (terms, ops)
    })
}

/// Evaluates a flat expression with Fai's precedence and associativity, computed
/// independently of the parser: `* / %` bind tighter than `+ -`, both groups
/// left-associative.
fn eval_flat(terms: &[i64], ops: &[char]) -> i64 {
    // Pass 1: fold the multiplicative operators left to right.
    let mut t = vec![terms[0]];
    let mut additive = Vec::new();
    for (i, &op) in ops.iter().enumerate() {
        let rhs = terms[i + 1];
        match op {
            '*' => *t.last_mut().unwrap() = t.last().unwrap().wrapping_mul(rhs),
            '/' => *t.last_mut().unwrap() = t.last().unwrap().wrapping_div(rhs),
            '%' => *t.last_mut().unwrap() = t.last().unwrap().wrapping_rem(rhs),
            other => {
                additive.push(other);
                t.push(rhs);
            }
        }
    }
    // Pass 2: fold the additive operators left to right.
    let mut acc = t[0];
    for (i, &op) in additive.iter().enumerate() {
        acc = if op == '+' { acc.wrapping_add(t[i + 1]) } else { acc.wrapping_sub(t[i + 1]) };
    }
    acc
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Operators parse and evaluate at their documented precedence: a flat,
    /// unparenthesized expression agrees with the independent two-level evaluator.
    #[test]
    fn operator_precedence_matches_evaluation(spec in flat_arith()) {
        let (terms, ops) = spec;
        let expected = eval_flat(&terms, &ops);
        let mut expr = terms[0].to_string();
        for (i, op) in ops.iter().enumerate() {
            expr.push_str(&format!(" {op} {}", terms[i + 1]));
        }
        let src = formatdoc! {r#"
            module M

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString ({expr}))
        "#};
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "leak or failure: {}", out);
        prop_assert_eq!(out.trim().parse::<i64>().expect("integer output"), expected);
    }

    /// A row-polymorphic accessor reads the named field's value no matter where it
    /// sits in the caller's record — the headline offset-evidence guarantee.
    #[test]
    fn row_polymorphic_access_reads_the_named_field(spec in record_spec()) {
        let (fields, idx) = spec;
        let (name, value) = fields[idx];
        let record = render_record(&fields);
        let src = formatdoc! {r#"
            module M

            getField : {{ {name} : Int | _ }} -> Int
            let getField rec = rec.{name}

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (getField {record}))
        "#};
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "leak or failure: {}", out);
        prop_assert_eq!(out.trim().parse::<i64>().expect("integer output"), value);
    }

    /// The *same* accessor, applied to two records that place the field at
    /// different slots, reads the right value from each — per-call-site evidence.
    #[test]
    fn row_polymorphic_access_is_consistent_across_layouts(
        a in record_spec(),
        b in record_spec(),
    ) {
        let (fa, ia) = a;
        let (name, va) = fa[ia];
        // Build `b` so it also contains `name` exactly once (drop any existing
        // entry, then add it) with a distinct value, so one accessor must read
        // both records correctly despite different field layouts.
        let (fb, _) = b;
        let vb = 1_000 + va;
        let mut fb: Vec<(&str, i64)> = fb.into_iter().filter(|(n, _)| *n != name).collect();
        fb.push((name, vb));
        let (ra, rb) = (render_record(&fa), render_record(&fb));
        let src = formatdoc! {r#"
            module M

            getField : {{ {name} : Int | _ }} -> Int
            let getField rec = rec.{name}

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (getField {ra} + getField {rb}))
        "#};
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "leak or failure: {}", out);
        prop_assert_eq!(out.trim().parse::<i64>().expect("integer output"), va + vb);
    }

    /// Calling an interface method returns that method's value no matter how many
    /// methods the interface declares — the dictionary slot is found by name.
    #[test]
    fn interface_method_dispatch_finds_the_method(spec in interface_spec()) {
        let (methods, idx) = spec;
        let (name, value) = methods[idx];
        let decls = methods
            .iter()
            .map(|(n, _)| format!("  {n} : Unit -> Int"))
            .collect::<Vec<_>>()
            .join("\n");
        let impls = methods
            .iter()
            .map(|(n, v)| format!("{n} u = {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = formatdoc! {r#"
            module M

            interface Thing =
            {decls}

            let inst = {{ Thing with {impls} }}

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (inst.{name} ()))
        "#};
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "leak or failure: {}", out);
        prop_assert_eq!(out.trim().parse::<i64>().expect("integer output"), value);
    }

    #[test]
    fn jit_matches_reference_evaluator(
        e in expr(),
        x in any::<i64>().prop_filter("avoid i64::MIN rendering", |v| *v != i64::MIN),
    ) {
        let expected = eval(&e, x);
        let src = formatdoc! {r#"
            module M

            let f x = {}

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (f {}))
        "#,
            render(&e),
            render_arg(x),
        };
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "program leaked or failed: {}", out);
        let got: i64 = out.trim().parse().expect("integer output");
        prop_assert_eq!(got, expected);
    }

    #[test]
    fn jit_matches_reference_boolean_evaluator(
        b in bool_expr(),
        x in any::<i64>().prop_filter("avoid i64::MIN rendering", |v| *v != i64::MIN),
    ) {
        let expected = if eval_b(&b, x) { 1 } else { 0 };
        let src = formatdoc! {r#"
            module M

            let f x = if {} then 1 else 0

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (f {}))
        "#,
            render_b(&b),
            render_arg(x),
        };
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "program leaked or failed: {}", out);
        let got: i64 = out.trim().parse().expect("integer output");
        prop_assert_eq!(got, expected);
    }

    /// A function reached through a `let` alias (`let g = f`) is copy-propagated to
    /// a direct call to `f`; the result and a leak-free exit must match the direct
    /// form, over arbitrary bodies.
    #[test]
    fn jit_aliased_call_matches_reference_evaluator(
        e in expr(),
        x in any::<i64>().prop_filter("avoid i64::MIN rendering", |v| *v != i64::MIN),
    ) {
        let expected = eval(&e, x);
        let src = formatdoc! {r#"
            module M

            let f x = {}

            public main : Runtime -> Unit
            let main r =
              let g = f
              r.console.writeLine (Int.toString (g {}))
        "#,
            render(&e),
            render_arg(x),
        };
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "program leaked or failed: {}", out);
        let got: i64 = out.trim().parse().expect("integer output");
        prop_assert_eq!(got, expected);
    }

    /// An over-application (`f x y`, where `f x` returns a closure) direct-calls the
    /// saturated prefix and `apply_n`s the surplus; the result and a leak-free exit
    /// must equal `e(x) + y`, over arbitrary prefix bodies.
    #[test]
    fn jit_over_application_matches_reference_evaluator(
        e in expr(),
        x in any::<i64>().prop_filter("avoid i64::MIN rendering", |v| *v != i64::MIN),
        y in (-1000i64..1000),
    ) {
        let expected = eval(&e, x).wrapping_add(y);
        let src = formatdoc! {r#"
            module M

            let f x = fun w -> {} + w

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (f {} {}))
        "#,
            render(&e),
            render_arg(x),
            render_arg(y),
        };
        let (code, out) = run(&src);
        prop_assert_eq!(code, 0, "program leaked or failed: {}", out);
        let got: i64 = out.trim().parse().expect("integer output");
        prop_assert_eq!(got, expected);
    }
}
