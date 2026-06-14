//! Interprocedural bounds-check-elimination facts: a definition's **entry facts**
//! (difference constraints over its parameters that hold on entry, established by
//! its in-file callers) and **result facts** (its result's length/bounds relative
//! to its parameters, consulted by a caller).
//!
//! Entry facts are *caller-directed*: a private definition's entire caller set is
//! in its own file (cross-file references can only name `public` members), so the
//! facts are a **file-local** fixpoint — the meet over every in-file call site of
//! the facts provable for the arguments. A `public` or first-class-used definition
//! gets no entry facts (its callers are unknown). This keeps `object_code` a pure
//! per-definition unit: a definition's facts depend only on its own module, so the
//! cross-module codegen firewall holds.
//!
//! Result facts are *callee-directed* (length/bounds of a result relative to its
//! parameters); they refine a caller's view of a call's result.

use std::sync::Arc;

use fai_core::bounds::{BoundSig, Bounds, PTerm, ResultSig, Term};
use fai_core::fuse_def;
use fai_core::ir::{CExpr, ExprKind as K, Lit};
use fai_db::{Db, SourceFile};
use fai_resolve::{LocalId, module_defs};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_syntax::ast::Visibility;
use fai_types::{Con, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// Difference constraints over parameters, accumulated as a map keyed by the
/// constrained term pair (so the meet can intersect by key).
type SigMap = FxHashMap<(PTerm, PTerm), i64>;

/// Constant magnitudes beyond this are dropped from a signature (sound — a missing
/// edge only weakens the facts), keeping the lattice finite for the fixpoint.
const SIG_CONST_CAP: i64 = 1 << 30;

/// Iteration cap for the file-local fixpoint; a monotone (narrowing) fixpoint over
/// a finite lattice converges far sooner, so this only bounds a pathological file.
const FIXPOINT_BOUND: usize = 64;

/// A definition's entry-fact signature (constraints over its parameters that hold
/// on entry). Empty for a public or first-class-used definition.
#[salsa::tracked]
pub fn entry_bounds(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<BoundSig> {
    let all = module_entry_facts(db, file);
    Arc::new(all.get(&name).cloned().unwrap_or_default())
}

/// A definition's result-fact signature (its result's length/bounds relative to
/// its parameters). Currently empty; reserved for length-threading inference.
#[salsa::tracked]
pub fn result_facts(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<ResultSig> {
    let _ = fuse_def(db, file, name);
    Arc::new(ResultSig::default())
}

/// The bounds-check-elimination entry facts for every definition in `file`,
/// computed as one file-local fixpoint. Projected per definition by
/// [`entry_bounds`] (so an unrelated edit that leaves a definition's facts
/// unchanged does not re-run its `object_code`).
#[salsa::tracked]
fn module_entry_facts(db: &dyn Db, file: SourceFile) -> Arc<FxHashMap<Symbol, BoundSig>> {
    let source = file.source(db);
    let defs = module_defs(db, file);

    // Per-definition body, parameters, and arity (the count a saturated call needs).
    struct DefData {
        params: Vec<LocalId>,
        body: CExpr,
    }
    let mut data: FxHashMap<Symbol, DefData> = FxHashMap::default();
    let mut order: Vec<Symbol> = Vec::new();
    for info in &defs.defs {
        // Analyze the reference-counted form (A-normal, tail-call-flattened) so call
        // arguments are atoms and a self-call appears as a `Recur` — the same body
        // shape code generation seeds these facts onto.
        let lowered = crate::rc(db, file, info.name);
        let entry = lowered.entry();
        data.insert(info.name, DefData { params: entry.params.clone(), body: entry.body.clone() });
        order.push(info.name);
    }

    // A definition is *eligible* to receive entry facts only if it is private (all
    // its callers are in this file) and never used first-class (every reference is
    // a saturated direct call, so every call site is one we see here).
    let arity = |n: Symbol| data.get(&n).map_or(0, |d| d.params.len());
    let mut eligible: FxHashSet<Symbol> =
        defs.defs.iter().filter(|d| d.visibility == Visibility::Private).map(|d| d.name).collect();
    for n in &order {
        if let Some(d) = data.get(n) {
            poison_first_class(&d.body, source, &arity, &mut eligible);
        }
    }

    // A greatest-fixpoint over inductive invariants: round 0 seeds the candidate
    // facts from each callee's **external** (non-self) call sites only — a
    // recursive call cannot establish a fact without first assuming it. Later
    // rounds re-derive from every call site (so a recursive call must *preserve*
    // each candidate) and **widen**: an edge whose weight grows from the previous
    // round is dropped, so an upward-creeping bound (a loop index's spurious upper
    // bound) converges to no constraint rather than diverging.
    let mut facts: FxHashMap<Symbol, SigMap> = FxHashMap::default();
    for round in 0..FIXPOINT_BOUND {
        let mut next: FxHashMap<Symbol, Option<SigMap>> = FxHashMap::default();
        for caller in &order {
            let Some(d) = data.get(caller) else { continue };
            let mut seed = Bounds::new();
            if let Some(sig) = facts.get(caller) {
                seed.seed_entry(&to_sig(sig.clone()), &d.params);
            }
            let caller = *caller;
            walk(&d.body, caller, seed, &mut |b: &Bounds, callee: Symbol, args: &[CExpr]| {
                if !eligible.contains(&callee) || args.len() != arity(callee) {
                    return;
                }
                // Round 0 considers only external call sites (a self-call would
                // constrain the callee by a fact not yet established).
                if round == 0 && callee == caller {
                    return;
                }
                let extracted = extract_call(b, args);
                let slot = next.entry(callee).or_insert(None);
                meet(slot, extracted);
            });
        }
        let mut updated: FxHashMap<Symbol, SigMap> = FxHashMap::default();
        for (n, m) in next {
            let m = m.unwrap_or_default();
            let widened = if round == 0 { m } else { widen(facts.get(&n), m) };
            updated.insert(n, widened);
        }
        if updated == facts {
            break;
        }
        facts = updated;
    }
    Arc::new(facts.into_iter().map(|(n, m)| (n, to_sig(m))).collect())
}

/// Walks `body` maintaining the bounds graph `b`, invoking `on_call` at every
/// direct call (`App` with a `Global` head) and at every `Recur` (a self-call to
/// `self_name`, since the tail-call transform turned the function's tail
/// self-recursion into a loop back-edge). Branches refine each side with the
/// dominating guard; reference-count wrappers are transparent.
fn walk(
    body: &CExpr,
    self_name: Symbol,
    mut b: Bounds,
    on_call: &mut dyn FnMut(&Bounds, Symbol, &[CExpr]),
) {
    walk_in(body, self_name, &mut b, on_call);
}

fn walk_in(
    e: &CExpr,
    self_name: Symbol,
    b: &mut Bounds,
    on_call: &mut dyn FnMut(&Bounds, Symbol, &[CExpr]),
) {
    match &e.kind {
        K::Let { local, value, body } => {
            walk_in(value, self_name, b, on_call);
            b.transfer_let(*local, value);
            walk_in(body, self_name, b, on_call);
        }
        K::If { cond, then, els } => {
            walk_in(cond, self_name, b, on_call);
            let mut bt = b.clone();
            bt.refine(cond, true);
            walk_in(then, self_name, &mut bt, on_call);
            let mut be = b.clone();
            be.refine(cond, false);
            walk_in(els, self_name, &mut be, on_call);
        }
        K::App { func, args, .. } => {
            if let K::Global(d) = &func.kind {
                on_call(b, d.name, args);
            }
            walk_in(func, self_name, b, on_call);
            for a in args {
                walk_in(a, self_name, b, on_call);
            }
        }
        // A tail self-call, flattened into the loop: its arguments are the next
        // iteration's parameter values, so it constrains this definition's own
        // entry facts exactly like an external call passing the same arguments.
        K::Recur { args } => {
            on_call(b, self_name, args);
            for a in args {
                walk_in(a, self_name, b, on_call);
            }
        }
        K::Prim { args, .. } | K::MakeData { args, .. } => {
            for a in args {
                walk_in(a, self_name, b, on_call);
            }
        }
        K::DataTag { base, .. } | K::DataField { base, .. } => walk_in(base, self_name, b, on_call),
        K::Reset { value, body, .. } => {
            walk_in(value, self_name, b, on_call);
            walk_in(body, self_name, b, on_call);
        }
        K::Dup { body, .. } | K::Drop { body, .. } | K::FreeReuse { body, .. } => {
            walk_in(body, self_name, b, on_call);
        }
        K::Join { body, .. } | K::HoleStart { body, .. } => walk_in(body, self_name, b, on_call),
        K::HoleFill { cell, .. } => walk_in(cell, self_name, b, on_call),
        K::HoleClose { base, .. } => walk_in(base, self_name, b, on_call),
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

/// Poisons (removes from `eligible`) every definition referenced as a value rather
/// than a saturated direct call: a `Global` head of an under-applied `App`, or any
/// other `Global` occurrence (a captured/first-class value). Such a definition has
/// call sites the file-local analysis cannot see, so it gets no entry facts.
fn poison_first_class(
    e: &CExpr,
    source: SourceId,
    arity: &dyn Fn(Symbol) -> usize,
    eligible: &mut FxHashSet<Symbol>,
) {
    match &e.kind {
        K::App { func, args, .. } => {
            // A saturated (or over-) application of a local `Global` is a real call;
            // its head is not first-class. Anything else about `func` is.
            if let K::Global(d) = &func.kind
                && d.file == source
                && args.len() >= arity(d.name)
            {
                // Real call: do not visit `func` as a value, only the arguments.
                for a in args {
                    poison_first_class(a, source, arity, eligible);
                }
                return;
            }
            poison_first_class(func, source, arity, eligible);
            for a in args {
                poison_first_class(a, source, arity, eligible);
            }
        }
        K::Global(d) => {
            if d.file == source {
                eligible.remove(&d.name);
            }
        }
        K::Let { value, body, .. } => {
            poison_first_class(value, source, arity, eligible);
            poison_first_class(body, source, arity, eligible);
        }
        K::If { cond, then, els } => {
            poison_first_class(cond, source, arity, eligible);
            poison_first_class(then, source, arity, eligible);
            poison_first_class(els, source, arity, eligible);
        }
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => {
            for a in args {
                poison_first_class(a, source, arity, eligible);
            }
        }
        K::DataTag { base, .. } | K::DataField { base, .. } => {
            poison_first_class(base, source, arity, eligible);
        }
        K::Reset { value, body, .. } => {
            poison_first_class(value, source, arity, eligible);
            poison_first_class(body, source, arity, eligible);
        }
        K::Dup { body, .. } | K::Drop { body, .. } | K::FreeReuse { body, .. } => {
            poison_first_class(body, source, arity, eligible);
        }
        K::Join { body, .. } | K::HoleStart { body, .. } => {
            poison_first_class(body, source, arity, eligible);
        }
        K::HoleFill { cell, .. } => poison_first_class(cell, source, arity, eligible),
        K::HoleClose { base, .. } => poison_first_class(base, source, arity, eligible),
        K::Lit(_) | K::Local(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

/// A call argument as a parameter term: an integer parameter's value, an array
/// parameter's length, plus the in-body term to read its fact from. A literal
/// integer argument is recorded as a constant.
enum ArgTerm {
    /// `Param(i)` (int) or `LenParam(i)` (array), reading from `term`.
    Var(PTerm, Term),
    /// An integer literal argument: `Param(i) == value`.
    Const(PTerm, i64),
}

/// The facts provable for a call's arguments, as parameter-indexed constraints for
/// the callee. For each ordered pair of argument terms (and the constant `0`), the
/// tightest difference the caller's graph entails becomes a constraint over the
/// callee's parameters.
fn extract_call(b: &Bounds, args: &[CExpr]) -> SigMap {
    let mut terms: Vec<ArgTerm> = vec![ArgTerm::Var(PTerm::Zero, Term::Zero)];
    for (i, arg) in args.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        match &arg.kind {
            K::Local(l) if is_int(&arg.ty) => {
                terms.push(ArgTerm::Var(PTerm::Param(idx), Term::Int(*l)))
            }
            K::Local(l) if is_array(&arg.ty) => {
                terms.push(ArgTerm::Var(PTerm::LenParam(idx), Term::Len(*l)));
            }
            K::Lit(Lit::Int(n)) if is_int(&arg.ty) => {
                terms.push(ArgTerm::Const(PTerm::Param(idx), *n))
            }
            _ => {}
        }
    }

    let mut sig = SigMap::default();
    for a in &terms {
        for c in &terms {
            match (a, c) {
                (ArgTerm::Var(pa, ta), ArgTerm::Var(pc, tc)) if pa != pc => {
                    if let Some(w) = b.bound(*ta, *tc)
                        && w.abs() <= SIG_CONST_CAP
                    {
                        tighten(&mut sig, *pa, *pc, w);
                    }
                }
                _ => {}
            }
        }
    }
    // Literal integer arguments are exact constants relative to `Zero`.
    for a in &terms {
        if let ArgTerm::Const(p, n) = a
            && n.abs() <= SIG_CONST_CAP
        {
            tighten(&mut sig, *p, PTerm::Zero, *n); // param <= n
            tighten(&mut sig, PTerm::Zero, *p, -*n); // param >= n
        }
    }
    sig
}

/// Records `a <= b + c`, keeping the tightest (smallest) `c`.
fn tighten(sig: &mut SigMap, a: PTerm, b: PTerm, c: i64) {
    let slot = sig.entry((a, b)).or_insert(c);
    if c < *slot {
        *slot = c;
    }
}

/// Meets `acc` with a new call site's facts: the result keeps only edges present
/// in both, at the weaker (larger) weight (the fact must hold at every call site).
/// The first contribution initializes `acc`.
fn meet(acc: &mut Option<SigMap>, new: SigMap) {
    match acc {
        None => *acc = Some(new),
        Some(cur) => {
            cur.retain(|k, c| match new.get(k) {
                Some(&nc) => {
                    *c = (*c).max(nc);
                    true
                }
                None => false,
            });
        }
    }
}

/// Widens `new` against the previous round `prev`: keep an edge only when it was
/// present before at a weight the new round does not exceed (a grown weight — an
/// upward-creeping bound — is dropped, guaranteeing termination); a newly appearing
/// edge is dropped too (the greatest fixpoint only weakens). `prev` absent (a
/// definition first seen after round 0) keeps `new` as-is.
fn widen(prev: Option<&SigMap>, new: SigMap) -> SigMap {
    let Some(prev) = prev else { return new };
    new.into_iter()
        .filter_map(|((a, b), c)| match prev.get(&(a, b)) {
            Some(&pc) if c <= pc => Some(((a, b), c)),
            _ => None,
        })
        .collect()
}

/// Converts an accumulated constraint map to a deterministically-ordered signature.
fn to_sig(map: SigMap) -> BoundSig {
    let mut edges: Vec<(PTerm, PTerm, i64)> =
        map.into_iter().map(|((a, b), c)| (a, b, c)).collect();
    edges.sort_by(|x, y| format!("{:?}", (x.0, x.1, x.2)).cmp(&format!("{:?}", (y.0, y.1, y.2))));
    BoundSig { edges }
}

/// Whether `ty` is a monomorphic `Int`.
fn is_int(ty: &Ty) -> bool {
    matches!(ty, Ty::Con(Con::Int))
}

/// Whether `ty`'s head is `Array`.
fn is_array(ty: &Ty) -> bool {
    fn head(ty: &Ty) -> bool {
        match ty {
            Ty::Con(Con::Array) => true,
            Ty::App(h, _) => head(h),
            _ => false,
        }
    }
    head(ty)
}
