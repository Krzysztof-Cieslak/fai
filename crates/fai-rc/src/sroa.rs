//! Scalar replacement of fixed-shape float aggregates (SROA).
//!
//! A **fixed-shape float aggregate** (FFA) — a tuple of all-`Float`, or a closed
//! record of all-`Float`, up to [`fai_core::ir::FFA_MAX_FIELDS`] fields (see
//! [`fai_core::ir::ffa_arity`]) — is held as its scalar `f64` components rather
//! than a heap cell, and crosses a direct call boundary in registers: an FFA
//! parameter occupies N consecutive `f64` registers and an FFA result is returned
//! via a Cranelift multi-result signature ([`fai_core::ir::Repr::Spread`]).
//!
//! This pass runs **after A-normal form and before reference counting**, so every
//! value is a named local and an FFA value's identity is one local. Per function
//! it rewrites:
//!
//! * a **spread parameter** into N component locals (recorded in
//!   [`fai_core::ir::LoweredDef::entry_spread_params`], which code generation binds
//!   to the incoming registers); the aggregate parameter slot is a dead anchor;
//! * a **construction** of an FFA (`MakeData`) into its component atoms (no cell);
//! * a **projection** of a tracked FFA into the component;
//! * a **spread-returning call** into a [`fai_core::ir::ExprKind::LetMany`] binding
//!   its result components;
//! * the **tail** of a spread-result function into a
//!   [`fai_core::ir::ExprKind::Spread`] (a multi-value return);
//! * a **spread call argument** into a [`fai_core::ir::ExprKind::Spread`].
//!
//! At a **boxed boundary** (a field of a non-FFA cell, a closure capture, a
//! uniform/`apply_n`/generic argument, a uniform-ABI return, a structural-op
//! operand) a tracked FFA is **reassembled** into the in-cell scalar-slot layout,
//! at most once per straight-line scope (cache-one). An FFA arriving **boxed** (a
//! generic call result, a CAF, a boxed parameter/capture, a field of a larger
//! cell) is left boxed and **exploded** with field loads only where a spread
//! boundary needs its components.

use std::sync::Arc;

use fai_core::abi_of;
use fai_core::ir::{CExpr, ExprKind as K, FieldIndex, FnAbi, ffa_arity};
use fai_db::Db;
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

/// One emitted binding: an ordinary `let`, or a `LetMany` destructuring a
/// spread-returning call's components.
enum Bind {
    Let(LocalId, CExpr),
    Many(Vec<LocalId>, CExpr),
}

/// Rewrites one function's A-normal-form `body` for SROA, given its calling
/// convention `abi` (the entry's real ABI, or the uniform default for a lifted
/// lambda) and `params`. Returns the rewritten body and, per parameter, the
/// component locals of a spread parameter (`None` for an ordinary parameter; the
/// whole vector is empty when there are no spread parameters). `next` supplies
/// fresh local slots.
pub fn sroa_fn(
    db: &dyn Db,
    body: CExpr,
    abi: &FnAbi,
    params: &[LocalId],
    next: &mut usize,
) -> (CExpr, Vec<Option<Vec<LocalId>>>) {
    let mut cx = Sroa { db, comps: FxHashMap::default(), boxed: FxHashMap::default(), next };
    let mut spread_params: Vec<Option<Vec<LocalId>>> = vec![None; params.len()];
    for (p, &param) in params.iter().enumerate() {
        if let Some(reprs) = abi.spread_param(p) {
            let locals: Vec<LocalId> = (0..reprs.len()).map(|_| cx.fresh()).collect();
            cx.comps.insert(param, locals.iter().map(|&l| local_f64(l)).collect());
            spread_params[p] = Some(locals);
        }
    }
    let want = abi.spread_return().map(<[_]>::len);
    let body = cx.rewrite(body, want);
    let spread_params =
        if spread_params.iter().any(Option::is_some) { spread_params } else { Vec::new() };
    (body, spread_params)
}

struct Sroa<'a> {
    db: &'a dyn Db,
    /// Decomposed FFA locals (no boxed cell exists) → their component atoms.
    comps: FxHashMap<LocalId, Vec<CExpr>>,
    /// Cache-one materialization within the current straight-line scope: a
    /// decomposed FFA local → the boxed local it was reassembled into.
    boxed: FxHashMap<LocalId, LocalId>,
    next: &'a mut usize,
}

impl Sroa<'_> {
    fn fresh(&mut self) -> LocalId {
        let id = LocalId::from_index(*self.next);
        *self.next += 1;
        id
    }

    /// Rewrites `e`. `want` is `Some(n)` when `e` is in tail position of a
    /// spread-result function with `n` components (so a produced FFA becomes a
    /// multi-value `Spread`).
    fn rewrite(&mut self, e: CExpr, want: Option<usize>) -> CExpr {
        let CExpr { kind, ty } = e;
        match kind {
            K::Let { local, value, body } => {
                let mut binds = Vec::new();
                let value = self.rewrite_op(*value, &mut binds);
                self.classify(local, value, &mut binds);
                let body = self.rewrite(*body, want);
                wrap(binds, body)
            }
            // A pre-existing `LetMany` would only arise from a re-run; pass through.
            K::LetMany { locals, value, body } => {
                let value = Box::new(self.rewrite(*value, None));
                let body = Box::new(self.rewrite(*body, want));
                CExpr::new(K::LetMany { locals, value, body }, ty)
            }
            K::If { cond, then, els } => {
                let mut binds = Vec::new();
                let cond = Box::new(self.rewrite_atom(*cond, &mut binds));
                let then = Box::new(self.scoped(*then, want));
                let els = Box::new(self.scoped(*els, want));
                wrap(binds, CExpr::new(K::If { cond, then, els }, ty))
            }
            other => {
                let e = CExpr::new(other, ty);
                let mut binds = Vec::new();
                let t = self.rewrite_tail(e, want, &mut binds);
                wrap(binds, t)
            }
        }
    }

    /// Rewrites a branch (or other nested scope) with its own materialization cache
    /// (a branch's reassemblies stay confined to it; never allocate on a path that
    /// does not box).
    fn scoped(&mut self, e: CExpr, want: Option<usize>) -> CExpr {
        let saved = std::mem::take(&mut self.boxed);
        let out = self.rewrite(e, want);
        self.boxed = saved;
        out
    }

    /// Records a binding `let local = value` (already operand-rewritten): an FFA
    /// construction is decomposed (the cell elided), a spread-returning call binds
    /// its components, an alias of a decomposed FFA inherits its components, and
    /// anything else keeps its `let`.
    fn classify(&mut self, local: LocalId, value: CExpr, binds: &mut Vec<Bind>) {
        // An FFA construction: track its component atoms, drop the cell.
        if let K::MakeData { args, reuse: None, .. } = &value.kind
            && ffa_arity(&value.ty).is_some()
        {
            let components = args.clone();
            self.comps.insert(local, components);
            return;
        }
        // A saturated spread-returning call: bind its result components.
        if let Some(n) = self.spread_call_arity(&value) {
            let locals: Vec<LocalId> = (0..n).map(|_| self.fresh()).collect();
            self.comps.insert(local, locals.iter().map(|&l| local_f64(l)).collect());
            binds.push(Bind::Many(locals.clone(), value));
            return;
        }
        // An alias of a decomposed FFA: share its components.
        if let K::Local(v) = &value.kind
            && let Some(cs) = self.comps.get(v)
        {
            let cs = cs.clone();
            self.comps.insert(local, cs);
            return;
        }
        binds.push(Bind::Let(local, value));
    }

    /// Rewrites a value-position operation: resolves projections of tracked FFAs to
    /// components, marshals call arguments, and boxes tracked-FFA operands that
    /// cross a uniform slot. Does not itself decide FFA-producer status.
    fn rewrite_op(&mut self, e: CExpr, binds: &mut Vec<Bind>) -> CExpr {
        let CExpr { kind, ty } = e;
        match kind {
            K::DataField { base, index, scalar, niche } => {
                if let (K::Local(v), FieldIndex::Const(i)) = (&base.kind, index)
                    && let Some(cs) = self.comps.get(v)
                {
                    return cs[i as usize].clone();
                }
                let base = Box::new(self.rewrite_atom(*base, binds));
                CExpr::new(K::DataField { base, index, scalar, niche }, ty)
            }
            K::App { func, args, reuse, alloc } => {
                let callee = self.callee_abi(&func);
                let new_args = args
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| match callee.as_ref().and_then(|abi| abi.spread_param(i)) {
                        Some(reprs) => {
                            let n = reprs.len();
                            self.spread_arg(a, n, binds)
                        }
                        None => self.rewrite_atom(a, binds),
                    })
                    .collect();
                CExpr::new(K::App { func, args: new_args, reuse, alloc }, ty)
            }
            K::MakeData { tag, args, reuse, scalars, niche } => {
                let args = args.into_iter().map(|a| self.rewrite_atom(a, binds)).collect();
                CExpr::new(K::MakeData { tag, args, reuse, scalars, niche }, ty)
            }
            K::Prim { op, args } => {
                let args = args.into_iter().map(|a| self.rewrite_atom(a, binds)).collect();
                CExpr::new(K::Prim { op, args }, ty)
            }
            K::Foreign { symbol, args } => {
                let args = args.into_iter().map(|a| self.rewrite_atom(a, binds)).collect();
                CExpr::new(K::Foreign { symbol, args }, ty)
            }
            K::DataTag { base, niche } => {
                let base = Box::new(self.rewrite_atom(*base, binds));
                CExpr::new(K::DataTag { base, niche }, ty)
            }
            K::MakeClosure { func, captures, alloc } => {
                // A captured slot crosses into the environment (a uniform slot); a
                // tracked-FFA capture must be reassembled. Captures are locals, so
                // box each tracked one in place (recording the substitution).
                let captures = captures
                    .into_iter()
                    .map(|c| match self.comps.contains_key(&c) {
                        true => self.materialize(c, binds),
                        false => c,
                    })
                    .collect();
                CExpr::new(K::MakeClosure { func, captures, alloc }, ty)
            }
            other => CExpr::new(other, ty),
        }
    }

    /// Rewrites an atom flowing into a boxed (uniform) position: a tracked FFA local
    /// is reassembled (cache-one); anything else is unchanged.
    fn rewrite_atom(&mut self, a: CExpr, binds: &mut Vec<Bind>) -> CExpr {
        if let K::Local(v) = &a.kind
            && self.comps.contains_key(v)
        {
            let b = self.materialize(*v, binds);
            return CExpr::new(K::Local(b), a.ty);
        }
        a
    }

    /// Produces a spread argument: the N component atoms of `a` as a `Spread`.
    fn spread_arg(&mut self, a: CExpr, n: usize, binds: &mut Vec<Bind>) -> CExpr {
        let ty = a.ty.clone();
        let components = self.as_components(a, n, binds);
        CExpr::new(K::Spread { components }, ty)
    }

    /// The tail of a function: a multi-value `Spread` when the result is spread,
    /// else the operand-rewritten value.
    fn rewrite_tail(&mut self, e: CExpr, want: Option<usize>, binds: &mut Vec<Bind>) -> CExpr {
        match want {
            Some(n) => {
                let ty = e.ty.clone();
                let components = self.as_components(e, n, binds);
                CExpr::new(K::Spread { components }, ty)
            }
            // A boxed (non-spread) result: an aggregate wider than the target's
            // return-register budget is returned as a cell. A decomposed FFA in
            // tail position has no register home, so reassemble it into its cell;
            // any other form (a direct construction, a boxed-returning call) lowers
            // to a boxed value through the ordinary operation rewrite.
            None => {
                if let K::Local(v) = &e.kind
                    && self.comps.contains_key(v)
                {
                    let b = self.materialize(*v, binds);
                    return CExpr::new(K::Local(b), e.ty);
                }
                self.rewrite_op(e, binds)
            }
        }
    }

    /// The N component atoms of an FFA value, decomposing or exploding as needed
    /// (binding into `binds`).
    fn as_components(&mut self, e: CExpr, n: usize, binds: &mut Vec<Bind>) -> Vec<CExpr> {
        // A construction: its arguments are the components.
        if let K::MakeData { args, reuse: None, .. } = &e.kind
            && ffa_arity(&e.ty).is_some()
        {
            return args.clone();
        }
        // A decomposed FFA local: its tracked components.
        if let K::Local(v) = &e.kind
            && let Some(cs) = self.comps.get(v)
        {
            return cs.clone();
        }
        // A saturated spread-returning call: marshal its arguments (a spread
        // argument becomes a `Spread` of components — without this its operands stay
        // raw), then bind its result components via a `LetMany`.
        if self.spread_call_arity(&e).is_some() {
            let call = self.rewrite_op(e, binds);
            let m = self.spread_call_arity(&call).unwrap_or(n);
            debug_assert_eq!(m, n);
            let locals: Vec<LocalId> = (0..n).map(|_| self.fresh()).collect();
            binds.push(Bind::Many(locals.clone(), call));
            return locals.iter().map(|&l| local_f64(l)).collect();
        }
        // A boxed FFA value (a boxed local/param/capture, a CAF, a generic call
        // result, a field of a larger cell): explode it with field loads. The base
        // keeps its **real** type (so reference counting drops it through the
        // correct, descriptor-aware path — not a fabricated tuple shape).
        let base_ty = e.ty.clone();
        let base = self.bind(e, binds);
        (0..n)
            .map(|i| {
                let c = self.fresh();
                let proj = CExpr::new(
                    K::DataField {
                        base: Box::new(CExpr::new(K::Local(base), base_ty.clone())),
                        index: FieldIndex::Const(u32::try_from(i).unwrap_or(0)),
                        scalar: true,
                        niche: None,
                    },
                    float_ty(),
                );
                binds.push(Bind::Let(c, proj));
                local_f64(c)
            })
            .collect()
    }

    /// Reassembles decomposed FFA local `v` into a boxed scalar-slot cell, at most
    /// once per scope, returning the boxed local.
    fn materialize(&mut self, v: LocalId, binds: &mut Vec<Bind>) -> LocalId {
        if let Some(&b) = self.boxed.get(&v) {
            return b;
        }
        let components = self.comps[&v].clone();
        let n = components.len();
        let scalars = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        let cell = CExpr::new(
            K::MakeData { tag: 0, args: components, reuse: None, scalars, niche: None },
            ffa_tuple_ty(n),
        );
        let b = self.fresh();
        binds.push(Bind::Let(b, cell));
        self.boxed.insert(v, b);
        b
    }

    /// Binds `e` to a fresh local unless it is already one, returning the local.
    fn bind(&mut self, e: CExpr, binds: &mut Vec<Bind>) -> LocalId {
        if let K::Local(l) = &e.kind {
            return *l;
        }
        let l = self.fresh();
        binds.push(Bind::Let(l, e));
        l
    }

    /// The component count if `call` is a **saturated** direct call whose callee
    /// returns a spread aggregate, else `None`.
    fn spread_call_arity(&self, call: &CExpr) -> Option<usize> {
        if let K::App { args, .. } = &call.kind {
            let abi = self.callee_abi(&app_func(call))?;
            if args.len() == abi.params.len() {
                return abi.spread_return().map(<[_]>::len);
            }
        }
        None
    }

    /// The callee ABI of a direct call to a top-level definition, else `None`.
    fn callee_abi(&self, func: &CExpr) -> Option<Arc<FnAbi>> {
        if let K::Global(def) = &func.kind { Some(abi_of(self.db, *def)) } else { None }
    }
}

fn app_func(call: &CExpr) -> CExpr {
    if let K::App { func, .. } = &call.kind { (**func).clone() } else { error() }
}

/// Wraps `body` in `binds` (innermost last).
fn wrap(binds: Vec<Bind>, body: CExpr) -> CExpr {
    let mut out = body;
    for b in binds.into_iter().rev() {
        let ty = out.ty.clone();
        out = match b {
            Bind::Let(local, value) => {
                CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(out) }, ty)
            }
            Bind::Many(locals, value) => {
                CExpr::new(K::LetMany { locals, value: Box::new(value), body: Box::new(out) }, ty)
            }
        };
    }
    out
}

fn float_ty() -> Ty {
    Ty::Con(Con::Float)
}

/// A closed tuple type of `n` `Float`s — a valid FFA shape, used as the type of a
/// reassembled cell / spread node. Code generation reads the scalar bitmap, not the
/// labels, so the exact record vs tuple form is immaterial.
fn ffa_tuple_ty(n: usize) -> Ty {
    Ty::Tuple(vec![Ty::Con(Con::Float); n])
}

fn local_f64(l: LocalId) -> CExpr {
    CExpr::new(K::Local(l), float_ty())
}

fn error() -> CExpr {
    CExpr::new(K::Error, Ty::Error)
}
