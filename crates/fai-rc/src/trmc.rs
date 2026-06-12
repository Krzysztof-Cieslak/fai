//! Tail-call flattening: rewrite a self-tail-recursive entry function into a loop.
//!
//! Runs after dup/drop insertion and reuse analysis, so it consumes the existing
//! `Reset`/`MakeData { reuse }` shape and preserves it. A function is eligible when
//! **every** reference to itself is a saturated self-call in tail position — either
//! a plain tail call or the single argument of one tail constructor (the "modulo
//! cons" case). The whole function is transformed or left untouched; there is no
//! partial flattening.
//!
//! A **row-polymorphic** function (one carrying leading offset-evidence
//! parameters) calls itself curried: lowering partially applies the function to
//! its evidence (`let t = self ev0 …`) and then applies that to the real arguments
//! (`t a b`). A fusion pre-pass ([`fuse_evidence_self_calls`]) normalizes that pair
//! back into a single saturated `self ev0 … a b` before detection runs, so a
//! row-polymorphic function is detected and rewritten exactly like a monomorphic
//! one. The evidence then rides through the loop as ordinary loop-carried
//! parameters, passed unchanged on every back-edge (Fai has no polymorphic
//! recursion, so a self-call always threads its own evidence).
//!
//! The rewrite introduces a generic loop ([`K::Join`]/[`K::Recur`]). A
//! constructor-wrapped recursion additionally uses **destination passing**: a
//! non-reference-counted "hole" token threads through the loop; each iteration
//! builds its cell with a placeholder recursive field, links it into the spine
//! ([`K::HoleFill`]), and advances; the base case fills the final hole
//! ([`K::HoleClose`]). The per-iteration reuse token is consumed by the cell build
//! *before* the back-edge, so a unique list still rebuilds with zero allocations
//! and the recursion runs in constant stack.

use fai_core::ir::{CExpr, ExprKind as K, FieldIndex, Lit};
use fai_resolve::{DefId, LocalId};
use fai_types::Ty;

use crate::fresh;

/// Flattens `body` (the entry function's body, after reference counting and reuse)
/// into a loop when it is tail-recursive; otherwise returns it unchanged.
///
/// `params` are the entry's parameter slots (a row-polymorphic function's leading
/// slots are its offset evidence, the rest its real parameters), `self_def` its
/// definition id, and `is_pure_total` reports whether calling a given top-level
/// function is pure and total (so a later constructor argument that calls it may be
/// hoisted ahead of the back-edge). `next` supplies fresh local slots for the hole.
pub(crate) fn flatten(
    body: CExpr,
    params: &[LocalId],
    self_def: DefId,
    is_pure_total: &dyn Fn(DefId) -> bool,
    next: &mut usize,
) -> CExpr {
    let arity = params.len();
    // A row-polymorphic function's curried self-calls were already normalized into
    // saturated form before reference counting (see [`fuse_evidence_self_calls`]),
    // so detection treats every function the same.
    match eligible(&body, self_def, arity, is_pure_total) {
        Some(uses_hole) => rewrite_into_loop(body, params, self_def, arity, uses_hole, next),
        None => body,
    }
}

/// Builds the loop from an eligible `body`: a [`K::Join`] header over the function
/// parameters (and, when a tail is constructor-wrapped, a destination [`K::HoleStart`]
/// and an extra hole parameter), with the body rewritten in tail position.
fn rewrite_into_loop(
    body: CExpr,
    params: &[LocalId],
    self_def: DefId,
    arity: usize,
    uses_hole: bool,
    next: &mut usize,
) -> CExpr {
    let result_ty = body.ty.clone();
    if uses_hole {
        let hole = fresh(next);
        let mut join_params = params.to_vec();
        join_params.push(hole);
        let loop_body = rewrite_tail(body, Some(hole), self_def, arity, next);
        let join =
            CExpr::new(K::Join { params: join_params, body: Box::new(loop_body) }, result_ty);
        let ty = join.ty.clone();
        CExpr::new(K::HoleStart { hole, body: Box::new(join) }, ty)
    } else {
        let loop_body = rewrite_tail(body, None, self_def, arity, next);
        CExpr::new(K::Join { params: params.to_vec(), body: Box::new(loop_body) }, result_ty)
    }
}

// ---------------------------------------------------------------------------
// Row-polymorphic self-call fusion (before reference counting).
// ---------------------------------------------------------------------------

/// Normalizes a row-polymorphic function's curried self-calls into flat saturated
/// ones, so that — once reference-counted and reuse-analyzed — they look exactly
/// like a monomorphic self-call and [`flatten`] handles every function uniformly.
///
/// Lowering references a row-polymorphic `self` as a partial application to its
/// leading offset evidence and then applies that to the real arguments, producing
/// the nested `App { func: App { Global(self), [ev0 … ev_{k-1}] }, args }`. This
/// rewrites it to the saturated `App { Global(self), [ev0 … ev_{k-1}] ++ args }`.
///
/// Run **before** reference counting, on the lowered body: at this point the
/// nested application is intact (A-normal form has not yet split it behind a
/// binder, and no `dup`/`drop` has been inserted), so the rewrite is a pure
/// structural substitution. Reference counting then treats the flat self-call's
/// operands — evidence included — as ordinary values consumed at the (tail) call,
/// with no partial-application closure to materialize and, crucially, no
/// `dup`/`drop` of the evidence forced by the partial application's early
/// consumption. Doing this *after* reference counting would leave that `dup`/`drop`
/// pair stranded when the call becomes a back-edge.
///
/// Only the exact shape lowering produces is fused: the inner application's
/// arguments must be precisely the leading evidence parameters, in order (Fai has
/// no polymorphic recursion, so a genuine self-call always threads its own
/// evidence). Anything else is left intact.
pub(crate) fn fuse_evidence_self_calls(e: CExpr, self_def: DefId, evidence: &[LocalId]) -> CExpr {
    let CExpr { kind, ty } = e;
    let sub = |e: CExpr| fuse_evidence_self_calls(e, self_def, evidence);
    match kind {
        K::App { func, args } => {
            // `self` partially applied to its evidence, then to the real arguments.
            if let K::App { func: inner, args: ev } = &func.kind
                && let K::Global(def) = &inner.kind
                && *def == self_def
                && ev.len() == evidence.len()
                && ev.iter().zip(evidence).all(|(a, &p)| is_local(a, p))
            {
                let head = (**inner).clone(); // `Global(self)` with its own type
                let mut new_args: Vec<CExpr> = ev.clone();
                new_args.extend(args.into_iter().map(sub));
                CExpr::new(K::App { func: Box::new(head), args: new_args }, ty)
            } else {
                let func = Box::new(fuse_evidence_self_calls(*func, self_def, evidence));
                let args = args.into_iter().map(sub).collect();
                CExpr::new(K::App { func, args }, ty)
            }
        }
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(fuse_evidence_self_calls(*cond, self_def, evidence)),
                then: Box::new(fuse_evidence_self_calls(*then, self_def, evidence)),
                els: Box::new(fuse_evidence_self_calls(*els, self_def, evidence)),
            },
            ty,
        ),
        K::Let { local, value, body } => CExpr::new(
            K::Let {
                local,
                value: Box::new(fuse_evidence_self_calls(*value, self_def, evidence)),
                body: Box::new(fuse_evidence_self_calls(*body, self_def, evidence)),
            },
            ty,
        ),
        K::Prim { op, args } => {
            CExpr::new(K::Prim { op, args: args.into_iter().map(sub).collect() }, ty)
        }
        K::MakeData { tag, args, reuse, scalars } => CExpr::new(
            K::MakeData { tag, args: args.into_iter().map(sub).collect(), reuse, scalars },
            ty,
        ),
        K::DataTag(base) => CExpr::new(K::DataTag(Box::new(sub(*base))), ty),
        K::DataField { base, index, scalar } => {
            CExpr::new(K::DataField { base: Box::new(sub(*base)), index, scalar }, ty)
        }
        // No self-calls live inside these before reference counting (a lifted
        // lambda is referenced by id, not inlined); return them unchanged.
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
            CExpr::new(kind, ty)
        }
        // The reference-counting and tail-call nodes do not exist yet; pass through
        // for exhaustiveness.
        K::Reset { .. }
        | K::FreeReuse { .. }
        | K::Dup { .. }
        | K::Drop { .. }
        | K::Join { .. }
        | K::Recur { .. }
        | K::HoleStart { .. }
        | K::HoleFill { .. }
        | K::HoleClose { .. } => CExpr::new(kind, ty),
    }
}

// ---------------------------------------------------------------------------
// Eligibility.
// ---------------------------------------------------------------------------

/// Whether `body` is tail-recursive and may be flattened. Returns `Some(uses_hole)`
/// when eligible (`uses_hole` true if any tail is constructor-wrapped, so the loop
/// needs a destination hole), or `None` to leave it untouched.
fn eligible(
    body: &CExpr,
    self_def: DefId,
    arity: usize,
    is_pure_total: &dyn Fn(DefId) -> bool,
) -> Option<bool> {
    let mut uses_hole = false;
    let mut found_tail = false;
    if !check_tail(body, self_def, arity, is_pure_total, &mut uses_hole, &mut found_tail) {
        return None;
    }
    // Nothing to flatten unless there is at least one tail self-call.
    found_tail.then_some(uses_hole)
}

/// Validates every tail position reachable from `e`. Returns false (ineligible) on
/// any self-reference that is not a saturated tail self-call in an allowed shape.
fn check_tail(
    e: &CExpr,
    self_def: DefId,
    arity: usize,
    is_pure_total: &dyn Fn(DefId) -> bool,
    uses_hole: &mut bool,
    found_tail: &mut bool,
) -> bool {
    match &e.kind {
        K::If { cond, then, els } => {
            !contains_self(cond, self_def)
                && check_tail(then, self_def, arity, is_pure_total, uses_hole, found_tail)
                && check_tail(els, self_def, arity, is_pure_total, uses_hole, found_tail)
        }
        K::Let { local, value, body } => {
            if self_call_args(value, self_def, arity).is_some() {
                // A constructor-wrapped recursion: the bound self-call result flows
                // through a (possibly nested) chain of constructors to the tail. It
                // must be used exactly once, so dropping its binder cannot orphan
                // another reference.
                count_uses(body, *local) == 1
                    && check_cons_wrap(*local, body, self_def, is_pure_total, uses_hole, found_tail)
            } else if contains_self(value, self_def) {
                // A self-reference anywhere off the tail path is unsupported.
                false
            } else {
                check_tail(body, self_def, arity, is_pure_total, uses_hole, found_tail)
            }
        }
        K::Reset { value, body, .. } => {
            !contains_self(value, self_def)
                && check_tail(body, self_def, arity, is_pure_total, uses_hole, found_tail)
        }
        K::FreeReuse { body, .. } | K::Dup { body, .. } | K::Drop { body, .. } => {
            check_tail(body, self_def, arity, is_pure_total, uses_hole, found_tail)
        }
        // A bare tail self-call (no surrounding constructor) is plain tail
        // recursion; any other tail must contain no self-reference (it is a base).
        _ => {
            if self_call_args(e, self_def, arity).is_some() {
                *found_tail = true;
                true
            } else {
                !contains_self(e, self_def)
            }
        }
    }
}

/// Validates the continuation of a constructor-wrapped recursion: `e` follows the
/// `let rec = <self-call>` binder and must carry `rec` through a (possibly nested)
/// chain of constructors to the tail, with every intervening (hoisted) binder pure
/// and total.
///
/// Invariant on entry: `rec` is used exactly once in `e` (the caller verified it),
/// which makes the recursion flow linear — so dropping its binder during the
/// rewrite cannot orphan another reference.
fn check_cons_wrap(
    rec: LocalId,
    e: &CExpr,
    self_def: DefId,
    is_pure_total: &dyn Fn(DefId) -> bool,
    uses_hole: &mut bool,
    found_tail: &mut bool,
) -> bool {
    match &e.kind {
        K::Let { local, value, body } => {
            if count_uses(value, rec) > 0 {
                // `rec` flows into this binding: it must be a constructor using
                // `rec` exactly once as a field (no other self-reference), and the
                // recursion carries on through `local` (the next cell, used once).
                // The construction may be wrapped in `dup`/`drop` that reference
                // counting placed on its other field operands.
                count_uses(value, rec) == 1
                    && is_construction(value)
                    && !contains_self(value, self_def)
                    && count_uses(body, *local) == 1
                    && check_cons_wrap(*local, body, self_def, is_pure_total, uses_hole, found_tail)
            } else {
                // A later constructor argument, hoisted before the back-edge: it
                // must be reorder-safe and carry no self-reference.
                pure_total(value, is_pure_total)
                    && !contains_self(value, self_def)
                    && check_cons_wrap(rec, body, self_def, is_pure_total, uses_hole, found_tail)
            }
        }
        K::Reset { value, body, .. } => {
            count_uses(value, rec) == 0
                && !contains_self(value, self_def)
                && check_cons_wrap(rec, body, self_def, is_pure_total, uses_hole, found_tail)
        }
        K::FreeReuse { token, body } => {
            *token != rec
                && check_cons_wrap(rec, body, self_def, is_pure_total, uses_hole, found_tail)
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            *local != rec
                && check_cons_wrap(rec, body, self_def, is_pure_total, uses_hole, found_tail)
        }
        K::MakeData { args, .. } => {
            // The outermost (tail) constructor: `rec` is exactly one field (the
            // invariant guarantees its single use is here), with no other
            // self-reference.
            args.iter().filter(|a| is_local(a, rec)).count() == 1
                && !contains_self(e, self_def)
                && {
                    *uses_hole = true;
                    *found_tail = true;
                    true
                }
        }
        // The recursion reaches a non-constructor tail (returned, tested, etc.):
        // not tail-modulo-cons.
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Rewrite.
// ---------------------------------------------------------------------------

/// Rewrites a tail-position expression: tail self-calls become [`K::Recur`],
/// constructor-wrapped recursions become [`K::HoleFill`] + `Recur`, and base cases
/// become [`K::HoleClose`] (when building a spine) or are left as-is (plain
/// tail-call loops). `hole` is `Some` exactly when the loop carries a destination.
fn rewrite_tail(
    e: CExpr,
    hole: Option<LocalId>,
    self_def: DefId,
    arity: usize,
    next: &mut usize,
) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::If { cond, then, els } => {
            let then = rewrite_tail(*then, hole, self_def, arity, next);
            let els = rewrite_tail(*els, hole, self_def, arity, next);
            CExpr::new(K::If { cond, then: Box::new(then), els: Box::new(els) }, ty)
        }
        K::Let { local, value, body } => {
            if let Some(sargs) = self_call_args(&value, self_def, arity) {
                // Drop the `let v = <self-call>` binder; the recursion becomes the
                // hole, threaded through the constructor's continuation.
                let hole = hole.expect("constructor-wrapped recursion implies a hole");
                let sargs: Vec<CExpr> = sargs.to_vec();
                rewrite_cons_wrap(local, *body, hole, sargs, next)
            } else {
                let body = rewrite_tail(*body, hole, self_def, arity, next);
                CExpr::new(K::Let { local, value, body: Box::new(body) }, ty)
            }
        }
        K::Reset { value, token, body } => {
            let body = rewrite_tail(*body, hole, self_def, arity, next);
            CExpr::new(K::Reset { value, token, body: Box::new(body) }, ty)
        }
        K::FreeReuse { token, body } => {
            let body = rewrite_tail(*body, hole, self_def, arity, next);
            CExpr::new(K::FreeReuse { token, body: Box::new(body) }, ty)
        }
        K::Dup { local, body } => {
            let body = rewrite_tail(*body, hole, self_def, arity, next);
            CExpr::new(K::Dup { local, body: Box::new(body) }, ty)
        }
        K::Drop { local, body } => {
            let body = rewrite_tail(*body, hole, self_def, arity, next);
            CExpr::new(K::Drop { local, body: Box::new(body) }, ty)
        }
        other => {
            let e = CExpr::new(other, ty);
            rewrite_final(e, hole, self_def, arity)
        }
    }
}

/// Rewrites a final (non-binder, non-`if`) tail expression: a plain tail self-call
/// becomes `Recur`; anything else is a base case (closed into the hole when
/// building a spine, otherwise returned as the loop's value).
fn rewrite_final(e: CExpr, hole: Option<LocalId>, self_def: DefId, arity: usize) -> CExpr {
    if let Some(sargs) = self_call_args(&e, self_def, arity) {
        let mut args: Vec<CExpr> = sargs.to_vec();
        if let Some(h) = hole {
            // Thread the destination unchanged (this iteration extends nothing).
            args.push(CExpr::new(K::Local(h), Ty::Error));
        }
        return CExpr::new(K::Recur { args }, Ty::Error);
    }
    match hole {
        Some(h) => {
            let ty = e.ty.clone();
            CExpr::new(K::HoleClose { hole: h, base: Box::new(e) }, ty)
        }
        None => e,
    }
}

/// One constructor on a (possibly nested) tail-cons chain: the cell expression
/// (a `MakeData`, possibly wrapped in the `dup`/`drop` reference counting placed on
/// its field operands) with its recursive field already replaced by a placeholder,
/// and that field's index (where the next inner cell, or the recursion, links in).
struct ChainLink {
    cell: CExpr,
    field: usize,
}

/// Rewrites the continuation after a dropped `let rec = <self-call>` binder: keep
/// the hoisted/reference-count wrappers, collect the chain of constructors carrying
/// the recursion, and link them into the spine with one [`K::HoleFill`] per cell.
fn rewrite_cons_wrap(
    rec: LocalId,
    e: CExpr,
    hole: LocalId,
    sargs: Vec<CExpr>,
    next: &mut usize,
) -> CExpr {
    let mut chain: Vec<ChainLink> = Vec::new();
    collect_chain(rec, e, hole, sargs, &mut chain, next)
}

/// Walks the continuation: rebuilds each wrapper node, records each chain-link
/// constructor (dropping its `let`), and at the tail emits the `HoleFill` chain.
fn collect_chain(
    rec: LocalId,
    e: CExpr,
    hole: LocalId,
    sargs: Vec<CExpr>,
    chain: &mut Vec<ChainLink>,
    next: &mut usize,
) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::Let { local, value, body } if uses_local(&value, rec) => {
            // A chain link: record the constructor (its `let` is dropped — the cell
            // is hole-linked, not bound), and carry the recursion through `local` to
            // the next cell.
            let (cell, field) = hole_cell(*value, rec);
            chain.push(ChainLink { cell, field });
            collect_chain(local, *body, hole, sargs, chain, next)
        }
        K::Let { local, value, body } => {
            // A hoisted later argument (or other binder): keep it.
            let body = collect_chain(rec, *body, hole, sargs, chain, next);
            CExpr::new(K::Let { local, value, body: Box::new(body) }, ty)
        }
        K::Reset { value, token, body } => {
            let body = collect_chain(rec, *body, hole, sargs, chain, next);
            CExpr::new(K::Reset { value, token, body: Box::new(body) }, ty)
        }
        K::FreeReuse { token, body } => {
            let body = collect_chain(rec, *body, hole, sargs, chain, next);
            CExpr::new(K::FreeReuse { token, body: Box::new(body) }, ty)
        }
        K::Dup { local, body } => {
            let body = collect_chain(rec, *body, hole, sargs, chain, next);
            CExpr::new(K::Dup { local, body: Box::new(body) }, ty)
        }
        K::Drop { local, body } => {
            let body = collect_chain(rec, *body, hole, sargs, chain, next);
            CExpr::new(K::Drop { local, body: Box::new(body) }, ty)
        }
        K::MakeData { tag, args, reuse, scalars } => {
            // The outermost (tail) constructor completes the chain.
            let (cell, field) =
                hole_cell(CExpr::new(K::MakeData { tag, args, reuse, scalars }, ty), rec);
            chain.push(ChainLink { cell, field });
            emit_holefill_chain(std::mem::take(chain), hole, sargs, next)
        }
        // `check_cons_wrap` guaranteed a constructor at the end of the chain.
        _ => unreachable!("constructor-wrapped recursion must end in a construction"),
    }
}

/// Turns a chain-link value into its destination-passing cell: replace the field
/// holding `rec` with a placeholder (an immediate, so it is drop-safe; the next
/// fill or close overwrites it) and report that field's index. Descends through any
/// `dup`/`drop` reference counting placed around the construction, preserving them.
fn hole_cell(value: CExpr, rec: LocalId) -> (CExpr, usize) {
    let CExpr { kind, ty } = value;
    match kind {
        K::Dup { local, body } => {
            let (body, field) = hole_cell(*body, rec);
            (CExpr::new(K::Dup { local, body: Box::new(body) }, ty), field)
        }
        K::Drop { local, body } => {
            let (body, field) = hole_cell(*body, rec);
            (CExpr::new(K::Drop { local, body: Box::new(body) }, ty), field)
        }
        K::MakeData { tag, mut args, reuse, scalars } => {
            let field =
                args.iter().position(|a| is_local(a, rec)).expect("recursive field present");
            args[field] = CExpr::new(K::Lit(Lit::Unit), Ty::Error);
            (CExpr::new(K::MakeData { tag, args, reuse, scalars }, ty), field)
        }
        // `check_cons_wrap` guaranteed the construction under any dup/drop.
        _ => unreachable!("a chain link is a construction"),
    }
}

/// Emits the destination-passing chain for the collected constructors (held
/// inner→outer): one [`K::HoleFill`] per cell, each storing its
/// (placeholder-filled) cell and advancing the hole into the recursive field,
/// ending in the back-edge.
///
/// The spine must be *linked* outer→inner (the outer cell goes at the loop hole;
/// the inner cell goes into the outer's field), but the cells must be *constructed*
/// inner→outer — the order reference counting assumed when it placed the `dup`/
/// `drop` on their shared field operands; building them in the linking order would
/// run a consume before its matching dup. So for a chain of more than one cell the
/// cells are built into locals first (in construction order), then linked
/// (referencing those locals). A single cell has no such ordering and is linked
/// inline.
fn emit_holefill_chain(
    chain: Vec<ChainLink>,
    hole: LocalId,
    sargs: Vec<CExpr>,
    next: &mut usize,
) -> CExpr {
    let recur = |cur_hole: LocalId, sargs: Vec<CExpr>| {
        let mut args = sargs;
        args.push(CExpr::new(K::Local(cur_hole), Ty::Error));
        CExpr::new(K::Recur { args }, Ty::Error)
    };
    let fill = |hole: LocalId, cell: CExpr, field: usize| {
        let field = u32::try_from(field).expect("field index fits u32");
        CExpr::new(K::HoleFill { hole, cell: Box::new(cell), field }, Ty::Error)
    };
    let let_ = |local, value, body| {
        CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, Ty::Error)
    };

    if chain.len() == 1 {
        let ChainLink { cell, field } = chain.into_iter().next().expect("one cell");
        let h = fresh(next);
        return let_(h, fill(hole, cell, field), recur(h, sargs));
    }

    // Build the cells (with their embedded dup/drop) in construction order.
    let cells: Vec<(LocalId, usize)> = chain.iter().map(|link| (fresh(next), link.field)).collect();
    // Link the built cells outer→inner, threading the hole.
    let mut cur_hole = hole;
    let mut hole_binds: Vec<(LocalId, CExpr)> = Vec::new();
    for &(c, field) in cells.iter().rev() {
        let h = fresh(next);
        hole_binds.push((h, fill(cur_hole, CExpr::new(K::Local(c), Ty::Error), field)));
        cur_hole = h;
    }
    let mut result = recur(cur_hole, sargs);
    for (h, f) in hole_binds.into_iter().rev() {
        result = let_(h, f, result);
    }
    for (link, &(c, _)) in chain.into_iter().zip(cells.iter()).rev() {
        result = let_(c, link.cell, result);
    }
    result
}

// ---------------------------------------------------------------------------
// Predicates.
// ---------------------------------------------------------------------------

/// The arguments of `e` if it is a saturated direct call to `self_def`.
fn self_call_args(e: &CExpr, self_def: DefId, arity: usize) -> Option<&[CExpr]> {
    if let K::App { func, args } = &e.kind
        && let K::Global(def) = &func.kind
        && *def == self_def
        && args.len() == arity
    {
        return Some(args);
    }
    None
}

/// Whether `e` is exactly `Local(v)`.
fn is_local(e: &CExpr, v: LocalId) -> bool {
    matches!(&e.kind, K::Local(x) if *x == v)
}

/// Whether `e` is a data construction, possibly wrapped in the `dup`/`drop` that
/// reference counting placed on its field operands. A chain-link cell has this
/// shape (the recursion threads through a constructor, not through a call or other
/// operation).
fn is_construction(e: &CExpr) -> bool {
    match &e.kind {
        K::MakeData { .. } => true,
        K::Dup { body, .. } | K::Drop { body, .. } => is_construction(body),
        _ => false,
    }
}

/// Whether `self_def` is referenced (as a value or call target) anywhere in `e`.
fn contains_self(e: &CExpr, self_def: DefId) -> bool {
    let mut found = false;
    walk(e, &mut |n| {
        if let K::Global(def) = &n.kind
            && *def == self_def
        {
            found = true;
        }
    });
    found
}

/// Whether `v` is read anywhere in `e`.
fn uses_local(e: &CExpr, v: LocalId) -> bool {
    let mut found = false;
    walk(e, &mut |n| {
        if is_local(n, v) {
            found = true;
        }
    });
    found
}

/// The number of times `x` is read in `e`, counting *every* `Local` position —
/// including closure captures, `dup`/`drop`, row-polymorphic field-access evidence,
/// and hole tokens, which [`walk`] skips. The chain-detection linearity check
/// relies on this completeness: a recursion result captured in a hoisted field, for
/// instance, must still be counted, so the function is left as ordinary recursion
/// rather than having its `let` binder dropped out from under the capture.
fn count_uses(e: &CExpr, x: LocalId) -> usize {
    let mut n = 0;
    count_local(e, x, &mut n);
    n
}

fn count_local(e: &CExpr, x: LocalId, n: &mut usize) {
    match &e.kind {
        K::Local(l) => {
            if *l == x {
                *n += 1;
            }
        }
        K::Lit(_) | K::Global(_) | K::Error => {}
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => {
            args.iter().for_each(|a| count_local(a, x, n));
        }
        K::App { func, args } => {
            count_local(func, x, n);
            args.iter().for_each(|a| count_local(a, x, n));
        }
        K::If { cond, then, els } => {
            count_local(cond, x, n);
            count_local(then, x, n);
            count_local(els, x, n);
        }
        K::Let { value, body, .. } => {
            count_local(value, x, n);
            count_local(body, x, n);
        }
        K::MakeClosure { captures, .. } => {
            for &c in captures {
                if c == x {
                    *n += 1;
                }
            }
        }
        K::DataTag(base) => count_local(base, x, n),
        K::DataField { base, index, .. } => {
            count_local(base, x, n);
            if let FieldIndex::Dyn { evidence, .. } = index
                && *evidence == x
            {
                *n += 1;
            }
        }
        K::Reset { value, body, .. } => {
            count_local(value, x, n);
            count_local(body, x, n);
        }
        K::FreeReuse { token, body } => {
            if *token == x {
                *n += 1;
            }
            count_local(body, x, n);
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            if *local == x {
                *n += 1;
            }
            count_local(body, x, n);
        }
        K::Join { body, .. } | K::HoleStart { body, .. } => count_local(body, x, n),
        K::HoleFill { hole, cell, .. } => {
            if *hole == x {
                *n += 1;
            }
            count_local(cell, x, n);
        }
        K::HoleClose { hole, base } => {
            if *hole == x {
                *n += 1;
            }
            count_local(base, x, n);
        }
    }
}

/// Whether `e` is pure and total — free of capability effects, of aborts (integer
/// division/remainder by a possibly-zero divisor), and of non-terminating or
/// effectful calls — so it may be hoisted ahead of the recursion without changing
/// observable behavior. A call is admitted only when it is a saturated-or-partial
/// application of a statically known top-level function that `is_pure_total`
/// reports pure and total; any indirect or curried call is rejected.
fn pure_total(e: &CExpr, is_pure_total: &dyn Fn(DefId) -> bool) -> bool {
    let mut ok = true;
    walk(e, &mut |n| match &n.kind {
        K::App { func, .. } => {
            let safe = matches!(&func.kind, K::Global(def) if is_pure_total(*def));
            if !safe {
                ok = false;
            }
        }
        K::Prim { op, args } if crate::purity::op_unsafe_to_reorder(*op, args) => ok = false,
        _ => {}
    });
    ok
}

/// Visits every subexpression of `e` (pre-order), including the children of nodes
/// the tail-call transform never sees (handled for completeness).
fn walk(e: &CExpr, f: &mut impl FnMut(&CExpr)) {
    f(e);
    match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => {
            args.iter().for_each(|a| walk(a, f));
        }
        K::App { func, args } => {
            walk(func, f);
            args.iter().for_each(|a| walk(a, f));
        }
        K::If { cond, then, els } => {
            walk(cond, f);
            walk(then, f);
            walk(els, f);
        }
        K::Let { value, body, .. } => {
            walk(value, f);
            walk(body, f);
        }
        K::Reset { value, body, .. } => {
            walk(value, f);
            walk(body, f);
        }
        K::DataTag(base) | K::DataField { base, .. } => walk(base, f),
        K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::FreeReuse { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => walk(body, f),
        K::HoleFill { cell, .. } => walk(cell, f),
        K::HoleClose { base, .. } => walk(base, f),
    }
}
