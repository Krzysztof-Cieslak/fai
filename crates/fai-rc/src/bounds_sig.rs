//! Interprocedural bounds-check-elimination facts: a definition's **entry facts**
//! (difference constraints over its parameters that hold on entry, established by
//! its in-file callers) and **result facts** (its result's length/bounds relative
//! to its parameters, consulted by a caller).
//!
//! Entry facts are *caller-directed*: a private definition's entire caller set is
//! in its own file (cross-file references can only name `public` members), so the
//! facts are a **file-local** fixpoint — the meet over every in-file call site of
//! the facts provable for the arguments. A `public` or first-class-used definition
//! gets no entry facts (its callers are unknown).
//!
//! Result facts are *callee-directed* (length/bounds of a result relative to its
//! parameters); they refine a caller's view of a call's result. They come in two
//! kinds, from two sources:
//!
//! * **Length equalities** (`len(result.component) == len(param k)`) are the
//!   coinductive part, produced by [`crate::length_preservation`] (the sole source).
//! * **Length inequalities and integer bounds** (e.g. `Array.init`'s `len >= n`, a
//!   partition's pivot in `[0, hi)`) are produced *here*, read off a definition's
//!   own entry facts at its return paths.
//!
//! The two fact families are **mutually recursive**: a caller's entry fact (`hi <=
//! len(a)` threading through a sort's recursion) needs a callee's result facts, and
//! a callee's result fact needs its own entry facts. They are resolved by one
//! **file-local coupled fixpoint** ([`module_bounds_facts`]): an outer loop
//! accumulates the numeric result facts monotonically, and an inner loop is the
//! entry-fact narrowing fixpoint run with the current result facts applied **from
//! its first round** (so a result-enabled entry edge sits in the inner round's
//! maximal set and survives widening). The coupling is internal to the one query,
//! so entry/result do not form a salsa cycle between themselves; the only salsa
//! cycle is the (defensive) cross-file one a cyclic module-call graph would form.

use std::sync::Arc;

use fai_core::bounds::{BoundSig, Bounds, PTerm, RTerm, ResultSig, Term, WHOLE};
use fai_core::ir::{CExpr, ExprKind as K, Lit};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId, module_defs};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_syntax::ast::Visibility;
use fai_types::{Con, Scheme, Ty, declared_or_inferred_scheme, evidence_count};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::length::length_preservation;

/// Difference constraints over parameters, accumulated as a map keyed by the
/// constrained term pair (so the meet can intersect by key).
type SigMap = FxHashMap<(PTerm, PTerm), i64>;

/// Difference constraints over a definition's result components and parameters,
/// accumulated as a map keyed by the constrained term pair.
type ResMap = FxHashMap<(RTerm, RTerm), i64>;

/// Constant magnitudes beyond this are dropped from a signature (sound — a missing
/// edge only weakens the facts), keeping the lattice finite for the fixpoint.
const SIG_CONST_CAP: i64 = 1 << 30;

/// Iteration cap for the inner entry-fact fixpoint; a monotone (narrowing) fixpoint
/// over a finite lattice converges far sooner, so this only bounds a pathological
/// file.
const FIXPOINT_BOUND: usize = 64;

/// Iteration cap for the outer result-fact accumulation; result facts grow
/// monotonically toward the (bounded) true facts, so this only bounds a
/// pathological file.
const OUTER_BOUND: usize = 32;

/// A definition's entry-fact signature (constraints over its parameters that hold
/// on entry). Empty for a public or first-class-used definition.
#[salsa::tracked]
pub fn entry_bounds(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<BoundSig> {
    let all = module_bounds_facts(db, file);
    Arc::new(all.entry.get(&name).cloned().unwrap_or_default())
}

/// A definition's result-fact signature: its result's length/bounds relative to its
/// parameters. Merges the coinductive length equalities ([`length_preservation`])
/// with the numeric facts (length inequalities and integer bounds) inferred by the
/// coupled fixpoint.
#[salsa::tracked]
pub fn result_facts(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<ResultSig> {
    let all = module_bounds_facts(db, file);
    let numeric = all.result.get(&name).cloned().unwrap_or_default();
    let lenpres = lenpres_edges(db, file, name);
    Arc::new(merge_result(numeric, lenpres))
}

/// The bounds-check-elimination entry and result facts for every definition in
/// `file`, computed as one file-local coupled fixpoint. Projected per definition by
/// [`entry_bounds`]/[`result_facts`] (so an unrelated edit that leaves a
/// definition's facts unchanged does not re-run its `object_code`).
///
/// A cyclic cross-module call graph would make this query reference itself across
/// files (a caller reads its cross-file callees' [`result_facts`]); that salsa cycle
/// is resolved by the monotone fixpoint declared here ([`module_bounds_initial`]/
/// [`module_bounds_recover`]) — never reached for the acyclic graphs in practice.
#[salsa::tracked(cycle_fn = module_bounds_recover, cycle_initial = module_bounds_initial)]
fn module_bounds_facts(db: &dyn Db, file: SourceFile) -> Arc<ModuleFacts> {
    let source = file.source(db);
    let defs = module_defs(db, file);

    let mut data: FxHashMap<Symbol, DefData> = FxHashMap::default();
    let mut order: Vec<Symbol> = Vec::new();
    for info in &defs.defs {
        // The reference-counted form (A-normal, tail-call-flattened) is the same
        // body shape code generation seeds these facts onto: call arguments are
        // atoms, a self tail-call is a `Recur`, and a tail-recursive function is a
        // `Join` loop whose only exits are its base cases.
        let lowered = crate::rc(db, file, info.name);
        let params = lowered.entry().params.clone();
        let bodies: Vec<CExpr> = lowered.fns.iter().map(|f| f.body.clone()).collect();
        let def = DefId::new(source, info.name);
        let (result_ty, param_kinds, evidence) = signature_shape(db, def, params.len());
        data.insert(info.name, DefData { params, bodies, result_ty, param_kinds, evidence });
        order.push(info.name);
    }

    let arity = |n: Symbol| data.get(&n).map_or(0, |d| d.params.len());

    // Entry-fact eligibility: a private definition never used first-class.
    let mut eligible: FxHashSet<Symbol> =
        defs.defs.iter().filter(|d| d.visibility == Visibility::Private).map(|d| d.name).collect();
    for n in &order {
        if let Some(d) = data.get(n) {
            for body in &d.bodies {
                poison_first_class(body, source, &arity, &mut eligible);
            }
        }
    }

    // Outer fixpoint: accumulate numeric result facts, recomputing the inner entry
    // fixpoint with the current result facts each round.
    let mut entry: FxHashMap<Symbol, SigMap> = FxHashMap::default();
    let mut result: FxHashMap<Symbol, ResMap> = FxHashMap::default();
    for _ in 0..OUTER_BOUND {
        let next_entry = entry_fixpoint(db, source, &data, &eligible, &arity, &result);
        let next_result = extract_results(db, source, &data, &next_entry, &result);
        if next_entry == entry && next_result == result {
            entry = next_entry;
            result = next_result;
            break;
        }
        entry = next_entry;
        result = next_result;
    }

    Arc::new(ModuleFacts {
        entry: entry.into_iter().map(|(n, m)| (n, to_sig(m))).collect(),
        result: result.into_iter().map(|(n, m)| (n, to_result_sig(m))).collect(),
    })
}

/// The optimistic start for a cross-module fixpoint cycle: no facts. The cycle
/// grows facts monotonically from this bottom as cross-file results stabilize.
fn module_bounds_initial(_db: &dyn Db, _id: salsa::Id, _file: SourceFile) -> Arc<ModuleFacts> {
    Arc::new(ModuleFacts::default())
}

/// Cycle recovery for [`module_bounds_facts`]: accept each iteration's value; past
/// a bound (unreachable for the monotone fixpoint over any realistic program) fall
/// back to no facts so the query stays total.
fn module_bounds_recover(
    _db: &dyn Db,
    cycle: &salsa::Cycle,
    _last: &Arc<ModuleFacts>,
    value: Arc<ModuleFacts>,
    _file: SourceFile,
) -> Arc<ModuleFacts> {
    if cycle.iteration() >= OUTER_BOUND as u32 {
        return Arc::new(ModuleFacts::default());
    }
    value
}

/// The entry and result facts for every definition in a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ModuleFacts {
    /// Per-definition entry facts.
    entry: FxHashMap<Symbol, BoundSig>,
    /// Per-definition numeric result facts (length equalities are added by
    /// [`result_facts`] from [`length_preservation`]).
    result: FxHashMap<Symbol, ResultSig>,
}

/// Per-definition data gathered once for the fixpoint.
struct DefData {
    /// The entry function's parameters.
    params: Vec<LocalId>,
    /// Every function body (entry plus lifted lambdas).
    bodies: Vec<CExpr>,
    /// The result type (a tuple's fields are result components, else the whole).
    result_ty: Ty,
    /// Each parameter's kind (int / array / neither), by position.
    param_kinds: Vec<ParamKind>,
    /// The leading offset-evidence-parameter count; a row-polymorphic definition
    /// (`evidence > 0`) gets no result facts (its positional indexing is shifted and
    /// it is only ever called curried).
    evidence: usize,
}

/// Whether a parameter is a monomorphic `Int`, an `Array`, or neither (for choosing
/// its difference-graph term).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    Int,
    Array,
    Other,
}

// ---------------------------------------------------------------------------
// Entry-fact fixpoint (inner): the narrowing greatest-fixpoint over parameters,
// run with the current result facts applied.
// ---------------------------------------------------------------------------

/// The entry-fact fixpoint for the file, given the current result facts `result`
/// (applied at every call during the walk). Two phases:
///
/// * **Phase 1 — external propagation.** Self-calls are skipped and no widening is
///   applied, so a fact established at one definition flows to its callees across the
///   call graph (e.g. `hi <= len(a)` from `run` to `qsort` to `partition`). This
///   seeds the candidate set, so a fact that only arrives once a *caller's* facts are
///   known is present before its preservation is checked.
/// * **Phase 2 — coinductive preservation.** Self-calls are included; each candidate
///   is kept only if every call site (including the self-recurrence, seeded with the
///   candidate) establishes it — a greatest fixpoint that *assumes* a fact to *prove*
///   it preserved. Widening from the second round drops a creeping bound.
fn entry_fixpoint(
    db: &dyn Db,
    source: SourceId,
    data: &FxHashMap<Symbol, DefData>,
    eligible: &FxHashSet<Symbol>,
    arity: &dyn Fn(Symbol) -> usize,
    result: &FxHashMap<Symbol, ResMap>,
) -> FxHashMap<Symbol, SigMap> {
    let result_of = |d: DefId| result_sig_for(db, source, d, result);

    // Phase 1: external-only propagation to a fixpoint.
    let mut facts: FxHashMap<Symbol, SigMap> = FxHashMap::default();
    for _ in 0..FIXPOINT_BOUND {
        let next = collect_round(data, eligible, arity, &result_of, &facts, false);
        if next == facts {
            break;
        }
        facts = next;
    }

    // Phase 2: self-inclusive narrowing with widening.
    for round in 0..FIXPOINT_BOUND {
        let next = collect_round(data, eligible, arity, &result_of, &facts, true);
        let updated: FxHashMap<Symbol, SigMap> = next
            .into_iter()
            .map(|(n, m)| {
                // The first self-inclusive round legitimately loosens a phase-1 bound
                // to its recurrence-preserved value; widen only afterward, so that
                // one step settles while a genuine creeper (growing every round) is
                // still dropped.
                let w = if round == 0 { m } else { widen(facts.get(&n), m) };
                (n, w)
            })
            .collect();
        if updated == facts {
            break;
        }
        facts = updated;
    }
    facts
}

/// One round of entry-fact collection: walk every definition's body (seeded with its
/// current entry facts), meeting the facts each call site establishes onto its
/// callee. With `include_self` false, self-calls are skipped (the external
/// propagation phase).
fn collect_round(
    data: &FxHashMap<Symbol, DefData>,
    eligible: &FxHashSet<Symbol>,
    arity: &dyn Fn(Symbol) -> usize,
    result_of: &dyn Fn(DefId) -> ResultSig,
    facts: &FxHashMap<Symbol, SigMap>,
    include_self: bool,
) -> FxHashMap<Symbol, SigMap> {
    let mut next: FxHashMap<Symbol, Option<SigMap>> = FxHashMap::default();
    for (caller, d) in data {
        let caller = *caller;
        for (i, body) in d.bodies.iter().enumerate() {
            let mut seed = Bounds::new();
            if i == 0
                && let Some(sig) = facts.get(&caller)
            {
                seed.seed_entry(&to_sig(sig.clone()), &d.params);
            }
            let mut on_call = |b: &Bounds, callee: Symbol, args: &[CExpr]| {
                if !eligible.contains(&callee) || args.len() != arity(callee) {
                    return;
                }
                if !include_self && callee == caller {
                    return;
                }
                let extracted = extract_call(b, args);
                let slot = next.entry(callee).or_insert(None);
                meet(slot, extracted);
            };
            walk(body, caller, seed, result_of, &mut on_call, &mut |_, _| {});
        }
    }
    next.into_iter().map(|(n, m)| (n, m.unwrap_or_default())).collect()
}

// ---------------------------------------------------------------------------
// Result-fact extraction (outer step): read each definition's result components'
// bounds off its (converged) entry facts at its return paths.
// ---------------------------------------------------------------------------

/// One outer step: re-derive each definition's numeric result facts from its entry
/// facts (and the current result facts, applied during the walk), creep-guarded
/// against the previous round so a growing constant cannot diverge.
fn extract_results(
    db: &dyn Db,
    source: SourceId,
    data: &FxHashMap<Symbol, DefData>,
    entry: &FxHashMap<Symbol, SigMap>,
    prev: &FxHashMap<Symbol, ResMap>,
) -> FxHashMap<Symbol, ResMap> {
    let result_of = |d: DefId| result_sig_for(db, source, d, prev);
    let mut out: FxHashMap<Symbol, ResMap> = FxHashMap::default();
    for (name, d) in data {
        if d.evidence > 0 {
            continue;
        }
        // Only the entry function (body 0) returns the definition's result.
        let Some(body) = d.bodies.first() else { continue };
        let mut seed = Bounds::new();
        if let Some(sig) = entry.get(name) {
            seed.seed_entry(&to_sig(sig.clone()), &d.params);
        }
        let mut acc: Option<ResMap> = None;
        let mut on_exit = |b: &Bounds, v: &CExpr| {
            let m = extract_result(b, v, d, &result_of);
            meet_result(&mut acc, m);
        };
        walk(body, *name, seed, &result_of, &mut |_, _, _| {}, &mut on_exit);
        let m = acc.unwrap_or_default();
        let guarded = widen_result(prev.get(name), m);
        if !guarded.is_empty() {
            out.insert(*name, guarded);
        }
    }
    out
}

/// A synthetic result local used to read a tail-call's result facts (a call in tail
/// position is the definition's result but is not bound to a body local). Far above
/// any real local index, so it never collides.
fn synthetic_result_local() -> LocalId {
    LocalId::from_index(1 << 30)
}

/// The numeric result facts established by a single return value `v` of `d`: for
/// each result component, the tightest difference between its value/length term and
/// each parameter term (and `Zero`). Length-vs-length pairs are excluded (the
/// coinductive [`length_preservation`] owns those equalities).
fn extract_result(
    b: &Bounds,
    v: &CExpr,
    d: &DefData,
    result_of: &dyn Fn(DefId) -> ResultSig,
) -> ResMap {
    let v = peel(v);
    // The graph to read from (a clone, when a tail call's result facts must be
    // applied to a synthetic result local) and the result component terms within it.
    let mut owned;
    let (graph, comps): (&Bounds, Vec<(RTerm, Term)>) = match &v.kind {
        // A direct tail call: apply the callee's result facts to a synthetic result
        // local, then read its component terms. Only the whole result is read this
        // way (a tuple-returning tail call would need field projection).
        K::App { func, args, .. } if matches!(&func.kind, K::Global(_)) => {
            let K::Global(g) = &func.kind else { unreachable!() };
            let sig = result_of(*g);
            if sig.is_empty() || matches!(d.result_ty, Ty::Tuple(_)) {
                return ResMap::default();
            }
            let syn = synthetic_result_local();
            owned = b.clone();
            owned.transfer_call(syn, &sig, args);
            (&owned, vec![(result_rterm(WHOLE, &d.result_ty), local_term(syn, &d.result_ty))])
        }
        // A tuple construction: read each field's atom term.
        K::MakeData { args, .. } if matches!(d.result_ty, Ty::Tuple(_)) => {
            let Ty::Tuple(elems) = &d.result_ty else { unreachable!() };
            let comps = elems
                .iter()
                .zip(args)
                .enumerate()
                .filter_map(|(f, (elem, arg))| {
                    component_term(arg, elem).map(|t| (result_rterm(f as u32, elem), t))
                })
                .collect();
            (b, comps)
        }
        // A whole result that is a body local (or literal handled as no term).
        _ => match component_term(v, &d.result_ty) {
            Some(t) => (b, vec![(result_rterm(WHOLE, &d.result_ty), t)]),
            None => return ResMap::default(),
        },
    };
    if comps.is_empty() {
        return ResMap::default();
    }

    // The parameter terms (and Zero) to relate result components to.
    let mut params: Vec<(RTerm, Term)> = vec![(RTerm::Zero, Term::Zero)];
    for (i, kind) in d.param_kinds.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        match kind {
            ParamKind::Int => params.push((RTerm::Param(idx), Term::Int(d.params[i]))),
            ParamKind::Array => params.push((RTerm::LenParam(idx), Term::Len(d.params[i]))),
            ParamKind::Other => {}
        }
    }

    let mut sig = ResMap::default();
    for (rc, rt) in &comps {
        for (rp, pt) in &params {
            // A length-vs-length relation is a preservation equality, owned by
            // `length_preservation`; skip it here to keep a single source of truth.
            if matches!(rc, RTerm::ResultLen(_)) && matches!(rp, RTerm::LenParam(_)) {
                continue;
            }
            if let Some(w) = graph.bound(*rt, *pt)
                && w.abs() <= SIG_CONST_CAP
            {
                tighten_result(&mut sig, *rc, *rp, w);
            }
            if let Some(w) = graph.bound(*pt, *rt)
                && w.abs() <= SIG_CONST_CAP
            {
                tighten_result(&mut sig, *rp, *rc, w);
            }
        }
    }
    sig
}

/// The in-body term for a local interpreted as a result component of type `ty` (its
/// array length, or its integer value).
fn local_term(l: LocalId, ty: &Ty) -> Term {
    if is_array(ty) { Term::Len(l) } else { Term::Int(l) }
}

/// The `RTerm` for a result component's value or length, by the component's type.
fn result_rterm(field: u32, ty: &Ty) -> RTerm {
    if is_array(ty) { RTerm::ResultLen(field) } else { RTerm::ResultVal(field) }
}

/// The in-body term for a result-component atom (a local's int value or array
/// length, by type, or an integer literal as a `Zero` offset is *not* a term —
/// returns `None`, since a literal result is recorded against `Zero` separately).
fn component_term(e: &CExpr, ty: &Ty) -> Option<Term> {
    match &peel(e).kind {
        K::Local(l) => Some(if is_array(ty) { Term::Len(*l) } else { Term::Int(*l) }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The shared body walk: applies result facts at lets, refines at branches, and
// reports calls (for entry facts) and tail exits (for result facts).
// ---------------------------------------------------------------------------

/// Walks `body` maintaining the bounds graph `b`, applying each saturated call's
/// callee result facts (`result_of`) at its binding, refining each branch with the
/// dominating guard, and invoking `on_call` at every direct call / `Recur` and
/// `on_exit` at every tail (non-`Recur`) result value.
fn walk(
    body: &CExpr,
    self_name: Symbol,
    mut b: Bounds,
    result_of: &dyn Fn(DefId) -> ResultSig,
    on_call: &mut dyn FnMut(&Bounds, Symbol, &[CExpr]),
    on_exit: &mut dyn FnMut(&Bounds, &CExpr),
) {
    walk_in(body, self_name, true, &mut b, result_of, on_call, on_exit);
}

#[allow(clippy::too_many_arguments)]
fn walk_in(
    e: &CExpr,
    self_name: Symbol,
    tail: bool,
    b: &mut Bounds,
    result_of: &dyn Fn(DefId) -> ResultSig,
    on_call: &mut dyn FnMut(&Bounds, Symbol, &[CExpr]),
    on_exit: &mut dyn FnMut(&Bounds, &CExpr),
) {
    match &e.kind {
        K::Let { local, value, body } => {
            walk_in(value, self_name, false, b, result_of, on_call, on_exit);
            bce_transfer(b, *local, value, result_of);
            walk_in(body, self_name, tail, b, result_of, on_call, on_exit);
        }
        K::If { cond, then, els } => {
            walk_in(cond, self_name, false, b, result_of, on_call, on_exit);
            let mut bt = b.clone();
            bt.refine(cond, true);
            walk_in(then, self_name, tail, &mut bt, result_of, on_call, on_exit);
            let mut be = b.clone();
            be.refine(cond, false);
            walk_in(els, self_name, tail, &mut be, result_of, on_call, on_exit);
        }
        K::App { func, args, .. } => {
            if let K::Global(d) = &func.kind {
                on_call(b, d.name, args);
            }
            walk_in(func, self_name, false, b, result_of, on_call, on_exit);
            for a in args {
                walk_in(a, self_name, false, b, result_of, on_call, on_exit);
            }
            if tail {
                on_exit(b, e);
            }
        }
        // A tail self-call, flattened into the loop: its arguments are the next
        // iteration's parameter values, so it constrains this definition's own entry
        // facts exactly like an external call. It is *not* a result exit (a
        // back-edge, not a return).
        K::Recur { args } => {
            on_call(b, self_name, args);
            for a in args {
                walk_in(a, self_name, false, b, result_of, on_call, on_exit);
            }
        }
        K::Prim { args, .. } | K::MakeData { args, .. } => {
            for a in args {
                walk_in(a, self_name, false, b, result_of, on_call, on_exit);
            }
            if tail {
                on_exit(b, e);
            }
        }
        K::DataTag { base, .. } | K::DataField { base, .. } => {
            walk_in(base, self_name, false, b, result_of, on_call, on_exit);
            if tail {
                on_exit(b, e);
            }
        }
        K::Reset { value, body, .. } => {
            walk_in(value, self_name, false, b, result_of, on_call, on_exit);
            walk_in(body, self_name, tail, b, result_of, on_call, on_exit);
        }
        K::Dup { body, .. } | K::Drop { body, .. } | K::FreeReuse { body, .. } => {
            walk_in(body, self_name, tail, b, result_of, on_call, on_exit);
        }
        K::Join { body, .. } | K::HoleStart { body, .. } => {
            walk_in(body, self_name, tail, b, result_of, on_call, on_exit);
        }
        K::HoleFill { cell, .. } => {
            walk_in(cell, self_name, false, b, result_of, on_call, on_exit);
        }
        K::HoleClose { base, .. } => {
            walk_in(base, self_name, tail, b, result_of, on_call, on_exit);
        }
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
            if tail {
                on_exit(b, e);
            }
        }
    }
}

/// Applies a binding `local = value` to the graph: a saturated direct call whose
/// callee has result facts threads them ([`Bounds::transfer_call`]); everything else
/// uses the value-shape transfer.
fn bce_transfer(
    b: &mut Bounds,
    local: LocalId,
    value: &CExpr,
    result_of: &dyn Fn(DefId) -> ResultSig,
) {
    let inner = fai_core::bounds::peel_rc(value);
    if let K::App { func, args, .. } = &inner.kind
        && let K::Global(d) = &func.kind
    {
        let sig = result_of(*d);
        if !sig.is_empty() {
            b.transfer_call(local, &sig, args);
            return;
        }
    }
    b.transfer_let(local, value);
}

/// The full result signature of `d` to apply at a call: in-file, the current
/// numeric facts merged with its length-preservation equalities; cross-file, the
/// public [`result_facts`] (already merged).
fn result_sig_for(
    db: &dyn Db,
    source: SourceId,
    d: DefId,
    result: &FxHashMap<Symbol, ResMap>,
) -> ResultSig {
    if d.file == source {
        let numeric = result.get(&d.name).cloned().map(to_result_sig).unwrap_or_default();
        let lenpres = file_lenpres_edges(db, source, d.name);
        merge_result(numeric, lenpres)
    } else if let Some(other) = db.source_file(d.file) {
        (*result_facts(db, other, d.name)).clone()
    } else {
        ResultSig::default()
    }
}

// ---------------------------------------------------------------------------
// Length-preservation edges (the coinductive equalities, threaded into ResultSigs).
// ---------------------------------------------------------------------------

/// The length-preservation equality edges of `name` in `file`, as `ResultSig`
/// edges (`ResultLen(component) == LenParam(param)`).
fn lenpres_edges(db: &dyn Db, file: SourceFile, name: Symbol) -> Vec<(RTerm, RTerm, i64)> {
    length_preservation(db, file, name)
        .pairs()
        .iter()
        .flat_map(|&(c, k)| {
            [
                (RTerm::ResultLen(c), RTerm::LenParam(k), 0),
                (RTerm::LenParam(k), RTerm::ResultLen(c), 0),
            ]
        })
        .collect()
}

/// As [`lenpres_edges`], by `SourceId` (the in-file form used during the fixpoint).
fn file_lenpres_edges(db: &dyn Db, source: SourceId, name: Symbol) -> Vec<(RTerm, RTerm, i64)> {
    match db.source_file(source) {
        Some(file) => lenpres_edges(db, file, name),
        None => Vec::new(),
    }
}

/// Merges numeric result facts with length-preservation edges into one signature.
fn merge_result(mut numeric: ResultSig, lenpres: Vec<(RTerm, RTerm, i64)>) -> ResultSig {
    numeric.edges.extend(lenpres);
    numeric
        .edges
        .sort_by(|a, b| format!("{:?}", (a.0, a.1, a.2)).cmp(&format!("{:?}", (b.0, b.1, b.2))));
    numeric.edges.dedup();
    numeric
}

// ---------------------------------------------------------------------------
// Signature shape (parameter kinds, result type, evidence count).
// ---------------------------------------------------------------------------

/// The result type, parameter kinds, and evidence count of `def` (with `nparams`
/// runtime parameters). A missing scheme yields a conservative all-`Other` shape.
fn signature_shape(db: &dyn Db, def: DefId, nparams: usize) -> (Ty, Vec<ParamKind>, usize) {
    let Some(scheme) = declared_or_inferred_scheme(db, def) else {
        return (Ty::Error, vec![ParamKind::Other; nparams], 0);
    };
    let evidence = evidence_count(&scheme);
    let (params, result) = decompose(&scheme, nparams);
    let kinds = (0..nparams)
        .map(|i| match params.get(i) {
            Some(t) if is_int(t) => ParamKind::Int,
            Some(t) if is_array(t) => ParamKind::Array,
            _ => ParamKind::Other,
        })
        .collect();
    (result.clone(), kinds, evidence)
}

/// Splits a scheme's type into the first `nparams` parameter types and the result.
fn decompose(scheme: &Scheme, nparams: usize) -> (Vec<&Ty>, &Ty) {
    let mut params = Vec::with_capacity(nparams);
    let mut cur = &scheme.ty;
    for _ in 0..nparams {
        let Ty::Arrow(from, to, _) = cur else { break };
        params.push(&**from);
        cur = to;
    }
    (params, cur)
}

// ---------------------------------------------------------------------------
// Unchanged helpers: first-class poison, call-fact extraction, meet/widen.
// ---------------------------------------------------------------------------

/// Peels leading reference-count wrappers to reach the underlying value.
fn peel(e: &CExpr) -> &CExpr {
    fai_core::bounds::peel_rc(e)
}

/// Poisons (removes from `eligible`) every definition referenced as a value rather
/// than a saturated direct call.
fn poison_first_class(
    e: &CExpr,
    source: SourceId,
    arity: &dyn Fn(Symbol) -> usize,
    eligible: &mut FxHashSet<Symbol>,
) {
    match &e.kind {
        K::App { func, args, .. } => {
            if let K::Global(d) = &func.kind
                && d.file == source
                && args.len() >= arity(d.name)
            {
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

/// A call argument as a parameter term, plus the in-body term to read its fact from.
enum ArgTerm {
    Var(PTerm, Term),
    Const(PTerm, i64),
}

/// The facts provable for a call's arguments, as parameter-indexed constraints for
/// the callee.
fn extract_call(b: &Bounds, args: &[CExpr]) -> SigMap {
    let mut terms: Vec<ArgTerm> = vec![ArgTerm::Var(PTerm::Zero, Term::Zero)];
    for (i, arg) in args.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        match &peel(arg).kind {
            K::Local(l) if is_int(&arg.ty) => {
                terms.push(ArgTerm::Var(PTerm::Param(idx), Term::Int(*l)))
            }
            K::Local(l) if is_array(&arg.ty) => {
                terms.push(ArgTerm::Var(PTerm::LenParam(idx), Term::Len(*l)));
            }
            // A literal `0` is the `Zero` term, so it relates to *other* argument
            // terms (capturing e.g. `len(acc) == i` when `i` starts at `0` and the
            // accumulator starts empty — the `Array.init` loop invariant), not just
            // its own `param == 0` bound.
            K::Lit(Lit::Int(0)) if is_int(&arg.ty) => {
                terms.push(ArgTerm::Var(PTerm::Param(idx), Term::Zero))
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
    for a in &terms {
        if let ArgTerm::Const(p, n) = a
            && n.abs() <= SIG_CONST_CAP
        {
            tighten(&mut sig, *p, PTerm::Zero, *n);
            tighten(&mut sig, PTerm::Zero, *p, -*n);
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

/// Records `a <= b + c` for result terms, keeping the tightest (smallest) `c`.
fn tighten_result(sig: &mut ResMap, a: RTerm, b: RTerm, c: i64) {
    let slot = sig.entry((a, b)).or_insert(c);
    if c < *slot {
        *slot = c;
    }
}

/// Meets `acc` with a new call site's facts: keep only edges present in both, at the
/// weaker (larger) weight. The first contribution initializes `acc`.
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

/// Meets `acc` with a new return path's result facts (the fact must hold on every
/// return path).
fn meet_result(acc: &mut Option<ResMap>, new: ResMap) {
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

/// Widens `new` against the previous round `prev` (entry facts): keep an edge only
/// at a weight the new round does not exceed; a grown or newly-appearing edge is
/// dropped (the greatest fixpoint only weakens).
fn widen(prev: Option<&SigMap>, new: SigMap) -> SigMap {
    let Some(prev) = prev else { return new };
    new.into_iter()
        .filter_map(|((a, b), c)| match prev.get(&(a, b)) {
            Some(&pc) if c <= pc => Some(((a, b), c)),
            _ => None,
        })
        .collect()
}

/// Creep-guards `new` result facts against the previous round `prev`: an edge whose
/// constant grew (loosened) since the previous round is dropped, so a constant
/// cannot drift upward across the cross-definition feedback. A newly-appearing edge
/// is kept (result facts grow monotonically toward the true facts as entry facts
/// strengthen — unlike the narrowing entry facts, whose new edges are widened away).
fn widen_result(prev: Option<&ResMap>, new: ResMap) -> ResMap {
    let Some(prev) = prev else { return new };
    new.into_iter()
        .filter(|((a, b), c)| match prev.get(&(*a, *b)) {
            // Present before: keep only if it did not loosen.
            Some(&pc) => *c <= pc,
            // New this round: keep (monotone growth).
            None => true,
        })
        .collect()
}

/// Converts an accumulated parameter-constraint map to a deterministic signature.
fn to_sig(map: SigMap) -> BoundSig {
    let mut edges: Vec<(PTerm, PTerm, i64)> =
        map.into_iter().map(|((a, b), c)| (a, b, c)).collect();
    edges.sort_by(|x, y| format!("{:?}", (x.0, x.1, x.2)).cmp(&format!("{:?}", (y.0, y.1, y.2))));
    BoundSig { edges }
}

/// Converts an accumulated result-constraint map to a deterministic signature.
fn to_result_sig(map: ResMap) -> ResultSig {
    let mut edges: Vec<(RTerm, RTerm, i64)> =
        map.into_iter().map(|((a, b), c)| (a, b, c)).collect();
    edges.sort_by(|x, y| format!("{:?}", (x.0, x.1, x.2)).cmp(&format!("{:?}", (y.0, y.1, y.2))));
    ResultSig { edges }
}

/// Whether `ty` is a monomorphic `Int`.
fn is_int(ty: &Ty) -> bool {
    matches!(ty, Ty::Con(Con::Int))
}

/// Whether `ty`'s head is `Array`.
fn is_array(ty: &Ty) -> bool {
    match ty {
        Ty::Con(Con::Array) => true,
        Ty::App(h, _) => is_array(h),
        _ => false,
    }
}
