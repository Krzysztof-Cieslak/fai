//! Intrinsic inlining: replacing a saturated call to an eta-expanded primitive
//! re-export with the primitive itself, before reference counting.
//!
//! The standard library re-exports each Rust intrinsic under a clean qualified
//! name through a one-line wrapper — `let toString n = Prim.intToString n`,
//! `let push x xs = Prim.arrayPush xs x` — so a use site is two calls deep
//! (caller → wrapper → runtime primitive). [`prim_wrapper`] recognizes such a
//! wrapper (a body that is exactly a primitive applied to a permutation of the
//! parameters), and [`core_inlined`] rewrites a saturated call to one into the
//! primitive itself, removing the wrapper hop.
//!
//! The rewrite runs on the pre-reference-counting Core, so reference counting then
//! balances the resulting primitive directly — a borrowing primitive such as
//! `stringLength` borrows its operand exactly as the wrapper's borrow signature
//! said it did, so an owned argument is still dropped at the call site. A wrapper
//! used first-class (not in a saturated call) keeps its `Global` reference, so it
//! is still compiled; one reached only through now-inlined calls drops out of
//! [`LoweredDef::referenced_globals`] and is dead-code-eliminated.

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_resolve::LocalId;
use fai_syntax::Symbol;
use fai_types::Ty;

use crate::core;
use crate::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, LoweredDef, Prim};

/// A definition that is exactly an eta-expanded primitive: a body of the form
/// `fun p0 … pk -> Prim.op <a permutation of p0 … pk>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimWrapper {
    /// The wrapped primitive.
    pub op: Prim,
    /// The source-parameter index supplying each primitive operand, in operand
    /// order — a permutation of `0..arity` (every parameter used exactly once).
    pub slots: Vec<usize>,
}

/// Whether `name`'s definition in `file` is a strict-bijection eta-prim-wrapper.
///
/// `Some` iff the lowered definition is a single function (no lifted lambdas) with
/// no captures whose body is exactly `Prim { op, operands }`, where the operands
/// are a permutation of the parameters (each used exactly once) and `op.arity()`
/// equals the parameter count. This rejects a nullary constant wrapper
/// (`Array.empty = Prim.arrayWithCapacity 0`, whose operand is a literal), a
/// wrapper with a nested body (`Array.isEmpty xs = Prim.arrayLength xs = 0`), and
/// any row-polymorphic wrapper (whose leading offset-evidence parameters never
/// appear among the operands).
///
/// The result is a tiny, body-derived value, so editing a wrapper's body ripples
/// to its callers (through [`core_inlined`]) only when the recognized primitive or
/// permutation actually changes (salsa early cutoff). Reads the non-inlined
/// [`core`] — a wrapper body is already a bare primitive, and reading the raw form
/// keeps this independent of [`core_inlined`] (no query cycle).
#[salsa::tracked]
pub fn prim_wrapper(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<PrimWrapper> {
    let lowered = core(db, file, name);
    if lowered.fns.len() != 1 {
        return None;
    }
    let entry = lowered.entry();
    if !entry.captures.is_empty() {
        return None;
    }
    let K::Prim { op, args } = &entry.body.kind else {
        return None;
    };
    let op = *op;
    // Saturated and arity-exact: one operand per parameter.
    if args.len() != op.arity() || entry.params.len() != op.arity() {
        return None;
    }
    let mut slots = Vec::with_capacity(args.len());
    let mut used = vec![false; entry.params.len()];
    for a in args {
        let K::Local(l) = &a.kind else {
            return None;
        };
        let pos = entry.params.iter().position(|p| p == l)?;
        if used[pos] {
            return None; // a parameter used twice — not a bijection
        }
        used[pos] = true;
        slots.push(pos);
    }
    Some(PrimWrapper { op, slots })
}

/// `name`'s lowered definition with every saturated call to an eta-prim-wrapper
/// replaced by the wrapped primitive.
///
/// This is the back end's view of Core: reference counting, borrow inference, the
/// mutual-recursion combined loop, and reachability read this rather than the raw
/// [`core`], so the wrapper hop is gone before any of them run.
#[salsa::tracked]
pub fn core_inlined(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let lowered = core(db, file, name);
    let mut next = next_free_local(&lowered);
    let mut changed = false;
    let fns: Vec<CoreFn> = lowered
        .fns
        .iter()
        .map(|f| CoreFn {
            params: f.params.clone(),
            captures: f.captures.clone(),
            body: inline_expr(db, &f.body, &mut next, &mut changed),
        })
        .collect();
    // Nothing inlined: return the original `Arc` so salsa's pointer-equality fast
    // path gives O(1) early cutoff for the common (no-wrapper-call) definition.
    if !changed {
        return lowered;
    }
    Arc::new(LoweredDef { def: lowered.def, fns, entry_borrowed: lowered.entry_borrowed.clone() })
}

/// Rewrites a single expression, inlining every saturated eta-prim-wrapper call it
/// contains. Sets `changed` when any rewrite happens.
fn inline_expr(db: &dyn Db, e: &CExpr, next: &mut usize, changed: &mut bool) -> CExpr {
    let ty = e.ty.clone();
    match &e.kind {
        K::App { func, args } => {
            if let K::Global(def) = &func.kind
                && let Some(callee_file) = db.source_file(def.file)
                && let Some(pw) = prim_wrapper(db, callee_file, def.name)
                && args.len() == pw.op.arity()
            {
                *changed = true;
                let args: Vec<CExpr> =
                    args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
                return build_prim(&pw, args, ty, next);
            }
            let func = Box::new(inline_expr(db, func, next, changed));
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(K::App { func, args }, ty)
        }
        K::Prim { op, args } => {
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(K::Prim { op: *op, args }, ty)
        }
        K::MakeData { tag, args, reuse, scalars } => {
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(K::MakeData { tag: *tag, args, reuse: *reuse, scalars: *scalars }, ty)
        }
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(inline_expr(db, cond, next, changed)),
                then: Box::new(inline_expr(db, then, next, changed)),
                els: Box::new(inline_expr(db, els, next, changed)),
            },
            ty,
        ),
        K::Let { local, value, body } => CExpr::new(
            K::Let {
                local: *local,
                value: Box::new(inline_expr(db, value, next, changed)),
                body: Box::new(inline_expr(db, body, next, changed)),
            },
            ty,
        ),
        K::DataTag(base) => {
            CExpr::new(K::DataTag(Box::new(inline_expr(db, base, next, changed))), ty)
        }
        K::DataField { base, index, scalar } => CExpr::new(
            K::DataField {
                base: Box::new(inline_expr(db, base, next, changed)),
                index: *index,
                scalar: *scalar,
            },
            ty,
        ),
        // Leaves, and nodes with no expression children, are copied unchanged. A
        // lifted lambda's body is a separate `CoreFn`, inlined by `core_inlined`'s
        // loop over `fns`, not here; its captures are locals, not expressions.
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
            CExpr::new(e.kind.clone(), ty)
        }
        // The reference-counting and tail-call nodes do not exist in the pre-count
        // Core this runs on; reconstructed with recursed children for completeness.
        K::Reset { value, token, body } => CExpr::new(
            K::Reset {
                value: Box::new(inline_expr(db, value, next, changed)),
                token: *token,
                body: Box::new(inline_expr(db, body, next, changed)),
            },
            ty,
        ),
        K::FreeReuse { token, body } => CExpr::new(
            K::FreeReuse { token: *token, body: Box::new(inline_expr(db, body, next, changed)) },
            ty,
        ),
        K::Dup { local, body } => CExpr::new(
            K::Dup { local: *local, body: Box::new(inline_expr(db, body, next, changed)) },
            ty,
        ),
        K::Drop { local, body } => CExpr::new(
            K::Drop { local: *local, body: Box::new(inline_expr(db, body, next, changed)) },
            ty,
        ),
        K::Join { params, body } => CExpr::new(
            K::Join {
                params: params.clone(),
                body: Box::new(inline_expr(db, body, next, changed)),
            },
            ty,
        ),
        K::Recur { args } => CExpr::new(
            K::Recur { args: args.iter().map(|a| inline_expr(db, a, next, changed)).collect() },
            ty,
        ),
        K::HoleStart { hole, body } => CExpr::new(
            K::HoleStart { hole: *hole, body: Box::new(inline_expr(db, body, next, changed)) },
            ty,
        ),
        K::HoleFill { hole, cell, field } => CExpr::new(
            K::HoleFill {
                hole: *hole,
                cell: Box::new(inline_expr(db, cell, next, changed)),
                field: *field,
            },
            ty,
        ),
        K::HoleClose { hole, base } => CExpr::new(
            K::HoleClose { hole: *hole, base: Box::new(inline_expr(db, base, next, changed)) },
            ty,
        ),
    }
}

/// Builds the primitive node for a saturated wrapper call, given the (already
/// inlined) call arguments in source order.
fn build_prim(pw: &PrimWrapper, args: Vec<CExpr>, ty: Ty, next: &mut usize) -> CExpr {
    // Identity permutation (every String/Int/Float/Char wrapper, plus
    // `Array.length`/`withCapacity`): splice the arguments straight into the
    // primitive, in order — no extra bindings.
    if pw.slots.iter().enumerate().all(|(j, &s)| j == s) {
        return CExpr::new(K::Prim { op: pw.op, args }, ty);
    }
    // A non-identity permutation (`Array.push`/`unsafeGet`/`unsafeSet`) would
    // reorder argument evaluation if spliced directly. Bind each argument to a
    // fresh local in source order, then reference them through the permutation, so
    // evaluation order (hence trap order) is preserved.
    let locals: Vec<(LocalId, CExpr)> = args
        .into_iter()
        .map(|a| {
            let l = LocalId::from_index(*next);
            *next += 1;
            (l, a)
        })
        .collect();
    let operands: Vec<CExpr> = pw
        .slots
        .iter()
        .map(|&s| CExpr::new(K::Local(locals[s].0), locals[s].1.ty.clone()))
        .collect();
    let mut body = CExpr::new(K::Prim { op: pw.op, args: operands }, ty);
    for (l, value) in locals.into_iter().rev() {
        let let_ty = body.ty.clone();
        body =
            CExpr::new(K::Let { local: l, value: Box::new(value), body: Box::new(body) }, let_ty);
    }
    body
}

/// The first local slot unused anywhere in `lowered`, so the bindings the inliner
/// synthesizes for a permuted wrapper never collide with an existing local (and
/// reference counting, which scans the same way, continues above them).
pub(crate) fn next_free_local(lowered: &LoweredDef) -> usize {
    let mut max = 0usize;
    for f in &lowered.fns {
        for &p in &f.params {
            max = max.max(p.index() + 1);
        }
        for &c in &f.captures {
            max = max.max(c.index() + 1);
        }
        max_local(&f.body, &mut max);
    }
    max
}

fn max_local(e: &CExpr, max: &mut usize) {
    let bump = |l: LocalId, max: &mut usize| *max = (*max).max(l.index() + 1);
    match &e.kind {
        K::Local(x) => bump(*x, max),
        K::Lit(_) | K::Global(_) | K::Error => {}
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => {
            args.iter().for_each(|a| max_local(a, max));
        }
        K::App { func, args } => {
            max_local(func, max);
            args.iter().for_each(|a| max_local(a, max));
        }
        K::If { cond, then, els } => {
            max_local(cond, max);
            max_local(then, max);
            max_local(els, max);
        }
        K::Let { local, value, body } => {
            bump(*local, max);
            max_local(value, max);
            max_local(body, max);
        }
        K::MakeClosure { captures, .. } => captures.iter().for_each(|c| bump(*c, max)),
        K::DataTag(base) => max_local(base, max),
        K::DataField { base, index, .. } => {
            max_local(base, max);
            if let FieldIndex::Dyn { evidence, .. } = index {
                bump(*evidence, max);
            }
        }
        K::Reset { value, token, body } => {
            bump(*token, max);
            max_local(value, max);
            max_local(body, max);
        }
        K::FreeReuse { token, body } => {
            bump(*token, max);
            max_local(body, max);
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            bump(*local, max);
            max_local(body, max);
        }
        K::Join { params, body } => {
            params.iter().for_each(|p| bump(*p, max));
            max_local(body, max);
        }
        K::HoleStart { hole, body } => {
            bump(*hole, max);
            max_local(body, max);
        }
        K::HoleFill { hole, cell, .. } => {
            bump(*hole, max);
            max_local(cell, max);
        }
        K::HoleClose { hole, base } => {
            bump(*hole, max);
            max_local(base, max);
        }
    }
}
