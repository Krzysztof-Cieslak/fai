//! The inference context: a mutable solver over type variables.
//!
//! Variables live in a union-find. Each variable carries an optional
//! [`Constraint`] (Numeric/Eq/Ord) tracked across unification. Solving produces a
//! substitution that [`InferCtx::reify`] applies to read back an immutable
//! [`Ty`]. The context is local to one inference call (one def or SCC); nothing
//! here is cached by salsa.

use crate::ty::{Con, Scheme, Ty, TyVarId};

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
    /// Unbound, possibly constrained.
    Free(Option<Constraint>),
    /// Bound to a (solver-level) type.
    Bound(SolveTy),
}

/// A solver-level type: like [`Ty`] but variables are solver ids and there is no
/// `Arc` sharing requirement (it is transient).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolveTy {
    /// A solver variable.
    Var(TyVarId),
    /// A nullary constructor.
    Con(Con),
    /// Application.
    App(Box<SolveTy>, Box<SolveTy>),
    /// Function type.
    Arrow(Box<SolveTy>, Box<SolveTy>),
    /// Tuple type.
    Tuple(Vec<SolveTy>),
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
    /// A function `from -> to`.
    pub fn arrow(from: SolveTy, to: SolveTy) -> SolveTy {
        SolveTy::Arrow(Box::new(from), Box::new(to))
    }
    /// A `List t`.
    pub fn list(elem: SolveTy) -> SolveTy {
        SolveTy::App(Box::new(SolveTy::Con(Con::List)), Box::new(elem))
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

/// The mutable inference solver.
pub struct InferCtx {
    vars: Vec<VarState>,
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
        Self { vars: Vec::new() }
    }

    /// Allocates a fresh, unconstrained variable.
    pub fn fresh(&mut self) -> SolveTy {
        self.fresh_constrained(None)
    }

    /// Allocates a fresh variable with an optional constraint.
    pub fn fresh_constrained(&mut self, c: Option<Constraint>) -> SolveTy {
        let id = TyVarId(u32::try_from(self.vars.len()).expect("type var overflow"));
        self.vars.push(VarState::Free(c));
        SolveTy::Var(id)
    }

    /// Follows bound variables to the representative shallow form.
    pub fn resolve_shallow(&self, ty: &SolveTy) -> SolveTy {
        let mut cur = ty.clone();
        while let SolveTy::Var(id) = cur {
            match &self.vars[id.0 as usize] {
                VarState::Bound(t) => cur = t.clone(),
                VarState::Free(_) => break,
            }
        }
        cur
    }

    fn constraint_of(&self, id: TyVarId) -> Option<Constraint> {
        match &self.vars[id.0 as usize] {
            VarState::Free(c) => *c,
            VarState::Bound(_) => None,
        }
    }

    /// Unifies two types, applying bindings. Constraint checks run as variables
    /// are bound.
    pub fn unify(&mut self, a: &SolveTy, b: &SolveTy) -> UnifyResult {
        let a = self.resolve_shallow(a);
        let b = self.resolve_shallow(b);
        match (&a, &b) {
            (SolveTy::Error, _) | (_, SolveTy::Error) => UnifyResult::Ok,
            (SolveTy::Var(x), SolveTy::Var(y)) if x == y => UnifyResult::Ok,
            (SolveTy::Var(x), _) => self.bind(*x, &b),
            (_, SolveTy::Var(y)) => self.bind(*y, &a),
            (SolveTy::Con(x), SolveTy::Con(y)) if x == y => UnifyResult::Ok,
            (SolveTy::Unit, SolveTy::Unit) => UnifyResult::Ok,
            (SolveTy::App(f1, a1), SolveTy::App(f2, a2)) => match self.unify(f1, f2) {
                UnifyResult::Ok => self.unify(a1, a2),
                other => other,
            },
            (SolveTy::Arrow(f1, t1), SolveTy::Arrow(f2, t2)) => match self.unify(f1, f2) {
                UnifyResult::Ok => self.unify(t1, t2),
                other => other,
            },
            (SolveTy::Tuple(xs), SolveTy::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    match self.unify(x, y) {
                        UnifyResult::Ok => {}
                        other => return other,
                    }
                }
                UnifyResult::Ok
            }
            _ => UnifyResult::Mismatch,
        }
    }

    /// Binds variable `id` to `ty`, running the occurs and constraint checks.
    fn bind(&mut self, id: TyVarId, ty: &SolveTy) -> UnifyResult {
        if self.occurs(id, ty) {
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
        if let VarState::Free(existing) = &mut self.vars[id.0 as usize] {
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
            Constraint::Ord => matches!(
                ty,
                SolveTy::Var(_)
                    | SolveTy::Error
                    | SolveTy::Con(Con::Int)
                    | SolveTy::Con(Con::Float)
                    | SolveTy::Con(Con::String)
                    | SolveTy::Con(Con::Char)
            ),
            // Eq admits any non-function type.
            Constraint::Eq => !matches!(ty, SolveTy::Arrow(_, _)),
        }
    }

    /// Whether `id` occurs in `ty` (the occurs check).
    fn occurs(&self, id: TyVarId, ty: &SolveTy) -> bool {
        match self.resolve_shallow(ty) {
            SolveTy::Var(other) => other == id,
            SolveTy::App(f, a) | SolveTy::Arrow(f, a) => self.occurs(id, &f) || self.occurs(id, &a),
            SolveTy::Tuple(elems) => elems.iter().any(|e| self.occurs(id, e)),
            SolveTy::Con(_) | SolveTy::Unit | SolveTy::Error => false,
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

    /// Reifies a solver type into an immutable [`Ty`], renumbering the remaining
    /// free variables compactly starting at 0 (so schemes are canonical).
    pub fn reify(&self, ty: &SolveTy) -> Ty {
        let mut renumber = Renumber::default();
        self.reify_inner(ty, &mut renumber)
    }

    /// Reifies into a [`Ty`] and reports the free variables it contains (for
    /// generalization), each renumbered compactly.
    pub fn reify_with_vars(&self, ty: &SolveTy) -> (Ty, Vec<TyVarId>) {
        let mut renumber = Renumber::default();
        let reified = self.reify_inner(ty, &mut renumber);
        (reified, renumber.order)
    }

    fn reify_inner(&self, ty: &SolveTy, renumber: &mut Renumber) -> Ty {
        match self.resolve_shallow(ty) {
            SolveTy::Var(id) => Ty::Var(renumber.map(id)),
            SolveTy::Con(c) => Ty::Con(c),
            SolveTy::Unit => Ty::Unit,
            SolveTy::Error => Ty::Error,
            SolveTy::App(f, a) => Ty::App(
                std::sync::Arc::new(self.reify_inner(&f, renumber)),
                std::sync::Arc::new(self.reify_inner(&a, renumber)),
            ),
            SolveTy::Arrow(f, t) => {
                Ty::arrow(self.reify_inner(&f, renumber), self.reify_inner(&t, renumber))
            }
            SolveTy::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|e| self.reify_inner(e, renumber)).collect())
            }
        }
    }

    /// Instantiates a scheme with fresh variables (no constraints recorded; M2
    /// schemes carry no constraints).
    pub fn instantiate(&mut self, scheme: &Scheme) -> SolveTy {
        let mut mapping = rustc_hash::FxHashMap::default();
        for &v in &scheme.vars {
            let fresh = self.fresh();
            if let SolveTy::Var(id) = fresh {
                mapping.insert(v, id);
            }
        }
        instantiate_ty(&scheme.ty, &mapping)
    }
}

/// Compact renumbering of free variables encountered during reification.
#[derive(Default)]
struct Renumber {
    map: rustc_hash::FxHashMap<TyVarId, TyVarId>,
    order: Vec<TyVarId>,
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
}

fn instantiate_ty(ty: &Ty, mapping: &rustc_hash::FxHashMap<TyVarId, TyVarId>) -> SolveTy {
    match ty {
        Ty::Var(v) => SolveTy::Var(*mapping.get(v).unwrap_or(v)),
        Ty::Con(c) => SolveTy::Con(*c),
        Ty::Unit => SolveTy::Unit,
        Ty::Error => SolveTy::Error,
        Ty::App(f, a) => {
            SolveTy::App(Box::new(instantiate_ty(f, mapping)), Box::new(instantiate_ty(a, mapping)))
        }
        Ty::Arrow(f, t) => SolveTy::arrow(instantiate_ty(f, mapping), instantiate_ty(t, mapping)),
        Ty::Tuple(elems) => {
            SolveTy::Tuple(elems.iter().map(|e| instantiate_ty(e, mapping)).collect())
        }
    }
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
