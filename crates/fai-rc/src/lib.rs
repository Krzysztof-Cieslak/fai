// salsa's `tracked` macro emits `unsafe impl`s; we write no unsafe by hand.
#![allow(unsafe_code)]

//! Precise, ownership-based reference-count insertion over the Core IR.
//!
//! Under the uniform **consume** convention (operations consume their value
//! operands), reference counting is precise rather than path-insensitive:
//!
//! * **A-normal form first.** Every operation's operands are normalized to atoms
//!   (a local, literal, or global), binding compound operands to fresh `let`s.
//!   This makes sequence points explicit, so the dup/drop rules below are exact,
//!   and it makes every projection base a local that reference counting can drop.
//! * **Duplicate only when still live.** A consuming use of an owned variable is
//!   preceded by `Dup` only when that variable is still needed afterward (used
//!   again later, or live in the continuation past the operation's drop point).
//!   The last consuming use transfers ownership with no dup.
//! * **Drop at the last use (drop-early).** An owned binding whose last use is a
//!   *borrow* (a projection base, or offset evidence) — or which is never used —
//!   is dropped immediately after that use; when the last use is a *consume*, the
//!   consuming operation performs the release.
//! * **Borrowing projections.** `DataField`/`DataTag` read through their base
//!   without consuming it (the runtime no longer drops the base), so a matched
//!   value survives its projections and is released once by reference counting —
//!   exactly where reuse will later recycle it.
//! * **Captures are borrowed.** A lifted function borrows its captured slots
//!   (dup on use; the closure releases them when it dies), so they are never
//!   dropped by the body. `MakeClosure` *consumes* the captures supplied at the
//!   original lambda position (their references move into the new environment).
//!
//! The per-callee/per-operand consume-vs-borrow classification flows from a
//! borrow-signature provider. For now every argument and primitive operand is
//! owned (matching the previous behavior); inferred argument borrowing fills this
//! in later.
//!
//! Duplicating immediates and dropping them are runtime no-ops (tag-checked), so
//! this is correct for every value kind; closures, partial applications, strings,
//! and data values are released precisely.

use std::sync::Arc;

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, FnAbi, LoweredDef};
use fai_core::{NicheKind, fuse_def, reassociate_concat};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;
use fai_types::{Con, Ty};
use rustc_hash::FxHashSet;

pub use borrow::{BorrowSig, borrow_signature};
pub use bounds_sig::{entry_bounds, result_facts};
pub use escape::{EscapeSig, escape_signature, mark_escaping_closures};
pub use forward::rc_emit;
pub use length::{LenPresSig, length_preservation};
pub use mutual::{Group, MutualGroups, combined_lowered, member_wrapper, mutual_groups};
pub use reuse_sig::{ReuseSig, forwards_to, reuse_class, reuse_signature};
pub use verify::check_rc;

mod borrow;
mod bounds_sig;
mod escape;
mod forward;
mod length;
mod mutual;
mod purity;
mod reuse_sig;
mod sroa;
mod trmc;
mod verify;

/// A set of locals (used for free-variable and liveness sets).
type Locals = FxHashSet<LocalId>;

/// Inserts reference-count operations into `name`'s lowered definition.
#[salsa::tracked]
pub fn rc(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    // Read the fused, fully-inlined Core: primitive re-export wrappers and small
    // non-recursive helpers are folded in, and recognized combinator pipelines are
    // rewritten to calls to synthesized loops (the loops themselves are emitted by
    // the driver, like a mutual-recursion combined loop). Reference counting
    // balances the merged body directly and reuse analysis can recycle a freed cell
    // into a construction that came from a helper (and self-/callee borrow
    // signatures, read from the same inlined form below, stay consistent).
    let lowered = fuse_def(db, file, name);
    // Mark each non-escaping capturing closure for stack allocation before
    // reference counting (which preserves the allocation flag). Escape analysis
    // leaves reference counting unchanged — only the cell's storage and whether it
    // is freed differ — so this is a representation choice layered on the same
    // dup/drop discipline.
    let mut lowered = lowered.body.clone();
    mark_escaping_closures(db, &mut lowered);
    // The borrow signature of `name` itself (the entry function's parameters): a
    // borrowed parameter is treated like a capture — never dropped, duplicated on
    // a consuming use. Lifted lambdas are reached only via `apply_n`, so their
    // parameters stay owned.
    let self_sig = borrow_signature(db, file, name);
    Arc::new(rc_lowered(db, &lowered, &self_sig))
}

/// Inserts reference-count operations into an already-lowered definition, given
/// its entry's borrow signature. Used by [`rc`] and by callers that synthesize a
/// [`LoweredDef`] outside the per-definition pipeline (e.g. contract harnesses,
/// which pass an all-owned signature).
#[must_use]
pub fn rc_lowered(db: &dyn Db, lowered: &LoweredDef, self_sig: &BorrowSig) -> LoweredDef {
    // Left-reassociate `++` chains first, so the runtime's in-place append into a
    // unique left accumulator fires for a (right-associative) source chain, and
    // reference counting balances the rewritten tree. Behavior-preserving (string
    // concatenation is pure and associative; operand evaluation order is kept).
    let reassociated = reassociate_concat(lowered);
    let lowered = &reassociated;
    let mut next = next_free_local(lowered);

    // Per-call argument borrowing: a saturated direct call to a known top-level
    // function borrows the parameters that function's signature marks borrowed.
    let arg_borrows = |def: DefId, nargs: usize| -> Vec<bool> {
        let Some(cf) = db.source_file(def.file) else { return vec![false; nargs] };
        let sig = borrow_signature(db, cf, def.name);
        if sig.exploitable_at(nargs) { sig.0.clone() } else { vec![false; nargs] }
    };

    // Whether calling a top-level function is pure and total, so the tail-call
    // transform may hoist a later constructor argument that calls it ahead of the
    // back-edge. Unknown/builtin targets are conservatively impure.
    let is_pure_total = |def: DefId| {
        db.source_file(def.file).is_some_and(|f| purity::is_pure_total(db, f, def.name))
    };

    // The entry's offset-evidence-parameter count: a row-polymorphic function
    // calls itself curried (partially applied to its evidence, then to the real
    // arguments), so those calls are normalized to a saturated self-call *before*
    // reference counting, where the nested application is still intact and no
    // dup/drop has been inserted (see [`trmc::fuse_evidence_self_calls`]). The
    // flattened loop then carries the evidence as an ordinary loop-carried
    // parameter, just like the real ones.
    let evidence = fai_types::declared_or_inferred_scheme(db, lowered.def)
        .map_or(0, |s| fai_types::evidence_count(&s));

    // The entry's native calling convention: a spread (fixed-shape float
    // aggregate) parameter/result is exploded into scalar `f64` components by the
    // SROA pass below (a lifted lambda is reached uniformly via `apply_n`, so it
    // keeps the boxed representation).
    let entry_abi = fai_core::abi_of(db, lowered.def);
    let entry_has_spread = entry_abi.spread_return().is_some()
        || (0..entry_abi.params.len()).any(|i| entry_abi.spread_param(i).is_some());

    let mut fns = Vec::with_capacity(lowered.fns.len());
    let mut entry_spread_params: Vec<Option<Vec<LocalId>>> = Vec::new();
    for (i, f) in lowered.fns.iter().enumerate() {
        let mut borrowed: Locals = f.captures.iter().copied().collect();
        if i == 0 {
            for (p, &param) in f.params.iter().enumerate() {
                if self_sig.is_borrowed(p) {
                    borrowed.insert(param);
                }
            }
        }
        let raw = if i == 0 && evidence > 0 {
            trmc::fuse_evidence_self_calls(f.body.clone(), lowered.def, &f.params[..evidence])
        } else {
            f.body.clone()
        };
        let body = anf(raw, &mut next);
        // Scalar-replace fixed-shape float aggregates: a spread parameter becomes
        // component locals, a constructed/returned aggregate its scalar components,
        // reassembling a cell only at a boxed boundary. The entry uses the
        // definition's spread ABI; a lifted lambda stays uniform.
        let fn_abi = if i == 0 { (*entry_abi).clone() } else { FnAbi::default() };
        let (body, spread_params) = sroa::sroa_fn(db, body, &fn_abi, &f.params, &mut next);
        // A spread parameter's aggregate anchor carries no runtime value — its
        // scalar components are bound directly from the incoming registers and the
        // body never references the anchor — so treat it like a borrowed parameter:
        // reference counting must not duplicate or drop it (a drop would read an
        // unbound slot).
        for (p, comps) in spread_params.iter().enumerate() {
            if comps.is_some() {
                borrowed.insert(f.params[p]);
            }
        }
        if i == 0 {
            entry_spread_params = spread_params;
        }
        let used = fv_owned(&body, &borrowed);
        let mut cx = Rc { captures: &borrowed, next, call_borrows: &arg_borrows };
        let body = cx.owned(body, &Locals::default());
        next = cx.next;
        // Recycle a dead data cell into a same-size construction where one follows.
        let data = data_typed_locals(&body);
        let mut body = reuse_pass(body, &data, &mut next);
        // Drop parameters that the body never mentions (drop-early, at entry) —
        // but never a borrowed parameter (the caller owns and releases it) nor a
        // spread-parameter anchor (it carries no runtime value; its components are
        // bound from the incoming registers and dropped as ordinary scalar locals).
        let is_spread_anchor = |p: usize| entry_spread_params.get(p).is_some_and(Option::is_some);
        for (p, &param) in f.params.iter().enumerate().rev() {
            let spread_anchor = i == 0 && is_spread_anchor(p);
            if !used.contains(&param) && !borrowed.contains(&param) && !spread_anchor {
                body = drop_(param, body);
            }
        }
        // Flatten self-tail-recursion in the entry function into a loop. Deferred
        // for a spread-ABI entry (loop-carried float-aggregate state is future
        // work): it recurses via direct calls instead. A no-op unless tail-recursive.
        if i == 0 && !entry_has_spread {
            body = trmc::flatten(body, &f.params, lowered.def, &is_pure_total, &mut next);
        }
        fns.push(CoreFn { params: f.params.clone(), captures: f.captures.clone(), body });
    }
    LoweredDef {
        def: lowered.def,
        fns,
        entry_borrowed: self_sig.0.clone(),
        reuse_entry: None,
        entry_spread_params,
    }
}

// ---------------------------------------------------------------------------
// Fresh locals.
// ---------------------------------------------------------------------------

/// The first local slot not used anywhere in `lowered` (so synthesized binders —
/// A-normal-form temporaries and projection results — never collide).
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
        K::Prim { args, .. } | K::MakeData { args, .. } => {
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
        K::Recur { args } => args.iter().for_each(|a| max_local(a, max)),
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

// ---------------------------------------------------------------------------
// A-normal form: every operand of an operation becomes an atom.
// ---------------------------------------------------------------------------

/// Whether `e` is an atom (needs no binding to appear as an operand).
fn is_atom(e: &CExpr) -> bool {
    matches!(e.kind, K::Lit(_) | K::Local(_) | K::Global(_) | K::Error)
}

pub(crate) fn fresh(next: &mut usize) -> LocalId {
    let id = LocalId::from_index(*next);
    *next += 1;
    id
}

/// Normalizes `e` so every operand of an operation is an atom, with all bindings
/// **flattened** into one straight-line sequence (sub-operand bindings are hoisted
/// to the enclosing sequence rather than nested in a `let` value). Flat sequencing
/// keeps a value's last use — and so its drop/reset — at the outer level where a
/// following construction can recycle it.
fn anf(e: CExpr, next: &mut usize) -> CExpr {
    let mut binds = Vec::new();
    let op = anf_op(e, &mut binds, next);
    wrap_binds(binds, op)
}

/// Normalizes `e` into an operation (or atom) whose operands are atoms, pushing
/// every binding — including the contents of any nested `let` — into `binds`.
fn anf_op(e: CExpr, binds: &mut Vec<(LocalId, CExpr)>, next: &mut usize) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::Error => CExpr::new(kind, ty),
        K::Prim { op, args } => {
            let args = args.into_iter().map(|a| atomize(a, binds, next)).collect();
            CExpr::new(K::Prim { op, args }, ty)
        }
        K::MakeData { tag, args, reuse, scalars, niche } => {
            let args = args.into_iter().map(|a| atomize(a, binds, next)).collect();
            CExpr::new(K::MakeData { tag, args, reuse, scalars, niche }, ty)
        }
        // A-normal form runs before reference counting forwards reuse tokens, so
        // `reuse` is empty here; it is carried through verbatim.
        K::App { func, args, reuse, alloc } => {
            let func = Box::new(atomize(*func, binds, next));
            let args = args.into_iter().map(|a| atomize(a, binds, next)).collect();
            CExpr::new(K::App { func, args, reuse, alloc }, ty)
        }
        // SROA runs after A-normal form, so these are not encountered here; handled
        // defensively (atomize a spread's components; normalize a letmany's halves).
        K::Spread { components } => {
            let components = components.into_iter().map(|a| atomize(a, binds, next)).collect();
            CExpr::new(K::Spread { components }, ty)
        }
        K::LetMany { locals, value, body } => CExpr::new(
            K::LetMany {
                locals,
                value: Box::new(anf(*value, next)),
                body: Box::new(anf(*body, next)),
            },
            ty,
        ),
        K::DataTag { base, niche } => {
            CExpr::new(K::DataTag { base: Box::new(to_local(*base, binds, next)), niche }, ty)
        }
        K::DataField { base, index, scalar, niche } => CExpr::new(
            K::DataField { base: Box::new(to_local(*base, binds, next)), index, scalar, niche },
            ty,
        ),
        // Branches keep their own scopes (a binding in one branch must not escape).
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(atomize(*cond, binds, next)),
                then: Box::new(anf(*then, next)),
                els: Box::new(anf(*els, next)),
            },
            ty,
        ),
        // Flatten the binding into the enclosing sequence and continue with body.
        K::Let { local, value, body } => {
            let value = anf_op(*value, binds, next);
            binds.push((local, value));
            anf_op(*body, binds, next)
        }
        // Captures are locals already; no compound operands.
        K::MakeClosure { func, captures, alloc } => {
            CExpr::new(K::MakeClosure { func, captures, alloc }, ty)
        }
        // Not produced by lowering; handled defensively for exhaustiveness.
        K::Reset { value, token, body } => CExpr::new(
            K::Reset {
                value: Box::new(anf(*value, next)),
                token,
                body: Box::new(anf(*body, next)),
            },
            ty,
        ),
        K::FreeReuse { token, body } => {
            CExpr::new(K::FreeReuse { token, body: Box::new(anf(*body, next)) }, ty)
        }
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(anf(*body, next)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(anf(*body, next)) }, ty)
        }
        // The tail-call transform runs after reference counting, so its loop and
        // hole nodes never reach A-normal form.
        K::Join { .. }
        | K::Recur { .. }
        | K::HoleStart { .. }
        | K::HoleFill { .. }
        | K::HoleClose { .. } => unreachable!("tail-call nodes precede A-normal form"),
    }
}

/// Normalizes `e` and, if the result is compound, binds it to a fresh local,
/// returning the bound atom; pushes all bindings into `binds`.
fn atomize(e: CExpr, binds: &mut Vec<(LocalId, CExpr)>, next: &mut usize) -> CExpr {
    let r = anf_op(e, binds, next);
    if is_atom(&r) {
        return r;
    }
    bind(r, binds, next)
}

/// Like [`atomize`], but always yields a *local* (binding even a global or
/// literal). A projection borrows its base, so the base must be an owned local
/// that reference counting can release — in particular a global naming a forced
/// zero-arity value, which allocates when read.
fn to_local(e: CExpr, binds: &mut Vec<(LocalId, CExpr)>, next: &mut usize) -> CExpr {
    let r = anf_op(e, binds, next);
    if matches!(r.kind, K::Local(_)) {
        return r;
    }
    bind(r, binds, next)
}

/// Binds `r` to a fresh local, recording the binding and returning the local.
fn bind(r: CExpr, binds: &mut Vec<(LocalId, CExpr)>, next: &mut usize) -> CExpr {
    let ty = r.ty.clone();
    let local = fresh(next);
    binds.push((local, r));
    CExpr::new(K::Local(local), ty)
}

/// Wraps `inner` in the bindings, outermost first (so they evaluate in order).
fn wrap_binds(binds: Vec<(LocalId, CExpr)>, inner: CExpr) -> CExpr {
    let mut e = inner;
    for (local, value) in binds.into_iter().rev() {
        let ty = e.ty.clone();
        e = CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(e) }, ty);
    }
    e
}

// ---------------------------------------------------------------------------
// Free owned variables.
// ---------------------------------------------------------------------------

/// The free owned locals of `e` (everything used, minus captures and locals bound
/// by enclosing `let`s within `e`).
fn fv_owned(e: &CExpr, captures: &Locals) -> Locals {
    let mut out = Locals::default();
    let mut bound = Locals::default();
    collect_fv(e, captures, &mut bound, &mut out);
    out
}

fn collect_fv(e: &CExpr, captures: &Locals, bound: &mut Locals, out: &mut Locals) {
    let note = |x: LocalId, bound: &Locals, out: &mut Locals| {
        if !captures.contains(&x) && !bound.contains(&x) {
            out.insert(x);
        }
    };
    match &e.kind {
        K::Local(x) => note(*x, bound, out),
        K::Lit(_) | K::Global(_) | K::Error => {}
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Spread { components: args } => {
            args.iter().for_each(|a| collect_fv(a, captures, bound, out));
        }
        // Reuse tokens are not reference-counted values (they are consumed once by
        // the callee), so they are not free *owned* variables; `reuse` is ignored.
        K::App { func, args, .. } => {
            collect_fv(func, captures, bound, out);
            args.iter().for_each(|a| collect_fv(a, captures, bound, out));
        }
        K::If { cond, then, els } => {
            collect_fv(cond, captures, bound, out);
            collect_fv(then, captures, bound, out);
            collect_fv(els, captures, bound, out);
        }
        K::Let { local, value, body } => {
            collect_fv(value, captures, bound, out);
            let added = bound.insert(*local);
            collect_fv(body, captures, bound, out);
            if added {
                bound.remove(local);
            }
        }
        K::LetMany { locals, value, body } => {
            collect_fv(value, captures, bound, out);
            let added: Vec<LocalId> = locals.iter().copied().filter(|l| bound.insert(*l)).collect();
            collect_fv(body, captures, bound, out);
            for l in added {
                bound.remove(&l);
            }
        }
        K::MakeClosure { captures: caps, .. } => {
            caps.iter().for_each(|c| note(*c, bound, out));
        }
        K::DataTag { base, .. } => collect_fv(base, captures, bound, out),
        K::DataField { base, index, .. } => {
            collect_fv(base, captures, bound, out);
            if let FieldIndex::Dyn { evidence, .. } = index {
                note(*evidence, bound, out);
            }
        }
        K::Reset { value, token, body } => {
            collect_fv(value, captures, bound, out);
            let added = bound.insert(*token);
            collect_fv(body, captures, bound, out);
            if added {
                bound.remove(token);
            }
        }
        K::FreeReuse { token, body } => {
            note(*token, bound, out);
            collect_fv(body, captures, bound, out);
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            note(*local, bound, out);
            collect_fv(body, captures, bound, out);
        }
        K::Join { params, body } => {
            let added: Vec<LocalId> = params.iter().copied().filter(|p| bound.insert(*p)).collect();
            collect_fv(body, captures, bound, out);
            for p in added {
                bound.remove(&p);
            }
        }
        K::Recur { args } => args.iter().for_each(|a| collect_fv(a, captures, bound, out)),
        K::HoleStart { hole, body } => {
            let added = bound.insert(*hole);
            collect_fv(body, captures, bound, out);
            if added {
                bound.remove(hole);
            }
        }
        K::HoleFill { hole, cell, .. } => {
            note(*hole, bound, out);
            collect_fv(cell, captures, bound, out);
        }
        K::HoleClose { hole, base } => {
            note(*hole, bound, out);
            collect_fv(base, captures, bound, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Precise reference counting.
// ---------------------------------------------------------------------------

/// Per-function reference-counting state.
struct Rc<'a> {
    /// Borrowed slots (captures and borrowed parameters): dup on a consuming use,
    /// never dropped here.
    captures: &'a Locals,
    /// The next free local slot (for projection-result temporaries).
    next: usize,
    /// Per-argument borrow flags for a saturated direct call to a top-level
    /// definition (empty/all-false when borrowing does not apply).
    call_borrows: &'a dyn Fn(DefId, usize) -> Vec<bool>,
}

impl Rc<'_> {
    fn fresh(&mut self) -> LocalId {
        fresh(&mut self.next)
    }

    fn is_capture(&self, x: LocalId) -> bool {
        self.captures.contains(&x)
    }

    /// Transforms `e`, producing its value, where `live` is the set of owned
    /// locals still used after `e`.
    fn owned(&mut self, e: CExpr, live: &Locals) -> CExpr {
        let CExpr { kind, ty } = e;
        match kind {
            K::Lit(_) | K::Global(_) | K::Error => CExpr::new(kind, ty),
            // A consuming use of an atom local.
            K::Local(x) => {
                let used = CExpr::new(K::Local(x), ty);
                if self.is_capture(x) || live.contains(&x) { dup_(x, used) } else { used }
            }
            K::Prim { op, args } => {
                let borrows = prim_borrows(op, &args);
                let rebuilt = |args| CExpr::new(K::Prim { op, args }, ty.clone());
                self.operands_rc(args, &borrows, live, rebuilt)
            }
            K::MakeData { tag, args, reuse, scalars, niche } => {
                // Constructor fields are stored (consumed); none are borrowed.
                let borrows = vec![false; args.len()];
                let rebuilt = move |args| {
                    CExpr::new(K::MakeData { tag, args, reuse, scalars, niche }, ty.clone())
                };
                self.operands_rc(args, &borrows, live, rebuilt)
            }
            // A spread aggregate's components are scalar floats (no reference
            // count): consumed into the multi-value result/argument, their dup/drop
            // are runtime no-ops, but the consume discipline is kept uniform.
            K::Spread { components } => {
                let borrows = vec![false; components.len()];
                let rebuilt = move |components| CExpr::new(K::Spread { components }, ty.clone());
                self.operands_rc(components, &borrows, live, rebuilt)
            }
            // Binds a spread-returning call's result components (scalar floats, no
            // reference count, so the bound locals need no drop). The call stays
            // multi-result: its boxed arguments are reference-counted in place
            // (`operands_rc`'s single-temporary wrapping would collapse the N
            // results), with consumed-but-live arguments duplicated before the call
            // and dead borrowed arguments dropped after the bind. A spread argument
            // is a `Spread` of scalar-float components (no reference count).
            K::LetMany { locals, value, body } => {
                let fvb = fv_owned(&body, self.captures);
                let mut live_after = fvb.clone();
                for l in &locals {
                    live_after.remove(l);
                }
                live_after.extend(live);
                let body2 = self.owned(*body, live);
                let K::App { func, args, reuse, alloc } = value.kind else {
                    // Defensive: a spread value that is not a direct call cannot be
                    // a multi-result bind; count it as an ordinary value.
                    let value2 = self.owned(CExpr::new(value.kind, value.ty), &live_after);
                    return CExpr::new(
                        K::LetMany { locals, value: Box::new(value2), body: Box::new(body2) },
                        ty,
                    );
                };
                let nargs = args.len();
                let borrows = match &func.kind {
                    K::Global(def) => (self.call_borrows)(*def, nargs),
                    _ => vec![false; nargs],
                };
                let is_borrow = |i: usize| borrows.get(i).copied().unwrap_or(false);
                let consumed: Locals = args
                    .iter()
                    .enumerate()
                    .filter_map(|(i, a)| match a.kind {
                        K::Local(x) if !is_borrow(i) => Some(x),
                        _ => None,
                    })
                    .collect();
                let mut dups = Vec::new();
                let mut dead = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let K::Local(x) = a.kind else { continue };
                    if is_borrow(i) {
                        if !self.is_capture(x)
                            && !live_after.contains(&x)
                            && !consumed.contains(&x)
                            && !dead.contains(&x)
                        {
                            dead.push(x);
                        }
                    } else {
                        let later =
                            args.iter().enumerate().skip(i + 1).any(|(j, b)| {
                                !is_borrow(j) && matches!(b.kind, K::Local(y) if y == x)
                            });
                        if self.is_capture(x) || live_after.contains(&x) || later {
                            dups.push(x);
                        }
                    }
                }
                let call = CExpr::new(K::App { func, args, reuse, alloc }, value.ty);
                let inner = dropify(dead, body2);
                let mut out = CExpr::new(
                    K::LetMany { locals, value: Box::new(call), body: Box::new(inner) },
                    ty,
                );
                for x in dups.into_iter().rev() {
                    out = dup_(x, out);
                }
                out
            }
            // Reference counting runs before reuse tokens are forwarded, so `reuse`
            // is empty here; it is carried through verbatim (tokens are not
            // reference-counted operands).
            K::App { func, args, reuse, alloc } => {
                // The callee value is consumed; arguments at a saturated direct
                // call to a top-level definition follow its borrow signature.
                let nargs = args.len();
                let arg_borrows = match &func.kind {
                    K::Global(def) => (self.call_borrows)(*def, nargs),
                    _ => vec![false; nargs],
                };
                let mut borrows = Vec::with_capacity(nargs + 1);
                borrows.push(false);
                borrows.extend(arg_borrows);
                let mut operands = Vec::with_capacity(nargs + 1);
                operands.push(*func);
                operands.extend(args);
                let rebuilt = move |mut ops: Vec<CExpr>| {
                    let func = Box::new(ops.remove(0));
                    CExpr::new(K::App { func, args: ops, reuse, alloc }, ty.clone())
                };
                self.operands_rc(operands, &borrows, live, rebuilt)
            }
            K::MakeClosure { func, captures, alloc } => {
                let inner =
                    CExpr::new(K::MakeClosure { func, captures: captures.clone(), alloc }, ty);
                // Each capture's reference moves into the new environment; dup it
                // when it is a borrowed slot or still needed afterward.
                let mut result = inner;
                for (i, &c) in captures.iter().enumerate().rev() {
                    let later = captures[i + 1..].contains(&c);
                    if self.is_capture(c) || live.contains(&c) || later {
                        result = dup_(c, result);
                    }
                }
                result
            }
            // Borrowing projections in tail position: read the field/tag, then
            // drop any owned base/evidence that dies here (drop-early).
            K::DataField { base, index, scalar, niche } => {
                let proj = CExpr::new(K::DataField { base, index, scalar, niche }, ty);
                let borrows = projection_borrows(&proj);
                self.borrow_tail(proj, borrows, live)
            }
            K::DataTag { base, niche } => {
                let proj = CExpr::new(K::DataTag { base, niche }, ty);
                let borrows = projection_borrows(&proj);
                self.borrow_tail(proj, borrows, live)
            }
            K::If { cond, then, els } => self.conditional(*cond, *then, *els, ty, live),
            K::Let { local, value, body } => self.binding(local, *value, *body, ty, live),
            // The reuse pass runs after this; pass through for exhaustiveness.
            K::Reset { value, token, body } => {
                let body = self.owned(*body, live);
                CExpr::new(K::Reset { value, token, body: Box::new(body) }, ty)
            }
            K::FreeReuse { token, body } => {
                let body = self.owned(*body, live);
                CExpr::new(K::FreeReuse { token, body: Box::new(body) }, ty)
            }
            // Lowering never emits these; pass through.
            K::Dup { local, body } => dup_(local, self.owned(*body, live)),
            K::Drop { local, body } => drop_(local, self.owned(*body, live)),
            // The tail-call transform runs after reference counting.
            K::Join { .. }
            | K::Recur { .. }
            | K::HoleStart { .. }
            | K::HoleFill { .. }
            | K::HoleClose { .. } => {
                unreachable!("tail-call nodes are inserted after reference counting")
            }
        }
    }

    /// Transforms an operation's atom operands, where `borrows[i]` marks operand
    /// `i` as borrowed (read, not consumed). A consume operand still needed
    /// afterward is duplicated before the operation; a borrowed operand whose last
    /// use is here is dropped right after it.
    fn operands_rc(
        &mut self,
        operands: Vec<CExpr>,
        borrows: &[bool],
        live: &Locals,
        rebuild: impl FnOnce(Vec<CExpr>) -> CExpr,
    ) -> CExpr {
        let is_borrow = |i: usize| borrows.get(i).copied().unwrap_or(false);

        // A borrowed operand must be a local the caller owns and releases. A
        // non-local (e.g. a boxed literal) is bound to a fresh local first, so it
        // is dropped after the operation rather than leaked.
        let mut pre_binds: Vec<(LocalId, CExpr)> = Vec::new();
        let operands: Vec<CExpr> = operands
            .into_iter()
            .enumerate()
            .map(|(i, a)| {
                if is_borrow(i) && !matches!(a.kind, K::Local(_)) {
                    let ty = a.ty.clone();
                    let t = self.fresh();
                    pre_binds.push((t, a));
                    CExpr::new(K::Local(t), ty)
                } else {
                    a
                }
            })
            .collect();

        // Locals this operation consumes (transfers ownership of).
        let mut consumed = Locals::default();
        for (i, a) in operands.iter().enumerate() {
            if !is_borrow(i)
                && let K::Local(x) = a.kind
            {
                consumed.insert(x);
            }
        }

        // Duplicate a consume operand still needed afterward: a borrowed slot,
        // live after the op, or consumed again at a later operand.
        let mut dups = Vec::new();
        for (i, a) in operands.iter().enumerate() {
            if is_borrow(i) {
                continue;
            }
            if let K::Local(x) = a.kind {
                let later = operands
                    .iter()
                    .enumerate()
                    .skip(i + 1)
                    .any(|(j, b)| !is_borrow(j) && matches!(b.kind, K::Local(y) if y == x));
                if self.is_capture(x) || live.contains(&x) || later {
                    dups.push(x);
                }
            }
        }

        // A borrowed operand whose last use is this op (not consumed here, not
        // live, not a capture) is released right after the operation.
        let mut dead = Vec::new();
        for (i, a) in operands.iter().enumerate() {
            if is_borrow(i)
                && let K::Local(x) = a.kind
                && !self.is_capture(x)
                && !live.contains(&x)
                && !consumed.contains(&x)
                && !dead.contains(&x)
            {
                dead.push(x);
            }
        }

        let mut e = rebuild(operands);
        if !dead.is_empty() {
            let ty = e.ty.clone();
            let tmp = self.fresh();
            let body = dropify(dead, CExpr::new(K::Local(tmp), ty.clone()));
            e = CExpr::new(K::Let { local: tmp, value: Box::new(e), body: Box::new(body) }, ty);
        }
        for x in dups.into_iter().rev() {
            e = dup_(x, e);
        }
        // Bind any borrowed non-local operands outermost (they evaluate first, are
        // borrowed by the operation, and were released by the dead-borrow drops).
        for (local, value) in pre_binds.into_iter().rev() {
            let ty = e.ty.clone();
            e = CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(e) }, ty);
        }
        e
    }

    /// Wraps a borrowing projection in tail position so that owned borrowed locals
    /// dead afterward are dropped right after the projection.
    fn borrow_tail(&mut self, proj: CExpr, borrows: Vec<LocalId>, live: &Locals) -> CExpr {
        let dead = self.dead_borrows(borrows, live);
        if dead.is_empty() {
            return proj;
        }
        let ty = proj.ty.clone();
        let tmp = self.fresh();
        let body = dropify(dead, CExpr::new(K::Local(tmp), ty.clone()));
        CExpr::new(K::Let { local: tmp, value: Box::new(proj), body: Box::new(body) }, ty)
    }

    /// The owned (non-capture) locals among `borrows` that are dead afterward.
    fn dead_borrows(&self, borrows: Vec<LocalId>, live: &Locals) -> Vec<LocalId> {
        borrows.into_iter().filter(|b| !self.is_capture(*b) && !live.contains(b)).collect()
    }

    fn conditional(
        &mut self,
        cond: CExpr,
        then: CExpr,
        els: CExpr,
        ty: fai_types::Ty,
        live: &Locals,
    ) -> CExpr {
        let fvt = fv_owned(&then, self.captures);
        let fve = fv_owned(&els, self.captures);

        // Vars alive entering the branches: what the branches or continuation use.
        let mut branch_in = live.clone();
        branch_in.extend(&fvt);
        branch_in.extend(&fve);

        let then2 = self.owned(then, live);
        let els2 = self.owned(els, live);
        let d_then: Vec<LocalId> =
            branch_in.iter().filter(|v| !fvt.contains(v) && !live.contains(v)).copied().collect();
        let d_els: Vec<LocalId> =
            branch_in.iter().filter(|v| !fve.contains(v) && !live.contains(v)).copied().collect();
        let then2 = dropify(d_then, then2);
        let els2 = dropify(d_els, els2);

        // The condition is an immediate `Bool`, consumed by the test; dup only if
        // it is also needed in a branch or afterward.
        let cond_dup = match cond.kind {
            K::Local(c) if self.is_capture(c) || branch_in.contains(&c) => Some(c),
            _ => None,
        };
        let if_expr = CExpr::new(
            K::If { cond: Box::new(cond), then: Box::new(then2), els: Box::new(els2) },
            ty,
        );
        match cond_dup {
            Some(c) => dup_(c, if_expr),
            None => if_expr,
        }
    }

    fn binding(
        &mut self,
        local: LocalId,
        value: CExpr,
        body: CExpr,
        ty: fai_types::Ty,
        live: &Locals,
    ) -> CExpr {
        let fvb = fv_owned(&body, self.captures);
        let mut live_value = fvb.clone();
        live_value.remove(&local);
        live_value.extend(live);

        let mut body2 = self.owned(body, live);
        if !fvb.contains(&local) {
            body2 = drop_(local, body2);
        }

        // A projection bound to `local` keeps borrowing semantics: emit it as-is
        // and drop any owned base/evidence that dies here at the body's start.
        let value2 = if is_projection(&value) {
            let dead = self.dead_borrows(projection_borrows(&value), &live_value);
            body2 = dropify(dead, body2);
            value
        } else {
            self.owned(value, &live_value)
        };

        CExpr::new(K::Let { local, value: Box::new(value2), body: Box::new(body2) }, ty)
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Per-operand borrow flags for a primitive. Inspect-only primitives (`=`,
/// `compare`, the `String` readers) borrow their operands when those are boxed,
/// reference-counted values; every other primitive consumes its operands. The
/// decision is uniform across a call's operands (those primitives are
/// homogeneous), keyed on the first operand's type, so it matches the variant
/// code generation selects.
fn prim_borrows(op: fai_core::ir::Prim, args: &[CExpr]) -> Vec<bool> {
    let borrow = args.first().is_some_and(|a| op.borrows_operand(&a.ty));
    vec![borrow; args.len()]
}

fn dup_(local: LocalId, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Dup { local, body: Box::new(body) }, ty)
}

pub(crate) fn drop_(local: LocalId, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Drop { local, body: Box::new(body) }, ty)
}

/// Prepends a deterministic (slot-ordered) sequence of drops to `e`.
fn dropify(mut drops: Vec<LocalId>, e: CExpr) -> CExpr {
    drops.sort_by_key(|l| l.index());
    drops.dedup();
    let mut e = e;
    for d in drops.into_iter().rev() {
        e = drop_(d, e);
    }
    e
}

/// Whether `e` is a bare borrowing projection (`DataField`/`DataTag`).
fn is_projection(e: &CExpr) -> bool {
    matches!(e.kind, K::DataField { .. } | K::DataTag { .. })
}

// ---------------------------------------------------------------------------
// Reuse analysis: recycle a dead data cell into a same-size construction.
// ---------------------------------------------------------------------------

/// Locals that name boxed data cells reuse may recycle: any local used as a
/// projection base (`DataField`/`DataTag`) is necessarily a data value (a match
/// scrutinee or a record being read/updated), and any local bound to a value of a
/// boxed data type also qualifies (e.g. a freshly constructed record).
pub(crate) fn data_typed_locals(e: &CExpr) -> Locals {
    let mut out = Locals::default();
    collect_data_locals(e, &mut out);
    out
}

fn collect_data_locals(e: &CExpr, out: &mut Locals) {
    let note_base = |base: &CExpr, out: &mut Locals| {
        if let K::Local(l) = base.kind {
            out.insert(l);
        }
    };
    match &e.kind {
        K::Let { local, value, body } => {
            if is_boxed_data_ty(&value.ty) {
                out.insert(*local);
            }
            collect_data_locals(value, out);
            collect_data_locals(body, out);
        }
        K::If { cond, then, els } => {
            collect_data_locals(cond, out);
            collect_data_locals(then, out);
            collect_data_locals(els, out);
        }
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Spread { components: args } => {
            args.iter().for_each(|a| collect_data_locals(a, out));
        }
        // A spread-returning call's result components are scalar floats, not boxed
        // data, so the bound locals are never reuse candidates; recurse the halves.
        K::LetMany { value, body, .. } => {
            collect_data_locals(value, out);
            collect_data_locals(body, out);
        }
        K::App { func, args, .. } => {
            collect_data_locals(func, out);
            args.iter().for_each(|a| collect_data_locals(a, out));
        }
        K::DataTag { base, .. } => {
            note_base(base, out);
            collect_data_locals(base, out);
        }
        K::DataField { base, .. } => {
            note_base(base, out);
            collect_data_locals(base, out);
        }
        K::Reset { value, body, .. } => {
            collect_data_locals(value, out);
            collect_data_locals(body, out);
        }
        K::FreeReuse { body, .. } => collect_data_locals(body, out),
        K::Dup { body, .. } | K::Drop { body, .. } => collect_data_locals(body, out),
        // The tail-call transform runs after reuse analysis; handled defensively.
        K::Join { body, .. } | K::HoleStart { body, .. } => collect_data_locals(body, out),
        K::Recur { args } => args.iter().for_each(|a| collect_data_locals(a, out)),
        K::HoleFill { cell, .. } => collect_data_locals(cell, out),
        K::HoleClose { base, .. } => collect_data_locals(base, out),
        K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

/// Whether values of `ty` are boxed data cells (so resetting one yields a usable
/// reuse token). Records, tuples, ADTs, lists, and interface dictionaries qualify;
/// scalars, strings, floats, functions, and type variables do not.
pub(crate) fn is_boxed_data_ty(ty: &Ty) -> bool {
    fn is_data_head(ty: &Ty) -> bool {
        match ty {
            Ty::Adt(_) | Ty::Interface(_) | Ty::Con(Con::List) => true,
            Ty::App(head, _) => is_data_head(head),
            _ => false,
        }
    }
    matches!(ty, Ty::Record(_) | Ty::Tuple(_)) || is_data_head(ty)
}

/// Rewrites the drop of a dead data cell into a reset whose token a same-size
/// construction on each path reuses; paths with no construction keep a plain drop.
fn reuse_pass(e: CExpr, data: &Locals, next: &mut usize) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::Drop { local, body } if data.contains(&local) => {
            let body = reuse_pass(*body, data, next);
            release(local, body, next)
        }
        K::Drop { local, body } => drop_(local, reuse_pass(*body, data, next)),
        K::Dup { local, body } => dup_(local, reuse_pass(*body, data, next)),
        K::Let { local, value, body } => CExpr::new(
            K::Let {
                local,
                value: Box::new(reuse_pass(*value, data, next)),
                body: Box::new(reuse_pass(*body, data, next)),
            },
            ty,
        ),
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(reuse_pass(*cond, data, next)),
                then: Box::new(reuse_pass(*then, data, next)),
                els: Box::new(reuse_pass(*els, data, next)),
            },
            ty,
        ),
        K::Reset { value, token, body } => {
            CExpr::new(K::Reset { value, token, body: Box::new(reuse_pass(*body, data, next)) }, ty)
        }
        K::FreeReuse { token, body } => {
            CExpr::new(K::FreeReuse { token, body: Box::new(reuse_pass(*body, data, next)) }, ty)
        }
        K::Prim { op, args } => CExpr::new(
            K::Prim { op, args: args.into_iter().map(|a| reuse_pass(a, data, next)).collect() },
            ty,
        ),
        K::MakeData { tag, args, reuse, scalars, niche } => CExpr::new(
            K::MakeData {
                tag,
                args: args.into_iter().map(|a| reuse_pass(a, data, next)).collect(),
                reuse,
                scalars,
                niche,
            },
            ty,
        ),
        K::Spread { components } => CExpr::new(
            K::Spread {
                components: components.into_iter().map(|a| reuse_pass(a, data, next)).collect(),
            },
            ty,
        ),
        K::LetMany { locals, value, body } => CExpr::new(
            K::LetMany {
                locals,
                value: Box::new(reuse_pass(*value, data, next)),
                body: Box::new(reuse_pass(*body, data, next)),
            },
            ty,
        ),
        K::App { func, args, reuse, alloc } => CExpr::new(
            K::App {
                func: Box::new(reuse_pass(*func, data, next)),
                args: args.into_iter().map(|a| reuse_pass(a, data, next)).collect(),
                reuse,
                alloc,
            },
            ty,
        ),
        K::DataTag { base, niche } => {
            CExpr::new(K::DataTag { base: Box::new(reuse_pass(*base, data, next)), niche }, ty)
        }
        K::DataField { base, index, scalar, niche } => CExpr::new(
            K::DataField { base: Box::new(reuse_pass(*base, data, next)), index, scalar, niche },
            ty,
        ),
        // The tail-call transform consumes the output of reuse analysis, so its
        // nodes are never present here.
        K::Join { .. }
        | K::Recur { .. }
        | K::HoleStart { .. }
        | K::HoleFill { .. }
        | K::HoleClose { .. } => unreachable!("tail-call nodes are inserted after reuse analysis"),
        K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
            CExpr::new(kind, ty)
        }
    }
}

/// Places the release of the dead cell `s` into `expr`, recycling its memory for a
/// construction where possible. `s`'s memory is reset at the **death point** (the
/// start of `expr`, before any recursive call) when a construction is reachable
/// through `expr`'s straight-line bindings *and* `if` branches — so the cell's
/// fields become unique for a recursive call bound in a `let`, the case a
/// "recurse-then-rebalance" function (`insert`/`remove`) needs. The token is
/// threaded forward to the construction on each path; a branch that builds nothing
/// frees it (so every path still consumes the token exactly once). When no
/// construction is thread-reachable (e.g. one nested in a call argument), the
/// release is pushed into the branches, each of which drops on its own. A path
/// with no construction at all keeps a plain drop. `expr` never uses `s` (it is
/// already dead).
fn release(s: LocalId, expr: CExpr, next: &mut usize) -> CExpr {
    if !has_construction(&expr) {
        // Nothing to recycle into: drop early, as plain reference counting would.
        return drop_(s, expr);
    }
    if reaches_construction(&expr) {
        // A construction post-dominates along every threaded path (through lets and
        // `if` branches): reset now — freeing the cell's fields ahead of any
        // recursive call bound before the branch — and thread the token to each
        // branch's construction, freeing it on any branch that builds nothing.
        let token = fresh(next);
        return reset_(s, token, thread_or_free(expr, token));
    }
    // A construction exists but is not thread-reachable (e.g. nested in a call
    // argument): peel straight-line lets and push the release into each branch,
    // which decides reset-and-reuse or drop on its own.
    let CExpr { kind, ty } = expr;
    match kind {
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond,
                then: Box::new(release(s, *then, next)),
                els: Box::new(release(s, *els, next)),
            },
            ty,
        ),
        K::Let { local, value, body } => {
            CExpr::new(K::Let { local, value, body: Box::new(release(s, *body, next)) }, ty)
        }
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(release(s, *body, next)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(release(s, *body, next)) }, ty)
        }
        K::Reset { value, token, body } => {
            CExpr::new(K::Reset { value, token, body: Box::new(release(s, *body, next)) }, ty)
        }
        other => drop_(s, CExpr::new(other, ty)),
    }
}

/// Whether `e` contains a non-nullary construction with no reuse token yet.
fn has_construction(e: &CExpr) -> bool {
    match &e.kind {
        K::MakeData { args, reuse, niche, .. } => {
            (reuse.is_none() && !args.is_empty() && !niche_wrapper_free(*niche))
                || args.iter().any(has_construction)
        }
        K::Let { value, body, .. } => has_construction(value) || has_construction(body),
        K::Spread { components } => components.iter().any(has_construction),
        K::LetMany { value, body, .. } => has_construction(value) || has_construction(body),
        K::If { cond, then, els } => {
            has_construction(cond) || has_construction(then) || has_construction(els)
        }
        K::Reset { value, body, .. } => has_construction(value) || has_construction(body),
        K::FreeReuse { body, .. } => has_construction(body),
        K::Dup { body, .. } | K::Drop { body, .. } => has_construction(body),
        K::Prim { args, .. } => args.iter().any(has_construction),
        K::App { func, args, .. } => has_construction(func) || args.iter().any(has_construction),
        K::DataTag { base, .. } => has_construction(base),
        K::DataField { base, .. } => has_construction(base),
        // The tail-call transform runs after reuse analysis; handled defensively.
        K::Join { body, .. } | K::HoleStart { body, .. } => has_construction(body),
        K::Recur { args } => args.iter().any(has_construction),
        K::HoleFill { cell, .. } => has_construction(cell),
        K::HoleClose { base, .. } => has_construction(base),
        K::Local(_) | K::Global(_) | K::Lit(_) | K::MakeClosure { .. } | K::Error => false,
    }
}

/// Whether a non-nullary construction is reachable by threading a reuse token
/// through `e` — along its straight-line bindings (`let`/`dup`/`drop`/`reset`) and
/// into **both** arms of an `if`. The token's reuse target is the first such
/// construction on each path; a reuse-target `let` value counts (the token attaches
/// to it). A construction nested in a call argument is *not* threadable.
fn reaches_construction(e: &CExpr) -> bool {
    match &e.kind {
        K::MakeData { args, reuse, niche, .. } => {
            reuse.is_none() && !args.is_empty() && !niche_wrapper_free(*niche)
        }
        K::Let { value, body, .. } => is_reuse_target(value) || reaches_construction(body),
        K::If { then, els, .. } => reaches_construction(then) || reaches_construction(els),
        K::Dup { body, .. } | K::Drop { body, .. } | K::Reset { body, .. } => {
            reaches_construction(body)
        }
        _ => false,
    }
}

/// Threads `token` to the first construction on every path of `e` — attaching it
/// there (so the build recycles the reset cell) and recursing into both arms of an
/// `if` — and, on any path that reaches no construction, frees the token with a
/// [`K::FreeReuse`]. Every path therefore consumes the token exactly once (reuse or
/// free). Assumes [`reaches_construction`] held for at least one path.
fn thread_or_free(e: CExpr, token: LocalId) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::MakeData { tag, args, reuse: None, scalars, niche }
            if !args.is_empty() && !niche_wrapper_free(niche) =>
        {
            CExpr::new(K::MakeData { tag, args, reuse: Some(token), scalars, niche }, ty)
        }
        K::Let { local, value, body } => {
            if is_reuse_target(&value) {
                let value = Box::new(attach_reuse(*value, token));
                CExpr::new(K::Let { local, value, body }, ty)
            } else {
                let body = Box::new(thread_or_free(*body, token));
                CExpr::new(K::Let { local, value, body }, ty)
            }
        }
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond,
                then: Box::new(thread_or_free(*then, token)),
                els: Box::new(thread_or_free(*els, token)),
            },
            ty,
        ),
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(thread_or_free(*body, token)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(thread_or_free(*body, token)) }, ty)
        }
        K::Reset { value, token: tok, body } => CExpr::new(
            K::Reset { value, token: tok, body: Box::new(thread_or_free(*body, token)) },
            ty,
        ),
        // A leaf with no construction (a call, projection, or atom): free the token.
        other => free_reuse_(token, CExpr::new(other, ty)),
    }
}

/// Whether a `MakeData`'s niche scheme makes it **wrapper-free** — a niche `Some`
/// (either scheme) is its payload itself, allocating no cell — so it is neither a
/// heap construction nor a reuse target (inter-procedural forwarding excludes one
/// too).
pub(crate) fn niche_wrapper_free(niche: Option<NicheKind>) -> bool {
    niche.is_some()
}

/// Whether `e` is a non-nullary, non-niche construction with no reuse token yet.
pub(crate) fn is_reuse_target(e: &CExpr) -> bool {
    matches!(&e.kind, K::MakeData { args, reuse: None, niche, .. }
        if !args.is_empty() && !niche_wrapper_free(*niche))
}

/// Attaches a reuse `token` to a construction (assumes [`is_reuse_target`]).
pub(crate) fn attach_reuse(e: CExpr, token: LocalId) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::MakeData { tag, args, reuse: None, scalars, niche } => {
            CExpr::new(K::MakeData { tag, args, reuse: Some(token), scalars, niche }, ty)
        }
        _ => unreachable!("attach_reuse on a non-target construction"),
    }
}

/// `reset s = Local(s); body` (binding the reuse `token`).
pub(crate) fn reset_(s: LocalId, token: LocalId, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    let value = Box::new(CExpr::new(K::Local(s), Ty::Error));
    CExpr::new(K::Reset { value, token, body: Box::new(body) }, ty)
}

/// `free-reuse token; body` (releasing a token no construction consumes).
pub(crate) fn free_reuse_(token: LocalId, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::FreeReuse { token, body: Box::new(body) }, ty)
}

/// The owned locals a projection borrows: its base, plus row-polymorphic offset
/// evidence. Empty for a non-projection or a non-local base.
fn projection_borrows(e: &CExpr) -> Vec<LocalId> {
    let mut out = Vec::new();
    match &e.kind {
        K::DataTag { base, .. } => {
            if let K::Local(s) = base.kind {
                out.push(s);
            }
        }
        K::DataField { base, index, .. } => {
            if let K::Local(s) = base.kind {
                out.push(s);
            }
            if let FieldIndex::Dyn { evidence, .. } = index {
                out.push(*evidence);
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod cases;
#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;
