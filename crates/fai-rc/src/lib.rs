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

use fai_core::core;
use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, LoweredDef};
use fai_db::{Db, SourceFile};
use fai_resolve::LocalId;
use fai_syntax::Symbol;
use rustc_hash::FxHashSet;

/// A set of locals (used for free-variable and liveness sets).
type Locals = FxHashSet<LocalId>;

/// Inserts reference-count operations into `name`'s lowered definition.
#[salsa::tracked]
pub fn rc(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let lowered = core(db, file, name);
    let mut next = next_free_local(&lowered);
    let mut fns = Vec::with_capacity(lowered.fns.len());
    for f in &lowered.fns {
        let captures: Locals = f.captures.iter().copied().collect();
        let body = anf(f.body.clone(), &mut next);
        let used = fv_owned(&body, &captures);
        let mut cx = Rc { captures: &captures, next };
        let mut body = cx.owned(body, &Locals::default());
        next = cx.next;
        // Drop parameters that the body never mentions (drop-early, at entry).
        for &p in f.params.iter().rev() {
            if !used.contains(&p) {
                body = drop_(p, body);
            }
        }
        fns.push(CoreFn { params: f.params.clone(), captures: f.captures.clone(), body });
    }
    Arc::new(LoweredDef { def: lowered.def, fns })
}

// ---------------------------------------------------------------------------
// Fresh locals.
// ---------------------------------------------------------------------------

/// The first local slot not used anywhere in `lowered` (so synthesized binders —
/// A-normal-form temporaries and projection results — never collide).
fn next_free_local(lowered: &LoweredDef) -> usize {
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
        K::DataField { base, index } => {
            max_local(base, max);
            if let FieldIndex::Dyn { evidence, .. } = index {
                bump(*evidence, max);
            }
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            bump(*local, max);
            max_local(body, max);
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

fn fresh(next: &mut usize) -> LocalId {
    let id = LocalId::from_index(*next);
    *next += 1;
    id
}

/// Normalizes `e` so every operand of an operation is an atom. Compound operands
/// are bound to fresh `let`s in evaluation order; flat expressions are unchanged.
fn anf(e: CExpr, next: &mut usize) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::Error => CExpr::new(kind, ty),
        K::Prim { op, args } => {
            let mut binds = Vec::new();
            let args = args.into_iter().map(|a| atomize(a, next, &mut binds)).collect();
            wrap_binds(binds, CExpr::new(K::Prim { op, args }, ty))
        }
        K::MakeData { tag, args } => {
            let mut binds = Vec::new();
            let args = args.into_iter().map(|a| atomize(a, next, &mut binds)).collect();
            wrap_binds(binds, CExpr::new(K::MakeData { tag, args }, ty))
        }
        K::App { func, args } => {
            let mut binds = Vec::new();
            let func = Box::new(atomize(*func, next, &mut binds));
            let args = args.into_iter().map(|a| atomize(a, next, &mut binds)).collect();
            wrap_binds(binds, CExpr::new(K::App { func, args }, ty))
        }
        K::DataTag(base) => {
            let mut binds = Vec::new();
            let base = Box::new(to_local(*base, next, &mut binds));
            wrap_binds(binds, CExpr::new(K::DataTag(base), ty))
        }
        K::DataField { base, index } => {
            let mut binds = Vec::new();
            let base = Box::new(to_local(*base, next, &mut binds));
            wrap_binds(binds, CExpr::new(K::DataField { base, index }, ty))
        }
        K::If { cond, then, els } => {
            let mut binds = Vec::new();
            let cond = Box::new(atomize(*cond, next, &mut binds));
            let then = Box::new(anf(*then, next));
            let els = Box::new(anf(*els, next));
            wrap_binds(binds, CExpr::new(K::If { cond, then, els }, ty))
        }
        K::Let { local, value, body } => {
            let value = Box::new(anf(*value, next));
            let body = Box::new(anf(*body, next));
            CExpr::new(K::Let { local, value, body }, ty)
        }
        // Captures are locals already; no compound operands.
        K::MakeClosure { func, captures } => CExpr::new(K::MakeClosure { func, captures }, ty),
        // Not produced by lowering; pass through defensively.
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(anf(*body, next)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(anf(*body, next)) }, ty)
        }
    }
}

/// Normalizes `e` and, if it is compound, binds it to a fresh local, returning
/// the bound atom; pushes any bindings (including the new one) into `binds`.
fn atomize(e: CExpr, next: &mut usize, binds: &mut Vec<(LocalId, CExpr)>) -> CExpr {
    if is_atom(&e) {
        return e;
    }
    let e = anf(e, next);
    let ty = e.ty.clone();
    let local = fresh(next);
    binds.push((local, e));
    CExpr::new(K::Local(local), ty)
}

/// Like [`atomize`], but always yields a *local* (binding even a global or
/// literal). A projection borrows its base, so the base must be an owned local
/// that reference counting can release — in particular a global naming a forced
/// zero-arity value, which allocates when read.
fn to_local(e: CExpr, next: &mut usize, binds: &mut Vec<(LocalId, CExpr)>) -> CExpr {
    if matches!(e.kind, K::Local(_)) {
        return e;
    }
    let e = anf(e, next);
    let ty = e.ty.clone();
    let local = fresh(next);
    binds.push((local, e));
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
        K::Prim { args, .. } | K::MakeData { args, .. } => {
            args.iter().for_each(|a| collect_fv(a, captures, bound, out));
        }
        K::App { func, args } => {
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
        K::MakeClosure { captures: caps, .. } => {
            caps.iter().for_each(|c| note(*c, bound, out));
        }
        K::DataTag(base) => collect_fv(base, captures, bound, out),
        K::DataField { base, index } => {
            collect_fv(base, captures, bound, out);
            if let FieldIndex::Dyn { evidence, .. } = index {
                note(*evidence, bound, out);
            }
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            note(*local, bound, out);
            collect_fv(body, captures, bound, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Precise reference counting.
// ---------------------------------------------------------------------------

/// Per-function reference-counting state.
struct Rc<'a> {
    /// The function's captured slots (borrowed: dup on use, never dropped).
    captures: &'a Locals,
    /// The next free local slot (for projection-result temporaries).
    next: usize,
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
                let rebuilt = |args| CExpr::new(K::Prim { op, args }, ty.clone());
                self.consume_operands(args, live, rebuilt)
            }
            K::MakeData { tag, args } => {
                let rebuilt = |args| CExpr::new(K::MakeData { tag, args }, ty.clone());
                self.consume_operands(args, live, rebuilt)
            }
            K::App { func, args } => {
                let mut operands = Vec::with_capacity(args.len() + 1);
                operands.push(*func);
                operands.extend(args);
                let rebuilt = |mut ops: Vec<CExpr>| {
                    let func = Box::new(ops.remove(0));
                    CExpr::new(K::App { func, args: ops }, ty.clone())
                };
                self.consume_operands(operands, live, rebuilt)
            }
            K::MakeClosure { func, captures } => {
                let inner = CExpr::new(K::MakeClosure { func, captures: captures.clone() }, ty);
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
            K::DataField { base, index } => {
                let proj = CExpr::new(K::DataField { base, index }, ty);
                let borrows = projection_borrows(&proj);
                self.borrow_tail(proj, borrows, live)
            }
            K::DataTag(base) => {
                let proj = CExpr::new(K::DataTag(base), ty);
                let borrows = projection_borrows(&proj);
                self.borrow_tail(proj, borrows, live)
            }
            K::If { cond, then, els } => self.conditional(*cond, *then, *els, ty, live),
            K::Let { local, value, body } => self.binding(local, *value, *body, ty, live),
            // Lowering never emits these; pass through.
            K::Dup { local, body } => dup_(local, self.owned(*body, live)),
            K::Drop { local, body } => drop_(local, self.owned(*body, live)),
        }
    }

    /// Transforms an operation whose operand atoms are all consumed, inserting a
    /// `Dup` before the operation for each owned operand that is still needed
    /// afterward (live, or consumed again at a later operand).
    fn consume_operands(
        &self,
        operands: Vec<CExpr>,
        live: &Locals,
        rebuild: impl FnOnce(Vec<CExpr>) -> CExpr,
    ) -> CExpr {
        let mut dups = Vec::new();
        for (i, a) in operands.iter().enumerate() {
            if let K::Local(x) = a.kind {
                let later =
                    operands[i + 1..].iter().any(|b| matches!(b.kind, K::Local(y) if y == x));
                if self.is_capture(x) || live.contains(&x) || later {
                    dups.push(x);
                }
            }
        }
        let mut e = rebuild(operands);
        for x in dups.into_iter().rev() {
            e = dup_(x, e);
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

fn dup_(local: LocalId, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Dup { local, body: Box::new(body) }, ty)
}

fn drop_(local: LocalId, body: CExpr) -> CExpr {
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
    matches!(e.kind, K::DataField { .. } | K::DataTag(_))
}

/// The owned locals a projection borrows: its base, plus row-polymorphic offset
/// evidence. Empty for a non-projection or a non-local base.
fn projection_borrows(e: &CExpr) -> Vec<LocalId> {
    let mut out = Vec::new();
    match &e.kind {
        K::DataTag(base) => {
            if let K::Local(s) = base.kind {
                out.push(s);
            }
        }
        K::DataField { base, index } => {
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
mod tests;
