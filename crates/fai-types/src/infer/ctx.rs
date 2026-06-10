//! The inference context: a mutable solver over type variables.
//!
//! Variables live in a union-find. Each variable carries an optional
//! [`Constraint`] (Numeric/Eq/Ord) tracked across unification. Solving produces a
//! substitution that [`InferCtx::reify`] applies to read back an immutable
//! [`Ty`]. The context is local to one inference call (one def or SCC); nothing
//! here is cached by salsa.

use std::rc::Rc;

use fai_resolve::{AdtRef, InterfaceRef};
use fai_syntax::Symbol;

use crate::ty::{
    Con, EffEnd, EffRowVarId, EffectRow, RecordRow, RowEnd, RowVarId, Scheme, Ty, TyVarId,
};

/// A constraint a type variable must satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// Admits `Int`/`Float`; defaults to `Int` when otherwise unconstrained.
    Numeric,
    /// Admits any non-function type (equality).
    Eq,
    /// Admits `Int`/`Float`/`String`/`Char` (ordering).
    Ord,
}

/// The binding of a solver variable.
#[derive(Debug, Clone)]
enum VarState {
    /// Unbound, possibly constrained, tagged with the binding *level* at which it
    /// was created. The level is lowered when the variable is unified with an
    /// outer (lower-level) one; a nested `let` generalizes only variables whose
    /// level is deeper than the enclosing scope's.
    Free(Option<Constraint>, u32),
    /// Bound to a (solver-level) type.
    Bound(SolveTy),
}

/// A solver record row: present fields plus a tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveRow {
    /// The present fields (unordered during solving).
    pub fields: Vec<(Symbol, SolveTy)>,
    /// The row's tail.
    pub tail: RowTail,
}

/// The tail of a solver record row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowTail {
    /// Exactly the listed fields.
    Closed,
    /// The listed fields plus an open row variable.
    Open(RowVarId),
}

/// The binding of a solver row variable.
#[derive(Debug, Clone)]
enum RowState {
    /// Unbound; the labels it must not contain (no duplicates).
    Free(Vec<Symbol>),
    /// Bound to extra fields plus a further tail.
    Bound(SolveRow),
}

/// A solver effect row: the capability atoms a function uses plus a tail. Atoms
/// are a *set* (unlike record fields they carry no payload and never duplicate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolveEffect {
    /// The effect atoms present (deduplicated on expansion).
    pub atoms: Vec<InterfaceRef>,
    /// The row's tail.
    pub tail: EffTail,
}

impl SolveEffect {
    /// The pure (empty, closed) effect.
    pub fn pure() -> SolveEffect {
        SolveEffect { atoms: Vec::new(), tail: EffTail::Closed }
    }
}

/// The tail of a solver effect row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffTail {
    /// Exactly the listed atoms.
    Closed,
    /// The listed atoms plus an open effect-row variable.
    Open(EffRowVarId),
}

/// The binding of a solver effect-row variable.
#[derive(Debug, Clone)]
enum EffState {
    /// Unbound.
    Free,
    /// Bound to extra atoms plus a further tail.
    Bound(SolveEffect),
}

/// A solver-level type: like [`Ty`] but variables are solver ids and there is no
/// `Arc` sharing requirement (it is transient).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveTy {
    /// A solver variable.
    Var(TyVarId),
    /// A nullary constructor.
    Con(Con),
    /// A user-declared nominal type constructor (applied via [`SolveTy::App`]).
    Adt(AdtRef),
    /// A nominal interface type (applied via [`SolveTy::App`] for parameters).
    Interface(InterfaceRef),
    /// Application. Children are `Rc`-shared so resolving/cloning a representative
    /// is O(1) (the deep clone otherwise dominates unification of large types).
    App(Rc<SolveTy>, Rc<SolveTy>),
    /// Function type `from -> to / effect`. Children are `Rc`-shared (see
    /// [`SolveTy::App`]); the effect row records the capabilities applying it uses.
    Arrow(Rc<SolveTy>, Rc<SolveTy>, SolveEffect),
    /// Tuple type.
    Tuple(Vec<SolveTy>),
    /// A structural record.
    Record(SolveRow),
    /// Unit.
    Unit,
    /// Error (unifies with anything).
    Error,
}

impl SolveTy {
    /// A `Bool`.
    pub fn bool() -> SolveTy {
        SolveTy::Con(Con::Bool)
    }
    /// An `Int`.
    pub fn int() -> SolveTy {
        SolveTy::Con(Con::Int)
    }
    /// A `String`.
    pub fn string() -> SolveTy {
        SolveTy::Con(Con::String)
    }
    /// A pure function `from -> to` (empty effect).
    pub fn arrow(from: SolveTy, to: SolveTy) -> SolveTy {
        SolveTy::Arrow(Rc::new(from), Rc::new(to), SolveEffect::pure())
    }

    /// A function `from -> to / effect`.
    pub fn arrow_eff(from: SolveTy, to: SolveTy, effect: SolveEffect) -> SolveTy {
        SolveTy::Arrow(Rc::new(from), Rc::new(to), effect)
    }
    /// A `List t`.
    pub fn list(elem: SolveTy) -> SolveTy {
        SolveTy::App(Rc::new(SolveTy::Con(Con::List)), Rc::new(elem))
    }

    /// A nominal ADT head applied to `args` (e.g. `Option a`).
    pub fn adt(adt: AdtRef, args: Vec<SolveTy>) -> SolveTy {
        let mut ty = SolveTy::Adt(adt);
        for a in args {
            ty = SolveTy::App(Rc::new(ty), Rc::new(a));
        }
        ty
    }

    /// Builds a curried arrow `p0 -> p1 -> ... -> result`.
    pub fn arrows_solver(params: Vec<SolveTy>, result: SolveTy) -> SolveTy {
        let mut ty = result;
        for p in params.into_iter().rev() {
            ty = SolveTy::arrow(p, ty);
        }
        ty
    }
}

/// The outcome of attempting to unify two types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifyResult {
    /// Unification succeeded.
    Ok,
    /// A constructor/shape mismatch (report as a type mismatch).
    Mismatch,
    /// The occurs check failed (an infinite type).
    Occurs,
    /// A constrained variable was unified with a type it does not admit.
    BadConstraint,
}

/// The representative a variable chain resolves to (see [`InferCtx::repr`]).
enum Repr<'a> {
    /// A free representative variable.
    Free(TyVarId),
    /// A bound representative variable and the structure it is bound to.
    Bound(TyVarId, &'a SolveTy),
}

/// The mutable inference solver.
pub struct InferCtx {
    vars: Vec<VarState>,
    rows: Vec<RowState>,
    /// The parallel union-find for effect-row variables (distinct from `rows`).
    effects: Vec<EffState>,
    /// When set, effect-row unification never *fails*: a closed-vs-closed atom
    /// difference is accepted rather than reported. This is the transitional
    /// "infer-don't-enforce" mode — effects are inferred (and threaded through
    /// open tails) but a declared-vs-body effect disagreement is not an error
    /// yet, so existing programs need no effect annotations. Enforcement flips
    /// this off.
    lenient_effects: bool,
    /// The current binding depth, bumped around a generalizable `let` right-hand
    /// side. A variable generalizes only if its level exceeds the enclosing one.
    current_level: u32,
}

impl Default for InferCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl InferCtx {
    /// Creates an empty context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vars: Vec::new(),
            rows: Vec::new(),
            effects: Vec::new(),
            lenient_effects: true,
            current_level: 0,
        }
    }

    /// Enters a deeper binding level (around a generalizable `let` right-hand
    /// side); variables created here generalize unless unified with an outer one.
    pub fn enter_level(&mut self) {
        self.current_level += 1;
    }

    /// Leaves the current binding level.
    pub fn exit_level(&mut self) {
        self.current_level -= 1;
    }

    /// The current binding level.
    #[must_use]
    pub fn current_level(&self) -> u32 {
        self.current_level
    }

    /// The binding level of a (free) variable. A bound variable reports level 0
    /// (it is never queried for generalization — free representatives are).
    pub(crate) fn level_of(&self, v: TyVarId) -> u32 {
        match &self.vars[v.0 as usize] {
            VarState::Free(_, level) => *level,
            VarState::Bound(_) => 0,
        }
    }

    /// Lowers a free variable's level toward `to` (never raises it).
    fn lower_level(&mut self, v: TyVarId, to: u32) {
        if let VarState::Free(_, level) = &mut self.vars[v.0 as usize]
            && *level > to
        {
            *level = to;
        }
    }

    /// Allocates a fresh row variable forbidden from containing `lacks`.
    pub fn fresh_row(&mut self, lacks: Vec<Symbol>) -> RowVarId {
        let id = RowVarId(u32::try_from(self.rows.len()).expect("row var overflow"));
        self.rows.push(RowState::Free(lacks));
        id
    }

    /// A fresh open record `{ fields | ρ }` with a fresh tail variable.
    pub fn fresh_open_record(&mut self, fields: Vec<(Symbol, SolveTy)>) -> SolveTy {
        let labels: Vec<Symbol> = fields.iter().map(|(l, _)| *l).collect();
        let tail = self.fresh_row(labels);
        SolveTy::Record(SolveRow { fields, tail: RowTail::Open(tail) })
    }

    /// Flattens a row, following bound tail variables and merging their fields.
    /// The result's tail is `Closed` or `Open` of a *free* row variable.
    fn expand_row(&self, row: &SolveRow) -> SolveRow {
        let mut fields = row.fields.clone();
        let mut tail = row.tail.clone();
        while let RowTail::Open(v) = tail {
            match &self.rows[v.0 as usize] {
                RowState::Bound(more) => {
                    fields.extend(more.fields.iter().cloned());
                    tail = more.tail.clone();
                }
                RowState::Free(_) => break,
            }
        }
        SolveRow { fields, tail }
    }

    fn row_lacks(&self, v: RowVarId) -> Vec<Symbol> {
        match &self.rows[v.0 as usize] {
            RowState::Free(l) => l.clone(),
            RowState::Bound(_) => Vec::new(),
        }
    }

    /// Binds a free row variable to `row`, checking the lacks constraint.
    fn bind_row(&mut self, v: RowVarId, row: SolveRow) -> UnifyResult {
        let lacks = self.row_lacks(v);
        for (label, _) in &row.fields {
            if lacks.contains(label) {
                return UnifyResult::Mismatch; // a duplicate label
            }
        }
        // Carry the lacks set onto the new tail (it inherits the forbidden labels
        // plus the ones just added).
        if let RowTail::Open(next) = row.tail
            && let RowState::Free(next_lacks) = &mut self.rows[next.0 as usize]
        {
            for l in &lacks {
                if !next_lacks.contains(l) {
                    next_lacks.push(*l);
                }
            }
            for (l, _) in &row.fields {
                if !next_lacks.contains(l) {
                    next_lacks.push(*l);
                }
            }
        }
        self.rows[v.0 as usize] = RowState::Bound(row);
        UnifyResult::Ok
    }

    /// Unifies two records by row unification.
    fn unify_rows(&mut self, r1: &SolveRow, r2: &SolveRow) -> UnifyResult {
        let r1 = self.expand_row(r1);
        let r2 = self.expand_row(r2);

        // Unify the types of common fields.
        for (label, t1) in &r1.fields {
            if let Some((_, t2)) = r2.fields.iter().find(|(l, _)| l == label) {
                match self.unify(t1, t2) {
                    UnifyResult::Ok => {}
                    other => return other,
                }
            }
        }
        let only1: Vec<(Symbol, SolveTy)> = r1
            .fields
            .iter()
            .filter(|(l, _)| !r2.fields.iter().any(|(m, _)| m == l))
            .cloned()
            .collect();
        let only2: Vec<(Symbol, SolveTy)> = r2
            .fields
            .iter()
            .filter(|(l, _)| !r1.fields.iter().any(|(m, _)| m == l))
            .cloned()
            .collect();

        match (r1.tail, r2.tail) {
            (RowTail::Closed, RowTail::Closed) => {
                if only1.is_empty() && only2.is_empty() {
                    UnifyResult::Ok
                } else {
                    UnifyResult::Mismatch
                }
            }
            (RowTail::Closed, RowTail::Open(v2)) => {
                if !only2.is_empty() {
                    return UnifyResult::Mismatch;
                }
                self.bind_row(v2, SolveRow { fields: only1, tail: RowTail::Closed })
            }
            (RowTail::Open(v1), RowTail::Closed) => {
                if !only1.is_empty() {
                    return UnifyResult::Mismatch;
                }
                self.bind_row(v1, SolveRow { fields: only2, tail: RowTail::Closed })
            }
            (RowTail::Open(v1), RowTail::Open(v2)) => {
                if v1 == v2 {
                    return if only1.is_empty() && only2.is_empty() {
                        UnifyResult::Ok
                    } else {
                        UnifyResult::Mismatch
                    };
                }
                let mut lacks: Vec<Symbol> = self.row_lacks(v1);
                for l in self.row_lacks(v2) {
                    if !lacks.contains(&l) {
                        lacks.push(l);
                    }
                }
                let fresh = self.fresh_row(lacks);
                match self.bind_row(v1, SolveRow { fields: only2, tail: RowTail::Open(fresh) }) {
                    UnifyResult::Ok => {}
                    other => return other,
                }
                self.bind_row(v2, SolveRow { fields: only1, tail: RowTail::Open(fresh) })
            }
        }
    }

    /// Allocates a fresh effect-row variable.
    pub fn fresh_effect(&mut self) -> EffRowVarId {
        let id = EffRowVarId(u32::try_from(self.effects.len()).expect("effect var overflow"));
        self.effects.push(EffState::Free);
        id
    }

    /// A fresh open effect `{ | ε }` with a fresh tail variable (no atoms).
    pub fn fresh_open_effect(&mut self) -> SolveEffect {
        let tail = self.fresh_effect();
        SolveEffect { atoms: Vec::new(), tail: EffTail::Open(tail) }
    }

    /// Flattens an effect row, following bound tail variables and merging atoms
    /// (deduplicated). The result's tail is `Closed` or `Open` of a *free* var.
    fn expand_effect(&self, eff: &SolveEffect) -> SolveEffect {
        let mut atoms = eff.atoms.clone();
        let mut tail = eff.tail.clone();
        while let EffTail::Open(v) = tail {
            match &self.effects[v.0 as usize] {
                EffState::Bound(more) => {
                    atoms.extend(more.atoms.iter().copied());
                    tail = more.tail.clone();
                }
                EffState::Free => break,
            }
        }
        dedup_atoms(&mut atoms);
        SolveEffect { atoms, tail }
    }

    /// Binds a free effect-row variable to `eff`.
    fn bind_effect(&mut self, v: EffRowVarId, eff: SolveEffect) -> UnifyResult {
        self.effects[v.0 as usize] = EffState::Bound(eff);
        UnifyResult::Ok
    }

    /// Unifies two effect rows by row unification (atoms as a set; no payloads).
    ///
    /// Phase note: this is plain unification. Use-site *subsumption* (the bipolar
    /// effect bounds) layers on top of this base when effect inference lands; until
    /// then every arrow carries the pure effect, so this only ever sees `{} ~ {}`.
    fn unify_effects(&mut self, e1: &SolveEffect, e2: &SolveEffect) -> UnifyResult {
        let e1 = self.expand_effect(e1);
        let e2 = self.expand_effect(e2);

        let only1: Vec<InterfaceRef> =
            e1.atoms.iter().filter(|a| !e2.atoms.contains(a)).copied().collect();
        let only2: Vec<InterfaceRef> =
            e2.atoms.iter().filter(|a| !e1.atoms.contains(a)).copied().collect();

        // In lenient mode an irreconcilable atom difference is accepted rather
        // than reported (infer-don't-enforce); open tails are still bound so
        // effect-polymorphic forwarding keeps threading.
        let lenient = self.lenient_effects;
        match (e1.tail, e2.tail) {
            (EffTail::Closed, EffTail::Closed) => {
                if only1.is_empty() && only2.is_empty() || lenient {
                    UnifyResult::Ok
                } else {
                    UnifyResult::Mismatch
                }
            }
            (EffTail::Closed, EffTail::Open(v2)) => {
                if !only2.is_empty() && !lenient {
                    return UnifyResult::Mismatch;
                }
                self.bind_effect(v2, SolveEffect { atoms: only1, tail: EffTail::Closed })
            }
            (EffTail::Open(v1), EffTail::Closed) => {
                if !only1.is_empty() && !lenient {
                    return UnifyResult::Mismatch;
                }
                self.bind_effect(v1, SolveEffect { atoms: only2, tail: EffTail::Closed })
            }
            (EffTail::Open(v1), EffTail::Open(v2)) => {
                if v1 == v2 {
                    return if only1.is_empty() && only2.is_empty() {
                        UnifyResult::Ok
                    } else {
                        UnifyResult::Mismatch
                    };
                }
                let fresh = self.fresh_effect();
                match self.bind_effect(v1, SolveEffect { atoms: only2, tail: EffTail::Open(fresh) })
                {
                    UnifyResult::Ok => {}
                    other => return other,
                }
                self.bind_effect(v2, SolveEffect { atoms: only1, tail: EffTail::Open(fresh) })
            }
        }
    }

    /// The union of two effect rows: all atoms of both, with the tails merged.
    /// Two distinct open tails are forced equal (bound to one fresh tail) — the
    /// conservative merge that keeps inference principal for the single-tail row
    /// representation; use-site subsumption refines this later.
    pub fn union_effects(&mut self, a: &SolveEffect, b: &SolveEffect) -> SolveEffect {
        let a = self.expand_effect(a);
        let b = self.expand_effect(b);
        let mut atoms = a.atoms;
        atoms.extend(b.atoms);
        dedup_atoms(&mut atoms);
        let tail = match (a.tail, b.tail) {
            (EffTail::Closed, t) | (t, EffTail::Closed) => t,
            (EffTail::Open(v1), EffTail::Open(v2)) if v1 == v2 => EffTail::Open(v1),
            (EffTail::Open(v1), EffTail::Open(v2)) => {
                let fresh = self.fresh_effect();
                self.bind_effect(v1, SolveEffect { atoms: Vec::new(), tail: EffTail::Open(fresh) });
                self.bind_effect(v2, SolveEffect { atoms: Vec::new(), tail: EffTail::Open(fresh) });
                EffTail::Open(fresh)
            }
        };
        SolveEffect { atoms, tail }
    }

    /// Reifies a solver effect row into an immutable [`EffectRow`] standalone (a
    /// fresh renumbering), for exposing a body's inferred effect.
    #[must_use]
    pub fn reify_effect_standalone(&self, eff: &SolveEffect) -> EffectRow {
        let mut renumber = Renumber::default();
        self.reify_effect(eff, &mut renumber)
    }

    /// Allocates a fresh, unconstrained variable.
    pub fn fresh(&mut self) -> SolveTy {
        self.fresh_constrained(None)
    }

    /// Allocates a fresh variable with an optional constraint, at the current
    /// binding level.
    pub fn fresh_constrained(&mut self, c: Option<Constraint>) -> SolveTy {
        let id = TyVarId(u32::try_from(self.vars.len()).expect("type var overflow"));
        self.vars.push(VarState::Free(c, self.current_level));
        SolveTy::Var(id)
    }

    /// Follows bound variables to the representative shallow form.
    pub fn resolve_shallow(&self, ty: &SolveTy) -> SolveTy {
        crate::perf::bump_resolve_clone();
        let mut cur = ty.clone();
        while let SolveTy::Var(id) = cur {
            match &self.vars[id.0 as usize] {
                VarState::Bound(t) => {
                    crate::perf::bump_resolve_clone();
                    cur = t.clone();
                }
                VarState::Free(..) => break,
            }
        }
        cur
    }

    /// Follows a variable chain to its representative *without cloning*: either a
    /// free variable, or a bound representative variable paired with the
    /// structure it points at (borrowed from the solver). The read-only walks
    /// (`occurs`, free-variable collection) use this to avoid the per-node clone
    /// that [`resolve_shallow`](InferCtx::resolve_shallow) makes, and to recover
    /// the representative variable so a shared (DAG) subterm is walked once.
    fn repr(&self, mut v: TyVarId) -> Repr<'_> {
        loop {
            match &self.vars[v.0 as usize] {
                VarState::Bound(SolveTy::Var(next)) => v = *next,
                VarState::Bound(t) => return Repr::Bound(v, t),
                VarState::Free(..) => return Repr::Free(v),
            }
        }
    }

    /// Like [`resolve_shallow`](InferCtx::resolve_shallow), but compresses the
    /// variable→variable chain it walks so later resolutions are O(1). Only
    /// variable links are rewritten (never structures), so no large value is
    /// duplicated. Used on the hot `&mut` solving path (unification), where long
    /// result-variable chains otherwise make repeated resolution quadratic.
    fn resolve_compress(&mut self, ty: &SolveTy) -> SolveTy {
        let SolveTy::Var(v0) = ty else {
            crate::perf::bump_resolve_clone();
            return ty.clone();
        };
        // Follow variable→variable links to the chain's last variable.
        let mut v = *v0;
        let mut chain: Vec<TyVarId> = Vec::new();
        while let VarState::Bound(SolveTy::Var(next)) = &self.vars[v.0 as usize] {
            chain.push(v);
            v = *next;
        }
        // Point every earlier link straight at that last variable.
        for link in chain {
            if link != v {
                self.vars[link.0 as usize] = VarState::Bound(SolveTy::Var(v));
            }
        }
        crate::perf::bump_resolve_clone();
        match &self.vars[v.0 as usize] {
            VarState::Bound(t) => t.clone(),
            VarState::Free(..) => SolveTy::Var(v),
        }
    }

    fn constraint_of(&self, id: TyVarId) -> Option<Constraint> {
        match &self.vars[id.0 as usize] {
            VarState::Free(c, _) => *c,
            VarState::Bound(_) => None,
        }
    }

    /// Collects the free (unbound) representative variables reachable from `ty`,
    /// following the substitution by borrowing (no clone). `visited` records the
    /// bound representatives already walked, so a variable shared across `ty` (a
    /// DAG, e.g. `(p, p)` repeated) is expanded only once.
    pub(crate) fn collect_free_vars(
        &self,
        ty: &SolveTy,
        out: &mut rustc_hash::FxHashSet<TyVarId>,
        visited: &mut rustc_hash::FxHashSet<TyVarId>,
    ) {
        crate::perf::bump_free_var_visit();
        match ty {
            SolveTy::Var(v0) => match self.repr(*v0) {
                Repr::Free(v) => {
                    out.insert(v);
                }
                Repr::Bound(v, t) => {
                    if visited.insert(v) {
                        self.collect_free_vars(t, out, visited);
                    }
                }
            },
            SolveTy::App(f, a) => {
                self.collect_free_vars(f, out, visited);
                self.collect_free_vars(a, out, visited);
            }
            // An effect row carries no *type* variables (its atoms are interface
            // refs, its tail an effect-row var), so only the operands are walked.
            SolveTy::Arrow(f, a, _) => {
                self.collect_free_vars(f, out, visited);
                self.collect_free_vars(a, out, visited);
            }
            SolveTy::Tuple(elems) => {
                for e in elems {
                    self.collect_free_vars(e, out, visited);
                }
            }
            // The immediate fields only (a bound row tail is not expanded here):
            // generalization quantifies the type variables it can see, matching
            // the record's principal-type fields.
            SolveTy::Record(row) => {
                for (_, t) in &row.fields {
                    self.collect_free_vars(t, out, visited);
                }
            }
            SolveTy::Con(_)
            | SolveTy::Adt(_)
            | SolveTy::Interface(_)
            | SolveTy::Unit
            | SolveTy::Error => {}
        }
    }

    /// Unifies two types, applying bindings. Constraint checks run as variables
    /// are bound.
    pub fn unify(&mut self, a: &SolveTy, b: &SolveTy) -> UnifyResult {
        let a = self.resolve_compress(a);
        let b = self.resolve_compress(b);
        match (&a, &b) {
            (SolveTy::Error, _) | (_, SolveTy::Error) => UnifyResult::Ok,
            (SolveTy::Var(x), SolveTy::Var(y)) if x == y => UnifyResult::Ok,
            (SolveTy::Var(x), _) => self.bind(*x, &b),
            (_, SolveTy::Var(y)) => self.bind(*y, &a),
            (SolveTy::Con(x), SolveTy::Con(y)) if x == y => UnifyResult::Ok,
            (SolveTy::Adt(x), SolveTy::Adt(y)) if x == y => UnifyResult::Ok,
            (SolveTy::Interface(x), SolveTy::Interface(y)) if x == y => UnifyResult::Ok,
            (SolveTy::Unit, SolveTy::Unit) => UnifyResult::Ok,
            (SolveTy::App(f1, a1), SolveTy::App(f2, a2)) => match self.unify(f1, f2) {
                UnifyResult::Ok => self.unify(a1, a2),
                other => other,
            },
            (SolveTy::Arrow(f1, t1, e1), SolveTy::Arrow(f2, t2, e2)) => {
                let (e1, e2) = (e1.clone(), e2.clone());
                match self.unify(f1, f2) {
                    UnifyResult::Ok => match self.unify(t1, t2) {
                        UnifyResult::Ok => self.unify_effects(&e1, &e2),
                        other => other,
                    },
                    other => other,
                }
            }
            (SolveTy::Tuple(xs), SolveTy::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    match self.unify(x, y) {
                        UnifyResult::Ok => {}
                        other => return other,
                    }
                }
                UnifyResult::Ok
            }
            (SolveTy::Record(r1), SolveTy::Record(r2)) => {
                let (r1, r2) = (r1.clone(), r2.clone());
                self.unify_rows(&r1, &r2)
            }
            _ => UnifyResult::Mismatch,
        }
    }

    /// Binds variable `id` to `ty`, running the occurs check, lowering the levels
    /// of `ty`'s variables to `id`'s (so a variable unified with an outer one is
    /// not generalized), and checking constraints.
    fn bind(&mut self, id: TyVarId, ty: &SolveTy) -> UnifyResult {
        let lower_to = self.level_of(id);
        let mut visited = rustc_hash::FxHashSet::default();
        if self.occurs_and_lower(id, lower_to, ty, &mut visited) {
            return UnifyResult::Occurs;
        }
        if let Some(c) = self.constraint_of(id) {
            match self.resolve_shallow(ty) {
                // Binding to another variable: carry the constraint to it so it
                // survives (e.g. a Numeric operand unified with an unconstrained
                // result still defaults to Int later).
                SolveTy::Var(other) if other != id => self.merge_constraint(other, c),
                SolveTy::Var(_) => {}
                // Binding to a concrete type: it must satisfy the constraint.
                other if !self.satisfies(c, &other) => return UnifyResult::BadConstraint,
                _ => {}
            }
        }
        self.vars[id.0 as usize] = VarState::Bound(ty.clone());
        UnifyResult::Ok
    }

    fn merge_constraint(&mut self, id: TyVarId, c: Constraint) {
        if let VarState::Free(existing, _) = &mut self.vars[id.0 as usize] {
            *existing = Some(stronger_constraint(*existing, c));
        }
    }

    /// Whether a *resolved* type satisfies a constraint. Variables and Error are
    /// treated as satisfying (deferred / suppressed).
    fn satisfies(&self, c: Constraint, ty: &SolveTy) -> bool {
        let ty = self.resolve_shallow(ty);
        match c {
            Constraint::Numeric => matches!(
                ty,
                SolveTy::Var(_)
                    | SolveTy::Error
                    | SolveTy::Con(Con::Int)
                    | SolveTy::Con(Con::Float)
            ),
            // Ordering and equality are structural (`fai_compare`/`fai_equal`),
            // admitting any type that does not (transitively) contain a function
            // or interface. A still-free variable is deferred (treated as
            // satisfying); a concrete function-bearing aggregate is rejected here.
            Constraint::Ord | Constraint::Eq => self.is_comparable(&ty),
        }
    }

    /// Whether a (resolved) type is structurally comparable: no function or
    /// interface anywhere in it. Free variables and `Error` are deferred (`true`).
    fn is_comparable(&self, ty: &SolveTy) -> bool {
        match self.resolve_shallow(ty) {
            SolveTy::Arrow(..) | SolveTy::Interface(_) => false,
            SolveTy::Var(_) | SolveTy::Error => true,
            SolveTy::Con(_) | SolveTy::Adt(_) | SolveTy::Unit => true,
            SolveTy::App(f, a) => self.is_comparable(&f) && self.is_comparable(&a),
            SolveTy::Tuple(elems) => elems.iter().all(|e| self.is_comparable(e)),
            SolveTy::Record(row) => {
                let row = self.expand_row(&row);
                row.fields.iter().all(|(_, t)| self.is_comparable(t))
            }
        }
    }

    /// Follows a variable chain to its representative, returning the
    /// representative id and (if bound) an *owned* shallow clone of the structure
    /// it points at. The clone releases the borrow on the solver so a `&mut self`
    /// walk can recurse; with `Rc`-shared children it is O(1) for the common
    /// arrow/application case.
    fn repr_owned(&self, mut v: TyVarId) -> (TyVarId, Option<SolveTy>) {
        loop {
            match &self.vars[v.0 as usize] {
                VarState::Bound(SolveTy::Var(next)) => v = *next,
                VarState::Bound(t) => return (v, Some(t.clone())),
                VarState::Free(..) => return (v, None),
            }
        }
    }

    /// The occurs check fused with level lowering: walks `ty`, lowering every free
    /// variable's level toward `lower_to`, and returns whether `target` occurs.
    /// Bound representatives are memoized in `visited` (a shared DAG subterm is
    /// expanded once) and syntactically variable-free subtrees are skipped (they
    /// have nothing to find or lower).
    fn occurs_and_lower(
        &mut self,
        target: TyVarId,
        lower_to: u32,
        ty: &SolveTy,
        visited: &mut rustc_hash::FxHashSet<TyVarId>,
    ) -> bool {
        crate::perf::bump_occurs_visit();
        match ty {
            SolveTy::Var(v0) => {
                let (v, bound) = self.repr_owned(*v0);
                match bound {
                    None => {
                        self.lower_level(v, lower_to);
                        v == target
                    }
                    Some(t) => {
                        // `target` is the free variable being bound, so it never
                        // equals a bound representative; the guard is defensive.
                        if v == target {
                            return true;
                        }
                        if !visited.insert(v) {
                            return false;
                        }
                        self.occurs_and_lower(target, lower_to, &t, visited)
                    }
                }
            }
            SolveTy::App(f, a) => {
                self.occurs_and_lower(target, lower_to, f, visited)
                    || self.occurs_and_lower(target, lower_to, a, visited)
            }
            SolveTy::Arrow(f, a, _) => {
                self.occurs_and_lower(target, lower_to, f, visited)
                    || self.occurs_and_lower(target, lower_to, a, visited)
            }
            SolveTy::Tuple(elems) => {
                elems.iter().any(|e| self.occurs_and_lower(target, lower_to, e, visited))
            }
            SolveTy::Record(row) => {
                let row = self.expand_row(row);
                row.fields.iter().any(|(_, t)| self.occurs_and_lower(target, lower_to, t, visited))
            }
            SolveTy::Con(_)
            | SolveTy::Adt(_)
            | SolveTy::Interface(_)
            | SolveTy::Unit
            | SolveTy::Error => false,
        }
    }

    /// The constraint currently attached to `ty` if it resolves to a free
    /// variable, else `None`.
    pub fn pending_constraint(&self, ty: &SolveTy) -> Option<Constraint> {
        if let SolveTy::Var(id) = self.resolve_shallow(ty) { self.constraint_of(id) } else { None }
    }

    /// Defaults a still-free Numeric variable to `Int`. Returns whether it did.
    pub fn default_numeric(&mut self, ty: &SolveTy) -> bool {
        if let SolveTy::Var(id) = self.resolve_shallow(ty)
            && self.constraint_of(id) == Some(Constraint::Numeric)
        {
            self.vars[id.0 as usize] = VarState::Bound(SolveTy::int());
            return true;
        }
        false
    }

    /// Recursively defaults every still-free Numeric variable reachable from `ty`
    /// to `Int` (the structural version of [`default_numeric`]).
    pub fn default_numerics_deep(&mut self, ty: &SolveTy) {
        match self.resolve_shallow(ty) {
            SolveTy::Var(_) => {
                self.default_numeric(ty);
            }
            SolveTy::App(f, a) => {
                self.default_numerics_deep(&f);
                self.default_numerics_deep(&a);
            }
            SolveTy::Arrow(f, a, _) => {
                self.default_numerics_deep(&f);
                self.default_numerics_deep(&a);
            }
            SolveTy::Tuple(elems) => {
                for e in &elems {
                    self.default_numerics_deep(e);
                }
            }
            SolveTy::Record(row) => {
                let row = self.expand_row(&row);
                for (_, t) in &row.fields {
                    self.default_numerics_deep(t);
                }
            }
            SolveTy::Con(_)
            | SolveTy::Adt(_)
            | SolveTy::Interface(_)
            | SolveTy::Unit
            | SolveTy::Error => {}
        }
    }

    /// Reifies a solver type into an immutable [`Ty`], renumbering the remaining
    /// free variables compactly starting at 0 (so schemes are canonical).
    pub fn reify(&self, ty: &SolveTy) -> Ty {
        let mut renumber = Renumber::default();
        self.reify_inner(ty, &mut renumber)
    }

    /// Reifies into a [`Ty`] and reports the free type and row variables it
    /// contains (for generalization), each renumbered compactly.
    pub fn reify_with_vars(
        &self,
        ty: &SolveTy,
    ) -> (Ty, Vec<TyVarId>, Vec<RowVarId>, Vec<EffRowVarId>) {
        let mut renumber = Renumber::default();
        let reified = self.reify_inner(ty, &mut renumber);
        (reified, renumber.order, renumber.row_order, renumber.eff_order)
    }

    /// Reifies several solver types against a *shared* renumbering, so a variable
    /// shared between them gets the same id (and hence the same display name) in
    /// each. First-appearance order across the whole slice determines the ids.
    pub fn reify_many(&self, tys: &[SolveTy]) -> Vec<Ty> {
        let mut renumber = Renumber::default();
        tys.iter().map(|ty| self.reify_inner(ty, &mut renumber)).collect()
    }

    fn reify_inner(&self, ty: &SolveTy, renumber: &mut Renumber) -> Ty {
        match self.resolve_shallow(ty) {
            SolveTy::Var(id) => Ty::Var(renumber.map(id)),
            SolveTy::Con(c) => Ty::Con(c),
            SolveTy::Adt(adt) => Ty::Adt(adt),
            SolveTy::Interface(i) => Ty::Interface(i),
            SolveTy::Unit => Ty::Unit,
            SolveTy::Error => Ty::Error,
            SolveTy::App(f, a) => Ty::App(
                std::sync::Arc::new(self.reify_inner(&f, renumber)),
                std::sync::Arc::new(self.reify_inner(&a, renumber)),
            ),
            SolveTy::Arrow(f, t, e) => Ty::arrow_eff(
                self.reify_inner(&f, renumber),
                self.reify_inner(&t, renumber),
                self.reify_effect(&e, renumber),
            ),
            SolveTy::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|e| self.reify_inner(e, renumber)).collect())
            }
            SolveTy::Record(row) => {
                let row = self.expand_row(&row);
                let mut fields: Vec<(Symbol, Ty)> =
                    row.fields.iter().map(|(l, t)| (*l, self.reify_inner(t, renumber))).collect();
                fields.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                let tail = match row.tail {
                    RowTail::Closed => RowEnd::Closed,
                    RowTail::Open(v) => RowEnd::Open(renumber.map_row(v)),
                };
                Ty::Record(RecordRow { fields, tail })
            }
        }
    }

    /// Reifies a solver effect row into an immutable [`EffectRow`], its atoms
    /// sorted by qualified name (canonical) and its free tail var renumbered.
    fn reify_effect(&self, eff: &SolveEffect, renumber: &mut Renumber) -> EffectRow {
        let eff = self.expand_effect(eff);
        let mut labels = eff.atoms;
        labels.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        labels.dedup();
        let tail = match eff.tail {
            EffTail::Closed => EffEnd::Closed,
            EffTail::Open(v) => EffEnd::Open(renumber.map_eff(v)),
        };
        EffectRow { labels, tail }
    }

    /// A fresh solver variable's id.
    pub fn fresh_var_id(&mut self) -> TyVarId {
        match self.fresh() {
            SolveTy::Var(id) => id,
            _ => unreachable!("fresh() always yields a Var"),
        }
    }

    /// Instantiates a scheme, binding its first `prefix.len()` quantified
    /// variables to the given solver variables (the rest get fresh ones). Used to
    /// share an interface's parameter variables across all of an instance's
    /// methods.
    pub fn instantiate_sharing(&mut self, scheme: &Scheme, prefix: &[TyVarId]) -> SolveTy {
        let mut map = InstMap::default();
        for (i, &v) in scheme.vars.iter().enumerate() {
            let id = if i < prefix.len() { prefix[i] } else { self.fresh_var_id() };
            map.types.insert(v, id);
        }
        self.instantiate_solve(&scheme.ty, &mut map)
    }

    /// Instantiates a scheme with fresh variables (no constraints recorded; M2
    /// schemes carry no constraints).
    pub fn instantiate(&mut self, scheme: &Scheme) -> SolveTy {
        self.instantiate_tracked(scheme).0
    }

    /// Whether none of `rows` — a signature's quantified row variables — gained a
    /// field while checking the body. A signature row variable forced to contain
    /// a field promises less than the body needs (the body would read a field the
    /// caller is not required to provide), so the signature is too general.
    #[must_use]
    pub fn rows_gained_no_fields(&self, rows: &[RowVarId]) -> bool {
        rows.iter().all(|&v| {
            self.expand_row(&SolveRow { fields: Vec::new(), tail: RowTail::Open(v) })
                .fields
                .is_empty()
        })
    }

    /// Like [`instantiate`](InferCtx::instantiate), but also returns the fresh
    /// variable id introduced for each of the scheme's quantified type *and* row
    /// variables. Used to check a signature is not *more general* than the body:
    /// if a fresh type var ends up bound to a concrete type or shared with
    /// another, or a fresh row var gains a field, the signature over-generalized.
    pub fn instantiate_tracked(
        &mut self,
        scheme: &Scheme,
    ) -> (SolveTy, Vec<TyVarId>, Vec<RowVarId>) {
        let mut map = InstMap::default();
        let mut fresh_vars = Vec::with_capacity(scheme.vars.len());
        for &v in &scheme.vars {
            if let SolveTy::Var(id) = self.fresh() {
                map.types.insert(v, id);
                fresh_vars.push(id);
            }
        }
        let solved = self.instantiate_solve(&scheme.ty, &mut map);
        let fresh_rows = scheme.row_vars.iter().filter_map(|v| map.rows.get(v).copied()).collect();
        (solved, fresh_vars, fresh_rows)
    }

    /// Builds a solver type from a scheme body, mapping quantified type variables
    /// via `map` and lazily creating fresh row variables (each forbidden from
    /// duplicating the labels already present in its record).
    fn instantiate_solve(&mut self, ty: &Ty, map: &mut InstMap) -> SolveTy {
        match ty {
            Ty::Var(v) => SolveTy::Var(*map.types.get(v).unwrap_or(v)),
            Ty::Con(c) => SolveTy::Con(*c),
            Ty::Adt(adt) => SolveTy::Adt(*adt),
            Ty::Interface(i) => SolveTy::Interface(*i),
            Ty::Unit => SolveTy::Unit,
            Ty::Error => SolveTy::Error,
            Ty::App(f, a) => SolveTy::App(
                Rc::new(self.instantiate_solve(f, map)),
                Rc::new(self.instantiate_solve(a, map)),
            ),
            Ty::Arrow(f, t, e) => {
                let from = self.instantiate_solve(f, map);
                let to = self.instantiate_solve(t, map);
                let effect = self.instantiate_effect(e, map);
                SolveTy::arrow_eff(from, to, effect)
            }
            Ty::Tuple(elems) => {
                SolveTy::Tuple(elems.iter().map(|e| self.instantiate_solve(e, map)).collect())
            }
            Ty::Record(row) => {
                let labels: Vec<Symbol> = row.fields.iter().map(|(l, _)| *l).collect();
                let fields: Vec<(Symbol, SolveTy)> =
                    row.fields.iter().map(|(l, t)| (*l, self.instantiate_solve(t, map))).collect();
                let tail = match row.tail {
                    RowEnd::Closed => RowTail::Closed,
                    RowEnd::Open(v) => {
                        let fresh = match map.rows.get(&v) {
                            Some(f) => *f,
                            None => {
                                let f = self.fresh_row(labels);
                                map.rows.insert(v, f);
                                f
                            }
                        };
                        RowTail::Open(fresh)
                    }
                };
                SolveTy::Record(SolveRow { fields, tail })
            }
        }
    }

    /// Builds a solver effect row from a reified one, mapping a quantified effect
    /// tail var to a fresh solver effect var (shared via `map`).
    fn instantiate_effect(&mut self, eff: &EffectRow, map: &mut InstMap) -> SolveEffect {
        let tail = match eff.tail {
            EffEnd::Closed => EffTail::Closed,
            EffEnd::Open(v) => {
                let fresh = match map.effects.get(&v) {
                    Some(f) => *f,
                    None => {
                        let f = self.fresh_effect();
                        map.effects.insert(v, f);
                        f
                    }
                };
                EffTail::Open(fresh)
            }
        };
        SolveEffect { atoms: eff.labels.clone(), tail }
    }

    /// Whether each id in `vars` still resolves to a *distinct* free variable.
    /// If two collapse to the same var, or any resolves to a concrete type, the
    /// scheme they came from was more general than the unified type.
    #[must_use]
    pub fn all_distinct_free(&self, vars: &[TyVarId]) -> bool {
        let mut seen = rustc_hash::FxHashSet::default();
        for &v in vars {
            match self.resolve_shallow(&SolveTy::Var(v)) {
                SolveTy::Var(r) => {
                    if !seen.insert(r) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

/// Compact renumbering of free variables encountered during reification.
#[derive(Default)]
struct Renumber {
    map: rustc_hash::FxHashMap<TyVarId, TyVarId>,
    order: Vec<TyVarId>,
    row_map: rustc_hash::FxHashMap<RowVarId, RowVarId>,
    row_order: Vec<RowVarId>,
    eff_map: rustc_hash::FxHashMap<EffRowVarId, EffRowVarId>,
    eff_order: Vec<EffRowVarId>,
}

impl Renumber {
    fn map(&mut self, id: TyVarId) -> TyVarId {
        if let Some(m) = self.map.get(&id) {
            return *m;
        }
        let next = TyVarId(u32::try_from(self.order.len()).expect("var overflow"));
        self.map.insert(id, next);
        self.order.push(next);
        next
    }

    fn map_row(&mut self, id: RowVarId) -> RowVarId {
        if let Some(m) = self.row_map.get(&id) {
            return *m;
        }
        let next = RowVarId(u32::try_from(self.row_order.len()).expect("row var overflow"));
        self.row_map.insert(id, next);
        self.row_order.push(next);
        next
    }

    fn map_eff(&mut self, id: EffRowVarId) -> EffRowVarId {
        if let Some(m) = self.eff_map.get(&id) {
            return *m;
        }
        let next = EffRowVarId(u32::try_from(self.eff_order.len()).expect("effect var overflow"));
        self.eff_map.insert(id, next);
        self.eff_order.push(next);
        next
    }
}

/// The mappings applied when instantiating a scheme's quantified variables.
#[derive(Default)]
struct InstMap {
    types: rustc_hash::FxHashMap<TyVarId, TyVarId>,
    rows: rustc_hash::FxHashMap<RowVarId, RowVarId>,
    effects: rustc_hash::FxHashMap<EffRowVarId, EffRowVarId>,
}

/// Removes duplicate effect atoms in place, preserving first-appearance order.
fn dedup_atoms(atoms: &mut Vec<InterfaceRef>) {
    let mut seen = rustc_hash::FxHashSet::default();
    atoms.retain(|a| seen.insert(*a));
}

/// Picks the stronger of two constraints when a variable accrues both. Ord
/// implies Eq-comparable; Numeric and Ord overlap on Int/Float. For M2 we keep
/// the most specific: Numeric < Ord (Ord is broader) < Eq (broadest). When in
/// doubt, keep the existing one.
fn stronger_constraint(existing: Option<Constraint>, new: Constraint) -> Constraint {
    match (existing, new) {
        (None, c) => c,
        (Some(Constraint::Numeric), _) | (_, Constraint::Numeric) => Constraint::Numeric,
        (Some(Constraint::Ord), _) | (_, Constraint::Ord) => Constraint::Ord,
        (Some(Constraint::Eq), Constraint::Eq) => Constraint::Eq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unifies_vars_and_cons() {
        let mut cx = InferCtx::new();
        let a = cx.fresh();
        assert_eq!(cx.unify(&a, &SolveTy::int()), UnifyResult::Ok);
        assert_eq!(cx.reify(&a), Ty::int());
    }

    #[test]
    fn occurs_check_fails() {
        let mut cx = InferCtx::new();
        let a = cx.fresh();
        let fa = SolveTy::arrow(a.clone(), SolveTy::int());
        assert_eq!(cx.unify(&a, &fa), UnifyResult::Occurs);
    }

    #[test]
    fn numeric_defaults_to_int() {
        let mut cx = InferCtx::new();
        let n = cx.fresh_constrained(Some(Constraint::Numeric));
        assert!(cx.default_numeric(&n));
        assert_eq!(cx.reify(&n), Ty::int());
    }

    #[test]
    fn numeric_rejects_bool() {
        let mut cx = InferCtx::new();
        let n = cx.fresh_constrained(Some(Constraint::Numeric));
        assert_eq!(cx.unify(&n, &SolveTy::bool()), UnifyResult::BadConstraint);
    }

    #[test]
    fn mismatch_reported() {
        let mut cx = InferCtx::new();
        assert_eq!(cx.unify(&SolveTy::int(), &SolveTy::bool()), UnifyResult::Mismatch);
    }

    #[test]
    fn error_unifies_with_anything() {
        let mut cx = InferCtx::new();
        assert_eq!(cx.unify(&SolveTy::Error, &SolveTy::int()), UnifyResult::Ok);
    }
}
