//! Algorithm W over a body, producing a principal solver type.
//!
//! The walker resolves each reference through the [`ResolvedBodies`] map: locals
//! get their bound (monomorphic) type; definitions and builtins are instantiated
//! from a [`Scheme`] supplied by the caller (so cross-def and cross-module uses
//! go through *types*, never bodies — the firewall). Operator typing and the
//! Numeric/Eq/Ord constraints live here.

use std::rc::Rc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{
    CtorRef, DefId, InterfaceRef, LocalId, Res, ResolvedBodies, interface_decls, type_decls,
};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{
    BinOp, ExprId, ExprKind, MethodImpl, Module, PatId, PatKind, UnOp, classify_op, classify_prefix,
};
use rustc_hash::FxHashMap;

use crate::infer::ctx::{
    Constraint, InferCtx, RowTail, SolveEffect, SolveRow, SolveTy, UnifyResult,
};
use crate::lower::{build_interface_method_scheme, interface_param_count, resolve_interface};
use crate::ty::Scheme;
use crate::{
    EQUALITY_ON_FUNCTION, INSTANCE_METHOD_SET, NOT_AN_INTERFACE, OCCURS_CHECK, OPAQUE_ACCESS,
    SEALED_INTERFACE, TYPE_MISMATCH, UNKNOWN_METHOD,
};

/// If `ty`'s head is an opaque type (possibly applied to arguments), its name.
/// Shared by the field-access/record checks here and the body-vs-signature check
/// in [`crate::infer`], so both report an opaque type used structurally.
pub(crate) fn opaque_adt_head_name(db: &dyn Db, ty: &crate::ty::Ty) -> Option<Symbol> {
    let mut head = ty;
    while let crate::ty::Ty::App(f, _) = head {
        head = f;
    }
    let crate::ty::Ty::Adt(adt) = head else { return None };
    let file = db.source_file(adt.file)?;
    type_decls(db, file).type_named(adt.name).filter(|info| info.opaque).map(|_| adt.name)
}

/// Supplies the scheme for a referenced definition or builtin, and signals
/// whether a same-SCC definition should be treated monomorphically.
pub trait Env {
    /// The scheme of a definition reference. `None` means "same SCC, use the
    /// monomorphic in-progress type provided via [`Env::scc_type`]".
    fn def_scheme(&mut self, def: DefId) -> Option<Scheme>;
    /// The in-progress monomorphic solver type of a same-SCC definition.
    fn scc_type(&mut self, def: DefId) -> Option<SolveTy>;
    /// The scheme of a builtin/prelude name.
    fn builtin_scheme(&mut self, name: Symbol) -> Option<Scheme>;
    /// The scheme of a data constructor (`Some : 'a -> Option 'a`).
    fn ctor_scheme(&mut self, ctor: CtorRef) -> Option<Scheme>;
}

/// A local binding's type: monomorphic (parameters, lambda binders, tuple
/// destructuring) or a generalized local scheme (a simple `let v = value`).
#[derive(Clone)]
enum LocalBinding {
    Mono(SolveTy),
    Poly { vars: Vec<crate::ty::TyVarId>, ty: SolveTy },
}

/// The per-body inference walker.
pub struct Walker<'a, E: Env> {
    pub db: &'a dyn Db,
    pub file: SourceFile,
    pub module: &'a Module,
    pub resolved: &'a ResolvedBodies,
    pub cx: &'a mut InferCtx,
    pub env: &'a mut E,
    /// Types of locals bound so far in the current body.
    locals: FxHashMap<LocalId, LocalBinding>,
    /// When set, every expression's solver type is recorded in `expr_types`.
    record_types: bool,
    /// Per-expression solver types (populated only when `record_types` is set).
    expr_types: FxHashMap<ExprId, SolveTy>,
    /// Per-pattern solver types (populated only when `record_types` is set).
    pat_types: FxHashMap<PatId, SolveTy>,
    /// The latent effect accumulated for the expression currently being walked
    /// (the body of the enclosing function or lambda): the union of the effects
    /// of every application performed. A lambda saves and resets it so the lambda
    /// carries its own body's effect, closing the capability-laundering hole.
    cur_effect: SolveEffect,
}

impl<'a, E: Env> Walker<'a, E> {
    /// Builds a walker over `module`'s body with an empty local scope.
    pub fn new(
        db: &'a dyn Db,
        file: SourceFile,
        module: &'a Module,
        resolved: &'a ResolvedBodies,
        cx: &'a mut InferCtx,
        env: &'a mut E,
    ) -> Self {
        Self {
            db,
            file,
            module,
            resolved,
            cx,
            env,
            locals: FxHashMap::default(),
            record_types: false,
            expr_types: FxHashMap::default(),
            pat_types: FxHashMap::default(),
            cur_effect: SolveEffect::pure(),
        }
    }
}

impl<E: Env> Walker<'_, E> {
    /// The inferred type of every local bound so far in the current body, keyed
    /// by its [`LocalId`], in allocation order. Defaults still-free numeric
    /// variables to `Int` and reifies all locals against a *shared* renumbering,
    /// so a type variable shared between locals (e.g. tuple-destructuring
    /// components) renders with the same name. Call this *after* inferring the
    /// body. Used by test introspection to assert on local inference directly.
    pub fn collect_local_types(&mut self) -> Vec<(LocalId, crate::ty::Ty)> {
        // Snapshot (id, solver type) in allocation order.
        let mut entries: Vec<(LocalId, SolveTy)> = self
            .locals
            .iter()
            .map(|(id, binding)| {
                let solve = match binding {
                    LocalBinding::Mono(t) => t.clone(),
                    LocalBinding::Poly { ty, .. } => ty.clone(),
                };
                (*id, solve)
            })
            .collect();
        entries.sort_by_key(|(id, _)| id.index());

        for (_, solve) in &entries {
            self.cx.default_numerics_deep(solve);
        }
        let solves: Vec<SolveTy> = entries.iter().map(|(_, s)| s.clone()).collect();
        let reified = self.cx.reify_many(&solves);
        entries.iter().map(|(id, _)| *id).zip(reified).collect()
    }

    fn span(&self, range: fai_span::TextRange) -> Span {
        Span::new(self.file.source(self.db), range)
    }

    fn mismatch(&self, range: fai_span::TextRange, msg: impl Into<String>) {
        emit(self.db, Diagnostic::error(TYPE_MISMATCH, msg, self.span(range)));
    }

    fn unify_at(&mut self, range: fai_span::TextRange, a: &SolveTy, b: &SolveTy, what: &str) {
        match self.cx.unify(a, b) {
            UnifyResult::Ok => {}
            UnifyResult::Occurs => {
                emit(
                    self.db,
                    Diagnostic::error(
                        OCCURS_CHECK,
                        format!("infinite type while checking {what}"),
                        self.span(range),
                    ),
                );
            }
            UnifyResult::Mismatch | UnifyResult::BadConstraint => {
                let a_re = self.cx.reify(a);
                let b_re = self.cx.reify(b);
                // Using an opaque type's value as a structural record — a field
                // access, `{ … }` construction, or `{ r with … }` update — surfaces
                // here as a record-shape-vs-opaque-`Adt` mismatch. Report it as
                // such rather than as a bare type mismatch.
                let opaque = match (&a_re, &b_re) {
                    (crate::ty::Ty::Record(_), other) | (other, crate::ty::Ty::Record(_)) => {
                        self.opaque_adt_name(other)
                    }
                    _ => None,
                };
                if let Some(name) = opaque {
                    emit(
                        self.db,
                        Diagnostic::error(
                            OPAQUE_ACCESS,
                            format!(
                                "the type `{name}` is opaque; its fields are not accessible \
                                 from this file"
                            ),
                            self.span(range),
                        ),
                    );
                    return;
                }
                let a_ty = crate::ty::render(&a_re, &crate::ty::VarNames::new());
                let b_ty = crate::ty::render(&b_re, &crate::ty::VarNames::new());
                self.mismatch(range, format!("type mismatch in {what}: `{a_ty}` vs `{b_ty}`"));
            }
        }
    }

    /// If `ty`'s head is an opaque type (possibly applied to arguments), its name.
    /// Used to report an attempt to treat an opaque type as a structural record.
    fn opaque_adt_name(&self, ty: &crate::ty::Ty) -> Option<Symbol> {
        opaque_adt_head_name(self.db, ty)
    }

    /// Records that evaluating the current expression performs effect `eff`,
    /// merging it into the body's accumulated latent effect.
    fn incur_effect(&mut self, eff: &SolveEffect) {
        let merged = self.cx.union_effects(&self.cur_effect, eff);
        self.cur_effect = merged;
    }

    /// The accumulated latent effect of the body walked so far, reified. This is
    /// the function/lambda's inferred effect — the capabilities it uses.
    #[must_use]
    pub fn body_effect(&self) -> crate::ty::EffectRow {
        self.cx.reify_effect_standalone(&self.cur_effect)
    }

    /// Binds a parameter pattern to fresh local types and returns its type.
    pub fn bind_param(&mut self, pat: PatId) -> SolveTy {
        self.bind_pattern_into(pat)
    }

    /// Binds a parameter and unifies it with its declared type, so the body is
    /// checked with the parameter at its signature type (needed for type-directed
    /// interface method access on a parameter).
    pub fn bind_param_checked(&mut self, pat: PatId, expected: &SolveTy) -> SolveTy {
        let pt = self.bind_pattern_into(pat);
        let span = self.module.pat(pat).span;
        self.unify_at(span, &pt, expected, "a parameter");
        pt
    }

    /// Enables recording of every visited expression's type (for `body_types`).
    pub fn enable_type_recording(&mut self) {
        self.record_types = true;
    }

    /// The recorded per-expression types, defaulted and reified against a shared
    /// renumbering (so a variable shared between expressions renders the same).
    /// Call after inferring the body; requires [`Self::enable_type_recording`].
    pub fn collect_expr_types(&mut self) -> Vec<(ExprId, crate::ty::Ty)> {
        let mut entries: Vec<(ExprId, SolveTy)> =
            self.expr_types.iter().map(|(id, ty)| (*id, ty.clone())).collect();
        entries.sort_by_key(|(id, _)| id.index());
        for (_, solve) in &entries {
            self.cx.default_numerics_deep(solve);
        }
        let solves: Vec<SolveTy> = entries.iter().map(|(_, s)| s.clone()).collect();
        let reified = self.cx.reify_many(&solves);
        entries.iter().map(|(id, _)| *id).zip(reified).collect()
    }

    /// The recorded per-pattern types, defaulted and reified. Requires
    /// [`Self::enable_type_recording`]; call after inferring the body.
    pub fn collect_pat_types(&mut self) -> Vec<(PatId, crate::ty::Ty)> {
        let mut entries: Vec<(PatId, SolveTy)> =
            self.pat_types.iter().map(|(id, ty)| (*id, ty.clone())).collect();
        entries.sort_by_key(|(id, _)| id.index());
        for (_, solve) in &entries {
            self.cx.default_numerics_deep(solve);
        }
        let solves: Vec<SolveTy> = entries.iter().map(|(_, s)| s.clone()).collect();
        let reified = self.cx.reify_many(&solves);
        entries.iter().map(|(id, _)| *id).zip(reified).collect()
    }

    /// Infers the type of an expression, recording it when enabled.
    pub fn infer_expr(&mut self, expr: ExprId) -> SolveTy {
        let ty = self.infer_expr_inner(expr);
        if self.record_types {
            self.expr_types.insert(expr, ty.clone());
        }
        ty
    }

    /// The core of [`Self::infer_expr`] (one expression node).
    fn infer_expr_inner(&mut self, expr: ExprId) -> SolveTy {
        let node = self.module.expr(expr);
        match &node.kind {
            ExprKind::Int(_) => self.cx.fresh_constrained(Some(Constraint::Numeric)),
            ExprKind::Float(_) => SolveTy::Con(crate::ty::Con::Float),
            ExprKind::String(_) => SolveTy::string(),
            ExprKind::Char(_) => SolveTy::Con(crate::ty::Con::Char),
            ExprKind::Unit => SolveTy::Unit,
            ExprKind::Var(name) => self.infer_ref(expr, *name, node.span),
            ExprKind::Field { base, field } => self.infer_field(expr, *base, *field, node.span),
            ExprKind::Record(fields) => {
                self.check_no_duplicate_labels(fields.iter().map(|f| (f.name, f.span)), node.span);
                let row: Vec<(Symbol, SolveTy)> =
                    fields.iter().map(|f| (f.name, self.infer_expr(f.value))).collect();
                SolveTy::Record(SolveRow { fields: row, tail: RowTail::Closed })
            }
            ExprKind::RecordUpdate { base, fields } => {
                let base_ty = self.infer_expr(*base);
                let updated: Vec<(Symbol, SolveTy)> =
                    fields.iter().map(|f| (f.name, self.infer_expr(f.value))).collect();
                let labels: Vec<Symbol> = updated.iter().map(|(l, _)| *l).collect();
                // The base is `{ labels : old | ρ }`; the result reuses ρ with the
                // updated field types — `{ r with x = v } : { x : typeof v | ρ }`.
                let rho = self.cx.fresh_row(labels.clone());
                let old: Vec<(Symbol, SolveTy)> =
                    labels.iter().map(|&l| (l, self.cx.fresh())).collect();
                let base_shape =
                    SolveTy::Record(SolveRow { fields: old, tail: RowTail::Open(rho) });
                self.unify_at(node.span, &base_ty, &base_shape, "a record update");
                SolveTy::Record(SolveRow { fields: updated, tail: RowTail::Open(rho) })
            }
            ExprKind::Instance { name, methods } => self.infer_instance(*name, methods, node.span),
            ExprKind::App { func, arg } => {
                let func_ty = self.infer_expr(*func);
                let arg_ty = self.infer_expr(*arg);
                let result = self.cx.fresh();
                let expected = SolveTy::arrow(arg_ty, result.clone());
                self.unify_at(node.span, &func_ty, &expected, "function application");
                // Incur the applied function's resolved effect (read, without
                // binding an effect variable into the function's type — so an
                // effect-polymorphic *type* is not forced here). A partial
                // application's outer arrow is pure; the saturating application's
                // arrow carries the real effect.
                if let SolveTy::Arrow(_, _, eff) = self.cx.resolve_shallow(&func_ty) {
                    self.incur_effect(&eff);
                }
                result
            }
            ExprKind::Infix { op, lhs, rhs } => self.infer_infix(*op, *lhs, *rhs, node.span),
            ExprKind::Prefix { op, operand } => self.infer_prefix(*op, *operand, node.span),
            ExprKind::If { cond, then_branch, else_branch } => {
                let cond_ty = self.infer_expr(*cond);
                self.unify_at(
                    self.module.expr(*cond).span,
                    &cond_ty,
                    &SolveTy::bool(),
                    "an `if` condition",
                );
                let then_ty = self.infer_expr(*then_branch);
                let else_ty = self.infer_expr(*else_branch);
                self.unify_at(
                    self.module.expr(*else_branch).span,
                    &then_ty,
                    &else_ty,
                    "the branches of an `if`",
                );
                then_ty
            }
            ExprKind::Lambda { params, body } => {
                let param_tys: Vec<SolveTy> =
                    params.iter().map(|&p| self.bind_pattern_into(p)).collect();
                // The lambda's body has its own latent effect: save and reset the
                // enclosing accumulator so the effect lands on the lambda's arrow
                // (closing the closure-laundering hole), then restore it.
                // The lambda's body has its own latent effect; it belongs on the
                // lambda's arrow, not the enclosing function. In this phase the
                // function type stays effect-free, so the scoped effect is
                // discarded after the body (computed effects ride `def_effect`,
                // and coupling effects into the type lands with enforcement).
                let saved = std::mem::replace(&mut self.cur_effect, SolveEffect::pure());
                let body_ty = self.infer_expr(*body);
                self.cur_effect = saved;
                SolveTy::arrows_solver(param_tys, body_ty)
            }
            ExprKind::Match { scrutinee, arms } => {
                let scrutinee_ty = self.infer_expr(*scrutinee);
                let result = self.cx.fresh();
                for arm in arms {
                    self.check_pattern(arm.pat, &scrutinee_ty);
                    let body_ty = self.infer_expr(arm.body);
                    self.unify_at(
                        self.module.expr(arm.body).span,
                        &result,
                        &body_ty,
                        "the arms of a `match`",
                    );
                }
                result
            }
            ExprKind::Block { stmts, tail } => {
                for stmt in stmts {
                    // Generalize only a simple `let v = value` whose right-hand
                    // side is a *syntactic value* (the value restriction). This is
                    // both standard and avoids generalizing expressions like
                    // `a + 1` whose type is fixed by the environment.
                    let is_simple_var = stmt.params.is_empty()
                        && matches!(self.module.pat(stmt.pat).kind, PatKind::Var(_))
                        && is_syntactic_value(self.module, stmt.value);

                    let value_ty = if is_simple_var {
                        // Infer a generalizable right-hand side one level deeper;
                        // variables created here that are not unified with an
                        // outer one (their level stays deeper) are generalized.
                        self.cx.enter_level();
                        let v = self.infer_expr(stmt.value);
                        self.cx.exit_level();
                        v
                    } else if stmt.params.is_empty() {
                        self.infer_expr(stmt.value)
                    } else {
                        let param_tys: Vec<SolveTy> =
                            stmt.params.iter().map(|&p| self.bind_pattern_into(p)).collect();
                        let v = self.infer_expr(stmt.value);
                        SolveTy::arrows_solver(param_tys, v)
                    };

                    if is_simple_var {
                        // Generalize a simple `let v = value`: quantify the value
                        // type's free variables created in its right-hand side and
                        // not fixed by the environment (standard let-polymorphism;
                        // sound because there are no mutable references).
                        let vars = self.generalizable_vars(&value_ty);
                        if let Some(slot) = self.resolved.local_of(stmt.pat) {
                            let binding = if vars.is_empty() {
                                LocalBinding::Mono(value_ty)
                            } else {
                                LocalBinding::Poly { vars, ty: value_ty }
                            };
                            self.locals.insert(slot, binding);
                        }
                    } else {
                        // Function and destructuring lets bind monomorphically.
                        self.check_pattern(stmt.pat, &value_ty);
                    }
                }
                self.infer_expr(*tail)
            }
            ExprKind::Paren(inner) => self.infer_expr(*inner),
            ExprKind::Tuple(elems) => {
                SolveTy::Tuple(elems.iter().map(|&e| self.infer_expr(e)).collect())
            }
            ExprKind::List(elems) => {
                let elem_ty = self.cx.fresh();
                for &e in elems {
                    let t = self.infer_expr(e);
                    self.unify_at(self.module.expr(e).span, &elem_ty, &t, "a list element");
                }
                SolveTy::list(elem_ty)
            }
            ExprKind::Error => SolveTy::Error,
        }
    }

    fn infer_ref(&mut self, expr: ExprId, name: Symbol, span: fai_span::TextRange) -> SolveTy {
        match self.resolved.get(expr) {
            Some(Res::Local(local)) => match self.locals.get(&local).cloned() {
                Some(LocalBinding::Mono(t)) => t,
                Some(LocalBinding::Poly { vars, ty }) => self.instantiate_local(&vars, &ty),
                None => SolveTy::Error,
            },
            Some(Res::Def(def)) => self.instantiate_def(def),
            Some(Res::Ctor(ctor)) => match self.env.ctor_scheme(ctor) {
                Some(scheme) => self.cx.instantiate(&scheme),
                None => SolveTy::Error,
            },
            Some(Res::Builtin(b)) => {
                // A built-in operator used in value position (`(+)`, `(::)`, …)
                // gets its operator type; other builtins use the scheme table.
                if let Some(ty) = self.operator_value_type(b) {
                    ty
                } else {
                    match self.env.builtin_scheme(b) {
                        Some(scheme) => self.cx.instantiate(&scheme),
                        None => SolveTy::Error,
                    }
                }
            }
            Some(Res::Error) | None => {
                let _ = (name, span);
                SolveTy::Error
            }
        }
    }

    fn instantiate_def(&mut self, def: DefId) -> SolveTy {
        // Same-SCC reference: use the monomorphic in-progress type.
        if let Some(mono) = self.env.scc_type(def) {
            return mono;
        }
        match self.env.def_scheme(def) {
            Some(scheme) => self.cx.instantiate(&scheme),
            None => SolveTy::Error,
        }
    }

    fn infer_field(
        &mut self,
        expr: ExprId,
        base: ExprId,
        field: Symbol,
        span: fai_span::TextRange,
    ) -> SolveTy {
        // A qualified `Foo.bar` resolved to a Def/Ctor/Builtin in resolution.
        match self.resolved.get(expr) {
            Some(Res::Def(def)) => return self.instantiate_def(def),
            Some(Res::Ctor(ctor)) => {
                return match self.env.ctor_scheme(ctor) {
                    Some(scheme) => self.cx.instantiate(&scheme),
                    None => SolveTy::Error,
                };
            }
            Some(Res::Builtin(b)) => {
                return match self.env.builtin_scheme(b) {
                    Some(scheme) => self.cx.instantiate(&scheme),
                    None => SolveTy::Error,
                };
            }
            Some(Res::Error) => return SolveTy::Error,
            // Not a qualified reference: ordinary record field access.
            Some(Res::Local(_)) | None => {}
        }
        let base_ty = self.infer_expr(base);
        // Type-directed: if the base is a (resolved) interface, `e.m` is method
        // access; otherwise it is ordinary record field access.
        if let Some((iref, args)) = self.as_interface(&base_ty) {
            return self.infer_method_access(iref, &args, field, span);
        }
        // `r.x` requires `r` to be a record with at least field `x` (open row).
        let field_ty = self.cx.fresh();
        let shape = self.cx.fresh_open_record(vec![(field, field_ty.clone())]);
        self.unify_at(span, &base_ty, &shape, "a record field access");
        field_ty
    }

    /// If `ty` resolves to an interface head `Interface(iref)` applied to args,
    /// returns the interface and its type arguments (in order).
    fn as_interface(&self, ty: &SolveTy) -> Option<(InterfaceRef, Vec<SolveTy>)> {
        let mut args = Vec::new();
        let mut cur = self.cx.resolve_shallow(ty);
        loop {
            match cur {
                SolveTy::Interface(iref) => {
                    args.reverse();
                    return Some((iref, args));
                }
                SolveTy::App(f, a) => {
                    args.push(self.cx.resolve_shallow(&a));
                    cur = self.cx.resolve_shallow(&f);
                }
                _ => return None,
            }
        }
    }

    /// Types `e.m` where `e : Interface(iref) args…`: looks up the method scheme,
    /// instantiates it, and unifies the interface's parameter instances with the
    /// actual type arguments.
    fn infer_method_access(
        &mut self,
        iref: InterfaceRef,
        args: &[SolveTy],
        method: Symbol,
        span: fai_span::TextRange,
    ) -> SolveTy {
        let Some(scheme) = build_interface_method_scheme(self.db, iref, method) else {
            emit(
                self.db,
                Diagnostic::error(
                    UNKNOWN_METHOD,
                    format!("interface `{}` has no method `{method}`", iref.name),
                    self.span(span),
                ),
            );
            return SolveTy::Error;
        };
        let (method_ty, fresh, _) = self.cx.instantiate_tracked(&scheme);
        // The leading fresh variables correspond to the interface parameters.
        let n = interface_param_count(self.db, iref);
        for (param_instance, arg) in fresh.iter().take(n).zip(args) {
            self.unify_at(span, &SolveTy::Var(*param_instance), arg, "an interface type argument");
        }
        method_ty
    }

    /// Types an interface instance `{ Name with m args = body, … }`: each method
    /// body is checked against the declared method type (the interface's
    /// parameters shared across methods), and the implemented set must match the
    /// declaration exactly.
    fn infer_instance(
        &mut self,
        name: Symbol,
        methods: &[MethodImpl],
        span: fai_span::TextRange,
    ) -> SolveTy {
        let Some(iref) = resolve_interface(self.db, self.file, name) else {
            emit(
                self.db,
                Diagnostic::error(
                    NOT_AN_INTERFACE,
                    format!("`{name}` is not an interface"),
                    self.span(span),
                ),
            );
            // Still type the method bodies so the rest of the body is coherent.
            for m in methods {
                for &p in &m.params {
                    self.bind_pattern_into(p);
                }
                self.infer_expr(m.body);
            }
            return SolveTy::Error;
        };

        // The built-in constraint interfaces (`Num`/`Eq`/`Ord`) are sealed: their
        // operators dispatch to primitives, so a hand-written instance would be
        // dead. Reject it (but keep typing the bodies for coherence).
        if self.is_sealed_interface(iref) {
            emit(
                self.db,
                Diagnostic::error(
                    SEALED_INTERFACE,
                    format!("`{name}` is a sealed built-in interface and cannot be instantiated"),
                    self.span(span),
                ),
            );
        }

        let n = interface_param_count(self.db, iref);
        let param_fresh: Vec<crate::ty::TyVarId> = (0..n).map(|_| self.cx.fresh_var_id()).collect();

        let declared: Vec<Symbol> = self
            .db
            .source_file(iref.file)
            .and_then(|f| {
                interface_decls(self.db, f).interface_named(iref.name).map(|i| i.methods.clone())
            })
            .unwrap_or_default();

        let mut implemented: Vec<Symbol> = Vec::new();
        for m in methods {
            let param_tys: Vec<SolveTy> =
                m.params.iter().map(|&p| self.bind_pattern_into(p)).collect();
            let body_ty = self.infer_expr(m.body);
            let impl_ty = SolveTy::arrows_solver(param_tys, body_ty);
            match build_interface_method_scheme(self.db, iref, m.name) {
                Some(scheme) => {
                    let expected = self.cx.instantiate_sharing(&scheme, &param_fresh);
                    self.unify_at(m.span, &impl_ty, &expected, "an interface method");
                    implemented.push(m.name);
                }
                None => emit(
                    self.db,
                    Diagnostic::error(
                        UNKNOWN_METHOD,
                        format!("interface `{name}` has no method `{}`", m.name),
                        self.span(m.span),
                    ),
                ),
            }
        }

        let missing: Vec<&Symbol> = declared.iter().filter(|d| !implemented.contains(d)).collect();
        if !missing.is_empty() {
            let names = missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("`, `");
            emit(
                self.db,
                Diagnostic::error(
                    INSTANCE_METHOD_SET,
                    format!("instance of `{name}` is missing method(s): `{names}`"),
                    self.span(span),
                ),
            );
        }

        // The instance's type is the interface applied to the (inferred) args.
        let mut t = SolveTy::Interface(iref);
        for &p in &param_fresh {
            t = SolveTy::App(Rc::new(t), Rc::new(SolveTy::Var(p)));
        }
        t
    }

    /// Emits [`crate::DUPLICATE_FIELD`] for any repeated label among `fields`.
    fn check_no_duplicate_labels(
        &mut self,
        fields: impl Iterator<Item = (Symbol, fai_span::TextRange)>,
        whole: fai_span::TextRange,
    ) {
        let mut seen: Vec<Symbol> = Vec::new();
        for (name, _) in fields {
            if seen.contains(&name) {
                emit(
                    self.db,
                    Diagnostic::error(
                        crate::DUPLICATE_FIELD,
                        format!("record field `{name}` is given more than once"),
                        self.span(whole),
                    ),
                );
            } else {
                seen.push(name);
            }
        }
    }

    /// The type of a built-in operator used in value position (`(+)`, `(::)`, …),
    /// or `None` if `name` is not a built-in operator. Mirrors the applied-form
    /// typing in [`Self::infer_builtin_binary`].
    fn operator_value_type(&mut self, name: Symbol) -> Option<SolveTy> {
        let arrow2 = |a: SolveTy, b: SolveTy, r: SolveTy| SolveTy::arrow(a, SolveTy::arrow(b, r));
        Some(match name.as_str() {
            "+" | "-" | "*" | "/" | "%" => {
                let n = self.cx.fresh_constrained(Some(Constraint::Numeric));
                arrow2(n.clone(), n.clone(), n)
            }
            "<" | "<=" | ">" | ">=" => {
                let o = self.cx.fresh_constrained(Some(Constraint::Ord));
                arrow2(o.clone(), o, SolveTy::bool())
            }
            "=" | "<>" => {
                let e = self.cx.fresh_constrained(Some(Constraint::Eq));
                arrow2(e.clone(), e, SolveTy::bool())
            }
            "&&" | "||" => arrow2(SolveTy::bool(), SolveTy::bool(), SolveTy::bool()),
            "::" => {
                let a = self.cx.fresh();
                arrow2(a.clone(), SolveTy::list(a.clone()), SolveTy::list(a))
            }
            _ => return None,
        })
    }

    /// Whether `iref` is a sealed built-in constraint interface (`Num`/`Eq`/`Ord`
    /// from the standard library), which is not user-instantiable.
    fn is_sealed_interface(&self, iref: InterfaceRef) -> bool {
        matches!(iref.name.as_str(), "Num" | "Eq" | "Ord")
            && self.db.source_file(iref.file).is_some_and(|f| fai_db::is_std_path(f.path(self.db)))
    }

    /// The operator symbol held in an operator `Var` node.
    fn op_symbol(&self, op: ExprId) -> Symbol {
        match &self.module.expr(op).kind {
            ExprKind::Var(s) => *s,
            _ => Symbol::intern(""),
        }
    }

    /// Whether the operator node `op` resolved to the built-in operator (rather
    /// than a shadowing user binding).
    fn is_builtin_op(&self, op: ExprId) -> bool {
        matches!(self.resolved.get(op), Some(Res::Builtin(_)))
    }

    fn infer_prefix(&mut self, op: ExprId, operand: ExprId, span: fai_span::TextRange) -> SolveTy {
        let sym = self.op_symbol(op);
        if self.is_builtin_op(op) && matches!(classify_prefix(sym), Some(UnOp::Neg)) {
            let t = self.infer_expr(operand);
            let num = self.cx.fresh_constrained(Some(Constraint::Numeric));
            self.unify_at(span, &t, &num, "a negation");
            return num;
        }
        // A user-defined prefix operator: an ordinary one-argument application.
        let op_ty = self.infer_expr(op);
        let operand_ty = self.infer_expr(operand);
        let result = self.cx.fresh();
        let expected = SolveTy::arrow(operand_ty, result.clone());
        self.unify_at(span, &op_ty, &expected, "a prefix operator application");
        result
    }

    fn infer_infix(
        &mut self,
        op: ExprId,
        lhs: ExprId,
        rhs: ExprId,
        span: fai_span::TextRange,
    ) -> SolveTy {
        let sym = self.op_symbol(op);
        if self.is_builtin_op(op)
            && let Some(binop) = classify_op(sym)
        {
            return self.infer_builtin_binary(binop, lhs, rhs, span);
        }
        // A user-defined operator (or a shadowed built-in): a curried application
        // of the resolved operator function to its two operands.
        let op_ty = self.infer_expr(op);
        let lt = self.infer_expr(lhs);
        let rt = self.infer_expr(rhs);
        let result = self.cx.fresh();
        let expected = SolveTy::arrow(lt, SolveTy::arrow(rt, result.clone()));
        self.unify_at(span, &op_ty, &expected, "an operator application");
        result
    }

    fn infer_builtin_binary(
        &mut self,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
        span: fai_span::TextRange,
    ) -> SolveTy {
        let lt = self.infer_expr(lhs);
        let rt = self.infer_expr(rhs);
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                let num = self.cx.fresh_constrained(Some(Constraint::Numeric));
                self.unify_at(span, &lt, &num, "an arithmetic operand");
                self.unify_at(span, &rt, &num, "an arithmetic operand");
                num
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let ord = self.cx.fresh_constrained(Some(Constraint::Ord));
                self.unify_at(span, &lt, &ord, "a comparison operand");
                self.unify_at(span, &rt, &ord, "a comparison operand");
                SolveTy::bool()
            }
            BinOp::Eq | BinOp::Ne => {
                // Equality requires the operands to share a type and that type to
                // be non-function. If either operand is already known to be a
                // function, report the dedicated diagnostic rather than a generic
                // constraint mismatch.
                let lhs_fn = matches!(self.cx.resolve_shallow(&lt), SolveTy::Arrow(..));
                let rhs_fn = matches!(self.cx.resolve_shallow(&rt), SolveTy::Arrow(..));
                if lhs_fn || rhs_fn {
                    // Still unify the two sides so the rest of the body stays
                    // coherent, but don't impose the Eq constraint (which would
                    // double-report as a mismatch).
                    self.unify_at(span, &lt, &rt, "the operands of `=`");
                    emit(
                        self.db,
                        Diagnostic::error(
                            EQUALITY_ON_FUNCTION,
                            "equality is not defined on function types",
                            self.span(span),
                        ),
                    );
                } else {
                    let eq = self.cx.fresh_constrained(Some(Constraint::Eq));
                    self.unify_at(span, &lt, &eq, "an equality operand");
                    self.unify_at(span, &rt, &eq, "an equality operand");
                    // A var that only later resolves to a function is caught here.
                    if matches!(self.cx.resolve_shallow(&eq), SolveTy::Arrow(..)) {
                        emit(
                            self.db,
                            Diagnostic::error(
                                EQUALITY_ON_FUNCTION,
                                "equality is not defined on function types",
                                self.span(span),
                            ),
                        );
                    }
                }
                SolveTy::bool()
            }
            BinOp::And | BinOp::Or => {
                self.unify_at(span, &lt, &SolveTy::bool(), "a boolean operand");
                self.unify_at(span, &rt, &SolveTy::bool(), "a boolean operand");
                SolveTy::bool()
            }
            BinOp::Cons => {
                let list = SolveTy::list(lt.clone());
                self.unify_at(span, &rt, &list, "a `::` tail");
                list
            }
        }
    }

    /// Binds an (irrefutable) parameter/lambda pattern, returning its fresh type.
    fn bind_pattern_into(&mut self, pat: PatId) -> SolveTy {
        let ty = self.cx.fresh();
        self.check_pattern(pat, &ty);
        ty
    }

    /// Checks a pattern against the `expected` scrutinee type, unifying the
    /// pattern's structure with it and binding its variables. Records the slot
    /// type of every bound variable so later uses resolve to it.
    fn check_pattern(&mut self, pat: PatId, expected: &SolveTy) {
        if self.record_types {
            self.pat_types.insert(pat, expected.clone());
        }
        let span = self.module.pat(pat).span;
        match &self.module.pat(pat).kind {
            PatKind::Var(_) | PatKind::Wildcard => {
                if let Some(slot) = self.resolved.local_of(pat) {
                    self.locals.insert(slot, LocalBinding::Mono(expected.clone()));
                }
            }
            PatKind::Unit => self.unify_at(span, expected, &SolveTy::Unit, "a `()` pattern"),
            PatKind::Int(_) => self.unify_at(span, expected, &SolveTy::int(), "an integer pattern"),
            PatKind::Float(_) => self.unify_at(
                span,
                expected,
                &SolveTy::Con(crate::ty::Con::Float),
                "a float pattern",
            ),
            PatKind::String(_) => {
                self.unify_at(span, expected, &SolveTy::string(), "a string pattern");
            }
            PatKind::Char(_) => {
                self.unify_at(
                    span,
                    expected,
                    &SolveTy::Con(crate::ty::Con::Char),
                    "a char pattern",
                );
            }
            PatKind::Bool(_) => {
                self.unify_at(span, expected, &SolveTy::bool(), "a boolean pattern")
            }
            PatKind::Paren(inner) => self.check_pattern(*inner, expected),
            PatKind::Tuple(elems) => {
                let part_tys: Vec<SolveTy> = elems.iter().map(|_| self.cx.fresh()).collect();
                self.unify_at(span, expected, &SolveTy::Tuple(part_tys.clone()), "a tuple pattern");
                for (&e, p) in elems.iter().zip(part_tys) {
                    self.check_pattern(e, &p);
                }
            }
            PatKind::List(elems) => {
                let elem_ty = self.cx.fresh();
                self.unify_at(span, expected, &SolveTy::list(elem_ty.clone()), "a list pattern");
                for &e in elems {
                    self.check_pattern(e, &elem_ty);
                }
            }
            PatKind::Cons { head, tail } => {
                let elem_ty = self.cx.fresh();
                let list = SolveTy::list(elem_ty.clone());
                self.unify_at(span, expected, &list, "a `::` pattern");
                self.check_pattern(*head, &elem_ty);
                self.check_pattern(*tail, &list);
            }
            PatKind::Or(alts) => {
                for &alt in alts {
                    self.check_pattern(alt, expected);
                }
            }
            PatKind::As { pat: inner, .. } => {
                // The alias name (keyed by the as-pattern node) has the scrutinee
                // type; the inner pattern is checked against it too.
                if let Some(slot) = self.resolved.local_of(pat) {
                    self.locals.insert(slot, LocalBinding::Mono(expected.clone()));
                }
                self.check_pattern(*inner, expected);
            }
            PatKind::Constructor { args, .. } => self.check_ctor_pattern(pat, args, expected, span),
            PatKind::Record { fields, open } => {
                // Each named field's sub-pattern is checked against a fresh field
                // type; the record is open iff the pattern is.
                let field_tys: Vec<(Symbol, SolveTy)> =
                    fields.iter().map(|f| (f.name, self.cx.fresh())).collect();
                let shape = if *open {
                    let labels = field_tys.iter().map(|(l, _)| *l).collect();
                    let tail = self.cx.fresh_row(labels);
                    SolveTy::Record(SolveRow {
                        fields: field_tys.clone(),
                        tail: RowTail::Open(tail),
                    })
                } else {
                    SolveTy::Record(SolveRow { fields: field_tys.clone(), tail: RowTail::Closed })
                };
                self.unify_at(span, expected, &shape, "a record pattern");
                for (field, (_, ty)) in fields.iter().zip(field_tys) {
                    self.check_pattern(field.pat, &ty);
                }
            }
            PatKind::Error => {}
        }
    }

    fn check_ctor_pattern(
        &mut self,
        pat: PatId,
        args: &[PatId],
        expected: &SolveTy,
        span: fai_span::TextRange,
    ) {
        let ctor_ty = match self.resolved.pat_res(pat) {
            Some(Res::Ctor(ctor)) => match self.env.ctor_scheme(ctor) {
                Some(scheme) => self.cx.instantiate(&scheme),
                None => SolveTy::Error,
            },
            _ => SolveTy::Error,
        };
        if matches!(ctor_ty, SolveTy::Error) {
            // Still check sub-patterns so their variables bind.
            for &a in args {
                self.check_pattern(a, &SolveTy::Error);
            }
            return;
        }
        // Peel one arrow per argument: the parameter types, then the result.
        let mut cur = ctor_ty;
        let mut arity_ok = true;
        for &a in args {
            match self.cx.resolve_shallow(&cur) {
                SolveTy::Arrow(from, to, _) => {
                    self.check_pattern(a, &from);
                    cur = Rc::unwrap_or_clone(to);
                }
                _ => {
                    arity_ok = false;
                    self.check_pattern(a, &SolveTy::Error);
                }
            }
        }
        if matches!(self.cx.resolve_shallow(&cur), SolveTy::Arrow(..)) {
            arity_ok = false; // too few arguments
        }
        if arity_ok {
            self.unify_at(span, expected, &cur, "a constructor pattern");
        } else {
            emit(
                self.db,
                Diagnostic::error(
                    crate::CONSTRUCTOR_ARITY,
                    "constructor pattern has the wrong number of arguments",
                    self.span(span),
                ),
            );
        }
    }

    /// The free variables of `ty` that may be generalized: those created in the
    /// just-inferred right-hand side (their level is deeper than the enclosing
    /// scope, i.e. they were not unified with an outer variable) and not still
    /// carrying a constraint.
    ///
    /// A *constrained* variable (Numeric/Eq/Ord) is left ungeneralized so it can
    /// be resolved or defaulted later — e.g. `let inc = fun a -> a + 1` keeps its
    /// numeric variable monomorphic so it defaults to `Int` (giving
    /// `Int -> Int`), rather than generalizing to `'a -> 'a`.
    fn generalizable_vars(&self, ty: &SolveTy) -> Vec<crate::ty::TyVarId> {
        let mut free = rustc_hash::FxHashSet::default();
        let mut visited = rustc_hash::FxHashSet::default();
        self.cx.collect_free_vars(ty, &mut free, &mut visited);
        let current = self.cx.current_level();
        let mut vars: Vec<crate::ty::TyVarId> = free
            .into_iter()
            .filter(|v| self.cx.level_of(*v) > current)
            .filter(|v| self.cx.pending_constraint(&SolveTy::Var(*v)).is_none())
            .collect();
        vars.sort();
        vars
    }

    /// Instantiates a local scheme with fresh variables for each quantified var.
    fn instantiate_local(&mut self, vars: &[crate::ty::TyVarId], ty: &SolveTy) -> SolveTy {
        let mut mapping = FxHashMap::default();
        for &v in vars {
            if let SolveTy::Var(fresh) = self.cx.fresh() {
                mapping.insert(v, fresh);
            }
        }
        subst(self.cx, ty, &mapping)
    }
}

/// Whether `expr` is a syntactic value (safe to generalize under the value
/// restriction): a lambda, a variable, a literal, or a tuple/list/paren of
/// values. Function *applications* and other computations are not values.
fn is_syntactic_value(module: &Module, expr: ExprId) -> bool {
    match &module.expr(expr).kind {
        ExprKind::Lambda { .. }
        | ExprKind::Var(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Unit => true,
        ExprKind::Paren(inner) => is_syntactic_value(module, *inner),
        ExprKind::Tuple(elems) | ExprKind::List(elems) => {
            elems.iter().all(|&e| is_syntactic_value(module, e))
        }
        _ => false,
    }
}

/// Substitutes solver variables in `ty` according to `mapping`, following the
/// current substitution for variables not in the map.
fn subst(
    cx: &InferCtx,
    ty: &SolveTy,
    mapping: &FxHashMap<crate::ty::TyVarId, crate::ty::TyVarId>,
) -> SolveTy {
    match cx.resolve_shallow(ty) {
        SolveTy::Var(v) => match mapping.get(&v) {
            Some(&fresh) => SolveTy::Var(fresh),
            None => SolveTy::Var(v),
        },
        SolveTy::App(f, a) => {
            SolveTy::App(Rc::new(subst(cx, &f, mapping)), Rc::new(subst(cx, &a, mapping)))
        }
        SolveTy::Arrow(f, a, e) => {
            SolveTy::arrow_eff(subst(cx, &f, mapping), subst(cx, &a, mapping), e.clone())
        }
        SolveTy::Tuple(elems) => {
            SolveTy::Tuple(elems.iter().map(|e| subst(cx, e, mapping)).collect())
        }
        SolveTy::Record(row) => SolveTy::Record(SolveRow {
            fields: row.fields.iter().map(|(l, t)| (*l, subst(cx, t, mapping))).collect(),
            tail: row.tail,
        }),
        other @ (SolveTy::Con(_)
        | SolveTy::Adt(_)
        | SolveTy::Interface(_)
        | SolveTy::Unit
        | SolveTy::Error) => other,
    }
}
