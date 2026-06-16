//! Coinductive length-preservation inference: which result components of a
//! definition have a length equal to one of its parameters' lengths
//! (`len(result.component) == len(param k)`).
//!
//! A recursive in-place sort returns an array of the same length as its argument
//! *through* its recursion, so the fact is **coinductive** — it must be assumed in
//! order to be proved. This is the callee-directed peer of [`crate::borrow_signature`]:
//! a per-definition **greatest fixpoint** over candidate `(result-component,
//! parameter)` pairs, started optimistic (every length-compatible pair, the top of
//! the lattice) and **demoted** wherever a return path does not preserve the length.
//! A saturated call to another function consults its [`length_preservation`]; a
//! self-call uses the in-progress assumption (the inner local fixpoint here). Mutual
//! recursion across functions forms a salsa cycle resolved by the monotone fixpoint
//! declared below.
//!
//! It is the **sole source** of result length-*equality* facts: code generation and
//! the numeric bounds fixpoint read these equalities; the numeric fixpoint itself
//! only produces length-*inequalities* (e.g. `Array.init`'s `len >= n`) and integer
//! bounds. The analysis runs on the fused, pre-tail-flattening body (self-recursion
//! still an [`K::App`]), so a return path is a recursive tail-value check with no
//! loop-invariant reasoning.

use fai_core::WHOLE;
use fai_core::fuse_def;
use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, Prim};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;
use fai_types::{Con, Scheme, Ty, declared_or_inferred_scheme, evidence_count};
use rustc_hash::FxHashMap;

/// Whether `ty`'s head constructor is `Array` (a monomorphic or generic array).
fn is_array(ty: &Ty) -> bool {
    match ty {
        Ty::Con(Con::Array) => true,
        Ty::App(h, _) => is_array(h),
        _ => false,
    }
}

/// Bound on the recursive walk of a result value, guarding against a pathological
/// binding chain (lets are acyclic in lowered code, so this is never reached in
/// practice).
const WALK_DEPTH: u32 = 256;

/// Iteration cap after which the cross-function length-preservation fixpoint falls
/// back to no preservation. Monotone over a finite lattice, so a realistic program
/// converges in far fewer rounds; this only keeps the query total.
const LENPRES_FIXPOINT_BOUND: u32 = 100;

/// Which `(result-component, parameter)` pairs a definition's result preserves the
/// length of. The component is [`WHOLE`] (the whole result) or a tuple field index;
/// the parameter is by position. Sorted for a deterministic, early-cutoff value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct LenPresSig(pub Vec<(u32, u32)>);

impl LenPresSig {
    /// Whether `len(result.component) == len(param)` is established.
    #[must_use]
    pub fn preserves(&self, component: u32, param: u32) -> bool {
        self.0.binary_search(&(component, param)).is_ok()
    }

    /// The preserved `(component, parameter)` pairs.
    #[must_use]
    pub fn pairs(&self) -> &[(u32, u32)] {
        &self.0
    }

    /// Whether the signature carries no preservation facts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The length-preservation signature of `name`'s entry function.
///
/// Callee-directed: a saturated direct call to another function consults its
/// signature (this query), so a forwarded array is preserved transitively. Mutual
/// recursion forms a salsa cycle resolved by the monotone fixpoint declared here
/// ([`lenpres_initial`]/[`lenpres_recover`]).
#[salsa::tracked(cycle_fn = lenpres_recover, cycle_initial = lenpres_initial)]
pub fn length_preservation(db: &dyn Db, file: SourceFile, name: Symbol) -> LenPresSig {
    let lowered = fuse_def(db, file, name);
    let entry = lowered.body.entry();
    let def = lowered.body.def;

    let candidates = candidate_pairs(db, def, entry);
    if candidates.is_empty() {
        return LenPresSig::default();
    }

    let cx = Analyzer::build(db, def, entry);
    // Local fixpoint over self-recursion: start optimistic (every candidate) and
    // demote a pair the moment a return path fails to preserve it (using the
    // in-progress set for self-calls). Demote-only over a finite set, so it
    // converges in at most `candidates.len()` rounds.
    let mut sig = candidates;
    loop {
        let kept: Vec<(u32, u32)> =
            sig.iter().copied().filter(|&(c, k)| cx.result_preserves(&sig, c, k)).collect();
        if kept.len() == sig.len() {
            break;
        }
        sig = kept;
    }
    sig.sort_unstable();
    LenPresSig(sig)
}

/// The optimistic start for a length-preservation cycle: every length-compatible
/// `(array result-component, array parameter)` pair (the top of the lattice), so
/// the monotone fixpoint converges to the greatest — most precise — sound set.
fn lenpres_initial(db: &dyn Db, _id: salsa::Id, file: SourceFile, name: Symbol) -> LenPresSig {
    let lowered = fuse_def(db, file, name);
    let mut pairs = candidate_pairs(db, lowered.body.def, lowered.body.entry());
    pairs.sort_unstable();
    LenPresSig(pairs)
}

/// Cycle recovery for [`length_preservation`]: accept each iteration's value
/// (salsa finalizes once it stops changing). Past [`LENPRES_FIXPOINT_BOUND`]
/// iterations — unreachable for a monotone fixpoint over any realistic program —
/// fall back to no preservation so the query stays total.
fn lenpres_recover(
    _db: &dyn Db,
    cycle: &salsa::Cycle,
    _last: &LenPresSig,
    value: LenPresSig,
    _file: SourceFile,
    _name: Symbol,
) -> LenPresSig {
    if cycle.iteration() >= LENPRES_FIXPOINT_BOUND {
        return LenPresSig::default();
    }
    value
}

/// Every length-compatible candidate pair: each array-typed result component paired
/// with each array-typed parameter. The greatest fixpoint only demotes, so this
/// superset is the lattice top. Empty for a row-polymorphic definition (its leading
/// offset-evidence parameters would misalign positional indexing, and it is only
/// ever called curried).
fn candidate_pairs(db: &dyn Db, def: DefId, entry: &CoreFn) -> Vec<(u32, u32)> {
    let Some(scheme) = declared_or_inferred_scheme(db, def) else { return Vec::new() };
    if evidence_count(&scheme) > 0 {
        return Vec::new();
    }
    let nparams = entry.params.len();
    let (params, result) = decompose(&scheme, nparams);

    let array_params: Vec<u32> =
        params.iter().enumerate().filter(|(_, t)| is_array(t)).map(|(i, _)| i as u32).collect();
    if array_params.is_empty() {
        return Vec::new();
    }

    let components: Vec<(u32, &Ty)> = match result {
        Ty::Tuple(elems) => elems.iter().enumerate().map(|(i, t)| (i as u32, t)).collect(),
        other => vec![(WHOLE, other)],
    };

    let mut out = Vec::new();
    for (c, ct) in components {
        if is_array(ct) {
            for &k in &array_params {
                out.push((c, k));
            }
        }
    }
    out
}

/// Splits a scheme's type into the first `nparams` parameter types and the result
/// type (what remains after stripping `nparams` arrows).
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

/// Reads the result value(s) of a body and decides preservation, given the bindings
/// and call structure collected once from the body.
struct Analyzer<'a> {
    db: &'a dyn Db,
    self_def: DefId,
    body: &'a CExpr,
    /// Each parameter's position (so a returned parameter is recognized).
    param_index: FxHashMap<LocalId, u32>,
    /// `let local = value` bindings (the value, for looking through aliases).
    bindings: FxHashMap<LocalId, &'a CExpr>,
    /// `let local = saturated-call g(args)` (so a tuple-field projection of the
    /// result can consult `g`'s per-field preservation).
    call_of: FxHashMap<LocalId, (DefId, &'a [CExpr])>,
}

impl<'a> Analyzer<'a> {
    fn build(db: &'a dyn Db, self_def: DefId, entry: &'a CoreFn) -> Self {
        let mut cx = Analyzer {
            db,
            self_def,
            body: &entry.body,
            param_index: entry.params.iter().copied().zip(0u32..).collect(),
            bindings: FxHashMap::default(),
            call_of: FxHashMap::default(),
        };
        cx.collect(&entry.body);
        cx
    }

    /// Records every `let` binding (and call binding) in the body.
    fn collect(&mut self, e: &'a CExpr) {
        match &e.kind {
            K::Let { local, value, body } => {
                self.bindings.insert(*local, value);
                if let K::App { func, args, .. } = &value.kind
                    && let K::Global(g) = &func.kind
                {
                    self.call_of.insert(*local, (*g, args));
                }
                self.collect(value);
                self.collect(body);
            }
            K::If { cond, then, els } => {
                self.collect(cond);
                self.collect(then);
                self.collect(els);
            }
            K::App { func, args, .. } => {
                self.collect(func);
                for a in args {
                    self.collect(a);
                }
            }
            K::Prim { args, .. } | K::Foreign { args, .. } | K::MakeData { args, .. } => {
                for a in args {
                    self.collect(a);
                }
            }
            K::DataField { base, .. } | K::DataTag { base, .. } => self.collect(base),
            _ => {}
        }
    }

    /// The length-preservation signature to use for callee `g`: the in-progress set
    /// for a self-call (never re-entering the query), the callee's query otherwise.
    fn callee_pairs(&self, g: DefId, sig: &[(u32, u32)]) -> Vec<(u32, u32)> {
        if g == self.self_def {
            sig.to_vec()
        } else if let Some(file) = self.db.source_file(g.file) {
            length_preservation(self.db, file, g.name).0
        } else {
            Vec::new()
        }
    }

    /// Whether every return path of the body preserves `len(result.component) ==
    /// len(param k)`, given the in-progress signature `sig` for self-calls.
    fn result_preserves(&self, sig: &[(u32, u32)], component: u32, k: u32) -> bool {
        self.tails_preserve(self.body, sig, component, k)
    }

    /// Whether every tail value reachable through `let`/`if` preserves the pair.
    fn tails_preserve(&self, e: &CExpr, sig: &[(u32, u32)], component: u32, k: u32) -> bool {
        match &e.kind {
            K::Let { body, .. } => self.tails_preserve(body, sig, component, k),
            K::If { then, els, .. } => {
                self.tails_preserve(then, sig, component, k)
                    && self.tails_preserve(els, sig, component, k)
            }
            _ => self.value_preserves(e, sig, component, k, 0),
        }
    }

    /// Whether the tail value `v` preserves `len(result.component) == len(param k)`.
    fn value_preserves(
        &self,
        v: &CExpr,
        sig: &[(u32, u32)],
        component: u32,
        k: u32,
        d: u32,
    ) -> bool {
        if component == WHOLE {
            self.whole_origin(v, sig, d) == Some(k)
        } else {
            self.field_origin(v, component, sig, d) == Some(k)
        }
    }

    /// The parameter whose length equals `v`'s (whole) array length, if any.
    fn whole_origin(&self, v: &CExpr, sig: &[(u32, u32)], d: u32) -> Option<u32> {
        if d > WALK_DEPTH {
            return None;
        }
        let d = d + 1;
        match &v.kind {
            // A value can itself be a `let`-chain (the operands are let-bound); the
            // chain's final expression is the value.
            K::Let { body, .. } => self.whole_origin(body, sig, d),
            K::Local(l) => {
                if let Some(&k) = self.param_index.get(l) {
                    return Some(k);
                }
                self.bindings.get(l).and_then(|val| self.whole_origin(val, sig, d))
            }
            // `arraySet` preserves its array operand's length — the only
            // length-preserving primitive.
            K::Prim { op: Prim::ArraySet, args } => {
                args.first().and_then(|base| self.whole_origin(base, sig, d))
            }
            // A saturated call: the callee's whole-result preservation `(WHOLE, j)`
            // composed with the length origin of argument `j`.
            K::App { func, args, .. } => {
                let K::Global(g) = &func.kind else { return None };
                let pairs = self.callee_pairs(*g, sig);
                let j = pairs.iter().find(|(c, _)| *c == WHOLE).map(|&(_, j)| j)?;
                args.get(j as usize).and_then(|a| self.whole_origin(a, sig, d))
            }
            // Projecting a tuple field of a call/tuple result.
            K::DataField { base, index: FieldIndex::Const(f), .. } => {
                self.field_origin(base, *f, sig, d)
            }
            // A length that both branches agree on is preserved.
            K::If { then, els, .. } => {
                let t = self.whole_origin(then, sig, d)?;
                (self.whole_origin(els, sig, d) == Some(t)).then_some(t)
            }
            _ => None,
        }
    }

    /// The parameter whose length equals the length of tuple field `f` of `v`.
    fn field_origin(&self, v: &CExpr, f: u32, sig: &[(u32, u32)], d: u32) -> Option<u32> {
        if d > WALK_DEPTH {
            return None;
        }
        let d = d + 1;
        match &v.kind {
            K::Let { body, .. } => self.field_origin(body, f, sig, d),
            K::Local(l) => {
                if let Some((g, args)) = self.call_of.get(l) {
                    let pairs = self.callee_pairs(*g, sig);
                    let j = pairs.iter().find(|(c, _)| *c == f).map(|&(_, j)| j)?;
                    return args.get(j as usize).and_then(|a| self.whole_origin(a, sig, d));
                }
                self.bindings.get(l).and_then(|val| self.field_origin(val, f, sig, d))
            }
            K::MakeData { args, .. } => {
                args.get(f as usize).and_then(|a| self.whole_origin(a, sig, d))
            }
            K::App { func, args, .. } => {
                let K::Global(g) = &func.kind else { return None };
                let pairs = self.callee_pairs(*g, sig);
                let j = pairs.iter().find(|(c, _)| *c == f).map(|&(_, j)| j)?;
                args.get(j as usize).and_then(|a| self.whole_origin(a, sig, d))
            }
            K::If { then, els, .. } => {
                let t = self.field_origin(then, f, sig, d)?;
                (self.field_origin(els, f, sig, d) == Some(t)).then_some(t)
            }
            _ => None,
        }
    }
}
