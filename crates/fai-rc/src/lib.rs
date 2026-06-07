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
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;
use fai_types::{Con, Ty};
use rustc_hash::FxHashSet;

pub use borrow::{BorrowSig, borrow_signature};
pub use verify::check_rc;

mod borrow;
mod verify;

/// A set of locals (used for free-variable and liveness sets).
type Locals = FxHashSet<LocalId>;

/// Inserts reference-count operations into `name`'s lowered definition.
#[salsa::tracked]
pub fn rc(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let lowered = core(db, file, name);
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
    let mut next = next_free_local(lowered);

    // Per-call argument borrowing: a saturated direct call to a known top-level
    // function borrows the parameters that function's signature marks borrowed.
    let arg_borrows = |def: DefId, nargs: usize| -> Vec<bool> {
        let Some(cf) = db.source_file(def.file) else { return vec![false; nargs] };
        let sig = borrow_signature(db, cf, def.name);
        if sig.exploitable_at(nargs) { sig.0.clone() } else { vec![false; nargs] }
    };

    let mut fns = Vec::with_capacity(lowered.fns.len());
    for (i, f) in lowered.fns.iter().enumerate() {
        let mut borrowed: Locals = f.captures.iter().copied().collect();
        if i == 0 {
            for (p, &param) in f.params.iter().enumerate() {
                if self_sig.is_borrowed(p) {
                    borrowed.insert(param);
                }
            }
        }
        let body = anf(f.body.clone(), &mut next);
        let used = fv_owned(&body, &borrowed);
        let mut cx = Rc { captures: &borrowed, next, call_borrows: &arg_borrows };
        let body = cx.owned(body, &Locals::default());
        next = cx.next;
        // Recycle a dead data cell into a same-size construction where one follows.
        let data = data_typed_locals(&body);
        let mut body = reuse_pass(body, &data, &mut next);
        // Drop parameters that the body never mentions (drop-early, at entry) —
        // but never a borrowed parameter (the caller owns and releases it).
        for &p in f.params.iter().rev() {
            if !used.contains(&p) && !borrowed.contains(&p) {
                body = drop_(p, body);
            }
        }
        fns.push(CoreFn { params: f.params.clone(), captures: f.captures.clone(), body });
    }
    LoweredDef { def: lowered.def, fns, entry_borrowed: self_sig.0.clone() }
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
        K::Reset { value, token, body } => {
            bump(*token, max);
            max_local(value, max);
            max_local(body, max);
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
        K::MakeData { tag, args, reuse } => {
            let args = args.into_iter().map(|a| atomize(a, binds, next)).collect();
            CExpr::new(K::MakeData { tag, args, reuse }, ty)
        }
        K::App { func, args } => {
            let func = Box::new(atomize(*func, binds, next));
            let args = args.into_iter().map(|a| atomize(a, binds, next)).collect();
            CExpr::new(K::App { func, args }, ty)
        }
        K::DataTag(base) => CExpr::new(K::DataTag(Box::new(to_local(*base, binds, next))), ty),
        K::DataField { base, index } => {
            CExpr::new(K::DataField { base: Box::new(to_local(*base, binds, next)), index }, ty)
        }
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
        K::MakeClosure { func, captures } => CExpr::new(K::MakeClosure { func, captures }, ty),
        // Not produced by lowering; handled defensively for exhaustiveness.
        K::Reset { value, token, body } => CExpr::new(
            K::Reset {
                value: Box::new(anf(*value, next)),
                token,
                body: Box::new(anf(*body, next)),
            },
            ty,
        ),
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(anf(*body, next)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(anf(*body, next)) }, ty)
        }
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
        K::Reset { value, token, body } => {
            collect_fv(value, captures, bound, out);
            let added = bound.insert(*token);
            collect_fv(body, captures, bound, out);
            if added {
                bound.remove(token);
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
            K::MakeData { tag, args, reuse } => {
                // Constructor fields are stored (consumed); none are borrowed.
                let borrows = vec![false; args.len()];
                let rebuilt = move |args| CExpr::new(K::MakeData { tag, args, reuse }, ty.clone());
                self.operands_rc(args, &borrows, live, rebuilt)
            }
            K::App { func, args } => {
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
                let rebuilt = |mut ops: Vec<CExpr>| {
                    let func = Box::new(ops.remove(0));
                    CExpr::new(K::App { func, args: ops }, ty.clone())
                };
                self.operands_rc(operands, &borrows, live, rebuilt)
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
            // The reuse pass runs after this; pass through for exhaustiveness.
            K::Reset { value, token, body } => {
                let body = self.owned(*body, live);
                CExpr::new(K::Reset { value, token, body: Box::new(body) }, ty)
            }
            // Lowering never emits these; pass through.
            K::Dup { local, body } => dup_(local, self.owned(*body, live)),
            K::Drop { local, body } => drop_(local, self.owned(*body, live)),
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

/// Per-operand borrow flags for a primitive. Currently every primitive consumes
/// its operands; inspect-only primitives that borrow are added later.
fn prim_borrows(_op: fai_core::ir::Prim, args: &[CExpr]) -> Vec<bool> {
    vec![false; args.len()]
}

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

// ---------------------------------------------------------------------------
// Reuse analysis: recycle a dead data cell into a same-size construction.
// ---------------------------------------------------------------------------

/// Locals that name boxed data cells reuse may recycle: any local used as a
/// projection base (`DataField`/`DataTag`) is necessarily a data value (a match
/// scrutinee or a record being read/updated), and any local bound to a value of a
/// boxed data type also qualifies (e.g. a freshly constructed record).
fn data_typed_locals(e: &CExpr) -> Locals {
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
        K::Prim { args, .. } | K::MakeData { args, .. } => {
            args.iter().for_each(|a| collect_data_locals(a, out));
        }
        K::App { func, args } => {
            collect_data_locals(func, out);
            args.iter().for_each(|a| collect_data_locals(a, out));
        }
        K::DataTag(base) => {
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
        K::Dup { body, .. } | K::Drop { body, .. } => collect_data_locals(body, out),
        K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

/// Whether values of `ty` are boxed data cells (so resetting one yields a usable
/// reuse token). Records, tuples, ADTs, lists, and interface dictionaries qualify;
/// scalars, strings, floats, functions, and type variables do not.
fn is_boxed_data_ty(ty: &Ty) -> bool {
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
        K::Prim { op, args } => CExpr::new(
            K::Prim { op, args: args.into_iter().map(|a| reuse_pass(a, data, next)).collect() },
            ty,
        ),
        K::MakeData { tag, args, reuse } => CExpr::new(
            K::MakeData {
                tag,
                args: args.into_iter().map(|a| reuse_pass(a, data, next)).collect(),
                reuse,
            },
            ty,
        ),
        K::App { func, args } => CExpr::new(
            K::App {
                func: Box::new(reuse_pass(*func, data, next)),
                args: args.into_iter().map(|a| reuse_pass(a, data, next)).collect(),
            },
            ty,
        ),
        K::DataTag(base) => CExpr::new(K::DataTag(Box::new(reuse_pass(*base, data, next))), ty),
        K::DataField { base, index } => {
            CExpr::new(K::DataField { base: Box::new(reuse_pass(*base, data, next)), index }, ty)
        }
        K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
            CExpr::new(kind, ty)
        }
    }
}

/// Places the release of the dead cell `s` into `expr`, recycling its memory for a
/// construction where possible. `s`'s memory is reset at the **death point** (the
/// start of `expr`, before any recursive call) when `expr` reaches a construction
/// on a single straight-line path — so the cell's fields become unique for that
/// call — with the token threaded forward to that construction. When a branch
/// intervenes, the responsibility is pushed into the branches (each resets and
/// reuses, or drops, on its own). A path with no construction keeps a plain drop.
/// `expr` never uses `s` (it is already dead).
fn release(s: LocalId, expr: CExpr, next: &mut usize) -> CExpr {
    if !has_construction(&expr) {
        // Nothing to recycle into: drop early, as plain reference counting would.
        return drop_(s, expr);
    }
    if linear_construction(&expr) {
        // A construction post-dominates on one path: reset now (freeing the cell's
        // fields for any recursive call) and thread the token to it.
        let token = fresh(next);
        return reset_(s, token, thread_token(expr, token));
    }
    // A branch precedes the construction: peel straight-line lets and push the
    // release into each branch, which decides reset-and-reuse or drop on its own.
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
        K::MakeData { args, reuse, .. } => {
            (reuse.is_none() && !args.is_empty()) || args.iter().any(has_construction)
        }
        K::Let { value, body, .. } => has_construction(value) || has_construction(body),
        K::If { cond, then, els } => {
            has_construction(cond) || has_construction(then) || has_construction(els)
        }
        K::Reset { value, body, .. } => has_construction(value) || has_construction(body),
        K::Dup { body, .. } | K::Drop { body, .. } => has_construction(body),
        K::Prim { args, .. } => args.iter().any(has_construction),
        K::App { func, args } => has_construction(func) || args.iter().any(has_construction),
        K::DataTag(base) => has_construction(base),
        K::DataField { base, .. } => has_construction(base),
        K::Local(_) | K::Global(_) | K::Lit(_) | K::MakeClosure { .. } | K::Error => false,
    }
}

/// Whether a non-nullary construction is reached on a single straight-line path
/// (through `let`/`dup`/`drop`/`reset`), with no `if` before it.
fn linear_construction(e: &CExpr) -> bool {
    match &e.kind {
        K::MakeData { args, reuse, .. } => reuse.is_none() && !args.is_empty(),
        K::Let { value, body, .. } => is_reuse_target(value) || linear_construction(body),
        K::Dup { body, .. } | K::Drop { body, .. } | K::Reset { body, .. } => {
            linear_construction(body)
        }
        _ => false,
    }
}

/// Attaches `token` to the first construction on the straight-line path (assumes
/// [`linear_construction`]).
fn thread_token(e: CExpr, token: LocalId) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::MakeData { tag, args, reuse: None } if !args.is_empty() => {
            CExpr::new(K::MakeData { tag, args, reuse: Some(token) }, ty)
        }
        K::Let { local, value, body } => {
            if is_reuse_target(&value) {
                let value = Box::new(attach_reuse(*value, token));
                CExpr::new(K::Let { local, value, body }, ty)
            } else {
                let body = Box::new(thread_token(*body, token));
                CExpr::new(K::Let { local, value, body }, ty)
            }
        }
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(thread_token(*body, token)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(thread_token(*body, token)) }, ty)
        }
        K::Reset { value, token: tok, body } => CExpr::new(
            K::Reset { value, token: tok, body: Box::new(thread_token(*body, token)) },
            ty,
        ),
        other => CExpr::new(other, ty),
    }
}

/// Whether `e` is a non-nullary construction with no reuse token yet.
fn is_reuse_target(e: &CExpr) -> bool {
    matches!(&e.kind, K::MakeData { args, reuse: None, .. } if !args.is_empty())
}

/// Attaches a reuse `token` to a construction (assumes [`is_reuse_target`]).
fn attach_reuse(e: CExpr, token: LocalId) -> CExpr {
    let CExpr { kind, ty } = e;
    match kind {
        K::MakeData { tag, args, reuse: None } => {
            CExpr::new(K::MakeData { tag, args, reuse: Some(token) }, ty)
        }
        _ => unreachable!("attach_reuse on a non-target construction"),
    }
}

/// `reset s = Local(s); body` (binding the reuse `token`).
fn reset_(s: LocalId, token: LocalId, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    let value = Box::new(CExpr::new(K::Local(s), Ty::Error));
    CExpr::new(K::Reset { value, token, body: Box::new(body) }, ty)
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
mod cases;
#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;
