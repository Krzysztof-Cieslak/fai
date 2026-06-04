//! Algorithm W over a body, producing a principal solver type.
//!
//! The walker resolves each reference through the [`ResolvedBodies`] map: locals
//! get their bound (monomorphic) type; definitions and builtins are instantiated
//! from a [`Scheme`] supplied by the caller (so cross-def and cross-module uses
//! go through *types*, never bodies — the firewall). Operator typing and the
//! Numeric/Eq/Ord constraints live here.

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{DefId, LocalId, Res, ResolvedBodies};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{BinOp, ExprId, ExprKind, Module, PatId, PatKind, UnOp};
use rustc_hash::FxHashMap;

use crate::infer::ctx::{Constraint, InferCtx, SolveTy, UnifyResult};
use crate::ty::Scheme;
use crate::{EQUALITY_ON_FUNCTION, OCCURS_CHECK, TYPE_MISMATCH, UNSUPPORTED_FIELD_ACCESS};

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
}

/// The per-body inference walker.
pub struct Walker<'a, E: Env> {
    pub db: &'a dyn Db,
    pub file: SourceFile,
    pub module: &'a Module,
    pub resolved: &'a ResolvedBodies,
    pub cx: &'a mut InferCtx,
    pub env: &'a mut E,
    /// Monomorphic types of locals bound so far in the current body.
    pub locals: FxHashMap<LocalId, SolveTy>,
}

impl<E: Env> Walker<'_, E> {
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

    /// Infers the type of an expression.
    pub fn infer_expr(&mut self, expr: ExprId) -> SolveTy {
        let node = self.module.expr(expr);
        match &node.kind {
            ExprKind::Int(_) => self.cx.fresh_constrained(Some(Constraint::Numeric)),
            ExprKind::Float(_) => SolveTy::Con(crate::ty::Con::Float),
            ExprKind::String(_) => SolveTy::string(),
            ExprKind::Char(_) => SolveTy::Con(crate::ty::Con::Char),
            ExprKind::Unit => SolveTy::Unit,
            ExprKind::Var(name) => self.infer_ref(expr, *name, node.span),
            ExprKind::Field { .. } => self.infer_field(expr, node.span),
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
            ExprKind::Block { stmts, tail } => {
                for stmt in stmts {
                    // A local function binding: build an arrow over its params.
                    let value_ty = if stmt.params.is_empty() {
                        self.infer_expr(stmt.value)
                    } else {
                        let param_tys: Vec<SolveTy> =
                            stmt.params.iter().map(|&p| self.bind_pattern_into(p)).collect();
                        let v = self.infer_expr(stmt.value);
                        SolveTy::arrows_solver(param_tys, v)
                    };
                    // Bind the pattern to the value type (monomorphic for tuple
                    // patterns; var patterns are also bound monomorphically here —
                    // local let-generalization is applied for simple var lets by
                    // re-instantiation is out of M2 scope for blocks; treat as
                    // monomorphic to keep inference predictable).
                    self.bind_pattern_to(stmt.pat, &value_ty);
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
            Some(Res::Local(local)) => self.locals.get(&local).cloned().unwrap_or(SolveTy::Error),
            Some(Res::Def(def)) => self.instantiate_def(def),
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

    fn infer_field(&mut self, expr: ExprId, span: fai_span::TextRange) -> SolveTy {
        // A qualified Foo.bar resolved to a Def/Builtin in resolution; if so, use
        // it. Otherwise it's record field access, unsupported in M2.
        match self.resolved.get(expr) {
            Some(Res::Def(def)) => self.instantiate_def(def),
            Some(Res::Builtin(b)) => match self.env.builtin_scheme(b) {
                Some(scheme) => self.cx.instantiate(&scheme),
                None => SolveTy::Error,
            },
            Some(Res::Error) => SolveTy::Error,
            _ => {
                emit(
                    self.db,
                    Diagnostic::error(
                        UNSUPPORTED_FIELD_ACCESS,
                        "record field access is not supported yet (records land in M4)",
                        self.span(span),
                    ),
                );
                SolveTy::Error
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

    // Pattern binding that records local slot types. Resolution assigns LocalIds
    // in the same left-to-right order we traverse here, so we mirror that order
    // with a shared counter via `next_local`.
    fn bind_pattern_into(&mut self, pat: PatId) -> SolveTy {
        let ty = self.fresh_pattern_type(pat);
        self.bind_pattern_to(pat, &ty);
        ty
    }

    fn fresh_pattern_type(&mut self, pat: PatId) -> SolveTy {
        match &self.module.pat(pat).kind {
            PatKind::Tuple(elems) => {
                SolveTy::Tuple(elems.iter().map(|&e| self.fresh_pattern_type(e)).collect())
            }
            PatKind::Paren(inner) => self.fresh_pattern_type(*inner),
            PatKind::Unit => SolveTy::Unit,
            PatKind::Error => SolveTy::Error,
            PatKind::Var(_) | PatKind::Wildcard => self.cx.fresh(),
        }
    }

    fn bind_pattern_to(&mut self, pat: PatId, ty: &SolveTy) {
        match &self.module.pat(pat).kind {
            PatKind::Var(_) | PatKind::Wildcard => {
                if let Some(slot) = self.resolved.local_of(pat) {
                    self.locals.insert(slot, ty.clone());
                }
            }
            PatKind::Tuple(elems) => {
                let resolved = self.cx.resolve_shallow(ty);
                if let SolveTy::Tuple(parts) = resolved
                    && parts.len() == elems.len()
                {
                    for (&e, p) in elems.iter().zip(parts) {
                        self.bind_pattern_to(e, &p);
                    }
                    return;
                }
                // Shape unknown/mismatched: bind each to a fresh type and unify.
                let part_tys: Vec<SolveTy> = elems.iter().map(|_| self.cx.fresh()).collect();
                let tuple = SolveTy::Tuple(part_tys.clone());
                let _ = self.cx.unify(ty, &tuple);
                for (&e, p) in elems.iter().zip(part_tys) {
                    self.bind_pattern_to(e, &p);
                }
            }
            PatKind::Paren(inner) => self.bind_pattern_to(*inner, ty),
            PatKind::Unit | PatKind::Error => {}
        }
    }
}
