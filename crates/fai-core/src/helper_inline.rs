//! General inlining of small, non-recursive helpers into their callers, run
//! before reference counting so the merged body is what reuse analysis sees.
//!
//! Reuse analysis (`fai-rc`) recycles a dead data cell into a same-size
//! construction **in the same function body**. A construction that happens inside
//! a *called* helper (a smart constructor like `Dict.bin`, a shared `balance`)
//! therefore cannot recycle the caller's freed cell. [`helper_inlined`] folds such
//! helpers back into the caller, so factored code still gets "functional but
//! in-place" reuse without hand-inlining every construction.
//!
//! The inliner is **layered on [`core_inlined`]** (the intrinsic, prim-wrapper
//! form): it folds that body, and at each eligible call site splices the callee's
//! own [`helper_inlined`] body — already fully folded, so inlining is **transitive**
//! (a helper that calls a smaller helper folds the whole chain). It is
//! **intra-file**: only same-file callees are inlined, which keeps the cross-module
//! firewall intact (a body edit never crosses a module boundary) and keeps an
//! opaque type transparent at the splice site.
//!
//! Eligibility ([`inline_summary`]) admits a callee that is **non-recursive**
//! (excluded via [`fai_resolve::recursive_defs`] — the guarantee that makes the
//! transitive query graph a DAG and so cycle-free), a single function with no
//! captures (so its body has no lifted lambda to renumber), non-row-polymorphic
//! (no offset-evidence parameters), with at least one parameter, and **small**
//! (its prim-folded body is at most [`INLINE_NODE_BUDGET`] nodes). A saturated or
//! over-applied direct call to such a callee is rewritten; the saturated prefix is
//! inlined and any surplus arguments are applied to the result.
//!
//! Substitution binds **every** argument to a fresh local and remaps the callee's
//! locals to fresh slots. Binding (rather than splicing) routes each argument
//! through code generation's single representation-coercion point, so a raw scalar
//! flowing into a generic position is tagged exactly as the call boundary would
//! have, and the callee body's own types are kept verbatim (no instantiation
//! needed under the uniform representation).

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId, recursive_defs};
use fai_syntax::Symbol;
use fai_types::Ty;
use rustc_hash::FxHashMap;

use crate::core_inlined;
use crate::inline::next_free_local;
use crate::ir::{CExpr, ClosureAlloc, CoreFn, ExprKind as K, FieldIndex, LoweredDef};

/// The largest a callee's (prim-folded) body may be, in Core nodes, to be inlined.
/// Comfortably admits the standard smart constructors (`bin`/`singleton`, a few
/// dozen nodes) while excluding the rebalancing `balance` (well over a hundred),
/// which stays a shared call. A tunable budget, not a hard contract.
const INLINE_NODE_BUDGET: usize = 64;

/// Whether `name` is an inlinable helper, and if so its parameter count (the arity
/// a call must saturate). `None` for any definition the inliner must not fold.
///
/// The recursion check is **first and cheap** (it reads only
/// [`fai_resolve::recursive_defs`], never [`helper_inlined`]): a recursive callee
/// is rejected before any body is folded, which is exactly what keeps a self-call
/// from forming a query cycle. The remaining checks read the intrinsic
/// [`core_inlined`] body and the signature. The result is a tiny value, so editing
/// a callee's body ripples to its callers only when its *eligibility or arity*
/// actually changes (salsa early cutoff) — the firewall the issue requires.
#[salsa::tracked]
pub fn inline_summary(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<usize> {
    // A recursive callee is never inlined: unrolling it is unwanted, the tail-call
    // transform relies on intact self-recursion, and excluding every cycle member
    // makes the transitive inlining graph acyclic. Checked before reading any body.
    let def = DefId::new(file.source(db), name);
    if recursive_defs(db, file).contains(&def) {
        return None;
    }
    let base = core_inlined(db, file, name);
    // A single function (no lifted lambdas, hence no `MakeClosure` to renumber) and
    // no captures (top-level helpers capture nothing), with at least one parameter
    // (a direct-callable; nullary constants are referenced as bare globals, not
    // calls, and inlining them is a separate concern).
    if base.fns.len() != 1 {
        return None;
    }
    let entry = base.entry();
    if !entry.captures.is_empty() || entry.params.is_empty() {
        return None;
    }
    // Row-polymorphic definitions take leading offset-evidence parameters and are
    // only ever called curried; leave them alone.
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |s| fai_types::evidence_count(&s));
    if evidence > 0 {
        return None;
    }
    // Small enough: measured on the prim-folded body, so an over-budget helper is
    // rejected without ever materializing its fold (its callers splice the folded
    // body via [`helper_inlined`] only once eligibility holds).
    if node_count(&entry.body) > INLINE_NODE_BUDGET {
        return None;
    }
    Some(entry.params.len())
}

/// `name`'s lowered definition with every saturated (or over-applied) direct call
/// to an eligible same-file helper folded in.
///
/// This is the back end's view of Core: reference counting, borrow inference, the
/// mutual-recursion combined loop, and reachability read this rather than the
/// intrinsic [`core_inlined`], so helpers are merged before any of them run. A
/// definition is produced for *every* binding (even one that is itself not
/// inlinable, such as a recursive `insert`), so its own body still gets its helper
/// calls folded.
#[salsa::tracked]
pub fn helper_inlined(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let base = core_inlined(db, file, name);
    let source = file.source(db);
    let mut next = next_free_local(&base);
    let mut changed = false;
    let fns: Vec<CoreFn> = base
        .fns
        .iter()
        .map(|f| CoreFn {
            params: f.params.clone(),
            captures: f.captures.clone(),
            body: inline_expr(db, file, source, &f.body, &mut next, &mut changed),
        })
        .collect();
    // Nothing folded: return the original `Arc` so salsa's pointer-equality fast
    // path gives O(1) early cutoff for the common (no-inlinable-call) definition.
    if !changed {
        return base;
    }
    Arc::new(LoweredDef {
        def: base.def,
        fns,
        entry_borrowed: base.entry_borrowed.clone(),
        reuse_entry: base.reuse_entry.clone(),
    })
}

/// Rewrites a single expression, folding every eligible same-file helper call it
/// contains (children first, so calls nested in arguments are folded too). Sets
/// `changed` when any fold happens.
fn inline_expr(
    db: &dyn Db,
    file: SourceFile,
    source: fai_span::SourceId,
    e: &CExpr,
    next: &mut usize,
    changed: &mut bool,
) -> CExpr {
    let ty = e.ty.clone();
    let go = |c: &CExpr, next: &mut usize, changed: &mut bool| {
        inline_expr(db, file, source, c, next, changed)
    };
    match &e.kind {
        // Helper inlining runs before reference counting inserts reuse tokens, so
        // `reuse` is always empty here; it is carried through verbatim.
        K::App { func, args, reuse, alloc } => {
            let func = go(func, next, changed);
            let args: Vec<CExpr> = args.iter().map(|a| go(a, next, changed)).collect();
            // A saturated (or over-applied) direct call to an eligible same-file
            // helper: splice its folded body.
            if let K::Global(callee) = &func.kind
                && callee.file == source
                && let Some(arity) = inline_summary(db, file, callee.name)
                && args.len() >= arity
            {
                let folded = helper_inlined(db, file, callee.name);
                *changed = true;
                return build_inline(folded.entry(), args, arity, ty, next);
            }
            CExpr::new(
                K::App { func: Box::new(func), args, reuse: reuse.clone(), alloc: *alloc },
                ty,
            )
        }
        K::Prim { op, args } => {
            let args = args.iter().map(|a| go(a, next, changed)).collect();
            CExpr::new(K::Prim { op: *op, args }, ty)
        }
        K::MakeData { tag, args, reuse, scalars, niche } => {
            let args = args.iter().map(|a| go(a, next, changed)).collect();
            CExpr::new(
                K::MakeData { tag: *tag, args, reuse: *reuse, scalars: *scalars, niche: *niche },
                ty,
            )
        }
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(go(cond, next, changed)),
                then: Box::new(go(then, next, changed)),
                els: Box::new(go(els, next, changed)),
            },
            ty,
        ),
        K::Let { local, value, body } => CExpr::new(
            K::Let {
                local: *local,
                value: Box::new(go(value, next, changed)),
                body: Box::new(go(body, next, changed)),
            },
            ty,
        ),
        K::DataTag { base, niche } => {
            CExpr::new(K::DataTag { base: Box::new(go(base, next, changed)), niche: *niche }, ty)
        }
        K::DataField { base, index, scalar, niche } => CExpr::new(
            K::DataField {
                base: Box::new(go(base, next, changed)),
                index: *index,
                scalar: *scalar,
                niche: *niche,
            },
            ty,
        ),
        // Leaves and nodes with no expression children, copied unchanged. A lifted
        // lambda's body is a separate `CoreFn`, folded by [`helper_inlined`]'s loop.
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
            CExpr::new(e.kind.clone(), ty)
        }
        // Reference-counting and tail-call nodes do not exist in the pre-count Core
        // this runs on; reconstructed with recursed children for completeness.
        K::Reset { value, token, body } => CExpr::new(
            K::Reset {
                value: Box::new(go(value, next, changed)),
                token: *token,
                body: Box::new(go(body, next, changed)),
            },
            ty,
        ),
        K::FreeReuse { token, body } => {
            CExpr::new(K::FreeReuse { token: *token, body: Box::new(go(body, next, changed)) }, ty)
        }
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local: *local, body: Box::new(go(body, next, changed)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local: *local, body: Box::new(go(body, next, changed)) }, ty)
        }
        K::Join { params, body } => CExpr::new(
            K::Join { params: params.clone(), body: Box::new(go(body, next, changed)) },
            ty,
        ),
        K::Recur { args } => {
            CExpr::new(K::Recur { args: args.iter().map(|a| go(a, next, changed)).collect() }, ty)
        }
        K::HoleStart { hole, body } => {
            CExpr::new(K::HoleStart { hole: *hole, body: Box::new(go(body, next, changed)) }, ty)
        }
        K::HoleFill { hole, cell, field } => CExpr::new(
            K::HoleFill { hole: *hole, cell: Box::new(go(cell, next, changed)), field: *field },
            ty,
        ),
        K::HoleClose { hole, base } => {
            CExpr::new(K::HoleClose { hole: *hole, base: Box::new(go(base, next, changed)) }, ty)
        }
    }
}

/// Builds the inlined form of a call: `let q0 = a0; …; let qN = aN; <body'>`, where
/// `body'` is the callee `entry`'s body with its parameters remapped to the prefix
/// locals `q0…q_{arity-1}` and its other locals freshened. Any surplus arguments
/// beyond `arity` (an over-application) are applied to the result. All arguments
/// are bound, in source order, so evaluation (hence trap) order is preserved and
/// each argument flows through the same representation coercion the call boundary
/// would have applied.
fn build_inline(
    entry: &CoreFn,
    args: Vec<CExpr>,
    arity: usize,
    call_ty: Ty,
    next: &mut usize,
) -> CExpr {
    // A fresh local for every argument (prefix parameters and any surplus), in
    // source order; the surplus locals carry their argument's type for the apply.
    let arg_tys: Vec<Ty> = args.iter().map(|a| a.ty.clone()).collect();
    let arg_locals: Vec<LocalId> = (0..args.len()).map(|_| fresh(next)).collect();

    // Remap the callee's locals: each parameter to its prefix local, every other
    // local to a fresh slot allocated on first sight.
    let mut subst: FxHashMap<LocalId, LocalId> = FxHashMap::default();
    for (i, &p) in entry.params.iter().enumerate() {
        subst.insert(p, arg_locals[i]);
    }
    let body = remap_expr(&entry.body, &mut subst, next);

    // Over-application: apply the saturated result to the surplus arguments.
    let mut result = if args.len() > arity {
        let surplus: Vec<CExpr> = arg_locals[arity..]
            .iter()
            .zip(&arg_tys[arity..])
            .map(|(&l, t)| CExpr::new(K::Local(l), t.clone()))
            .collect();
        CExpr::new(
            K::App {
                func: Box::new(body),
                args: surplus,
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            call_ty,
        )
    } else {
        body
    };

    // Bind every argument, outermost first, so they evaluate in source order.
    for (local, value) in arg_locals.into_iter().zip(args).rev() {
        let ty = result.ty.clone();
        result = CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(result) }, ty);
    }
    result
}

/// Copies `e`, remapping every local through `subst` (allocating a fresh slot the
/// first time a local is seen) and keeping the node's type verbatim.
fn remap_expr(e: &CExpr, subst: &mut FxHashMap<LocalId, LocalId>, next: &mut usize) -> CExpr {
    let ty = e.ty.clone();
    let kind = match &e.kind {
        K::Local(l) => K::Local(remap_local(*l, subst, next)),
        K::Lit(_) | K::Global(_) | K::Error => e.kind.clone(),
        K::Prim { op, args } => {
            K::Prim { op: *op, args: args.iter().map(|a| remap_expr(a, subst, next)).collect() }
        }
        K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
            tag: *tag,
            args: args.iter().map(|a| remap_expr(a, subst, next)).collect(),
            reuse: reuse.map(|t| remap_local(t, subst, next)),
            scalars: *scalars,
            niche: *niche,
        },
        K::App { func, args, reuse, alloc } => K::App {
            func: Box::new(remap_expr(func, subst, next)),
            args: args.iter().map(|a| remap_expr(a, subst, next)).collect(),
            reuse: reuse.iter().map(|t| t.map(|l| remap_local(l, subst, next))).collect(),
            alloc: *alloc,
        },
        K::If { cond, then, els } => K::If {
            cond: Box::new(remap_expr(cond, subst, next)),
            then: Box::new(remap_expr(then, subst, next)),
            els: Box::new(remap_expr(els, subst, next)),
        },
        K::Let { local, value, body } => {
            // The value is in the outer scope; remap it before binding `local`.
            let value = Box::new(remap_expr(value, subst, next));
            let local = remap_local(*local, subst, next);
            K::Let { local, value, body: Box::new(remap_expr(body, subst, next)) }
        }
        K::DataTag { base, niche } => {
            K::DataTag { base: Box::new(remap_expr(base, subst, next)), niche: *niche }
        }
        K::DataField { base, index, scalar, niche } => {
            let index = match index {
                FieldIndex::Dyn { base: off, evidence } => {
                    FieldIndex::Dyn { base: *off, evidence: remap_local(*evidence, subst, next) }
                }
                c => *c,
            };
            K::DataField {
                base: Box::new(remap_expr(base, subst, next)),
                index,
                scalar: *scalar,
                niche: *niche,
            }
        }
        // An eligible callee is a single function, so its body has no `MakeClosure`
        // (and no reference-counting or tail-call node, which are inserted later);
        // these arms keep the remap total. A `MakeClosure`'s `FnId` would dangle, so
        // its presence would be a bug upstream — but eligibility forbids it.
        K::MakeClosure { func, captures, alloc } => K::MakeClosure {
            func: *func,
            captures: captures.iter().map(|c| remap_local(*c, subst, next)).collect(),
            alloc: *alloc,
        },
        K::Reset { value, token, body } => K::Reset {
            value: Box::new(remap_expr(value, subst, next)),
            token: remap_local(*token, subst, next),
            body: Box::new(remap_expr(body, subst, next)),
        },
        K::FreeReuse { token, body } => K::FreeReuse {
            token: remap_local(*token, subst, next),
            body: Box::new(remap_expr(body, subst, next)),
        },
        K::Dup { local, body } => K::Dup {
            local: remap_local(*local, subst, next),
            body: Box::new(remap_expr(body, subst, next)),
        },
        K::Drop { local, body } => K::Drop {
            local: remap_local(*local, subst, next),
            body: Box::new(remap_expr(body, subst, next)),
        },
        K::Join { params, body } => K::Join {
            params: params.iter().map(|p| remap_local(*p, subst, next)).collect(),
            body: Box::new(remap_expr(body, subst, next)),
        },
        K::Recur { args } => {
            K::Recur { args: args.iter().map(|a| remap_expr(a, subst, next)).collect() }
        }
        K::HoleStart { hole, body } => K::HoleStart {
            hole: remap_local(*hole, subst, next),
            body: Box::new(remap_expr(body, subst, next)),
        },
        K::HoleFill { hole, cell, field } => K::HoleFill {
            hole: remap_local(*hole, subst, next),
            cell: Box::new(remap_expr(cell, subst, next)),
            field: *field,
        },
        K::HoleClose { hole, base } => K::HoleClose {
            hole: remap_local(*hole, subst, next),
            base: Box::new(remap_expr(base, subst, next)),
        },
    };
    CExpr::new(kind, ty)
}

/// Maps `l` to its fresh slot, allocating one the first time it is seen.
fn remap_local(l: LocalId, subst: &mut FxHashMap<LocalId, LocalId>, next: &mut usize) -> LocalId {
    if let Some(&r) = subst.get(&l) {
        return r;
    }
    let r = fresh(next);
    subst.insert(l, r);
    r
}

fn fresh(next: &mut usize) -> LocalId {
    let id = LocalId::from_index(*next);
    *next += 1;
    id
}

/// The number of Core nodes in `e` (every [`CExpr`] counts as one, recursing into
/// its expression children). The size budget eligibility is measured against.
fn node_count(e: &CExpr) -> usize {
    let kids = |xs: &[CExpr]| -> usize { xs.iter().map(node_count).sum() };
    1 + match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::Error | K::MakeClosure { .. } => 0,
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => kids(args),
        K::App { func, args, .. } => node_count(func) + kids(args),
        K::If { cond, then, els } => node_count(cond) + node_count(then) + node_count(els),
        K::Let { value, body, .. } => node_count(value) + node_count(body),
        K::DataTag { base, .. } | K::DataField { base, .. } => node_count(base),
        K::Reset { value, body, .. } => node_count(value) + node_count(body),
        K::FreeReuse { body, .. } | K::Dup { body, .. } | K::Drop { body, .. } => node_count(body),
        K::Join { body, .. } | K::HoleStart { body, .. } => node_count(body),
        K::HoleFill { cell, .. } => node_count(cell),
        K::HoleClose { base, .. } => node_count(base),
    }
}
