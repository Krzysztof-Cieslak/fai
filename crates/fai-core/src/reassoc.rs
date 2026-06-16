//! Left-reassociation of `++` (string concatenation) chains, applied to the Core
//! just before reference counting.
//!
//! `++` is right-associative — `a ++ b ++ c` parses as `a ++ (b ++ c)` — and after
//! the prim/helper inliners both an infix `++` and a `Prim.stringConcat` call are a
//! [`Prim::StrConcat`] node, so a source chain is a right-leaning tree of them. The
//! runtime appends into a concatenation's *left* operand in place when that operand
//! is uniquely owned. A right-leaning chain never offers a unique left accumulator
//! (each left operand is a fresh, small piece), so a long chain re-copies the
//! growing suffix at every step — O(n²). Rewriting a maximal StrConcat tree to
//! **left-nested** form (`(a ++ b) ++ c`) makes the growing prefix the left operand
//! so the in-place append fires: building a chain becomes amortized O(total length).
//!
//! The rewrite is **behavior-preserving**: concatenation is pure and associative,
//! and code generation evaluates a primitive's operands left to right, so flattening
//! a tree to its ordered leaves and rebuilding it left-nested leaves the operand
//! evaluation order — and therefore any effects performed while computing the
//! operands — unchanged.

use crate::ir::{CExpr, CoreFn, ExprKind as K, LoweredDef, Prim};

/// Returns `def` with every `++` chain in every function body rewritten to
/// left-nested form. Cheap and behavior-preserving; intended to run on the
/// pre-reference-counting Core (it is a structural identity elsewhere).
#[must_use]
pub fn reassociate_concat(def: &LoweredDef) -> LoweredDef {
    let fns = def
        .fns
        .iter()
        .map(|f| CoreFn {
            params: f.params.clone(),
            captures: f.captures.clone(),
            body: reassoc(&f.body),
        })
        .collect();
    LoweredDef {
        def: def.def,
        fns,
        entry_borrowed: def.entry_borrowed.clone(),
        reuse_entry: def.reuse_entry.clone(),
        entry_spread_params: def.entry_spread_params.clone(),
    }
}

/// Rewrites `e`: if it heads a StrConcat tree, flatten that tree to its ordered
/// leaves and rebuild it left-nested; otherwise recurse structurally.
fn reassoc(e: &CExpr) -> CExpr {
    if let K::Prim { op: Prim::StrConcat, .. } = &e.kind {
        let mut leaves = Vec::new();
        flatten_concat(e, &mut leaves);
        // A maximal StrConcat tree has at least two leaves (a binary node); fold
        // them left-associatively. Each rebuilt node is a `String` like the input.
        let mut leaves = leaves.into_iter();
        let mut acc = leaves.next().expect("a StrConcat node has operands");
        for leaf in leaves {
            acc = CExpr::new(K::Prim { op: Prim::StrConcat, args: vec![acc, leaf] }, e.ty.clone());
        }
        return acc;
    }
    map_children(e, reassoc)
}

/// Appends the operands of the maximal StrConcat tree rooted at `e` to `out` in
/// left-to-right order, descending through nested StrConcat nodes. A non-concat
/// operand is itself reassociated (it may contain other chains) before being
/// pushed as a leaf.
fn flatten_concat(e: &CExpr, out: &mut Vec<CExpr>) {
    if let K::Prim { op: Prim::StrConcat, args } = &e.kind {
        for arg in args {
            flatten_concat(arg, out);
        }
    } else {
        out.push(reassoc(e));
    }
}

/// Rebuilds `e` with `f` applied to each child expression, preserving its kind,
/// type, and all non-expression fields. Total over every [`ExprKind`] so the pass
/// is a correct structural identity on forms it does not target (including the
/// reference-counting/reuse forms, should it ever run on a post-`rc` body).
fn map_children(e: &CExpr, f: fn(&CExpr) -> CExpr) -> CExpr {
    let kind = match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => e.kind.clone(),
        K::Prim { op, args } => K::Prim { op: *op, args: args.iter().map(f).collect() },
        K::Foreign { symbol, args } => {
            K::Foreign { symbol: *symbol, args: args.iter().map(f).collect() }
        }
        K::App { func, args, reuse, alloc } => K::App {
            func: Box::new(f(func)),
            args: args.iter().map(f).collect(),
            reuse: reuse.clone(),
            alloc: *alloc,
        },
        K::If { cond, then, els } => {
            K::If { cond: Box::new(f(cond)), then: Box::new(f(then)), els: Box::new(f(els)) }
        }
        K::Let { local, value, body } => {
            K::Let { local: *local, value: Box::new(f(value)), body: Box::new(f(body)) }
        }
        K::Spread { components } => K::Spread { components: components.iter().map(f).collect() },
        K::LetMany { locals, value, body } => K::LetMany {
            locals: locals.clone(),
            value: Box::new(f(value)),
            body: Box::new(f(body)),
        },
        K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
            tag: *tag,
            args: args.iter().map(f).collect(),
            reuse: *reuse,
            scalars: *scalars,
            niche: *niche,
        },
        K::DataTag { base, niche } => K::DataTag { base: Box::new(f(base)), niche: *niche },
        K::DataField { base, index, scalar, niche } => {
            K::DataField { base: Box::new(f(base)), index: *index, scalar: *scalar, niche: *niche }
        }
        K::Reset { value, token, body } => {
            K::Reset { value: Box::new(f(value)), token: *token, body: Box::new(f(body)) }
        }
        K::FreeReuse { token, body } => K::FreeReuse { token: *token, body: Box::new(f(body)) },
        K::Dup { local, body } => K::Dup { local: *local, body: Box::new(f(body)) },
        K::Drop { local, body } => K::Drop { local: *local, body: Box::new(f(body)) },
        K::Join { params, body } => K::Join { params: params.clone(), body: Box::new(f(body)) },
        K::Recur { args } => K::Recur { args: args.iter().map(f).collect() },
        K::HoleStart { hole, body } => K::HoleStart { hole: *hole, body: Box::new(f(body)) },
        K::HoleFill { hole, cell, field } => {
            K::HoleFill { hole: *hole, cell: Box::new(f(cell)), field: *field }
        }
        K::HoleClose { hole, base } => K::HoleClose { hole: *hole, base: Box::new(f(base)) },
    };
    CExpr::new(kind, e.ty.clone())
}

#[cfg(test)]
mod tests {
    use fai_resolve::LocalId;
    use fai_types::{Con, Ty};

    use super::reassoc;
    use crate::ir::{CExpr, ExprKind as K, Prim};

    fn sty() -> Ty {
        Ty::Con(Con::String)
    }

    /// A distinct leaf, identified by its local index in [`show`].
    fn local(i: usize) -> CExpr {
        CExpr::new(K::Local(LocalId::from_index(i)), sty())
    }

    fn cat(l: CExpr, r: CExpr) -> CExpr {
        CExpr::new(K::Prim { op: Prim::StrConcat, args: vec![l, r] }, sty())
    }

    /// Renders an expression's concat nesting so a test can pin the exact tree
    /// shape *and* the left-to-right leaf order.
    fn show(e: &CExpr) -> String {
        match &e.kind {
            K::Prim { op: Prim::StrConcat, args } => {
                format!("({} ++ {})", show(&args[0]), show(&args[1]))
            }
            K::Local(l) => l.index().to_string(),
            K::If { cond, then, els } => {
                format!("if({}, {}, {})", show(cond), show(then), show(els))
            }
            K::Let { local, value, body } => {
                format!("let {} = {} in {}", local.index(), show(value), show(body))
            }
            other => panic!("unexpected node in test tree: {other:?}"),
        }
    }

    #[track_caller]
    fn assert_reassoc(input: CExpr, expected: &str) {
        assert_eq!(show(&reassoc(&input)), expected);
    }

    #[test]
    fn single_concat_is_unchanged() {
        assert_reassoc(cat(local(0), local(1)), "(0 ++ 1)");
    }

    #[test]
    fn right_nested_chain_becomes_left_nested() {
        // `0 ++ (1 ++ 2)` -> `(0 ++ 1) ++ 2`, leaf order preserved.
        assert_reassoc(cat(local(0), cat(local(1), local(2))), "((0 ++ 1) ++ 2)");
    }

    #[test]
    fn deep_right_chain_is_fully_left_nested() {
        let input = cat(local(0), cat(local(1), cat(local(2), cat(local(3), local(4)))));
        assert_reassoc(input, "((((0 ++ 1) ++ 2) ++ 3) ++ 4)");
    }

    #[test]
    fn balanced_tree_flattens_left_in_order() {
        // `(0 ++ 1) ++ (2 ++ 3)` -> `((0 ++ 1) ++ 2) ++ 3`.
        let input = cat(cat(local(0), local(1)), cat(local(2), local(3)));
        assert_reassoc(input, "(((0 ++ 1) ++ 2) ++ 3)");
    }

    #[test]
    fn chain_inside_if_branch_is_reassociated() {
        let input = CExpr::new(
            K::If {
                cond: Box::new(local(9)),
                then: Box::new(cat(local(0), cat(local(1), local(2)))),
                els: Box::new(local(3)),
            },
            sty(),
        );
        assert_reassoc(input, "if(9, ((0 ++ 1) ++ 2), 3)");
    }

    #[test]
    fn chain_inside_let_value_and_body_is_reassociated() {
        let input = CExpr::new(
            K::Let {
                local: LocalId::from_index(5),
                value: Box::new(cat(local(0), cat(local(1), local(2)))),
                body: Box::new(cat(local(3), cat(local(4), local(6)))),
            },
            sty(),
        );
        assert_reassoc(input, "let 5 = ((0 ++ 1) ++ 2) in ((3 ++ 4) ++ 6)");
    }

    #[test]
    fn non_concat_leaf_is_preserved_and_reassociated_within() {
        // A chain whose middle operand is an `if` keeps that operand as one leaf,
        // in position, and reassociates any chain nested inside it.
        let inner = CExpr::new(
            K::If {
                cond: Box::new(local(9)),
                then: Box::new(cat(local(1), cat(local(2), local(3)))),
                els: Box::new(local(4)),
            },
            sty(),
        );
        let input = cat(local(0), cat(inner, local(5)));
        assert_reassoc(input, "((0 ++ if(9, ((1 ++ 2) ++ 3), 4)) ++ 5)");
    }
}
