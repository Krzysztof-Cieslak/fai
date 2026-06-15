//! Reuse-signature inference: which size-classed reuse tokens a function's
//! specialized entry can consume from a caller.
//!
//! Reuse analysis (in [`crate::rc`]) recycles a dead cell into a same-size
//! construction **within one function body**. Inter-procedural reuse-token
//! passing lifts that across a call: a caller forwards a freed cell, and a
//! construction in the *callee* recycles it. [`reuse_signature`] is the callee
//! side — the vector of size classes (field counts) the callee's token-taking
//! entry accepts, in canonical (ascending) order. Code generation turns a
//! non-empty signature into a `{base}__reuse` entry taking those tokens as leading
//! parameters; a caller holding a matching freed cell forwards it there.
//!
//! A token is consumed by a **sink** on a path: a construction the token reuses
//! in place, or a forwardable saturated direct call that absorbs it onward
//! ("forward-through"). The signature counts the sinks reachable by threading
//! (tail position, an `if`'s branches, a reuse-target `let` value) **net of the
//! function's own local resets**, taking the per-class maximum across paths — the
//! most a caller could usefully supply. Forward-through makes the analysis
//! inter-procedural (a forward sink contributes the callee's full capacity), so it
//! is a monotone fixpoint over the call graph: an acyclic graph resolves as
//! ordinary query dependencies; a mutual-builder cycle resolves through salsa
//! cycle recovery (start empty, grow to the least fixpoint, capped). The result
//! feeds the codegen firewall: early cutoff on the small signature bounds the
//! ripple of a callee edit to callers that actually forward to it.

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, LoweredDef};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;
use fai_types::{Con, RowEnd, Ty};
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;

/// The size classes (field counts) of the reuse-token slots a function's
/// specialized entry accepts, in canonical ascending order. Empty means the
/// function accepts no forwarded tokens (no `{base}__reuse` entry).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ReuseSig(pub Vec<u32>);

impl ReuseSig {
    /// Whether the function accepts no forwarded tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of token slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The size classes, ascending.
    #[must_use]
    pub fn classes(&self) -> &[u32] {
        &self.0
    }
}

/// The largest number of reuse-token parameters a specialized entry may take. A
/// generous bound that comfortably covers realistic code (the weight-balanced
/// tree sinks need at most two); it caps the calling-convention width and bounds
/// the fixpoint lattice for a pathological forward-through cluster. A tunable
/// budget, not a hard contract.
const TOTAL_SLOT_CAP: usize = 4;

/// Iteration count after which the forward-through fixpoint gives up and falls
/// back to the empty signature, keeping the query total for a pathologically large
/// mutual forward-through cluster (a monotone fixpoint over a finite lattice
/// converges in far fewer rounds for any realistic program).
const REUSE_FIXPOINT_BOUND: u32 = 100;

/// The size class — the boxed cell field count — of a value of type `ty`, when it
/// is statically determined, so a freed cell of this type can be matched to a
/// callee's token slot of the same class.
///
/// A record or tuple is its field count; a discriminated union (or `List`) whose
/// only non-nullary constructor is unique is that constructor's arity (nullary
/// variants are tagged immediates, never reset). Anything else — an open record,
/// a union with several non-nullary constructors, an `Array` (variable length), a
/// scalar, a function, a type variable — has no single static class and yields
/// `None`, so a caller never forwards it (it keeps its local drop instead).
#[must_use]
pub fn reuse_class(db: &dyn Db, ty: &Ty) -> Option<u32> {
    match ty {
        Ty::Record(row) if row.tail == RowEnd::Closed => u32::try_from(row.fields.len()).ok(),
        Ty::Tuple(elems) => u32::try_from(elems.len()).ok(),
        // A `List` value that is not `[]` is a `Cons` cell (head + tail).
        Ty::Con(Con::List) => Some(2),
        Ty::App(head, _) => reuse_class(db, head),
        Ty::Adt(adt) => {
            let file = db.source_file(adt.file)?;
            let decls = fai_resolve::type_decls(db, file);
            let info = decls.type_named(adt.name)?;
            // The unique non-nullary constructor's arity, or `None` when the union
            // has none (no cell to reuse) or several (no single static class).
            let mut nonnullary = info.ctors.iter().filter_map(|c| {
                let arity = decls.ctor(*c)?.arity;
                if arity > 0 { Some(arity) } else { None }
            });
            let first = nonnullary.next()?;
            if nonnullary.next().is_some() { None } else { u32::try_from(first).ok() }
        }
        _ => None,
    }
}

/// The reuse signature of `name`'s entry: the size-classed token slots its
/// specialized entry accepts (see the module docs).
///
/// Inter-procedural via forward-through: a forwardable saturated direct call
/// contributes the callee's [`reuse_signature`], so a mutual forward-through cycle
/// forms a salsa cycle resolved by the monotone fixpoint declared here.
#[salsa::tracked(cycle_fn = reuse_recover, cycle_initial = reuse_initial)]
pub fn reuse_signature(db: &dyn Db, file: SourceFile, name: Symbol) -> ReuseSig {
    let def = DefId::new(file.source(db), name);
    // Row-polymorphic entries are reached only curried through `apply_n`, never as
    // a saturated direct call, so a token-taking entry could never be exploited.
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |s| fai_types::evidence_count(&s));
    if evidence > 0 {
        return ReuseSig(Vec::new());
    }
    // Analyze the reference-counted entry body, where local resets are already
    // placed (a filled construction carries a reuse token; an unhomed one is a
    // `FreeReuse`), so the incoming-token capacity is exactly the sinks left over,
    // net of the function's own dying cells (which forward into the same sinks).
    let lowered = crate::rc(db, file, name);
    let entry = lowered.entry();
    let cx = Cx { db, self_def: def, deaths: forwardable_deaths(db, def, entry) };
    let acc = cx.incoming(&entry.body, true);
    let mut classes = net_to_classes(acc);
    classes.sort_unstable();
    classes.truncate(TOTAL_SLOT_CAP);
    ReuseSig(classes)
}

/// The locals whose drop is a forwardable cell death — an owned boxed-data value
/// reaching its end — as opposed to balancing a borrow's duplicate. A caller's
/// own such cell forwards into a sink, competing with an incoming token for it.
///
/// Data-typed parameters are owned cells (matched through a duplicate, so they are
/// not projection bases and would otherwise be missed); a local bound to a `Dup`
/// is a borrow copy whose drop only balances the duplicate, so it is excluded.
fn forwardable_deaths(db: &dyn Db, def: DefId, entry: &CoreFn) -> FxHashSet<LocalId> {
    let mut deaths = crate::data_typed_locals(&entry.body);
    // Data-typed parameters (owned cells) are deaths too.
    if let Some(scheme) = fai_types::declared_or_inferred_scheme(db, def) {
        let mut ty = &scheme.ty;
        for &p in &entry.params {
            let Ty::Arrow(from, to, _) = ty else { break };
            if crate::is_boxed_data_ty(from) {
                deaths.insert(p);
            }
            ty = to;
        }
    }
    // A local bound to a duplicate is a borrow copy, not a death.
    let mut dup_bound = FxHashSet::default();
    collect_dup_bound(&entry.body, &mut dup_bound);
    deaths.retain(|l| !dup_bound.contains(l));
    deaths
}

/// Collects locals bound by `let x = (dup …)` — borrow copies whose drop balances
/// the duplicate rather than ending an owned value.
fn collect_dup_bound(e: &CExpr, out: &mut FxHashSet<LocalId>) {
    if let K::Let { local, value, body } = &e.kind {
        if matches!(value.kind, K::Dup { .. }) {
            out.insert(*local);
        }
        collect_dup_bound(value, out);
        collect_dup_bound(body, out);
        return;
    }
    e_children(e, &mut |c| collect_dup_bound(c, out));
}

/// Applies `f` to each immediate sub-expression of `e`.
fn e_children(e: &CExpr, f: &mut impl FnMut(&CExpr)) {
    match &e.kind {
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => {
            args.iter().for_each(f)
        }
        K::App { func, args, .. } => {
            f(func);
            args.iter().for_each(f);
        }
        K::If { cond, then, els } => {
            f(cond);
            f(then);
            f(els);
        }
        K::Let { value, body, .. }
        | K::Reset { value, body, .. }
        | K::LetMany { value, body, .. } => {
            f(value);
            f(body);
        }
        K::Spread { components } => components.iter().for_each(f),
        K::FreeReuse { body, .. }
        | K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => f(body),
        K::DataTag { base, .. } | K::HoleClose { base, .. } => f(base),
        K::DataField { base, .. } => f(base),
        K::HoleFill { cell, .. } => f(cell),
        K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

/// The optimistic start for a forward-through cycle: the empty signature (bottom),
/// from which the monotone step grows to the least fixpoint.
fn reuse_initial(_db: &dyn Db, _id: salsa::Id, _file: SourceFile, _name: Symbol) -> ReuseSig {
    ReuseSig(Vec::new())
}

/// Cycle recovery for [`reuse_signature`]: accept each iteration's value (salsa
/// finalizes once it stops changing). Past [`REUSE_FIXPOINT_BOUND`] iterations —
/// unreachable for a monotone fixpoint over any realistic program — fall back to
/// the empty signature so the query stays total.
fn reuse_recover(
    _db: &dyn Db,
    cycle: &salsa::Cycle,
    _last: &ReuseSig,
    value: ReuseSig,
    _file: SourceFile,
    _name: Symbol,
) -> ReuseSig {
    if cycle.iteration() >= REUSE_FIXPOINT_BOUND {
        return ReuseSig(Vec::new());
    }
    value
}

/// Per-path sink capacity: the size-classed sinks reachable by threading on a
/// path, plus the count of the function's own local resets competing for them
/// (each will consume one sink unit when it forwards).
#[derive(Default)]
struct Acc {
    /// Sink capacity by size class (constructions + forward-through capacity).
    sinks: BTreeMap<u32, usize>,
    /// Local resets that found no home and will forward (each debits one sink).
    freed: usize,
}

impl Acc {
    fn sink(class: u32) -> Acc {
        let mut sinks = BTreeMap::new();
        sinks.insert(class, 1);
        Acc { sinks, freed: 0 }
    }

    fn forward(sig: &ReuseSig) -> Acc {
        let mut sinks = BTreeMap::new();
        for &c in &sig.0 {
            *sinks.entry(c).or_insert(0) += 1;
        }
        Acc { sinks, freed: 0 }
    }

    fn freed() -> Acc {
        Acc { sinks: BTreeMap::new(), freed: 1 }
    }

    /// Straight-line composition: sinks and debits both accumulate.
    fn seq(mut self, other: Acc) -> Acc {
        for (c, n) in other.sinks {
            *self.sinks.entry(c).or_insert(0) += n;
        }
        self.freed += other.freed;
        self
    }
}

/// The sinks left for incoming tokens after netting out a path's local resets,
/// class-blind (a freed reset consumes any one sink unit). Exact for a single
/// size class; a sound approximation otherwise (the runtime size check guards a
/// mispairing, and over- or under-counting only adds or drops a token slot).
fn net(acc: &Acc) -> BTreeMap<u32, usize> {
    let mut sinks = acc.sinks.clone();
    let mut debits = acc.freed;
    // Drain debits against the available sinks (largest class first is irrelevant
    // when there is one class; deterministic by `BTreeMap` order otherwise).
    let classes: Vec<u32> = sinks.keys().copied().collect();
    for c in classes {
        if debits == 0 {
            break;
        }
        let n = sinks.get_mut(&c).expect("class present");
        let take = (*n).min(debits);
        *n -= take;
        debits -= take;
        if *n == 0 {
            sinks.remove(&c);
        }
    }
    sinks
}

/// Flattens a netted accumulator into a multiset of size classes (one entry per
/// available slot).
fn net_to_classes(acc: Acc) -> Vec<u32> {
    let mut out = Vec::new();
    for (c, n) in net(&acc) {
        out.extend(std::iter::repeat_n(c, n));
    }
    out
}

/// Branch composition: take the per-class maximum of the two branches' netted
/// capacities — a caller's tokens follow one path, so the best path bounds the
/// useful slots. Debits are resolved within each branch (so the result carries
/// none).
fn branch(then: Acc, els: Acc) -> Acc {
    let a = net(&then);
    let b = net(&els);
    let mut sinks = a;
    for (c, n) in b {
        let e = sinks.entry(c).or_insert(0);
        *e = (*e).max(n);
    }
    Acc { sinks, freed: 0 }
}

/// The context for the capacity walk over one function's body.
struct Cx<'a> {
    db: &'a dyn Db,
    self_def: DefId,
    /// Locals whose drop is a forwardable cell death (see [`forwardable_deaths`]).
    deaths: FxHashSet<LocalId>,
}

impl Cx<'_> {
    /// The incoming-token sink capacity of `e` in `tail` (threadable) position.
    fn incoming(&self, e: &CExpr, tail: bool) -> Acc {
        match &e.kind {
            // A tail construction with no reuse token yet is a sink of its field count.
            K::MakeData { args, reuse, .. } if tail && reuse.is_none() && !args.is_empty() => {
                Acc::sink(args.len() as u32)
            }
            // A tail forwardable call absorbs the callee's full token capacity.
            K::App { func, args, .. } if tail => {
                match forward_target(self.db, self.self_def, func, args.len()) {
                    Some(sig) => Acc::forward(&sig),
                    None => Acc::default(),
                }
            }
            K::Let { value, body, .. } => {
                // A reuse-target construction bound in a `let` is a sink; other `let`
                // values (a call whose result is used, the recursion) are not — the
                // token threads on to the body.
                let from_value = match &value.kind {
                    K::MakeData { args, reuse: None, .. } if !args.is_empty() => {
                        Acc::sink(args.len() as u32)
                    }
                    _ => Acc::default(),
                };
                from_value.seq(self.incoming(body, tail))
            }
            K::If { then, els, .. } => branch(self.incoming(then, tail), self.incoming(els, tail)),
            K::Reset { body, .. } | K::Dup { body, .. } => self.incoming(body, tail),
            // A dropped owned data cell will forward, competing with incoming tokens.
            K::Drop { local, body } => {
                let here = if self.deaths.contains(local) { Acc::freed() } else { Acc::default() };
                here.seq(self.incoming(body, tail))
            }
            // A freed local reset will forward, competing with incoming tokens.
            K::FreeReuse { body, .. } => Acc::freed().seq(self.incoming(body, tail)),
            // The tail-call loop forms (a flattened builder uses the destination hole,
            // not token forwarding): recurse into the loop body, treat back-edges and
            // already-tokened constructions as non-sinks.
            K::Join { body, .. } => self.incoming(body, tail),
            _ => Acc::default(),
        }
    }
}

/// The callee's reuse signature if `func` applied to `nargs` arguments is a
/// forwardable saturated direct call: a non-self, non-row-polymorphic top-level
/// function whose parameters this call saturates and which accepts tokens. (A
/// self-call is excluded — the tail-call transform owns per-iteration loop reuse.)
pub(crate) fn forward_target(
    db: &dyn Db,
    self_def: DefId,
    func: &CExpr,
    nargs: usize,
) -> Option<ReuseSig> {
    let K::Global(g) = &func.kind else { return None };
    if *g == self_def {
        return None;
    }
    let gfile = db.source_file(g.file)?;
    let evidence =
        fai_types::declared_or_inferred_scheme(db, *g).map_or(0, |s| fai_types::evidence_count(&s));
    if evidence > 0 {
        return None;
    }
    // The callee's entry arity from its borrow signature (one flag per parameter) —
    // a firewall-stable value with early cutoff, so a callee *body* edit that leaves
    // the arity unchanged does not ripple here (reading the callee's full lowering
    // for the count would couple every caller to every callee body edit).
    let arity = crate::borrow_signature(db, gfile, g.name).0.len();
    if arity == 0 || nargs != arity {
        return None;
    }
    let sig = reuse_signature(db, gfile, g.name);
    if sig.is_empty() { None } else { Some(sig) }
}

/// Reachability/marshalling helper: whether `def`'s lowering forwards any reuse
/// token to a callee (so the caller's object code must be emitted with the
/// forwarding calls). Reads the final (forwarded) lowering.
#[must_use]
pub fn forwards_to(lowered: &LoweredDef) -> Vec<DefId> {
    let mut out = Vec::new();
    for f in &lowered.fns {
        collect_forwards(&f.body, &mut out);
    }
    if let Some(re) = &lowered.reuse_entry {
        collect_forwards(&re.body, &mut out);
    }
    out
}

fn collect_forwards(e: &CExpr, out: &mut Vec<DefId>) {
    match &e.kind {
        K::App { func, args, reuse, .. } => {
            if reuse.iter().any(Option::is_some)
                && let K::Global(g) = &func.kind
            {
                out.push(*g);
            }
            collect_forwards(func, out);
            args.iter().for_each(|a| collect_forwards(a, out));
        }
        K::Prim { args, .. } | K::MakeData { args, .. } | K::Recur { args } => {
            args.iter().for_each(|a| collect_forwards(a, out));
        }
        K::If { cond, then, els } => {
            collect_forwards(cond, out);
            collect_forwards(then, out);
            collect_forwards(els, out);
        }
        K::Let { value, body, .. } => {
            collect_forwards(value, out);
            collect_forwards(body, out);
        }
        K::Reset { value, body, .. } | K::LetMany { value, body, .. } => {
            collect_forwards(value, out);
            collect_forwards(body, out);
        }
        K::Spread { components } => components.iter().for_each(|a| collect_forwards(a, out)),
        K::FreeReuse { body, .. }
        | K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => collect_forwards(body, out),
        K::DataTag { base, .. } | K::HoleClose { base, .. } => collect_forwards(base, out),
        K::DataField { base, .. } => collect_forwards(base, out),
        K::HoleFill { cell, .. } => collect_forwards(cell, out),
        K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}
