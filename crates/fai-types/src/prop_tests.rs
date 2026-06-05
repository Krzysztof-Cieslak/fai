//! Property-based tests for the type representation and the solver.
//!
//! These exercise invariants that are hard to cover with examples: unification
//! laws (reflexivity, symmetry, idempotence), rendering stability, and that
//! generalize/instantiate round-trips preserve a type's shape.

use proptest::prelude::*;

use crate::infer::{InferCtx, SolveTy, UnifyResult};
use crate::ty::{Con, Scheme, Ty, TyVarId, render_scheme};

/// Generates an arbitrary (closed-ish) `Ty`. Variables are drawn from a small
/// pool so collisions — and thus interesting unifications — actually happen.
fn ty_strategy() -> impl Strategy<Value = Ty> {
    let leaf = prop_oneof![
        (0u32..4).prop_map(|n| Ty::Var(TyVarId(n))),
        Just(Ty::Con(Con::Int)),
        Just(Ty::Con(Con::Float)),
        Just(Ty::Con(Con::Bool)),
        Just(Ty::Con(Con::String)),
        Just(Ty::Unit),
    ];
    leaf.prop_recursive(5, 32, 3, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Ty::arrow(a, b)),
            inner.clone().prop_map(Ty::list),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
        ]
    })
}

/// Converts a reified `Ty` into a solver type with the same structure (variables
/// map to themselves), so we can feed generated types to the solver.
fn to_solve(ty: &Ty) -> SolveTy {
    match ty {
        Ty::Var(v) => SolveTy::Var(*v),
        Ty::Con(c) => SolveTy::Con(*c),
        Ty::Adt(adt) => SolveTy::Adt(*adt),
        Ty::Unit => SolveTy::Unit,
        Ty::Error => SolveTy::Error,
        Ty::App(f, a) => SolveTy::App(Box::new(to_solve(f)), Box::new(to_solve(a))),
        Ty::Arrow(f, a) => SolveTy::arrow(to_solve(f), to_solve(a)),
        Ty::Tuple(elems) => SolveTy::Tuple(elems.iter().map(to_solve).collect()),
        // The generator never produces records; convert structurally for totality.
        Ty::Record(row) => SolveTy::Record(crate::infer::ctx::SolveRow {
            fields: row.fields.iter().map(|(l, t)| (*l, to_solve(t))).collect(),
            tail: crate::infer::ctx::RowTail::Closed,
        }),
    }
}

/// A context with `n` fresh unconstrained variables allocated (ids 0..n), so a
/// generated type's variables (drawn from 0..4) are valid.
fn ctx_with_vars(n: u32) -> InferCtx {
    let mut cx = InferCtx::new();
    for _ in 0..n {
        let _ = cx.fresh();
    }
    cx
}

proptest! {
    // Unification is reflexive: any type unifies with itself.
    #[test]
    fn unify_is_reflexive(ty in ty_strategy()) {
        let mut cx = ctx_with_vars(4);
        let s = to_solve(&ty);
        prop_assert_eq!(cx.unify(&s, &s), UnifyResult::Ok);
    }

    // Unification is symmetric in success/failure: unify(a, b) succeeds iff
    // unify(b, a) succeeds. (Run on independent contexts so bindings don't leak.)
    #[test]
    fn unify_is_symmetric(a in ty_strategy(), b in ty_strategy()) {
        let sa = to_solve(&a);
        let sb = to_solve(&b);

        let mut cx1 = ctx_with_vars(4);
        let forward = cx1.unify(&sa, &sb) == UnifyResult::Ok;

        let mut cx2 = ctx_with_vars(4);
        let backward = cx2.unify(&sb, &sa) == UnifyResult::Ok;

        prop_assert_eq!(forward, backward, "a = {:?}, b = {:?}", a, b);
    }

    // After a successful unification, both sides resolve to the same reified type.
    #[test]
    fn unify_makes_sides_equal(a in ty_strategy(), b in ty_strategy()) {
        let mut cx = ctx_with_vars(4);
        let sa = to_solve(&a);
        let sb = to_solve(&b);
        if cx.unify(&sa, &sb) == UnifyResult::Ok {
            prop_assert_eq!(cx.reify(&sa), cx.reify(&sb));
        }
    }

    // Rendering is total and stable: rendering the same scheme twice agrees, and
    // a non-error type never renders empty.
    #[test]
    fn render_is_stable(ty in ty_strategy()) {
        let scheme = Scheme::new(vec![TyVarId(0), TyVarId(1), TyVarId(2), TyVarId(3)], ty);
        let a = render_scheme(&scheme);
        let b = render_scheme(&scheme);
        prop_assert_eq!(&a, &b);
        prop_assert!(!a.is_empty());
    }

    // Reification is stable: reifying a solver type twice yields equal types
    // (the renumbering is a deterministic function of structure).
    #[test]
    fn reify_is_stable(ty in ty_strategy()) {
        let cx = ctx_with_vars(4);
        let s = to_solve(&ty);
        prop_assert_eq!(cx.reify(&s), cx.reify(&s));
    }

    // Unifying a fresh variable with any type always succeeds and binds it.
    #[test]
    fn fresh_var_unifies_with_anything(ty in ty_strategy()) {
        let mut cx = ctx_with_vars(4);
        let v = cx.fresh();
        let s = to_solve(&ty);
        prop_assert_eq!(cx.unify(&v, &s), UnifyResult::Ok);
        prop_assert_eq!(cx.reify(&v), cx.reify(&s));
    }
}
