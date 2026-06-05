//! Algorithm W over a body, producing a principal solver type.
//!
//! The walker resolves each reference through the [`ResolvedBodies`] map: locals
//! get their bound (monomorphic) type; definitions and builtins are instantiated
//! from a [`Scheme`] supplied by the caller (so cross-def and cross-module uses
//! go through *types*, never bodies — the firewall). Operator typing and the
//! Numeric/Eq/Ord constraints live here.

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{CtorRef, DefId, LocalId, Res, ResolvedBodies};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{BinOp, ExprId, ExprKind, Module, PatId, PatKind, UnOp};
use rustc_hash::FxHashMap;

use crate::infer::ctx::{Constraint, InferCtx, RowTail, SolveRow, SolveTy, UnifyResult};
use crate::ty::Scheme;
use crate::{EQUALITY_ON_FUNCTION, OCCURS_CHECK, TYPE_MISMATCH};

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
                let a_ty = crate::ty::render(&self.cx.reify(a), &crate::ty::VarNames::new());
                let b_ty = crate::ty::render(&self.cx.reify(b), &crate::ty::VarNames::new());
                self.mismatch(range, format!("type mismatch in {what}: `{a_ty}` vs `{b_ty}`"));
            }
        }
    }

    /// Binds a parameter pattern to fresh local types and returns its type.
    pub fn bind_param(&mut self, pat: PatId) -> SolveTy {
        self.bind_pattern_into(pat)
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
            ExprKind::App { func, arg } => {
                let func_ty = self.infer_expr(*func);
                let arg_ty = self.infer_expr(*arg);
                let result = self.cx.fresh();
                let expected = SolveTy::arrow(arg_ty, result.clone());
                self.unify_at(node.span, &func_ty, &expected, "function application");
                result
            }
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, *lhs, *rhs, node.span),
            ExprKind::Unary { op, operand } => self.infer_unary(*op, *operand, node.span),
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
                let body_ty = self.infer_expr(*body);
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
                    // The vars already fixed by the enclosing environment must not
                    // be generalized; snapshot them before inferring the value.
                    let env_vars = self.env_free_vars();

                    let value_ty = if stmt.params.is_empty() {
                        self.infer_expr(stmt.value)
                    } else {
                        let param_tys: Vec<SolveTy> =
                            stmt.params.iter().map(|&p| self.bind_pattern_into(p)).collect();
                        let v = self.infer_expr(stmt.value);
                        SolveTy::arrows_solver(param_tys, v)
                    };

                    if is_simple_var {
                        // Generalize a simple `let v = value`: quantify the value
                        // type's free variables that are not fixed by the
                        // environment (standard let-polymorphism; sound because M2
                        // has no mutable references).
                        let vars = self.generalizable_vars(&value_ty, &env_vars);
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
            Some(Res::Builtin(b)) => match self.env.builtin_scheme(b) {
                Some(scheme) => self.cx.instantiate(&scheme),
                None => SolveTy::Error,
            },
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
        // `r.x` requires `r` to be a record with at least field `x` (open row).
        let base_ty = self.infer_expr(base);
        let field_ty = self.cx.fresh();
        let shape = self.cx.fresh_open_record(vec![(field, field_ty.clone())]);
        self.unify_at(span, &base_ty, &shape, "a record field access");
        field_ty
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

    fn infer_unary(&mut self, op: UnOp, operand: ExprId, span: fai_span::TextRange) -> SolveTy {
        let UnOp::Neg = op;
        let t = self.infer_expr(operand);
        let num = self.cx.fresh_constrained(Some(Constraint::Numeric));
        self.unify_at(span, &t, &num, "a negation");
        num
    }

    fn infer_binary(
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
                let lhs_fn = matches!(self.cx.resolve_shallow(&lt), SolveTy::Arrow(_, _));
                let rhs_fn = matches!(self.cx.resolve_shallow(&rt), SolveTy::Arrow(_, _));
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
                    if matches!(self.cx.resolve_shallow(&eq), SolveTy::Arrow(_, _)) {
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
            BinOp::Concat => {
                self.unify_at(span, &lt, &SolveTy::string(), "a `++` operand");
                self.unify_at(span, &rt, &SolveTy::string(), "a `++` operand");
                SolveTy::string()
            }
            BinOp::Cons => {
                let list = SolveTy::list(lt.clone());
                self.unify_at(span, &rt, &list, "a `::` tail");
                list
            }
            BinOp::Pipe => {
                // a |> f : f a, with f : a -> b
                let result = self.cx.fresh();
                let f = SolveTy::arrow(lt.clone(), result.clone());
                self.unify_at(span, &rt, &f, "a `|>` function");
                result
            }
            BinOp::Compose => {
                // f >> g : a -> c, with f : a -> b, g : b -> c
                let a = self.cx.fresh();
                let b = self.cx.fresh();
                let c = self.cx.fresh();
                self.unify_at(span, &lt, &SolveTy::arrow(a.clone(), b.clone()), "a `>>` left");
                self.unify_at(span, &rt, &SolveTy::arrow(b, c.clone()), "a `>>` right");
                SolveTy::arrow(a, c)
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
                SolveTy::Arrow(from, to) => {
                    self.check_pattern(a, &from);
                    cur = *to;
                }
                _ => {
                    arity_ok = false;
                    self.check_pattern(a, &SolveTy::Error);
                }
            }
        }
        if matches!(self.cx.resolve_shallow(&cur), SolveTy::Arrow(_, _)) {
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

    /// The solver variables currently fixed by the environment (all in-scope
    /// locals), which must not be generalized by a nested `let`.
    fn env_free_vars(&self) -> rustc_hash::FxHashSet<crate::ty::TyVarId> {
        let mut set = rustc_hash::FxHashSet::default();
        for binding in self.locals.values() {
            match binding {
                LocalBinding::Mono(t) => self.collect_free_vars(t, &mut set),
                // A poly local's quantified vars are bound, not free; only its
                // free (non-quantified) vars constrain generalization.
                LocalBinding::Poly { vars, ty } => {
                    let mut local = rustc_hash::FxHashSet::default();
                    self.collect_free_vars(ty, &mut local);
                    for v in vars {
                        local.remove(v);
                    }
                    set.extend(local);
                }
            }
        }
        set
    }

    /// The free variables of `ty` that may be generalized: those not fixed by the
    /// environment and not still carrying a constraint.
    ///
    /// A *constrained* variable (Numeric/Eq/Ord) is left ungeneralized so it can
    /// be resolved or defaulted later — e.g. `let inc = fun a -> a + 1` keeps its
    /// numeric variable monomorphic so it defaults to `Int` (giving
    /// `Int -> Int`), rather than generalizing to `'a -> 'a`.
    fn generalizable_vars(
        &self,
        ty: &SolveTy,
        env_vars: &rustc_hash::FxHashSet<crate::ty::TyVarId>,
    ) -> Vec<crate::ty::TyVarId> {
        let mut free = rustc_hash::FxHashSet::default();
        self.collect_free_vars(ty, &mut free);
        let mut vars: Vec<crate::ty::TyVarId> = free
            .into_iter()
            .filter(|v| !env_vars.contains(v))
            .filter(|v| self.cx.pending_constraint(&SolveTy::Var(*v)).is_none())
            .collect();
        vars.sort();
        vars
    }

    /// Collects the free (unbound) solver variables of `ty`, following the
    /// current substitution.
    fn collect_free_vars(&self, ty: &SolveTy, out: &mut rustc_hash::FxHashSet<crate::ty::TyVarId>) {
        match self.cx.resolve_shallow(ty) {
            SolveTy::Var(v) => {
                out.insert(v);
            }
            SolveTy::App(f, a) | SolveTy::Arrow(f, a) => {
                self.collect_free_vars(&f, out);
                self.collect_free_vars(&a, out);
            }
            SolveTy::Tuple(elems) => {
                for e in &elems {
                    self.collect_free_vars(e, out);
                }
            }
            SolveTy::Record(row) => {
                for (_, t) in &row.fields {
                    self.collect_free_vars(t, out);
                }
            }
            SolveTy::Con(_) | SolveTy::Adt(_) | SolveTy::Unit | SolveTy::Error => {}
        }
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
            SolveTy::App(Box::new(subst(cx, &f, mapping)), Box::new(subst(cx, &a, mapping)))
        }
        SolveTy::Arrow(f, a) => SolveTy::arrow(subst(cx, &f, mapping), subst(cx, &a, mapping)),
        SolveTy::Tuple(elems) => {
            SolveTy::Tuple(elems.iter().map(|e| subst(cx, e, mapping)).collect())
        }
        SolveTy::Record(row) => SolveTy::Record(SolveRow {
            fields: row.fields.iter().map(|(l, t)| (*l, subst(cx, t, mapping))).collect(),
            tail: row.tail,
        }),
        other @ (SolveTy::Con(_) | SolveTy::Adt(_) | SolveTy::Unit | SolveTy::Error) => other,
    }
}
