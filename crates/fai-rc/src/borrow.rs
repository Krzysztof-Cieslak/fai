//! Borrow inference: deciding which parameters a function only *inspects*, so a
//! caller can lend them (retain ownership) instead of transferring it.
//!
//! Borrowing is always **sound** — a borrowed parameter is treated like a capture
//! (duplicated on a consuming use, never dropped), and the caller releases it at
//! its own last use — so this analysis is purely a performance choice: borrow a
//! parameter that is only read, own one whose contents escape (so that, e.g., a
//! rebuilt list keeps being reused in place).
//!
//! One ownership rule is not about churn but about enabling a later transform: a
//! parameter that flows into a **saturated self-call in tail position** is owned,
//! never borrowed. A lent argument must be dropped *after* the call, which would
//! push the call out of tail position; owning it keeps the self-call in tail
//! position so the recursion can be flattened into a loop (turning, e.g., an
//! accumulator fold into constant stack space). Non-tail self-calls (`1 + f r`)
//! are unaffected and still borrow.
//!
//! The analysis is **inter-procedural**: a parameter that is only forwarded to
//! another function's borrowing parameter is itself borrowed. A saturated direct
//! call to another function consults that function's [`borrow_signature`] (a
//! self-call still uses the in-progress signature, handled by the local fixpoint
//! below). Acyclic call graphs resolve as ordinary query dependencies; mutual
//! recursion forms a salsa cycle resolved by a **monotone fixpoint** — start
//! optimistic (every parameter borrowed) and demote on escape/reconstruct. The
//! lattice is finite and the step is monotone (a more-borrowed callee can only
//! make a forwarding caller more-borrowed), so it converges; a high-iteration
//! fallback to all-owned keeps the query total even for a pathologically large
//! recursion cluster. Because the result feeds the codegen firewall, editing a
//! callee's body only ripples to callers when its borrow signature actually
//! changes (early cutoff on the small [`BorrowSig`] value).

use fai_core::core;
use fai_core::ir::{CExpr, CoreFn, ExprKind as K};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};

/// Which of a function's parameters are borrowed (true) versus owned, by position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BorrowSig(pub Vec<bool>);

impl BorrowSig {
    /// Whether parameter `i` is borrowed.
    #[must_use]
    pub fn is_borrowed(&self, i: usize) -> bool {
        self.0.get(i).copied().unwrap_or(false)
    }

    /// Whether any parameter is borrowed (so the function needs an owned-ABI
    /// wrapper for first-class/indirect use).
    #[must_use]
    pub fn any(&self) -> bool {
        self.0.iter().any(|&b| b)
    }

    /// Whether a call passing `nargs` arguments may use this signature directly: a
    /// saturated direct call whose argument count matches the parameter count.
    #[must_use]
    pub fn exploitable_at(&self, nargs: usize) -> bool {
        !self.0.is_empty() && nargs == self.0.len()
    }
}

/// The borrow signature of `name`'s entry function.
///
/// Inter-procedural: a saturated direct call to another function consults its
/// borrow signature (this query), so a forwarded inspect-only parameter is
/// borrowed transitively. Mutual recursion forms a salsa cycle resolved by the
/// monotone fixpoint declared here ([`borrow_initial`]/[`borrow_recover`]).
#[salsa::tracked(cycle_fn = borrow_recover, cycle_initial = borrow_initial)]
pub fn borrow_signature(db: &dyn Db, file: SourceFile, name: Symbol) -> BorrowSig {
    let lowered = core(db, file, name);
    let entry = lowered.entry();
    let n = entry.params.len();
    if n == 0 {
        return BorrowSig(Vec::new());
    }
    // Row-polymorphic functions take leading offset-evidence parameters and are
    // only ever called curried (through `apply_n`), never as a saturated direct
    // call, so borrowing them would never be exploited; keep them all-owned.
    let def = lowered.def;
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |s| fai_types::evidence_count(&s));
    if evidence > 0 {
        return BorrowSig(vec![false; n]);
    }

    // Local fixpoint over self-recursion: start optimistic (all borrowed) and
    // demote a parameter to owned once it escapes (using the in-progress signature
    // for self-calls, and callees' signatures for cross-function calls) or is
    // matched-and-reconstructed (owned so its cell is reused in place). Monotone,
    // so it converges in ≤ n rounds. (Cross-function mutual recursion is the outer
    // salsa fixpoint; this inner loop only resolves the self-calls.)
    let mut sig = vec![true; n];
    loop {
        let a = analyze(db, entry, def, &sig);
        let mut changed = false;
        for (i, p) in entry.params.iter().enumerate() {
            let owned = a.escaped.contains(p) || (a.reconstructs && a.matched.contains(p));
            if sig[i] && owned {
                sig[i] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    BorrowSig(sig)
}

/// Iteration count after which the cross-function borrow fixpoint gives up and
/// falls back to all-owned. The fixpoint is monotone over a finite lattice, so it
/// converges in far fewer rounds for any realistic program; this bound only keeps
/// the query total for a pathologically large mutual-recursion cluster, and sits
/// well below salsa's own iteration cap.
const BORROW_FIXPOINT_BOUND: u32 = 100;

/// The optimistic start for a borrow-signature cycle: every parameter borrowed
/// (the top of the lattice), so the monotone fixpoint converges to the greatest —
/// most precise — sound signature. (An all-owned start would be a trivial
/// fixpoint that never borrows across a cycle.)
fn borrow_initial(db: &dyn Db, _id: salsa::Id, file: SourceFile, name: Symbol) -> BorrowSig {
    let n = core(db, file, name).entry().params.len();
    BorrowSig(vec![true; n])
}

/// Cycle recovery for [`borrow_signature`]: accept each iteration's value (salsa
/// finalizes once it stops changing). Past [`BORROW_FIXPOINT_BOUND`] iterations —
/// unreachable for a monotone fixpoint over any realistic program — fall back to
/// all-owned so the query stays total rather than reaching salsa's iteration cap.
fn borrow_recover(
    _db: &dyn Db,
    cycle: &salsa::Cycle,
    _last: &BorrowSig,
    value: BorrowSig,
    _file: SourceFile,
    _name: Symbol,
) -> BorrowSig {
    if cycle.iteration() >= BORROW_FIXPOINT_BOUND {
        return BorrowSig(vec![false; value.0.len()]);
    }
    value
}

/// The result of analyzing a function body for borrowing.
struct Facts {
    /// Parameters whose value escapes (stored, returned, or passed to a function).
    escaped: FxHashSet<LocalId>,
    /// Parameters that are matched (a cell of theirs is projected).
    matched: FxHashSet<LocalId>,
    /// Whether the body constructs a data value (so a matched cell can be reused).
    reconstructs: bool,
}

/// The parameters that are *owned* under the current self signature (and callees'
/// signatures, consulted via `db`): a value derived from the parameter (by
/// projection or aliasing) reaches a consuming position.
fn analyze<'a>(db: &'a dyn Db, entry: &CoreFn, self_def: DefId, self_sig: &'a [bool]) -> Facts {
    let mut origins: FxHashMap<LocalId, LocalId> = FxHashMap::default();
    for &p in &entry.params {
        origins.insert(p, p);
    }
    let mut cx = Analyzer {
        db,
        self_def,
        self_sig,
        origins,
        field: FxHashSet::default(),
        owned: FxHashSet::default(),
        matched: FxHashSet::default(),
        reconstructs: false,
    };
    cx.scan(&entry.body, true);
    Facts { escaped: cx.owned, matched: cx.matched, reconstructs: cx.reconstructs }
}

struct Analyzer<'a> {
    db: &'a dyn Db,
    self_def: DefId,
    self_sig: &'a [bool],
    /// The parameter each local is a projection/alias of, if any.
    origins: FxHashMap<LocalId, LocalId>,
    /// Locals that are a *projected field* (an independent value), as opposed to a
    /// whole value (a parameter or an alias of one).
    field: FxHashSet<LocalId>,
    /// Parameters whose contents escape (so they must be owned).
    owned: FxHashSet<LocalId>,
    /// Parameters that are matched (a cell of theirs is projected).
    matched: FxHashSet<LocalId>,
    /// Whether the body constructs a data value (so a matched cell can be reused).
    reconstructs: bool,
}

impl Analyzer<'_> {
    /// The parameter an expression's value derives from (a projection/alias
    /// chain), if any.
    fn origin(&self, e: &CExpr) -> Option<LocalId> {
        match &e.kind {
            K::Local(x) => self.origins.get(x).copied(),
            K::DataField { base, .. } => self.origin(base),
            _ => None,
        }
    }

    /// Whether `e`'s value is a projected field rather than a whole value.
    fn is_field(&self, e: &CExpr) -> bool {
        match &e.kind {
            K::Local(x) => self.field.contains(x),
            K::DataField { .. } => true,
            _ => false,
        }
    }

    /// Escape: the value flows somewhere that retains it (stored, returned, or
    /// passed to a function), so its parameter must be owned — even a field, since
    /// that signals the cell is rebuilt and should be reused in place.
    fn consume(&mut self, e: &CExpr) {
        if let Some(p) = self.origin(e) {
            self.owned.insert(p);
        }
    }

    /// Inspect: a primitive reads the value. Only a *whole* parameter (or alias)
    /// is owned by this — a projected field is an independent value.
    fn inspect(&mut self, e: &CExpr) {
        if !self.is_field(e)
            && let Some(p) = self.origin(e)
        {
            self.owned.insert(p);
        }
    }

    fn consume_local(&mut self, l: LocalId) {
        if let Some(p) = self.origins.get(&l).copied() {
            self.owned.insert(p);
        }
    }

    /// Records that the parameter `base` derives from is matched (a cell of it is
    /// projected), so owning it would let that cell be reused.
    fn record_match(&mut self, base: &CExpr) {
        if let Some(p) = self.origin(base) {
            self.matched.insert(p);
        }
    }

    /// Per-argument borrow flags for a call: a saturated self-call uses the
    /// in-progress signature; a saturated call to another function consults that
    /// function's borrow signature; every other call consumes its arguments.
    fn call_arg_borrows(&self, func: &CExpr, nargs: usize) -> Vec<bool> {
        if let K::Global(def) = &func.kind {
            if *def == self.self_def {
                // A self-call uses the in-progress signature (resolved by the
                // local fixpoint); never re-enter the query for self.
                if nargs == self.self_sig.len() {
                    return self.self_sig.to_vec();
                }
            } else if let Some(file) = self.db.source_file(def.file) {
                // A saturated direct call to another function borrows the
                // parameters its signature marks borrowed. Reading the query here
                // is what makes inference inter-procedural; mutual recursion
                // resolves through the salsa fixpoint, and the same `exploitable_at`
                // gating keeps this consistent with the call-site exploit in `rc`.
                let sig = borrow_signature(self.db, file, def.name);
                if sig.exploitable_at(nargs) {
                    return sig.0.clone();
                }
            }
        }
        vec![false; nargs]
    }

    /// Whether `func` applied to `nargs` arguments is a saturated call to this very
    /// function. A *tail* such call can be flattened into a loop, which requires
    /// its arguments to be owned (transferred), so we never borrow a parameter that
    /// flows into one.
    fn is_self_call(&self, func: &CExpr, nargs: usize) -> bool {
        matches!(&func.kind, K::Global(def) if *def == self.self_def)
            && nargs == self.self_sig.len()
    }

    fn scan(&mut self, e: &CExpr, tail: bool) {
        match &e.kind {
            // A returned local has its value consumed (passed to the caller).
            K::Local(_) => {
                if tail {
                    self.consume(e);
                }
            }
            K::Lit(_) | K::Global(_) | K::Error => {}
            K::Let { local, value, body } => {
                self.scan_value(value, *local);
                self.scan(body, tail);
            }
            K::If { cond, then, els } => {
                // The condition is inspected (read), not stored, so it does not
                // force ownership.
                self.scan(cond, false);
                self.scan(then, tail);
                self.scan(els, tail);
            }
            // A primitive inspects its operands. A whole parameter consumed by a
            // primitive is owned (so it is not needlessly duplicated); a projected
            // field is independent, so it does not force its parent to be owned.
            K::Prim { args, .. } => {
                for a in args {
                    self.scan(a, false);
                    self.inspect(a);
                }
            }
            K::MakeData { args, .. } => {
                // A non-nullary construction means a matched cell can be reused.
                if !args.is_empty() {
                    self.reconstructs = true;
                }
                for a in args {
                    self.scan(a, false);
                    self.consume(a);
                }
            }
            K::MakeClosure { captures, .. } => {
                for &c in captures {
                    self.consume_local(c);
                }
            }
            K::App { func, args } => {
                self.scan(func, false);
                self.consume(func);
                // A saturated self-call in tail position owns its arguments. A lent
                // argument must be dropped *after* the call, which would push the
                // call out of tail position; owning it keeps the call in tail
                // position so the recursion can later be flattened into a loop.
                // Other calls (and non-tail self-calls) follow the borrow signature.
                let tail_self_call = tail && self.is_self_call(func, args.len());
                let borrows = self.call_arg_borrows(func, args.len());
                for (i, a) in args.iter().enumerate() {
                    self.scan(a, false);
                    if tail_self_call || !borrows.get(i).copied().unwrap_or(false) {
                        self.consume(a);
                    }
                }
            }
            // Projections read (borrow) their base; they do not consume it, but
            // they do *match* the base (its cell could be reused).
            K::DataTag(base) => {
                self.record_match(base);
                self.scan(base, false);
            }
            K::DataField { base, .. } => {
                self.record_match(base);
                self.scan(base, false);
            }
            // Reference-counting and tail-call nodes are not present in the
            // pre-count IR borrow inference runs on.
            K::Reset { .. }
            | K::Dup { .. }
            | K::Drop { .. }
            | K::Join { .. }
            | K::Recur { .. }
            | K::HoleStart { .. }
            | K::HoleFill { .. }
            | K::HoleClose { .. } => {}
        }
    }

    /// Records the binding `local = value`, propagating projection/alias origins
    /// (which carry no consume) and scanning operations (which do).
    fn scan_value(&mut self, value: &CExpr, local: LocalId) {
        match &value.kind {
            // An alias is the same whole value; it inherits the origin and field
            // status of what it names.
            K::Local(x) => {
                if let Some(o) = self.origins.get(x).copied() {
                    self.origins.insert(local, o);
                }
                if self.field.contains(x) {
                    self.field.insert(local);
                }
            }
            // A projection is an independent field of its base (read, not consumed).
            K::DataField { base, .. } => {
                if let Some(o) = self.origin(base) {
                    self.origins.insert(local, o);
                }
                self.field.insert(local);
                self.record_match(base);
                self.scan(base, false);
            }
            K::DataTag(base) => {
                self.record_match(base);
                self.scan(base, false);
            }
            _ => self.scan(value, false),
        }
    }
}
