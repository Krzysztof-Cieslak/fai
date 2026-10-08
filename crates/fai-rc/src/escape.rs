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
//! that itself escapes. An under-application retains its callee in a partial
//! application, so applying an unknown closure is safe only when its runtime
//! arity is known to fit. Parameter summaries retain that arity requirement,
//! allowing a known saturated lambda passed to `List.map`/`foldl` to stay on the
//! stack while a possibly under-applied callback stays on the heap.
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
use rustc_hash::FxHashMap;

/// Which parameters may escape without knowing their runtime arity, by position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EscapeSig(pub Vec<bool>);

/// How a parameter may be retained. An applied-only parameter stays confined
/// when its actual runtime arity is no greater than every application site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeUse {
    Never,
    Applied(usize),
    Always,
}

impl EscapeUse {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Always, _) | (_, Self::Always) => Self::Always,
            (Self::Never, other) | (other, Self::Never) => other,
            (Self::Applied(a), Self::Applied(b)) => Self::Applied(a.min(b)),
        }
    }

    fn confined(self, arity: Option<usize>) -> bool {
        match self {
            Self::Never => true,
            Self::Applied(n) => arity.is_some_and(|arity| arity <= n),
            Self::Always => false,
        }
    }
}

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
/// The conservative projection of the internal arity-aware profile. Call-site
/// marking uses the profile directly so known saturated callbacks stay confined.
#[salsa::tracked]
pub fn escape_signature(db: &dyn Db, file: SourceFile, name: Symbol) -> EscapeSig {
    EscapeSig(escape_profile(db, file, name).iter().map(|use_| *use_ != EscapeUse::Never).collect())
}

#[salsa::tracked(cycle_fn = escape_recover, cycle_initial = escape_initial)]
fn escape_profile(db: &dyn Db, file: SourceFile, name: Symbol) -> Vec<EscapeUse> {
    // Analyze the fully-inlined body, the same form `rc` reference-counts and
    // `mark_escaping_closures` rewrites, so the signature matches actual use.
    let lowered = helper_inlined(db, file, name);
    let entry = lowered.entry();
    let n = entry.params.len();
    if n == 0 {
        return Vec::new();
    }
    // Row-polymorphic functions take leading offset-evidence parameters and are
    // only ever called curried (through `apply_n`), never as a saturated direct
    // call, so their signature is never consulted; report the conservative
    // all-escape value.
    let def = lowered.def;
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |s| fai_types::evidence_count(&s));
    if evidence > 0 {
        return vec![EscapeUse::Always; n];
    }

    // Local fixpoint over self-recursion: start optimistic (nothing escapes) and
    // promote a parameter to escaping once a value derived from it reaches an
    // escaping sink (using the in-progress signature for self-calls, callees'
    // signatures for cross-function calls). Requirements only strengthen over
    // the finite set of call-site arities. Cross-function mutual recursion is the
    // outer salsa fixpoint.
    let arities: Vec<_> = lowered.fns.iter().map(|f| f.params.len()).collect();
    let mut sig = vec![EscapeUse::Never; n];
    loop {
        let analysis = analyze(db, &entry.params, &entry.body, def, Some(&sig), &arities);
        let mut changed = false;
        for (i, p) in entry.params.iter().enumerate() {
            let use_ = sig[i].merge(analysis.escaped.get(p).copied().unwrap_or(EscapeUse::Never));
            if sig[i] != use_ {
                sig[i] = use_;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    sig
}

/// Iteration count after which the cross-function escape fixpoint gives up and
/// falls back to all-escape. The fixpoint is monotone over a finite lattice, so
/// it converges in far fewer rounds for any realistic program; this bound only
/// keeps the query total for a pathologically large mutual-recursion cluster.
const ESCAPE_FIXPOINT_BOUND: u32 = 100;

/// The optimistic start for an escape-signature cycle: nothing escapes (the bottom
/// of the lattice), so the monotone fixpoint converges to the least — most
/// precise — sound signature.
fn escape_initial(db: &dyn Db, _id: salsa::Id, file: SourceFile, name: Symbol) -> Vec<EscapeUse> {
    let n = core(db, file, name).entry().params.len();
    vec![EscapeUse::Never; n]
}

/// Cycle recovery for [`escape_profile`]: accept each iteration's value (salsa
/// finalizes once it stops changing). Past [`ESCAPE_FIXPOINT_BOUND`] iterations —
/// unreachable for a monotone fixpoint over any realistic program — fall back to
/// all-escape so the query stays total.
fn escape_recover(
    _db: &dyn Db,
    cycle: &salsa::Cycle,
    _last: &Vec<EscapeUse>,
    value: Vec<EscapeUse>,
    _file: SourceFile,
    _name: Symbol,
) -> Vec<EscapeUse> {
    if cycle.iteration() >= ESCAPE_FIXPOINT_BOUND {
        return vec![EscapeUse::Always; value.len()];
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
    let arities: Vec<_> = lowered.fns.iter().map(|f| f.params.len()).collect();
    for f in &mut lowered.fns {
        // Marking runs after the signatures are finalized, so self-calls consult
        // the memoized query (not an in-progress signature).
        let analysis = analyze(db, &f.params, &f.body, def, None, &arities);
        let marker = Marker { db, self_def: def, arities: &arities, locals: &analysis.arities };
        marker.mark(&mut f.body, &analysis.escaped);
    }
}

/// The context-aware closure marker: decides each `MakeClosure`'s allocation from
/// the position it occupies, recursing through the body.
struct Marker<'a> {
    db: &'a dyn Db,
    self_def: DefId,
    arities: &'a [usize],
    locals: &'a FxHashMap<LocalId, usize>,
}

impl Marker<'_> {
    /// Marks a closure or partial-application *value* stack-allocated when it does
    /// not escape its position. A capturing `MakeClosure` becomes `Stack` (a
    /// non-capturing one is already `Static`); an **under-application** of a known
    /// function becomes `Stack` so code generation builds its partial application in
    /// a stack cell. A saturated or over-application builds no partial application,
    /// so it is left alone (keeping the allocation flag meaningful — only a genuine
    /// stack cell carries `Stack`). An escaping value keeps its `Heap` default.
    fn set_stack_if(&self, e: &mut CExpr, non_escaping: bool) {
        if !non_escaping {
            return;
        }
        // Decide under-application up front (an immutable read) so the arity query
        // does not overlap the mutable borrow below.
        let under_applied = match &e.kind {
            K::App { func, args, .. } => match func.kind {
                K::Global(def) => args.len() < self.callee_arity(def),
                _ => false,
            },
            _ => false,
        };
        match &mut e.kind {
            K::MakeClosure { captures, alloc, .. } if !captures.is_empty() => {
                *alloc = ClosureAlloc::Stack;
            }
            K::App { alloc, .. } if under_applied => *alloc = ClosureAlloc::Stack,
            _ => {}
        }
    }

    /// The runtime arity (entry parameter count) of a callee, read off its escape
    /// profile (whose length is exactly that count); zero for an unresolved def.
    fn callee_arity(&self, def: DefId) -> usize {
        self.db.source_file(def.file).map_or(0, |f| escape_profile(self.db, f, def.name).len())
    }

    fn arity(&self, e: &CExpr) -> Option<usize> {
        runtime_arity(self.db, self.self_def, None, self.arities, self.locals, e)
    }

    fn mark(&self, e: &mut CExpr, escaped: &FxHashMap<LocalId, EscapeUse>) {
        match &mut e.kind {
            // A `let`-bound closure stack-allocates iff its local never escapes.
            K::Let { local, value, body } => {
                self.set_stack_if(value, !escaped.contains_key(local));
                self.mark(value, escaped);
                self.mark(body, escaped);
            }
            K::App { func, args, .. } => {
                // Only a proven saturated call releases its callee rather than
                // storing it in a potentially escaping partial application.
                self.set_stack_if(func, self.arity(func).is_some_and(|n| n <= args.len()));
                self.mark(func, escaped);
                // An inline closure argument escapes iff the callee's matching
                // parameter does.
                let esc = call_arg_uses(self.db, self.self_def, None, func, args.len());
                for (i, a) in args.iter_mut().enumerate() {
                    let use_ = esc.get(i).copied().unwrap_or(EscapeUse::Always);
                    self.set_stack_if(a, use_.confined(self.arity(a)));
                    self.mark(a, escaped);
                }
            }
            // A stored field (constructor/record), a primitive operand, or a foreign
            // operand may outlive the call: an inline closure there escapes (kept
            // `Heap`).
            K::MakeData { args, .. } | K::Prim { args, .. } | K::Foreign { args, .. } => {
                for a in args {
                    self.mark(a, escaped);
                }
            }
            K::If { cond, then, els } => {
                self.mark(cond, escaped);
                self.mark(then, escaped);
                self.mark(els, escaped);
            }
            // Spread/LetMany are produced after this pass; recurse for safety (they
            // carry no closure to restamp).
            K::Spread { components } => {
                for a in components {
                    self.mark(a, escaped);
                }
            }
            K::LetMany { value, body, .. } => {
                self.mark(value, escaped);
                self.mark(body, escaped);
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
    self_sig: Option<&[EscapeUse]>,
    arities: &[usize],
) -> Analysis {
    let mut origins: FxHashMap<LocalId, Vec<LocalId>> = FxHashMap::default();
    for &p in params {
        origins.insert(p, vec![p]);
    }
    let mut cx = Analyzer {
        db,
        self_def,
        self_sig,
        origins,
        fn_arities: arities,
        arities: FxHashMap::default(),
        escaped: FxHashMap::default(),
    };
    cx.scan(body, true);
    Analysis { escaped: cx.escaped, arities: cx.arities }
}

struct Analysis {
    escaped: FxHashMap<LocalId, EscapeUse>,
    arities: FxHashMap<LocalId, usize>,
}

/// Per-argument escape flags for a call: a saturated self-call uses the in-progress
/// signature (during the fixpoint) or the finalized query (during marking); a
/// saturated call to another function consults its escape signature; every other
/// call (a first-class callee, or an under-application whose closure rides into a
/// partial application) escapes its arguments.
fn call_arg_uses(
    db: &dyn Db,
    self_def: DefId,
    self_sig: Option<&[EscapeUse]>,
    func: &CExpr,
    nargs: usize,
) -> Vec<EscapeUse> {
    if let K::Global(def) = &func.kind {
        if *def == self_def {
            if let Some(sig) = self_sig {
                if !sig.is_empty() && nargs >= sig.len() {
                    return sig.to_vec();
                }
            } else if let Some(file) = db.source_file(def.file) {
                let sig = escape_profile(db, file, def.name);
                if !sig.is_empty() && nargs >= sig.len() {
                    return sig;
                }
            }
        } else if let Some(file) = db.source_file(def.file) {
            let sig = escape_profile(db, file, def.name);
            if !sig.is_empty() && nargs >= sig.len() {
                return sig;
            }
        }
    }
    vec![EscapeUse::Always; nargs]
}

/// Exact runtime arity where syntax or a binding proves it. Function types alone
/// are insufficient: a polymorphic or opaque result can itself be a function.
fn runtime_arity(
    db: &dyn Db,
    self_def: DefId,
    self_sig: Option<&[EscapeUse]>,
    fns: &[usize],
    locals: &FxHashMap<LocalId, usize>,
    e: &CExpr,
) -> Option<usize> {
    let of = |e: &CExpr| runtime_arity(db, self_def, self_sig, fns, locals, e);
    match &e.kind {
        K::MakeClosure { func, .. } => fns.get(func.index()).copied(),
        K::Global(def) => {
            let n = if *def == self_def && self_sig.is_some() {
                self_sig?.len()
            } else {
                escape_profile(db, db.source_file(def.file)?, def.name).len()
            };
            // A nullary global is forced and may return a closure of any arity.
            (n > 0).then_some(n)
        }
        K::Local(local) => locals.get(local).copied(),
        K::App { func, args, .. } => of(func)?.checked_sub(args.len()).filter(|n| *n > 0),
        K::If { then, els, .. } => {
            let a = of(then)?;
            (of(els)? == a).then_some(a)
        }
        K::Let { body, .. } => of(body),
        _ => None,
    }
}

struct Analyzer<'a> {
    db: &'a dyn Db,
    self_def: DefId,
    /// The in-progress self signature during the fixpoint (`Some`), or `None` when
    /// marking (self-calls then consult the finalized query).
    self_sig: Option<&'a [EscapeUse]>,
    /// The tracked roots each local derives from, including both arms of an
    /// alias-producing branch and the result of a nested let expression.
    origins: FxHashMap<LocalId, Vec<LocalId>>,
    fn_arities: &'a [usize],
    arities: FxHashMap<LocalId, usize>,
    /// Tracked roots whose value escapes the activation.
    escaped: FxHashMap<LocalId, EscapeUse>,
}

impl Analyzer<'_> {
    /// The tracked root an expression's value derives from, if any.
    fn origins(&self, e: &CExpr) -> Vec<LocalId> {
        match &e.kind {
            K::Local(x) => self.origins.get(x).cloned().unwrap_or_default(),
            K::DataField { base, .. } => self.origins(base),
            K::Let { body, .. } => self.origins(body),
            K::If { then, els, .. } => {
                let mut roots = self.origins(then);
                for root in self.origins(els) {
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                roots
            }
            _ => Vec::new(),
        }
    }

    /// Marks the root an expression derives from as escaping.
    fn escape(&mut self, e: &CExpr, use_: EscapeUse) {
        for root in self.origins(e) {
            self.escape_root(root, use_);
        }
    }

    fn escape_root(&mut self, root: LocalId, use_: EscapeUse) {
        if !use_.confined(self.arities.get(&root).copied()) {
            let slot = self.escaped.entry(root).or_insert(EscapeUse::Never);
            *slot = slot.merge(use_);
        }
    }

    /// Marks the root a local derives from as escaping.
    fn escape_local(&mut self, l: LocalId) {
        if let Some(roots) = self.origins.get(&l).cloned() {
            for root in roots {
                self.escape_root(root, EscapeUse::Always);
            }
        }
    }

    fn scan(&mut self, e: &CExpr, tail: bool) {
        match &e.kind {
            // A returned value escapes (passed to the caller).
            K::Local(_) => {
                if tail {
                    self.escape(e, EscapeUse::Always);
                }
            }
            K::Lit(_) | K::Global(_) | K::Error => {}
            K::Let { local, value, body } => {
                self.scan_value(value, *local);
                self.scan(body, tail);
            }
            // Spread/LetMany are produced after this pass; recurse for safety (their
            // components are scalar floats — never closures — so escape is moot).
            K::Spread { components } => {
                for a in components {
                    self.scan(a, false);
                }
            }
            K::LetMany { value, body, .. } => {
                self.scan(value, false);
                self.scan(body, tail);
            }
            K::If { cond, then, els } => {
                self.scan(cond, false);
                self.scan(then, tail);
                self.scan(els, tail);
            }
            // A primitive or foreign call may store its operand (an array
            // `set`/`push`, a record update, a host write), so an operand is treated
            // as escaping — the conservative default (arithmetic/comparison operands
            // are rarely closures).
            K::Prim { args, .. } | K::Foreign { args, .. } => {
                for a in args {
                    self.scan(a, false);
                    self.escape(a, EscapeUse::Always);
                }
            }
            // A constructed value may outlive the call, so every field escapes.
            K::MakeData { args, .. } => {
                for a in args {
                    self.scan(a, false);
                    self.escape(a, EscapeUse::Always);
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
                self.scan(func, false);
                self.escape(func, EscapeUse::Applied(args.len()));
                let escapes =
                    call_arg_uses(self.db, self.self_def, self.self_sig, func, args.len());
                for (i, a) in args.iter().enumerate() {
                    self.scan(a, false);
                    self.escape(a, escapes.get(i).copied().unwrap_or(EscapeUse::Always));
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
        self.scan(value, false);
        // A let-bound application may be a PAP. Track its own storage lifetime,
        // just like a literal closure, even if it has no source-local origin.
        let roots = if matches!(&value.kind, K::MakeClosure { .. } | K::App { .. }) {
            vec![local]
        } else {
            self.origins(value)
        };
        self.origins.insert(local, roots);
        if let Some(arity) = runtime_arity(
            self.db,
            self.self_def,
            self.self_sig,
            self.fn_arities,
            &self.arities,
            value,
        ) {
            self.arities.insert(local, arity);
        }
    }
}

#[cfg(test)]
mod tests;
