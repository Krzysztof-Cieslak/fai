//! Property-based soundness: randomly generated, well-typed programs are
//! reference-count sound.
//!
//! Each generator emits a *structural* description (no names, no free choices
//! that could mistype) and renders it to source under a fixed typing discipline,
//! so every generated program typechecks by construction. The soundness oracle
//! ([`crate::check_rc`], driven via [`crate::tests::check_program`]) then walks
//! the reference-counted result on every path. `check_program` itself rejects any
//! program that fails to typecheck, so a generator bug surfaces as a test failure
//! rather than a vacuous pass.
//!
//! The generators target the constructs where ownership is interesting: list
//! recursion that destructures and rebuilds (the reuse-shaped path), higher
//! shapes that only inspect, record literal/update chains, algebraic-type
//! construction and exhaustive `match`, and deep `let` sharing (the dup path).

use indoc::formatdoc;
use proptest::prelude::*;

use crate::tests::check_program;

// ---------------------------------------------------------------------------
// A small, always-well-typed `Int` expression over a set of in-scope variables.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
}

impl Op {
    fn sym(self) -> char {
        match self {
            Op::Add => '+',
            Op::Sub => '-',
            Op::Mul => '*',
        }
    }
}

/// An `Int`-typed expression. `Var(i)` selects an in-scope `Int` variable by
/// `i % vars.len()` at render time (falling back to a literal when none are in
/// scope), so the expression is well typed for any variable set.
#[derive(Debug, Clone)]
enum IntE {
    Lit(u16),
    Var(usize),
    Bin(Op, Box<IntE>, Box<IntE>),
    IfLt(Box<IntE>, Box<IntE>, Box<IntE>, Box<IntE>),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::Add), Just(Op::Sub), Just(Op::Mul)]
}

fn int_e() -> impl Strategy<Value = IntE> {
    let leaf = prop_oneof![(0u16..1000).prop_map(IntE::Lit), (0usize..4).prop_map(IntE::Var),];
    leaf.prop_recursive(4, 48, 4, |inner| {
        prop_oneof![
            (op(), inner.clone(), inner.clone()).prop_map(|(o, a, b)| IntE::Bin(
                o,
                Box::new(a),
                Box::new(b)
            )),
            (inner.clone(), inner.clone(), inner.clone(), inner.clone()).prop_map(
                |(a, b, t, e)| { IntE::IfLt(Box::new(a), Box::new(b), Box::new(t), Box::new(e)) }
            ),
        ]
    })
}

/// Renders an [`IntE`] to source. `vars` are the names in scope (each `Int`).
fn render_int(e: &IntE, vars: &[&str]) -> String {
    match e {
        IntE::Lit(n) => n.to_string(),
        IntE::Var(i) => {
            if vars.is_empty() {
                "0".to_string()
            } else {
                vars[i % vars.len()].to_string()
            }
        }
        IntE::Bin(o, a, b) => {
            format!("({} {} {})", render_int(a, vars), o.sym(), render_int(b, vars))
        }
        IntE::IfLt(a, b, t, e) => format!(
            "(if {} < {} then {} else {})",
            render_int(a, vars),
            render_int(b, vars),
            render_int(t, vars),
            render_int(e, vars),
        ),
    }
}

// ---------------------------------------------------------------------------
// A composition pipeline stage (an `Int -> Int` function), for the closure
// confinement (`simplify`) path: random `>>` chains of lambdas, partial
// applications, and `identity`, which the simplifier reduces before reference
// counting. The rewritten body must still be reference-count sound.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Stage {
    /// `(fun x -> x + k)` — a non-capturing lambda.
    AddK(u16),
    /// `(fun x -> x * k)` — a non-capturing lambda.
    MulK(u16),
    /// `shift k` — a partial application of a same-file binary function.
    ShiftK(u16),
    /// `scale k` — a partial application of a same-file binary function.
    ScaleK(u16),
    /// `identity` — a `Prelude` combinator.
    Id,
}

fn stage() -> impl Strategy<Value = Stage> {
    prop_oneof![
        (0u16..100).prop_map(Stage::AddK),
        (1u16..20).prop_map(Stage::MulK),
        (0u16..100).prop_map(Stage::ShiftK),
        (1u16..20).prop_map(Stage::ScaleK),
        Just(Stage::Id),
    ]
}

fn render_stage(s: &Stage) -> String {
    match s {
        Stage::AddK(k) => format!("(fun x -> x + {k})"),
        Stage::MulK(k) => format!("(fun x -> x * {k})"),
        Stage::ShiftK(k) => format!("shift {k}"),
        Stage::ScaleK(k) => format!("scale {k}"),
        Stage::Id => "identity".to_string(),
    }
}

/// Renders the stages into a `>>` chain (`s1 >> s2 >> …`).
fn render_chain(stages: &[Stage]) -> String {
    stages.iter().map(render_stage).collect::<Vec<_>>().join(" >> ")
}

proptest! {
    // A composed/partially-applied `transform` CAF folded over a range — the
    // confinement path. The simplifier inlines the CAF, reduces the `>>`
    // chain, and beta-reduces the lambdas; the resulting body must be reference-count
    // sound for any chain shape.
    #[test]
    fn composed_caf_pipeline_is_sound(stages in prop::collection::vec(stage(), 1..6)) {
        let chain = render_chain(&stages);
        let src = formatdoc! {r#"
            module M

            let shift k x = x + k

            let scale k x = x * k

            let transform = {chain}

            let run n = List.foldl (fun acc x -> acc + transform x) 0 (List.range 0 n)
        "#};
        let r = check_program(&src, "run");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }

    // The same composition applied directly (no CAF), so the `>>` reduction and beta
    // fire without CAF inlining. The reduced body must be sound for any chain.
    #[test]
    fn inline_composed_pipeline_is_sound(stages in prop::collection::vec(stage(), 1..6)) {
        let chain = render_chain(&stages);
        let src = formatdoc! {r#"
            module M

            let shift k x = x + k

            let scale k x = x * k

            let run n = List.foldl (fun acc x -> acc + ({chain}) x) 0 (List.range 0 n)
        "#};
        let r = check_program(&src, "run");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }

    // A left fold over `List Int`: destructure each cons, recurse on the tail,
    // and combine the head with the recursive result. Exercises list `match`,
    // recursion, and borrowing/consuming the projected head.
    #[test]
    fn int_fold_over_a_list_is_sound(base in int_e(), combine in int_e()) {
        let src = formatdoc! {r#"
            module M

            let f xs =
              match xs with
              | [] -> {base}
              | h :: t ->
                let r = f t
                {combine}
        "#,
            base = render_int(&base, &[]),
            combine = render_int(&combine, &["h", "r"]),
        };
        let r = check_program(&src, "f");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }

    // A structure-rebuilding recursion over `List Int` (map/filter shaped): the
    // reuse-critical path, where a unique spine's cons cells are recycled in
    // place. `keep` chooses a filtering branch; `head` builds the new element.
    #[test]
    fn list_rebuild_over_a_list_is_sound(head in int_e(), cond in int_e(), keep in any::<bool>()) {
        let arm = if keep {
            format!("if {} < 0 then f t else {} :: f t",
                render_int(&cond, &["h"]), render_int(&head, &["h"]))
        } else {
            format!("{} :: f t", render_int(&head, &["h"]))
        };
        let src = formatdoc! {r#"
            module M

            let f xs =
              match xs with
              | [] -> []
              | h :: t -> {arm}
        "#};
        let r = check_program(&src, "f");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }

    // A record literal plus an update chain over a fixed three-field record.
    // Exercises construction, field projection (borrowing the base), and the
    // in-place `{ r with ... }` update — including using the base twice (which
    // forces a duplicate before the destructive update).
    #[test]
    fn record_update_chain_is_sound(
        a in int_e(), b in int_e(), c in int_e(), twice in any::<bool>(),
    ) {
        let fields = ["r.a", "r.b", "r.c"];
        // Build the indented body by hand: the offside rule makes layout
        // significant, so every body line sits two spaces under `let f r =`.
        let body = if twice {
            format!(
                "  let s = {{ r with a = {a} }}\n  {{ s with b = {b}, c = {c} }}",
                a = render_int(&a, &fields),
                b = render_int(&b, &fields),
                c = render_int(&c, &fields),
            )
        } else {
            format!(
                "  {{ r with a = {}, b = {}, c = {} }}",
                render_int(&a, &fields),
                render_int(&b, &fields),
                render_int(&c, &fields),
            )
        };
        let src = format!(
            "module M\n\ntype R = {{ a : Int, b : Int, c : Int }}\n\nlet f r =\n{body}\n"
        );
        let r = check_program(&src, "f");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }

    // An exhaustive `match` over an algebraic type whose constructors carry
    // varying numbers of `Int` fields. Exercises constructor tag/field
    // projection on every arm and a nullary constructor.
    #[test]
    fn adt_match_is_sound(a in int_e(), b in int_e(), c in int_e()) {
        let src = formatdoc! {r#"
            module M

            type T =
              | A Int
              | B Int Int
              | C

            let f t =
              match t with
              | A x -> {a}
              | B x y -> {b}
              | C -> {c}
        "#,
            a = render_int(&a, &["x"]),
            b = render_int(&b, &["x", "y"]),
            c = render_int(&c, &[]),
        };
        let r = check_program(&src, "f");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }

    // A structure-rebuilding recursion whose cons goes through a small,
    // non-recursive "smart constructor" helper, so the helper inliner folds it back
    // into the caller before reference counting. The folded body must still balance
    // dup/drop on a recycled spine (soundness), and the call must actually be
    // inlined (non-vacuity), across random element expressions.
    #[test]
    fn inlined_smart_constructor_rebuild_is_sound(head in int_e(), cond in int_e(), keep in any::<bool>()) {
        let arm = if keep {
            format!("if {} < 0 then f t else cons ({}) (f t)",
                render_int(&cond, &["h"]), render_int(&head, &["h"]))
        } else {
            format!("cons ({}) (f t)", render_int(&head, &["h"]))
        };
        let src = formatdoc! {r#"
            module M

            let cons h t = h :: t

            let f xs =
              match xs with
              | [] -> []
              | h :: t -> {arm}
        "#};
        let r = check_program(&src, "f");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
        let out = crate::tests::rc_checked(&src, "f");
        prop_assert!(!out.contains("@cons"), "the smart constructor must be inlined:\n{out}");
    }

    // Deeply nested `let` bindings that reuse earlier names, stressing the
    // duplicate-when-still-live path: each binding may be used many times later.
    #[test]
    fn let_sharing_is_sound(e in int_e()) {
        let src = formatdoc! {r#"
            module M

            let f x =
              let a = {e0}
              let b = (a + x)
              let c = ((a + b) * a)
              ((a + b) + (c + c))
        "#,
            e0 = render_int(&e, &["x"]),
        };
        let r = check_program(&src, "f");
        prop_assert!(r.is_ok(), "{}\n{src}", r.unwrap_err());
    }
}
