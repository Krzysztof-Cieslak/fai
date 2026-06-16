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
use rustc_hash::FxHashMap;

use crate::core;
use crate::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, FnId, LoweredDef, Prim};

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

/// A definition that is exactly an eta-expanded foreign call: a body of the form
/// `fun p0 … pk -> foreign <a permutation of p0 … pk>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignWrapper {
    /// The wrapped foreign function's native runtime symbol.
    pub symbol: Symbol,
    /// The source-parameter index supplying each foreign operand, in operand
    /// order — a permutation of `0..arity` (every parameter used exactly once).
    pub slots: Vec<usize>,
    /// Whether the wrapped foreign call uses the marshalled ABI.
    pub marshalled: bool,
}

/// Whether `name`'s definition in `file` is a strict-bijection eta-foreign-wrapper.
///
/// The foreign peer of [`prim_wrapper`]: `Some` iff the lowered definition is a
/// single function with no captures whose body is exactly `Foreign { symbol, args }`
/// where the operands are a permutation of the parameters (each used exactly once).
#[salsa::tracked]
pub fn foreign_wrapper(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<ForeignWrapper> {
    let lowered = core(db, file, name);
    if lowered.fns.len() != 1 {
        return None;
    }
    let entry = lowered.entry();
    if !entry.captures.is_empty() {
        return None;
    }
    let K::Foreign { symbol, args, marshalled } = &entry.body.kind else {
        return None;
    };
    let symbol = *symbol;
    let marshalled = *marshalled;
    if args.len() != entry.params.len() {
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
    Some(ForeignWrapper { symbol, slots, marshalled })
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
    Arc::new(LoweredDef {
        def: lowered.def,
        fns,
        entry_borrowed: lowered.entry_borrowed.clone(),
        reuse_entry: lowered.reuse_entry.clone(),
        entry_spread_params: lowered.entry_spread_params.clone(),
    })
}

/// Rewrites a single expression, inlining every saturated eta-prim-wrapper call it
/// contains. Sets `changed` when any rewrite happens.
fn inline_expr(db: &dyn Db, e: &CExpr, next: &mut usize, changed: &mut bool) -> CExpr {
    let ty = e.ty.clone();
    match &e.kind {
        // The intrinsic inliner runs before reference counting inserts reuse
        // tokens, so `reuse` is always empty here; it is carried through verbatim.
        K::App { func, args, reuse, alloc } => {
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
            // A one-line foreign wrapper (`let send s = netSend s`) folds the same
            // way, so a use site calls the foreign function with no wrapper hop.
            if let K::Global(def) = &func.kind
                && let Some(callee_file) = db.source_file(def.file)
                && let Some(fw) = foreign_wrapper(db, callee_file, def.name)
                && args.len() == fw.slots.len()
            {
                *changed = true;
                let args: Vec<CExpr> =
                    args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
                return build_foreign(&fw, args, ty, next);
            }
            let func = Box::new(inline_expr(db, func, next, changed));
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(K::App { func, args, reuse: reuse.clone(), alloc: *alloc }, ty)
        }
        K::Prim { op, args } => {
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(K::Prim { op: *op, args }, ty)
        }
        K::Foreign { symbol, args, marshalled } => {
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(K::Foreign { symbol: *symbol, args, marshalled: *marshalled }, ty)
        }
        K::MakeData { tag, args, reuse, scalars, niche } => {
            let args = args.iter().map(|a| inline_expr(db, a, next, changed)).collect();
            CExpr::new(
                K::MakeData { tag: *tag, args, reuse: *reuse, scalars: *scalars, niche: *niche },
                ty,
            )
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
        K::DataTag { base, niche } => CExpr::new(
            K::DataTag { base: Box::new(inline_expr(db, base, next, changed)), niche: *niche },
            ty,
        ),
        K::DataField { base, index, scalar, niche } => CExpr::new(
            K::DataField {
                base: Box::new(inline_expr(db, base, next, changed)),
                index: *index,
                scalar: *scalar,
                niche: *niche,
            },
            ty,
        ),
        // Spread/LetMany are produced after this pre-count pass; recurse for safety.
        K::Spread { components } => CExpr::new(
            K::Spread {
                components: components.iter().map(|a| inline_expr(db, a, next, changed)).collect(),
            },
            ty,
        ),
        K::LetMany { locals, value, body } => CExpr::new(
            K::LetMany {
                locals: locals.clone(),
                value: Box::new(inline_expr(db, value, next, changed)),
                body: Box::new(inline_expr(db, body, next, changed)),
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

/// Builds the foreign-call node for a saturated wrapper call, given the (already
/// inlined) call arguments in source order. The peer of [`build_prim`]: an
/// identity permutation splices directly; any other binds each argument to a fresh
/// local first so evaluation (hence trap) order is preserved.
fn build_foreign(fw: &ForeignWrapper, args: Vec<CExpr>, ty: Ty, next: &mut usize) -> CExpr {
    if fw.slots.iter().enumerate().all(|(j, &s)| j == s) {
        return CExpr::new(K::Foreign { symbol: fw.symbol, args, marshalled: fw.marshalled }, ty);
    }
    let locals: Vec<(LocalId, CExpr)> = args
        .into_iter()
        .map(|a| {
            let l = LocalId::from_index(*next);
            *next += 1;
            (l, a)
        })
        .collect();
    let operands: Vec<CExpr> = fw
        .slots
        .iter()
        .map(|&s| CExpr::new(K::Local(locals[s].0), locals[s].1.ty.clone()))
        .collect();
    let mut body =
        CExpr::new(K::Foreign { symbol: fw.symbol, args: operands, marshalled: fw.marshalled }, ty);
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
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args } => {
            args.iter().for_each(|a| max_local(a, max));
        }
        K::App { func, args, reuse, .. } => {
            max_local(func, max);
            args.iter().for_each(|a| max_local(a, max));
            reuse.iter().flatten().for_each(|&t| bump(t, max));
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
        K::Spread { components } => components.iter().for_each(|a| max_local(a, max)),
        K::LetMany { locals, value, body } => {
            locals.iter().for_each(|&l| bump(l, max));
            max_local(value, max);
            max_local(body, max);
        }
        K::DataTag { base, .. } => max_local(base, max),
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

/// Allocates a fresh local slot, bumping `next` (the next free slot index).
pub(crate) fn fresh_local(next: &mut usize) -> LocalId {
    let id = LocalId::from_index(*next);
    *next += 1;
    id
}

/// Maps `l` through `locals`, allocating a fresh slot the first time it is seen.
pub(crate) fn remap_local(
    l: LocalId,
    locals: &mut FxHashMap<LocalId, LocalId>,
    next: &mut usize,
) -> LocalId {
    if let Some(&r) = locals.get(&l) {
        return r;
    }
    let r = fresh_local(next);
    locals.insert(l, r);
    r
}

/// Maps a lifted-function id through `fns`, leaving an id absent from the map
/// unchanged (the identity case the helper inliner uses, where the copied body has
/// no lifted lambda).
fn remap_fn(f: FnId, fns: &FxHashMap<FnId, FnId>) -> FnId {
    fns.get(&f).copied().unwrap_or(f)
}

/// Copies `e`, freshening every local through `locals` (allocating on first sight)
/// and every `MakeClosure` lifted id through `fns`, preserving each node's type.
///
/// The single substitution routine shared by the inliner passes. `fns` is **empty**
/// when the copied body has no lifted lambda (the helper inliner splices a single
/// function), and maps each relocated lambda to its fresh id when it does (CAF
/// inlining in `simplify` copies a definition's lifted lambdas into the caller).
pub(crate) fn remap_expr(
    e: &CExpr,
    locals: &mut FxHashMap<LocalId, LocalId>,
    fns: &FxHashMap<FnId, FnId>,
    next: &mut usize,
) -> CExpr {
    let ty = e.ty.clone();
    let kind = match &e.kind {
        K::Local(l) => K::Local(remap_local(*l, locals, next)),
        K::Lit(_) | K::Global(_) | K::Error => e.kind.clone(),
        K::Prim { op, args } => K::Prim {
            op: *op,
            args: args.iter().map(|a| remap_expr(a, locals, fns, next)).collect(),
        },
        K::Foreign { symbol, args, marshalled } => K::Foreign {
            symbol: *symbol,
            args: args.iter().map(|a| remap_expr(a, locals, fns, next)).collect(),
            marshalled: *marshalled,
        },
        K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
            tag: *tag,
            args: args.iter().map(|a| remap_expr(a, locals, fns, next)).collect(),
            reuse: reuse.map(|t| remap_local(t, locals, next)),
            scalars: *scalars,
            niche: *niche,
        },
        K::App { func, args, reuse, alloc } => K::App {
            func: Box::new(remap_expr(func, locals, fns, next)),
            args: args.iter().map(|a| remap_expr(a, locals, fns, next)).collect(),
            reuse: reuse.iter().map(|t| t.map(|l| remap_local(l, locals, next))).collect(),
            alloc: *alloc,
        },
        K::If { cond, then, els } => K::If {
            cond: Box::new(remap_expr(cond, locals, fns, next)),
            then: Box::new(remap_expr(then, locals, fns, next)),
            els: Box::new(remap_expr(els, locals, fns, next)),
        },
        K::Let { local, value, body } => {
            // The value is in the outer scope; remap it before binding `local`.
            let value = Box::new(remap_expr(value, locals, fns, next));
            let local = remap_local(*local, locals, next);
            K::Let { local, value, body: Box::new(remap_expr(body, locals, fns, next)) }
        }
        K::Spread { components } => K::Spread {
            components: components.iter().map(|a| remap_expr(a, locals, fns, next)).collect(),
        },
        K::LetMany { locals: bound, value, body } => {
            // The value is in the outer scope; remap it before binding `bound`.
            let value = Box::new(remap_expr(value, locals, fns, next));
            let bound = bound.iter().map(|&l| remap_local(l, locals, next)).collect();
            K::LetMany { locals: bound, value, body: Box::new(remap_expr(body, locals, fns, next)) }
        }
        K::DataTag { base, niche } => {
            K::DataTag { base: Box::new(remap_expr(base, locals, fns, next)), niche: *niche }
        }
        K::DataField { base, index, scalar, niche } => {
            let index = match index {
                FieldIndex::Dyn { base: off, evidence } => {
                    FieldIndex::Dyn { base: *off, evidence: remap_local(*evidence, locals, next) }
                }
                c => *c,
            };
            K::DataField {
                base: Box::new(remap_expr(base, locals, fns, next)),
                index,
                scalar: *scalar,
                niche: *niche,
            }
        }
        K::MakeClosure { func, captures, alloc } => K::MakeClosure {
            func: remap_fn(*func, fns),
            captures: captures.iter().map(|c| remap_local(*c, locals, next)).collect(),
            alloc: *alloc,
        },
        K::Reset { value, token, body } => K::Reset {
            value: Box::new(remap_expr(value, locals, fns, next)),
            token: remap_local(*token, locals, next),
            body: Box::new(remap_expr(body, locals, fns, next)),
        },
        K::FreeReuse { token, body } => K::FreeReuse {
            token: remap_local(*token, locals, next),
            body: Box::new(remap_expr(body, locals, fns, next)),
        },
        K::Dup { local, body } => K::Dup {
            local: remap_local(*local, locals, next),
            body: Box::new(remap_expr(body, locals, fns, next)),
        },
        K::Drop { local, body } => K::Drop {
            local: remap_local(*local, locals, next),
            body: Box::new(remap_expr(body, locals, fns, next)),
        },
        K::Join { params, body } => K::Join {
            params: params.iter().map(|p| remap_local(*p, locals, next)).collect(),
            body: Box::new(remap_expr(body, locals, fns, next)),
        },
        K::Recur { args } => {
            K::Recur { args: args.iter().map(|a| remap_expr(a, locals, fns, next)).collect() }
        }
        K::HoleStart { hole, body } => K::HoleStart {
            hole: remap_local(*hole, locals, next),
            body: Box::new(remap_expr(body, locals, fns, next)),
        },
        K::HoleFill { hole, cell, field } => K::HoleFill {
            hole: remap_local(*hole, locals, next),
            cell: Box::new(remap_expr(cell, locals, fns, next)),
            field: *field,
        },
        K::HoleClose { hole, base } => K::HoleClose {
            hole: remap_local(*hole, locals, next),
            base: Box::new(remap_expr(base, locals, fns, next)),
        },
    };
    CExpr::new(kind, ty)
}
