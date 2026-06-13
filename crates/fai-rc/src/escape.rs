//! Closure escape analysis: deciding which `fun`-literals provably do **not**
//! outlive the activation that creates them, so the cell can live on the stack
//! instead of the heap.
//!
//! A heap closure is reference-counted and freed when it dies. A non-escaping
//! closure can instead be a stack cell, reclaimed when the frame returns — the
//! reference-count discipline is unchanged (its captures are still released when
//! it dies), only the cell's storage and the elided free differ. The single new
//! soundness obligation is therefore that the closure's pointer never outlives the
//! frame: that is exactly what this analysis establishes, conservatively.
//!
//! A value **escapes** when it flows somewhere that may outlive the call:
//! returned, stored in a constructor/record/array (a `MakeData`/storing
//! primitive), captured into another closure, or passed to a callee parameter
//! that itself escapes. Crucially, **applying** a closure (the callee position of
//! an application) does *not* escape it — the runtime calls it and drops it, never
//! retaining it past the call — which is the precision a plain "is it owned?"
//! (borrow) view lacks, and is what lets a lambda handed to `List.map`/`foldl`
//! stack-allocate.
//!
//! Two products:
//!
//! * [`escape_signature`] — per **parameter**, does it escape its activation?
//!   Consulted at a saturated direct call to relate a closure argument to the
//!   callee's parameter. Inter-procedural: a self-call uses the in-progress
//!   signature (an inner monotone fixpoint), a cross-function call reads the
//!   callee's signature (a salsa cycle for mutual recursion, like
//!   [`crate::borrow`]). Row-polymorphic definitions (only ever called curried)
//!   report all-escape, the conservative value.
//! * [`mark_escaping_closures`] — rewrites each `MakeClosure` that captures and
//!   does not escape to [`ClosureAlloc::Stack`]. A single pass per function body,
//!   given the (finalized) signatures.
//!
//! Conservative defaults keep it sound: an unknown (first-class) callee, a
//! primitive operand, and any capture are all treated as escaping.

use fai_core::ir::{CExpr, ClosureAlloc, ExprKind as K, LoweredDef};
use fai_core::{core, helper_inlined};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};

/// Which of a function's parameters escape their activation (true), by position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EscapeSig(pub Vec<bool>);

impl EscapeSig {
    /// Whether parameter `i` escapes (the conservative default for an out-of-range
    /// index, e.g. an over-application's surplus argument).
    #[must_use]
    pub fn escapes(&self, i: usize) -> bool {
        self.0.get(i).copied().unwrap_or(true)
    }

    /// Whether a saturated (or over-applied) direct call passing `nargs` arguments
    /// may consult this signature — the same gating as a borrow signature.
    #[must_use]
    pub fn usable_at(&self, nargs: usize) -> bool {
        !self.0.is_empty() && nargs >= self.0.len()
    }
}

/// The escape signature of `name`'s entry function.
///
/// Inter-procedural: a saturated direct call to another function consults its
/// escape signature (this query). Mutual recursion forms a salsa cycle resolved
/// by the monotone fixpoint declared here ([`escape_initial`]/[`escape_recover`]).
#[salsa::tracked(cycle_fn = escape_recover, cycle_initial = escape_initial)]
pub fn escape_signature(db: &dyn Db, file: SourceFile, name: Symbol) -> EscapeSig {
    // Analyze the fully-inlined body, the same form `rc` reference-counts and
    // `mark_escaping_closures` rewrites, so the signature matches actual use.
    let lowered = helper_inlined(db, file, name);
    let entry = lowered.entry();
    let n = entry.params.len();
    if n == 0 {
        return EscapeSig(Vec::new());
    }
    // Row-polymorphic functions take leading offset-evidence parameters and are
    // only ever called curried (through `apply_n`), never as a saturated direct
    // call, so their signature is never consulted; report the conservative
    // all-escape value.
    let def = lowered.def;
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |s| fai_types::evidence_count(&s));
    if evidence > 0 {
        return EscapeSig(vec![true; n]);
    }

    // Local fixpoint over self-recursion: start optimistic (nothing escapes) and
    // promote a parameter to escaping once a value derived from it reaches an
    // escaping sink (using the in-progress signature for self-calls, callees'
    // signatures for cross-function calls). Monotone, so it converges in ≤ n
    // rounds. (Cross-function mutual recursion is the outer salsa fixpoint.)
    let mut sig = vec![false; n];
    loop {
        let escaped = analyze(db, &entry.params, &entry.body, def, Some(&sig));
        let mut changed = false;
        for (i, p) in entry.params.iter().enumerate() {
            if !sig[i] && escaped.contains(p) {
                sig[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    EscapeSig(sig)
}

/// Iteration count after which the cross-function escape fixpoint gives up and
/// falls back to all-escape. The fixpoint is monotone over a finite lattice, so
/// it converges in far fewer rounds for any realistic program; this bound only
/// keeps the query total for a pathologically large mutual-recursion cluster.
const ESCAPE_FIXPOINT_BOUND: u32 = 100;

/// The optimistic start for an escape-signature cycle: nothing escapes (the bottom
/// of the lattice), so the monotone fixpoint converges to the least — most
/// precise — sound signature.
fn escape_initial(db: &dyn Db, _id: salsa::Id, file: SourceFile, name: Symbol) -> EscapeSig {
    let n = core(db, file, name).entry().params.len();
    EscapeSig(vec![false; n])
}

/// Cycle recovery for [`escape_signature`]: accept each iteration's value (salsa
/// finalizes once it stops changing). Past [`ESCAPE_FIXPOINT_BOUND`] iterations —
/// unreachable for a monotone fixpoint over any realistic program — fall back to
/// all-escape so the query stays total.
fn escape_recover(
    _db: &dyn Db,
    cycle: &salsa::Cycle,
    _last: &EscapeSig,
    value: EscapeSig,
    _file: SourceFile,
    _name: Symbol,
) -> EscapeSig {
    if cycle.iteration() >= ESCAPE_FIXPOINT_BOUND {
        return EscapeSig(vec![true; value.0.len()]);
    }
    value
}

/// Rewrites every `MakeClosure` in `lowered` that captures and does not escape its
/// creating activation to [`ClosureAlloc::Stack`]. Each function body is analyzed
/// independently (its own parameters and closure locals); a non-capturing closure
/// is already `Static` (set at lowering) and is left untouched.
///
/// Runs on the pre-count, pre-A-normal-form body, where a `MakeClosure` may appear
/// inline (a lambda argument to a combinator) as well as `let`-bound, so the
/// marker is **context-aware**: a closure's fate is decided by the position it
/// occupies (applied vs. stored vs. passed to a known callee), and a `let`-bound
/// closure by whether its local reaches an escaping sink (the `escaped` set).
pub fn mark_escaping_closures(db: &dyn Db, lowered: &mut LoweredDef) {
    let def = lowered.def;
    for f in &mut lowered.fns {
        // Marking runs after the signatures are finalized, so self-calls consult
        // the memoized query (not an in-progress signature).
        let escaped = analyze(db, &f.params, &f.body, def, None);
        let marker = Marker { db, self_def: def };
        marker.mark(&mut f.body, &escaped);
    }
}

/// If `e` is a capturing closure, mark it stack-allocated when `non_escaping`. A
/// non-capturing closure is already `Static`; an escaping one keeps its `Heap`
/// default.
fn set_stack_if(e: &mut CExpr, non_escaping: bool) {
    if let K::MakeClosure { captures, alloc, .. } = &mut e.kind
        && non_escaping
        && !captures.is_empty()
    {
        *alloc = ClosureAlloc::Stack;
    }
}

/// The context-aware closure marker: decides each `MakeClosure`'s allocation from
/// the position it occupies, recursing through the body.
struct Marker<'a> {
    db: &'a dyn Db,
    self_def: DefId,
}

impl Marker<'_> {
    fn mark(&self, e: &mut CExpr, escaped: &FxHashSet<LocalId>) {
        match &mut e.kind {
            // A `let`-bound closure stack-allocates iff its local never escapes.
            K::Let { local, value, body } => {
                set_stack_if(value, !escaped.contains(local));
                self.mark(value, escaped);
                self.mark(body, escaped);
            }
            K::App { func, args, .. } => {
                // The callee position is *applied*, not stored, so an inline
                // closure there does not escape.
                set_stack_if(func, true);
                self.mark(func, escaped);
                // An inline closure argument escapes iff the callee's matching
                // parameter does.
                let esc = call_arg_escapes(self.db, self.self_def, None, func, args.len());
                for (i, a) in args.iter_mut().enumerate() {
                    set_stack_if(a, !esc.get(i).copied().unwrap_or(true));
                    self.mark(a, escaped);
                }
            }
            // A stored field (constructor/record) or a primitive operand may
            // outlive the call: an inline closure there escapes (kept `Heap`).
            K::MakeData { args, .. } | K::Prim { args, .. } => {
                for a in args {
                    self.mark(a, escaped);
                }
            }
            K::If { cond, then, els } => {
                self.mark(cond, escaped);
                self.mark(then, escaped);
                self.mark(els, escaped);
            }
            K::DataTag { base, .. } | K::DataField { base, .. } => self.mark(base, escaped),
            // A bare (tail-position) closure is returned, so it escapes; leaves and
            // reference-counting nodes (absent pre-count) carry nothing to rewrite.
            K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
            K::Reset { .. }
            | K::FreeReuse { .. }
            | K::Dup { .. }
            | K::Drop { .. }
            | K::Join { .. }
            | K::Recur { .. }
            | K::HoleStart { .. }
            | K::HoleFill { .. }
            | K::HoleClose { .. } => {}
        }
    }
}

/// The set of tracked locals (parameters and closure-bound locals) that escape the
/// function's activation, under the given signatures.
fn analyze(
    db: &dyn Db,
    params: &[LocalId],
    body: &CExpr,
    self_def: DefId,
    self_sig: Option<&[bool]>,
) -> FxHashSet<LocalId> {
    let mut origins: FxHashMap<LocalId, LocalId> = FxHashMap::default();
    for &p in params {
        origins.insert(p, p);
    }
    let mut cx = Analyzer {
        db,
        self_def,
        self_sig,
        origins,
        field: FxHashSet::default(),
        escaped: FxHashSet::default(),
    };
    cx.scan(body, true);
    cx.escaped
}

/// Per-argument escape flags for a call: a saturated self-call uses the in-progress
/// signature (during the fixpoint) or the finalized query (during marking); a
/// saturated call to another function consults its escape signature; every other
/// call (a first-class callee, or an under-application whose closure rides into a
/// partial application) escapes its arguments.
fn call_arg_escapes(
    db: &dyn Db,
    self_def: DefId,
    self_sig: Option<&[bool]>,
    func: &CExpr,
    nargs: usize,
) -> Vec<bool> {
    if let K::Global(def) = &func.kind {
        if *def == self_def {
            if let Some(sig) = self_sig {
                if !sig.is_empty() && nargs >= sig.len() {
                    return sig.to_vec();
                }
            } else if let Some(file) = db.source_file(def.file) {
                let sig = escape_signature(db, file, def.name);
                if sig.usable_at(nargs) {
                    return sig.0.clone();
                }
            }
        } else if let Some(file) = db.source_file(def.file) {
            let sig = escape_signature(db, file, def.name);
            if sig.usable_at(nargs) {
                return sig.0.clone();
            }
        }
    }
    vec![true; nargs]
}

struct Analyzer<'a> {
    db: &'a dyn Db,
    self_def: DefId,
    /// The in-progress self signature during the fixpoint (`Some`), or `None` when
    /// marking (self-calls then consult the finalized query).
    self_sig: Option<&'a [bool]>,
    /// The tracked root each local derives from (a parameter, or a closure-bound
    /// local), following alias/projection chains.
    origins: FxHashMap<LocalId, LocalId>,
    /// Locals that are a projected field (an independent value).
    field: FxHashSet<LocalId>,
    /// Tracked roots whose value escapes the activation.
    escaped: FxHashSet<LocalId>,
}

impl Analyzer<'_> {
    /// The tracked root an expression's value derives from, if any.
    fn origin(&self, e: &CExpr) -> Option<LocalId> {
        match &e.kind {
            K::Local(x) => self.origins.get(x).copied(),
            K::DataField { base, .. } => self.origin(base),
            _ => None,
        }
    }

    /// Marks the root an expression derives from as escaping.
    fn escape(&mut self, e: &CExpr) {
        if let Some(p) = self.origin(e) {
            self.escaped.insert(p);
        }
    }

    /// Marks the root a local derives from as escaping.
    fn escape_local(&mut self, l: LocalId) {
        if let Some(p) = self.origins.get(&l).copied() {
            self.escaped.insert(p);
        }
    }

    fn scan(&mut self, e: &CExpr, tail: bool) {
        match &e.kind {
            // A returned value escapes (passed to the caller).
            K::Local(_) => {
                if tail {
                    self.escape(e);
                }
            }
            K::Lit(_) | K::Global(_) | K::Error => {}
            K::Let { local, value, body } => {
                self.scan_value(value, *local);
                self.scan(body, tail);
            }
            K::If { cond, then, els } => {
                self.scan(cond, false);
                self.scan(then, tail);
                self.scan(els, tail);
            }
            // A primitive may store its operand (an array `set`/`push`, a record
            // update), so an operand is treated as escaping — the conservative
            // default (arithmetic/comparison operands are rarely closures).
            K::Prim { args, .. } => {
                for a in args {
                    self.scan(a, false);
                    self.escape(a);
                }
            }
            // A constructed value may outlive the call, so every field escapes.
            K::MakeData { args, .. } => {
                for a in args {
                    self.scan(a, false);
                    self.escape(a);
                }
            }
            // A captured value rides into the new closure's environment; treat it
            // as escaping (a stack closure captured into another closure is left to
            // a later refinement).
            K::MakeClosure { captures, .. } => {
                for &c in captures {
                    self.escape_local(c);
                }
            }
            K::App { func, args, .. } => {
                // The callee position is *applied*, not stored: the runtime calls
                // and drops it, so it does not escape.
                self.scan(func, false);
                let escapes =
                    call_arg_escapes(self.db, self.self_def, self.self_sig, func, args.len());
                for (i, a) in args.iter().enumerate() {
                    self.scan(a, false);
                    if escapes.get(i).copied().unwrap_or(true) {
                        self.escape(a);
                    }
                }
            }
            // A projection reads its base (a new value), so the base does not
            // escape through it.
            K::DataTag { base, .. } | K::DataField { base, .. } => self.scan(base, false),
            // Reference-counting and tail-call nodes are absent in the pre-count IR.
            K::Reset { .. }
            | K::FreeReuse { .. }
            | K::Dup { .. }
            | K::Drop { .. }
            | K::Join { .. }
            | K::Recur { .. }
            | K::HoleStart { .. }
            | K::HoleFill { .. }
            | K::HoleClose { .. } => {}
        }
    }

    /// Records the binding `local = value`, registering a closure local as a
    /// tracked root and propagating alias/projection origins.
    fn scan_value(&mut self, value: &CExpr, local: LocalId) {
        match &value.kind {
            // An alias is the same value: inherit the origin and field status.
            K::Local(x) => {
                if let Some(o) = self.origins.get(x).copied() {
                    self.origins.insert(local, o);
                }
                if self.field.contains(x) {
                    self.field.insert(local);
                }
            }
            // A projection is an independent field of its base.
            K::DataField { base, .. } => {
                if let Some(o) = self.origin(base) {
                    self.origins.insert(local, o);
                }
                self.field.insert(local);
                self.scan(base, false);
            }
            K::DataTag { base, .. } => self.scan(base, false),
            // A closure value is itself a tracked root; its captures escape.
            K::MakeClosure { captures, .. } => {
                self.origins.insert(local, local);
                for &c in captures {
                    self.escape_local(c);
                }
            }
            _ => self.scan(value, false),
        }
    }
}
