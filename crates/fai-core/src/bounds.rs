//! The bounds-check-elimination fact engine: a small difference-bound abstract
//! domain over integer locals and array-length terms, shared by the
//! reference-count crate's interprocedural fact inference and code generation.
//!
//! A fact is a difference constraint `a <= b + c` over [`Term`]s (the constant
//! `0`, an integer-valued local, or `len(array local)`). The constraints form a
//! weighted graph (`a --c--> b` meaning `a <= b + c`); an inequality `a <= b + c`
//! is entailed when the shortest path `a -> b` has total weight `<= c`. So
//! `i >= 0` is the path `Zero -> i` with weight `<= 0`, and `i < len(a)` is the
//! path `i -> Len(a)` with weight `<= -1`.
//!
//! Soundness rests on the runtime invariant that a valid `Array`'s length is far
//! below `i64::MAX` (the allocator aborts long before), so an index `< len` can be
//! incremented without two's-complement wraparound — the same invariant Rust's
//! bounds-check elimination relies on. Every transfer function is conservative:
//! an unrecognized operation yields a fresh, unconstrained term, so the worst
//! case is a missed elision, never an unsound one.
//!
//! The in-body [`Bounds`] graph is keyed by [`LocalId`]; the portable
//! [`BoundSig`]/[`ResultSig`] signatures are keyed by *parameter index*, so a
//! definition's inferred facts survive the object cache's wire form and apply
//! regardless of which lowering form re-derives the local facts.

use fai_resolve::LocalId;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{CExpr, ExprKind as K, FieldIndex, Lit, Prim};

/// The maximum number of distinct terms a single [`Bounds`] graph tracks. Beyond
/// it the graph stops admitting new terms and answers conservatively (no facts),
/// bounding closure cost on a pathological definition. Real definitions stay far
/// under it.
const TERM_CAP: usize = 96;

/// Constant magnitudes are clamped to this bound so a path sum cannot overflow and
/// the lattice stays finite. A genuine constant beyond it is simply not recorded
/// (sound: a missing edge only weakens the facts).
const CONST_CAP: i64 = 1 << 40;

/// A symbolic term in the difference-constraint graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Term {
    /// The integer constant `0`.
    Zero,
    /// The value of an integer-valued local.
    Int(LocalId),
    /// The length of an array-valued local.
    Len(LocalId),
}

/// A difference-bound constraint set over [`Term`]s: a weighted directed graph
/// where an edge `from --w--> to` means `from <= to + w`.
#[derive(Debug, Clone, Default)]
pub struct Bounds {
    /// `edges[a][b] = c` records the tightest known `a <= b + c`.
    edges: FxHashMap<Term, FxHashMap<Term, i64>>,
    /// Locals bound to a comparison primitive, so a later use as an `if` condition
    /// can refine the branch. Maps the boolean local to the comparison it computes.
    conds: FxHashMap<LocalId, Cond>,
    /// Locals bound to a saturated call whose callee has result facts, kept so a
    /// later tuple projection of the result can instantiate the per-field facts.
    /// Maps the result local to (the callee's result signature, the call's argument
    /// locals by parameter position).
    call_results: FxHashMap<LocalId, (ResultSig, Vec<Option<LocalId>>)>,
    /// Once the term budget is exceeded the graph is poisoned: it admits no new
    /// terms and entails nothing (so elision is suppressed for the definition).
    poisoned: bool,
}

/// A comparison `lhs OP rhs` bound to a boolean local, recorded so a branch on it
/// can be refined.
#[derive(Debug, Clone, Copy)]
struct Cond {
    op: Prim,
    lhs: Term,
    rhs: Term,
}

impl Bounds {
    /// An empty constraint set (no facts).
    #[must_use]
    pub fn new() -> Self {
        Bounds::default()
    }

    /// The number of distinct terms currently in the graph.
    fn term_count(&self) -> usize {
        let mut seen: FxHashSet<Term> = FxHashSet::default();
        for (a, m) in &self.edges {
            seen.insert(*a);
            for b in m.keys() {
                seen.insert(*b);
            }
        }
        seen.len()
    }

    /// Records `from <= to + c`, keeping the tightest (smallest) weight. Clamped
    /// and budget-checked; a clamp or budget overflow drops the edge (sound).
    fn add_edge(&mut self, from: Term, to: Term, c: i64) {
        if self.poisoned || from == to {
            return;
        }
        if c.abs() > CONST_CAP {
            return;
        }
        // Admit new terms only within the budget; once exceeded, poison so the
        // graph answers conservatively rather than growing without bound.
        let new_terms = usize::from(!self.has_term(from)) + usize::from(!self.has_term(to));
        if new_terms > 0 && self.term_count() + new_terms > TERM_CAP {
            self.poisoned = true;
            return;
        }
        let slot = self.edges.entry(from).or_default().entry(to).or_insert(c);
        if c < *slot {
            *slot = c;
        }
    }

    fn has_term(&self, t: Term) -> bool {
        if self.edges.contains_key(&t) {
            return true;
        }
        self.edges.values().any(|m| m.contains_key(&t))
    }

    /// Records `a == b + c` (both `a <= b + c` and `b <= a - c`).
    fn add_eq(&mut self, a: Term, b: Term, c: i64) {
        self.add_edge(a, b, c);
        self.add_edge(b, a, -c);
    }

    /// Records the constant fact `t == n` (`t` equals the literal `n`).
    fn set_const(&mut self, t: Term, n: i64) {
        self.add_eq(t, Term::Zero, n);
    }

    /// Records `t >= n` (a lower bound).
    fn set_ge(&mut self, t: Term, n: i64) {
        // Zero <= t + (-n)  ==>  t >= n.
        self.add_edge(Term::Zero, t, -n);
    }

    /// The minimum weight of any path `from -> to` (so `from <= to + weight`), or
    /// `None` when `to` is unreachable from `from`. Bellman-Ford over the present
    /// terms, bounded by the term count; a negative cycle (contradictory, i.e.
    /// dead code) saturates toward a very negative bound, which only strengthens
    /// entailment and is sound for unreachable code.
    fn shortest(&self, from: Term, to: Term) -> Option<i64> {
        if from == to {
            return Some(0);
        }
        if self.poisoned {
            return None;
        }
        let mut dist: FxHashMap<Term, i64> = FxHashMap::default();
        dist.insert(from, 0);
        let n = self.term_count() + 1;
        for _ in 0..n {
            let mut changed = false;
            // Relax every edge.
            for (a, m) in &self.edges {
                let Some(&da) = dist.get(a) else { continue };
                for (b, &w) in m {
                    let nd = da.saturating_add(w);
                    let entry = dist.entry(*b).or_insert(i64::MAX);
                    if nd < *entry {
                        *entry = nd;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        dist.get(&to).copied()
    }

    /// The tightest known `a <= b + c` (the shortest path weight), or `None` when
    /// no relation is entailed. Used by interprocedural inference to read a fact
    /// between two terms (e.g. a call argument and a parameter length).
    #[must_use]
    pub fn bound(&self, a: Term, b: Term) -> Option<i64> {
        self.shortest(a, b)
    }

    /// Whether `a <= b + c` is entailed.
    fn entails_le(&self, a: Term, b: Term, c: i64) -> bool {
        self.shortest(a, b).is_some_and(|d| d <= c)
    }

    /// Whether `t >= 0` is entailed.
    fn entails_nonneg(&self, t: Term) -> bool {
        // Zero <= t + 0  ==>  t >= 0.
        self.entails_le(Term::Zero, t, 0)
    }

    /// Whether `a < b` is entailed (`a <= b - 1`).
    fn entails_lt(&self, a: Term, b: Term) -> bool {
        self.entails_le(a, b, -1)
    }

    /// Whether the index given by `index` is provably within `0 .. len(array)`, so
    /// its inline bounds check can be elided. `index` is an atom (a local or an
    /// integer literal) after A-normal form.
    #[must_use]
    pub fn index_in_bounds(&self, array: LocalId, index: &CExpr) -> bool {
        if self.poisoned {
            return false;
        }
        let len = Term::Len(array);
        match &index.kind {
            K::Local(i) => {
                let t = Term::Int(*i);
                self.entails_nonneg(t) && self.entails_lt(t, len)
            }
            K::Lit(Lit::Int(n)) => {
                // A literal index `n >= 0` is in bounds iff `len(array) > n`.
                *n >= 0 && self.entails_le(Term::Zero, len, -(n.saturating_add(1)))
            }
            _ => false,
        }
    }

    /// Applies the effect of a binding `local = value` to the graph (and records a
    /// comparison so a later branch on `local` refines). `value`'s operands are
    /// atoms after A-normal form; any leading reference-count wrappers the rc pass
    /// inserted (`Dup`/`Drop`/`Reset`/`FreeReuse`) are peeled to reach the operation.
    pub fn transfer_let(&mut self, local: LocalId, value: &CExpr) {
        let value = peel_rc(value);
        match &value.kind {
            K::Lit(Lit::Int(n)) => self.set_const(Term::Int(local), *n),
            // An alias is the same value.
            K::Local(x) => {
                // Could be an int or array alias; relate both interpretations
                // (harmless if one term never participates).
                self.add_eq(Term::Int(local), Term::Int(*x), 0);
                self.add_eq(Term::Len(local), Term::Len(*x), 0);
            }
            K::Prim { op, args } => self.transfer_prim(local, *op, args),
            // Projecting a tuple field of a call result instantiates that callee's
            // per-field result facts onto the projected local.
            K::DataField { base, index, .. } => {
                if let (K::Local(b), FieldIndex::Const(k)) = (&base.kind, index) {
                    self.transfer_result_field(local, *b, *k);
                }
            }
            _ => {}
        }
    }

    fn transfer_prim(&mut self, local: LocalId, op: Prim, args: &[CExpr]) {
        match op {
            Prim::IntAdd => self.transfer_addsub(local, args, 1),
            Prim::IntSub => self.transfer_addsub(local, args, -1),
            Prim::IntAnd => self.transfer_mask(local, args),
            Prim::IntRem => self.transfer_rem(local, args),
            Prim::ArrayLength => {
                if let Some(K::Local(a)) = args.first().map(|e| &e.kind) {
                    // local == len(a), and a length is non-negative.
                    self.add_eq(Term::Int(local), Term::Len(*a), 0);
                    self.set_ge(Term::Int(local), 0);
                }
            }
            Prim::ArrayWithCapacity => {
                // A fresh array starts at length 0.
                self.set_const(Term::Len(local), 0);
            }
            Prim::ArrayPush => {
                if let Some(K::Local(a)) = args.first().map(|e| &e.kind) {
                    // len(local) == len(a) + 1.
                    self.add_eq(Term::Len(local), Term::Len(*a), 1);
                }
                self.set_ge(Term::Len(local), 0);
            }
            Prim::ArraySet => {
                if let Some(K::Local(a)) = args.first().map(|e| &e.kind) {
                    // In place or copied, the length is unchanged.
                    self.add_eq(Term::Len(local), Term::Len(*a), 0);
                }
            }
            Prim::IntLt | Prim::IntLe | Prim::IntGt | Prim::IntGe | Prim::Eq => {
                if let (Some(lhs), Some(rhs)) = (args.first(), args.get(1))
                    && let (Some(l), Some(r)) = (atom_term(lhs), atom_term(rhs))
                {
                    self.conds.insert(local, Cond { op, lhs: l, rhs: r });
                }
            }
            _ => {}
        }
        // Every array length is non-negative.
        self.set_ge(Term::Len(local), 0);
    }

    /// `local = a + b` (`sign = 1`) or `local = a - b` (`sign = -1`). Only a
    /// literal second (or, for `+`, first) operand yields a difference edge.
    fn transfer_addsub(&mut self, local: LocalId, args: &[CExpr], sign: i64) {
        let (Some(a), Some(b)) = (args.first(), args.get(1)) else { return };
        let lt = Term::Int(local);
        match (&a.kind, &b.kind) {
            // local = x + k  /  local = x - k
            (K::Local(x), K::Lit(Lit::Int(k))) => self.add_eq(lt, Term::Int(*x), sign * k),
            // local = k + x   (addition only; subtraction `k - x` is not a diff edge)
            (K::Lit(Lit::Int(k)), K::Local(x)) if sign == 1 => self.add_eq(lt, Term::Int(*x), *k),
            (K::Lit(Lit::Int(p)), K::Lit(Lit::Int(q))) => {
                self.set_const(lt, p.wrapping_add(sign * q))
            }
            _ => {}
        }
    }

    /// `local = x & m`: when `m >= 0` is provable, `0 <= local <= m`. A negative
    /// mask (e.g. `-1`) is left unconstrained.
    fn transfer_mask(&mut self, local: LocalId, args: &[CExpr]) {
        let (Some(a), Some(b)) = (args.first(), args.get(1)) else { return };
        // Try either operand as the mask (`&` is commutative).
        for (mask, _other) in [(b, a), (a, b)] {
            if let Some(m) = atom_term(mask)
                && self.entails_nonneg(m)
            {
                self.set_ge(Term::Int(local), 0);
                self.add_edge(Term::Int(local), m, 0); // local <= m
                return;
            }
        }
    }

    /// `local = a % n`: when `a >= 0` is provable, `a % n >= 0`.
    fn transfer_rem(&mut self, local: LocalId, args: &[CExpr]) {
        if let Some(a) = args.first().and_then(atom_term)
            && self.entails_nonneg(a)
        {
            self.set_ge(Term::Int(local), 0);
        }
    }

    /// Refines the graph along the `taken` branch of an `if` whose condition is
    /// `cond` (an atom). A condition local recorded as a comparison contributes the
    /// branch's directional fact; anything else contributes nothing.
    pub fn refine(&mut self, cond: &CExpr, taken: bool) {
        let K::Local(c) = &cond.kind else { return };
        let Some(cond) = self.conds.get(c).copied() else { return };
        if cond.op == Prim::Eq {
            // Equality's branches: `x == y` on the true side; on the false side a
            // disequality, useful only to bump a known bound past a constant (e.g.
            // `cap != 0` with `cap >= 0` gives `cap >= 1`, so a `cap - 1` mask is
            // non-negative).
            if taken {
                self.add_eq(cond.lhs, cond.rhs, 0);
            } else {
                self.refine_ne(cond.lhs, cond.rhs);
            }
            return;
        }
        let op = if taken { cond.op } else { negate(cond.op) };
        self.assert_cmp(op, cond.lhs, cond.rhs);
    }

    /// Refines on `lhs != rhs` where one side is the constant `0`: a value known
    /// `>= 0` becomes `>= 1`, and one known `<= 0` becomes `<= -1`. (A general
    /// disequality is not a difference constraint, so only this constant-bump case
    /// is recorded.)
    fn refine_ne(&mut self, lhs: Term, rhs: Term) {
        let x = match (lhs, rhs) {
            (Term::Zero, t) | (t, Term::Zero) => t,
            _ => return,
        };
        if x == Term::Zero {
            return;
        }
        // x >= 0 known (path Zero -> x with weight <= 0) ⇒ x >= 1.
        if self.entails_le(Term::Zero, x, 0) {
            self.add_edge(Term::Zero, x, -1);
        }
        // x <= 0 known ⇒ x <= -1.
        if self.entails_le(x, Term::Zero, 0) {
            self.add_edge(x, Term::Zero, -1);
        }
    }

    /// Records the constraint asserted by `lhs OP rhs` holding.
    fn assert_cmp(&mut self, op: Prim, lhs: Term, rhs: Term) {
        match op {
            // lhs < rhs  ==>  lhs <= rhs - 1
            Prim::IntLt => self.add_edge(lhs, rhs, -1),
            // lhs <= rhs
            Prim::IntLe => self.add_edge(lhs, rhs, 0),
            // lhs > rhs  ==>  rhs < lhs
            Prim::IntGt => self.add_edge(rhs, lhs, -1),
            // lhs >= rhs  ==>  rhs <= lhs
            Prim::IntGe => self.add_edge(rhs, lhs, 0),
            // lhs == rhs
            Prim::Eq => self.add_eq(lhs, rhs, 0),
            _ => {}
        }
    }

    /// Seeds the graph with an interprocedural entry-fact signature, mapping each
    /// parameter index to its local. Used by code generation at function entry.
    pub fn seed_entry(&mut self, sig: &BoundSig, params: &[LocalId]) {
        for &(a, b, c) in &sig.edges {
            if let (Some(a), Some(b)) = (pterm_to_term(a, params), pterm_to_term(b, params)) {
                self.add_edge(a, b, c);
            }
        }
    }

    /// Applies a saturated call `local = f(args)` whose callee has result facts
    /// `sig`. The whole-result facts are added immediately; per-tuple-field facts
    /// are deferred (recorded) until the field is projected.
    pub fn transfer_call(&mut self, local: LocalId, sig: &ResultSig, args: &[CExpr]) {
        let arg_locals: Vec<Option<LocalId>> = args
            .iter()
            .map(|a| match &a.kind {
                K::Local(l) => Some(*l),
                _ => None,
            })
            .collect();
        // Whole-result facts apply to `local` directly.
        for &(a, b, c) in &sig.edges {
            if let (Some(a), Some(b)) =
                (rterm_to_term(a, local, &arg_locals), rterm_to_term(b, local, &arg_locals))
            {
                self.add_edge(a, b, c);
            }
        }
        if sig.edges.iter().any(|(a, b, _)| a.is_field() || b.is_field()) {
            self.call_results.insert(local, (sig.clone(), arg_locals));
        }
    }

    /// Instantiates the result facts for tuple field `k` of a recorded call result
    /// `base` onto the projected local.
    fn transfer_result_field(&mut self, local: LocalId, base: LocalId, k: u32) {
        let Some((sig, arg_locals)) = self.call_results.get(&base).cloned() else { return };
        for &(a, b, c) in &sig.edges {
            // Map a result-component term naming field `k` to the projected `local`;
            // an edge mentioning a different field is irrelevant here.
            let (Some(a), Some(b)) = (
                rterm_field_to_term(a, k, local, &arg_locals),
                rterm_field_to_term(b, k, local, &arg_locals),
            ) else {
                continue;
            };
            self.add_edge(a, b, c);
        }
    }
}

/// Peels leading reference-count wrappers (`Dup`/`Drop`/`Reset`/`FreeReuse`),
/// which the rc pass inserts around a `let`'s value, to reach the underlying
/// operation the value computes.
pub fn peel_rc(e: &CExpr) -> &CExpr {
    match &e.kind {
        K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Reset { body, .. }
        | K::FreeReuse { body, .. } => peel_rc(body),
        _ => e,
    }
}

/// The term denoting an atom operand (a local or, for an integer literal, a
/// constant offset from `Zero`), or `None` for a non-atom.
fn atom_term(e: &CExpr) -> Option<Term> {
    match &e.kind {
        K::Local(l) => Some(Term::Int(*l)),
        // A literal `n` is `Zero + n`; callers needing the offset handle it
        // separately, so a bare literal term is only the constant node when n == 0.
        K::Lit(Lit::Int(0)) => Some(Term::Zero),
        _ => None,
    }
}

/// The complement of an ordering comparison (the relation that holds on the false
/// branch). `Eq` is handled separately (see [`Bounds::refine_ne`]).
fn negate(op: Prim) -> Prim {
    match op {
        Prim::IntLt => Prim::IntGe,
        Prim::IntLe => Prim::IntGt,
        Prim::IntGt => Prim::IntLe,
        Prim::IntGe => Prim::IntLt,
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Portable, parameter-indexed signatures (interprocedural facts).
// ---------------------------------------------------------------------------

/// A parameter-indexed term: the constant `0`, a parameter's integer value, or a
/// parameter's array length. Portable across lowering forms and the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PTerm {
    /// The constant `0`.
    Zero,
    /// The integer value of parameter `i`.
    Param(u32),
    /// The length of array parameter `i`.
    LenParam(u32),
}

/// A definition's **entry-fact** signature: difference constraints `a <= b + c`
/// over its parameters that hold on entry (established by all in-file callers).
/// Empty for a public or first-class-used definition (its callers are unknown).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct BoundSig {
    /// Constraints `a <= b + c`.
    pub edges: Vec<(PTerm, PTerm, i64)>,
}

impl BoundSig {
    /// Whether the signature carries no facts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Maps a parameter-indexed term to an in-body term given the entry parameter
/// locals.
fn pterm_to_term(p: PTerm, params: &[LocalId]) -> Option<Term> {
    match p {
        PTerm::Zero => Some(Term::Zero),
        PTerm::Param(i) => params.get(i as usize).map(|l| Term::Int(*l)),
        PTerm::LenParam(i) => params.get(i as usize).map(|l| Term::Len(*l)),
    }
}

/// The tuple-field marker for a function's whole (non-tuple) result.
pub const WHOLE: u32 = u32::MAX;

/// A term in a function's **result-fact** space: a parameter, a parameter's
/// length, or a component of the result (an integer value or array length of the
/// whole result or a tuple field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RTerm {
    /// The constant `0`.
    Zero,
    /// The integer value of parameter `i`.
    Param(u32),
    /// The length of array parameter `i`.
    LenParam(u32),
    /// The integer value of result component `field` ([`WHOLE`] for the whole
    /// result, else a tuple field index).
    ResultVal(u32),
    /// The length of result component `field`.
    ResultLen(u32),
}

impl RTerm {
    /// Whether this names a specific tuple field of the result (not the whole
    /// result and not a parameter).
    fn is_field(self) -> bool {
        matches!(self, RTerm::ResultVal(k) | RTerm::ResultLen(k) if k != WHOLE)
    }
}

/// A definition's **result-fact** signature: difference constraints `a <= b + c`
/// relating its result components to its parameters, used by a caller's fold/
/// codegen to learn the length/bounds of a call's result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct ResultSig {
    /// Constraints `a <= b + c` over [`RTerm`]s.
    pub edges: Vec<(RTerm, RTerm, i64)>,
}

impl ResultSig {
    /// Whether the signature carries no facts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Maps a result-fact term to an in-body term for the *whole-result* application
/// (`local` is the call result); a tuple-field term yields `None` (deferred to the
/// projection).
fn rterm_to_term(t: RTerm, local: LocalId, args: &[Option<LocalId>]) -> Option<Term> {
    match t {
        RTerm::Zero => Some(Term::Zero),
        RTerm::Param(i) => args.get(i as usize).copied().flatten().map(Term::Int),
        RTerm::LenParam(i) => args.get(i as usize).copied().flatten().map(Term::Len),
        RTerm::ResultVal(WHOLE) => Some(Term::Int(local)),
        RTerm::ResultLen(WHOLE) => Some(Term::Len(local)),
        RTerm::ResultVal(_) | RTerm::ResultLen(_) => None,
    }
}

/// Maps a result-fact term for tuple field `k` to an in-body term, where
/// `field_local` is the projected field's local.
fn rterm_field_to_term(
    t: RTerm,
    k: u32,
    field_local: LocalId,
    args: &[Option<LocalId>],
) -> Option<Term> {
    match t {
        RTerm::Zero => Some(Term::Zero),
        RTerm::Param(i) => args.get(i as usize).copied().flatten().map(Term::Int),
        RTerm::LenParam(i) => args.get(i as usize).copied().flatten().map(Term::Len),
        RTerm::ResultVal(f) if f == k => Some(Term::Int(field_local)),
        RTerm::ResultLen(f) if f == k => Some(Term::Len(field_local)),
        RTerm::ResultVal(_) | RTerm::ResultLen(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_types::{Con, Ty};

    fn local(n: usize) -> LocalId {
        LocalId::from_index(n)
    }

    fn int_lit(n: i64) -> CExpr {
        CExpr::new(K::Lit(Lit::Int(n)), Ty::Con(Con::Int))
    }

    fn local_expr(n: usize) -> CExpr {
        CExpr::new(K::Local(local(n)), Ty::Con(Con::Int))
    }

    fn prim(op: Prim, args: Vec<CExpr>) -> CExpr {
        CExpr::new(K::Prim { op, args }, Ty::Con(Con::Int))
    }

    #[test]
    fn explicit_guards_prove_in_bounds() {
        // The safe `get` shape: i >= 0 and i < len(a) both dominate.
        let a = local(0);
        let mut b = Bounds::new();
        // len = Prim.arrayLength a
        b.transfer_let(local(2), &prim(Prim::ArrayLength, vec![local_expr(0)]));
        // c0 = i >= 0
        b.transfer_let(local(3), &prim(Prim::IntGe, vec![local_expr(1), int_lit(0)]));
        b.refine(&local_expr(3), true);
        // c1 = i < len
        b.transfer_let(local(4), &prim(Prim::IntLt, vec![local_expr(1), local_expr(2)]));
        b.refine(&local_expr(4), true);
        assert!(b.index_in_bounds(a, &local_expr(1)), "i in [0,len) after both guards");
    }

    #[test]
    fn loop_exit_guard_gives_upper_bound_only() {
        // `if i >= len then exit else <body>`: the else branch has i < len but not
        // i >= 0 (that needs the interprocedural entry fact), so not yet in bounds.
        let a = local(0);
        let mut b = Bounds::new();
        b.transfer_let(local(2), &prim(Prim::ArrayLength, vec![local_expr(0)]));
        b.transfer_let(local(3), &prim(Prim::IntGe, vec![local_expr(1), local_expr(2)]));
        b.refine(&local_expr(3), false); // else: i < len
        assert!(!b.index_in_bounds(a, &local_expr(1)), "missing i >= 0");
        // Seed non-negativity as an entry fact would.
        b.set_ge(Term::Int(local(1)), 0);
        assert!(b.index_in_bounds(a, &local_expr(1)), "now i in [0,len)");
    }

    #[test]
    fn chained_bound_through_int_local() {
        // j < hi and hi <= len(a) imply j < len(a).
        let a = local(0);
        let mut b = Bounds::new();
        // len term for a, and hi <= len(a)
        b.add_edge(Term::Int(local(5)), Term::Len(a), 0); // hi <= len
        b.set_ge(Term::Int(local(6)), 0); // j >= 0 (entry fact)
        // j < hi
        b.transfer_let(local(7), &prim(Prim::IntLt, vec![local_expr(6), local_expr(5)]));
        b.refine(&local_expr(7), true);
        assert!(b.index_in_bounds(a, &local_expr(6)), "j < hi <= len");
    }

    #[test]
    fn minus_one_offset_with_lower_bound() {
        // hi1 = hi - 1, hi <= len(a), hi >= 1  ==>  0 <= hi1 < len(a).
        let a = local(0);
        let mut b = Bounds::new();
        b.add_edge(Term::Int(local(5)), Term::Len(a), 0); // hi <= len
        b.set_ge(Term::Int(local(5)), 1); // hi >= 1
        b.transfer_let(local(8), &prim(Prim::IntSub, vec![local_expr(5), int_lit(1)]));
        assert!(b.index_in_bounds(a, &local_expr(8)), "hi-1 in [0,len)");
    }

    #[test]
    fn mask_with_len_minus_one_is_in_bounds() {
        // The hash-bucket idiom: cap == len(slots), cap >= 1, idx = h & (cap-1).
        let slots = local(0);
        let mut b = Bounds::new();
        // cap == len(slots)
        b.transfer_let(local(1), &prim(Prim::ArrayLength, vec![local_expr(0)]));
        // cap >= 1 (from the `cap = 0` guard's else)
        b.set_ge(Term::Int(local(1)), 1);
        // m = cap - 1
        b.transfer_let(local(2), &prim(Prim::IntSub, vec![local_expr(1), int_lit(1)]));
        // idx = h & m
        b.transfer_let(local(4), &prim(Prim::IntAnd, vec![local_expr(3), local_expr(2)]));
        assert!(b.index_in_bounds(slots, &local_expr(4)), "h & (len-1) in [0,len)");
    }

    #[test]
    fn unprovable_index_keeps_check() {
        // A bare parameter index with no facts is not in bounds.
        let a = local(0);
        let b = Bounds::new();
        assert!(!b.index_in_bounds(a, &local_expr(1)));
    }

    #[test]
    fn negative_literal_index_not_in_bounds() {
        let a = local(0);
        let b = Bounds::new();
        assert!(!b.index_in_bounds(a, &int_lit(-1)));
    }
}
