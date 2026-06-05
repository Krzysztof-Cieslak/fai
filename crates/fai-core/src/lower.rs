//! Lowering the surface AST to Core IR.
//!
//! [`core`] is a per-definition salsa query: it lowers one top-level binding
//! (its parameters and body) into a [`LoweredDef`], lambda-lifting nested
//! lambdas and desugaring operators, pipes, composition, and short-circuit
//! booleans. Constructs outside the M3 native subset (`Float`, `Char`, tuples,
//! lists, records, `match`) are reported as [`crate::UNSUPPORTED_NATIVE`] and
//! lowered to an error placeholder, so an unused such definition never blocks a
//! build (only the reachable closure is lowered).

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{DefId, LocalId, Res, ResolvedBodies, resolve};
use fai_span::{Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{BinOp, ExprId, ExprKind, Module, PatId, PatKind, UnOp};
use fai_types::{BodyTypes, Ty, body_types};
use rustc_hash::FxHashSet;

use crate::UNSUPPORTED_NATIVE;
use crate::ir::{CExpr, CoreFn, ExprKind as K, FnId, Lit, LoweredDef, Prim};

/// Lowers `name`'s definition in `file` to Core IR.
#[salsa::tracked]
pub fn core(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let parsed = fai_syntax::parse(db, file);
    let resolved = resolve(db, file);
    let types = body_types(db, file, name);
    let def = DefId::new(file.source(db), name);

    let Some((params, body)) = binding_body(&parsed.module, name) else {
        return Arc::new(LoweredDef {
            def,
            fns: vec![CoreFn { params: Vec::new(), captures: Vec::new(), body: error_expr() }],
        });
    };

    let mut lowerer = Lowerer {
        db,
        file,
        module: &parsed.module,
        resolved: &resolved,
        types: &types,
        prelude: prelude_file(db),
        next_local: first_free_local(&resolved),
        fns: vec![placeholder_fn()],
    };

    let param_locals: Vec<LocalId> = params.iter().map(|&p| lowerer.param_local(p)).collect();
    let body = lowerer.lower_expr(body);
    lowerer.fns[0] = CoreFn { params: param_locals, captures: Vec::new(), body };
    Arc::new(LoweredDef { def, fns: lowerer.fns })
}

/// The body item (params + body expr) of a definition.
fn binding_body(module: &Module, name: Symbol) -> Option<(Vec<PatId>, ExprId)> {
    module.items.iter().find_map(|it| match &it.kind {
        fai_syntax::ast::ItemKind::Binding { name: n, params, body, .. } if *n == name => {
            Some((params.clone(), *body))
        }
        _ => None,
    })
}

/// The embedded prelude file, if loaded (for resolving prelude-defined helpers).
fn prelude_file(db: &dyn Db) -> Option<SourceFile> {
    db.all_source_files().into_iter().find(|f| fai_types::prelude::is_prelude_path(f.path(db)))
}

/// The first `LocalId` index not used by resolution (so synthesized binders —
/// e.g. composition's parameter — never collide with real locals).
fn first_free_local(resolved: &ResolvedBodies) -> usize {
    resolved.pat_locals.values().map(|l| l.index() + 1).max().unwrap_or(0)
}

fn placeholder_fn() -> CoreFn {
    CoreFn { params: Vec::new(), captures: Vec::new(), body: error_expr() }
}

fn error_expr() -> CExpr {
    CExpr::new(K::Error, Ty::Error)
}

/// The per-definition lowering state.
struct Lowerer<'a> {
    db: &'a dyn Db,
    file: SourceFile,
    module: &'a Module,
    resolved: &'a ResolvedBodies,
    types: &'a BodyTypes,
    prelude: Option<SourceFile>,
    next_local: usize,
    fns: Vec<CoreFn>,
}

impl Lowerer<'_> {
    fn span(&self, range: TextRange) -> Span {
        Span::new(self.file.source(self.db), range)
    }

    fn ty_of(&self, expr: ExprId) -> Ty {
        self.types.get(expr).cloned().unwrap_or(Ty::Error)
    }

    /// Reports an unsupported construct and yields an error placeholder.
    fn unsupported(&self, range: TextRange, feature: &str) -> CExpr {
        emit(
            self.db,
            Diagnostic::error(
                UNSUPPORTED_NATIVE,
                format!("{feature} is not supported by the native backend yet"),
                self.span(range),
            )
            .with_help(
                "the M3 native subset is Int, Bool, String, functions, let, if, and arithmetic",
            ),
        );
        error_expr()
    }

    fn fresh_local(&mut self) -> LocalId {
        let id = LocalId::from_index(self.next_local);
        self.next_local += 1;
        id
    }

    /// Appends a lifted function, returning its id.
    fn push_fn(&mut self, f: CoreFn) -> FnId {
        let id = FnId(u32::try_from(self.fns.len()).expect("function-id overflow"));
        self.fns.push(f);
        id
    }

    /// The local bound by a parameter/let pattern (var or wildcard only).
    fn param_local(&mut self, pat: PatId) -> LocalId {
        match self.module.pat(pat).kind {
            PatKind::Var(_) | PatKind::Wildcard => {
                self.resolved.local_of(pat).unwrap_or_else(|| self.fresh_local())
            }
            PatKind::Paren(inner) => self.param_local(inner),
            _ => {
                self.unsupported(self.module.pat(pat).span, "this pattern");
                self.fresh_local()
            }
        }
    }

    fn lower_expr(&mut self, expr: ExprId) -> CExpr {
        let ty = self.ty_of(expr);
        let node = self.module.expr(expr);
        let kind = match &node.kind {
            ExprKind::Int(raw) => {
                K::Lit(Lit::Int(crate::lit::decode_int(raw.as_str()).unwrap_or(0)))
            }
            ExprKind::String(raw) => K::Lit(Lit::Str(crate::lit::decode_string(raw.as_str()))),
            ExprKind::Unit => K::Lit(Lit::Unit),
            ExprKind::Float(_) => return self.unsupported(node.span, "Float"),
            ExprKind::Char(_) => return self.unsupported(node.span, "the Char type"),
            ExprKind::Var(_) => self.lower_ref(expr).kind,
            ExprKind::Field { .. } => self.lower_ref(expr).kind,
            ExprKind::App { .. } => {
                let (head, args) = self.app_spine(expr);
                return self.lower_application(head, &args, ty);
            }
            ExprKind::Binary { op, lhs, rhs } => return self.lower_binary(*op, *lhs, *rhs, ty),
            ExprKind::Unary { op, operand } => {
                let UnOp::Neg = op;
                let zero = CExpr::new(K::Lit(Lit::Int(0)), Ty::int());
                let operand = self.lower_expr(*operand);
                K::Prim { op: Prim::IntSub, args: vec![zero, operand] }
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                let cond = Box::new(self.lower_expr(*cond));
                let then = Box::new(self.lower_expr(*then_branch));
                let els = Box::new(self.lower_expr(*else_branch));
                K::If { cond, then, els }
            }
            ExprKind::Lambda { params, body } => {
                return self.lower_lambda(params, *body, ty);
            }
            ExprKind::Block { stmts, tail } => return self.lower_block(stmts, *tail),
            ExprKind::Paren(inner) => return self.lower_expr(*inner),
            ExprKind::Tuple(_) => return self.unsupported(node.span, "tuples"),
            ExprKind::List(_) => return self.unsupported(node.span, "lists"),
            ExprKind::Error => K::Error,
        };
        CExpr::new(kind, ty)
    }

    /// Lowers a name/field reference (in value position).
    fn lower_ref(&mut self, expr: ExprId) -> CExpr {
        let ty = self.ty_of(expr);
        let span = self.module.expr(expr).span;
        match self.resolved.get(expr) {
            Some(Res::Local(id)) => CExpr::new(K::Local(id), ty),
            Some(Res::Def(def)) => CExpr::new(K::Global(def), ty),
            Some(Res::Builtin(name)) => self.lower_builtin_ref(name, ty, span),
            Some(Res::Error) | None => error_expr(),
        }
    }

    /// Lowers a builtin reference used as a value: booleans become literals,
    /// primitives are eta-expanded into a closure, and prelude-defined helpers
    /// become global references.
    fn lower_builtin_ref(&mut self, name: Symbol, ty: Ty, span: TextRange) -> CExpr {
        match name.as_str() {
            "true" => return CExpr::new(K::Lit(Lit::Bool(true)), ty),
            "false" => return CExpr::new(K::Lit(Lit::Bool(false)), ty),
            "identity" | "const" | "notEqual" => {
                if let Some(prelude) = self.prelude {
                    return CExpr::new(K::Global(DefId::new(prelude.source(self.db), name)), ty);
                }
                return self.unsupported(span, "this prelude function");
            }
            _ => {}
        }
        if let Some(prim) = Prim::from_builtin(name.as_str()) {
            return self.eta_expand_prim(prim, ty);
        }
        self.unsupported(span, "this prelude function")
    }

    /// Builds a closure `fun a0 … -> prim a0 …` for a primitive used as a value.
    fn eta_expand_prim(&mut self, prim: Prim, ty: Ty) -> CExpr {
        let params: Vec<LocalId> = (0..prim.arity()).map(|_| self.fresh_local()).collect();
        let args = params.iter().map(|&p| CExpr::new(K::Local(p), Ty::Error)).collect();
        let body = CExpr::new(K::Prim { op: prim, args }, Ty::Error);
        let fn_id = self.push_fn(CoreFn { params, captures: Vec::new(), body });
        CExpr::new(K::MakeClosure { func: fn_id, captures: Vec::new() }, ty)
    }

    /// Collects an application spine `f a b c` into its head and arguments.
    fn app_spine(&self, expr: ExprId) -> (ExprId, Vec<ExprId>) {
        let mut args = Vec::new();
        let mut cur = expr;
        while let ExprKind::App { func, arg } = &self.module.expr(cur).kind {
            args.push(*arg);
            cur = *func;
        }
        args.reverse();
        (cur, args)
    }

    /// Lowers an application of `head` to `args`. A saturated primitive head
    /// becomes a `Prim`; everything else routes through `apply_n`.
    fn lower_application(&mut self, head: ExprId, args: &[ExprId], ty: Ty) -> CExpr {
        if let Some(Res::Builtin(name)) = self.resolved.get(head)
            && let Some(prim) = Prim::from_builtin(name.as_str())
            && prim.arity() == args.len()
        {
            let args = args.iter().map(|&a| self.lower_expr(a)).collect();
            return CExpr::new(K::Prim { op: prim, args }, ty);
        }
        let func = Box::new(self.lower_expr(head));
        let args = args.iter().map(|&a| self.lower_expr(a)).collect();
        CExpr::new(K::App { func, args }, ty)
    }

    fn lower_binary(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId, ty: Ty) -> CExpr {
        let prim = match op {
            BinOp::Add => Some(Prim::IntAdd),
            BinOp::Sub => Some(Prim::IntSub),
            BinOp::Mul => Some(Prim::IntMul),
            BinOp::Div => Some(Prim::IntDiv),
            BinOp::Rem => Some(Prim::IntRem),
            BinOp::Lt => Some(Prim::IntLt),
            BinOp::Le => Some(Prim::IntLe),
            BinOp::Gt => Some(Prim::IntGt),
            BinOp::Ge => Some(Prim::IntGe),
            BinOp::Eq => Some(Prim::Eq),
            BinOp::Concat => Some(Prim::StrConcat),
            _ => None,
        };
        if let Some(op) = prim {
            let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
            return CExpr::new(K::Prim { op, args }, ty);
        }
        match op {
            BinOp::Ne => {
                let eq = CExpr::new(
                    K::Prim {
                        op: Prim::Eq,
                        args: vec![self.lower_expr(lhs), self.lower_expr(rhs)],
                    },
                    Ty::bool(),
                );
                CExpr::new(K::Prim { op: Prim::Not, args: vec![eq] }, ty)
            }
            // `a && b` ≡ `if a then b else false`; `a || b` ≡ `if a then true else b`.
            BinOp::And => {
                let cond = Box::new(self.lower_expr(lhs));
                let then = Box::new(self.lower_expr(rhs));
                let els = Box::new(CExpr::new(K::Lit(Lit::Bool(false)), Ty::bool()));
                CExpr::new(K::If { cond, then, els }, ty)
            }
            BinOp::Or => {
                let cond = Box::new(self.lower_expr(lhs));
                let then = Box::new(CExpr::new(K::Lit(Lit::Bool(true)), Ty::bool()));
                let els = Box::new(self.lower_expr(rhs));
                CExpr::new(K::If { cond, then, els }, ty)
            }
            // `a |> f` ≡ `f a`.
            BinOp::Pipe => self.lower_application(rhs, &[lhs], ty),
            // `f >> g` ≡ `fun x -> g (f x)`.
            BinOp::Compose => self.lower_compose(lhs, rhs, ty),
            BinOp::Cons => self.unsupported(self.module.expr(lhs).span, "the list cons `::`"),
            _ => error_expr(),
        }
    }

    /// Lowers `f >> g` to a lifted `fun x -> g (f x)`.
    fn lower_compose(&mut self, lhs: ExprId, rhs: ExprId, ty: Ty) -> CExpr {
        let (param_ty, result_ty) = match &ty {
            Ty::Arrow(a, b) => ((**a).clone(), (**b).clone()),
            _ => (Ty::Error, Ty::Error),
        };
        let x = self.fresh_local();
        let f = self.lower_expr(lhs);
        let g = self.lower_expr(rhs);
        let xref = CExpr::new(K::Local(x), param_ty);
        let fx = CExpr::new(K::App { func: Box::new(f), args: vec![xref] }, Ty::Error);
        let gfx = CExpr::new(K::App { func: Box::new(g), args: vec![fx] }, result_ty);
        self.lift_lambda(vec![x], gfx, ty)
    }

    fn lower_lambda(&mut self, params: &[PatId], body: ExprId, ty: Ty) -> CExpr {
        let param_locals: Vec<LocalId> = params.iter().map(|&p| self.param_local(p)).collect();
        let body = self.lower_expr(body);
        self.lift_lambda(param_locals, body, ty)
    }

    /// Lifts a lambda (its parameters and lowered body) to a top-level function,
    /// computing its captures, and yields a `MakeClosure` at its position.
    fn lift_lambda(&mut self, params: Vec<LocalId>, body: CExpr, ty: Ty) -> CExpr {
        let captures = captures_of(&body, &params);
        let fn_id = self.push_fn(CoreFn { params, captures: captures.clone(), body });
        CExpr::new(K::MakeClosure { func: fn_id, captures }, ty)
    }

    fn lower_block(&mut self, stmts: &[fai_syntax::ast::LetStmt], tail: ExprId) -> CExpr {
        self.lower_stmts(stmts, tail)
    }

    fn lower_stmts(&mut self, stmts: &[fai_syntax::ast::LetStmt], tail: ExprId) -> CExpr {
        let Some((stmt, rest)) = stmts.split_first() else {
            return self.lower_expr(tail);
        };
        // A local function binding (`let f x = …`) binds a closure; a plain
        // `let v = …` binds its value.
        let value = if stmt.params.is_empty() {
            self.lower_expr(stmt.value)
        } else {
            let value_ty = self.ty_of(stmt.value);
            let param_locals: Vec<LocalId> =
                stmt.params.iter().map(|&p| self.param_local(p)).collect();
            let body = self.lower_expr(stmt.value);
            self.lift_lambda(param_locals, body, value_ty)
        };
        let local = self.param_local(stmt.pat);
        let body = self.lower_stmts(rest, tail);
        let ty = body.ty.clone();
        CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, ty)
    }
}

/// The captured variables of a lifted function: the free locals of its body that
/// are not its parameters. Returned sorted for determinism.
fn captures_of(body: &CExpr, params: &[LocalId]) -> Vec<LocalId> {
    let mut bound: FxHashSet<LocalId> = params.iter().copied().collect();
    let mut free = FxHashSet::default();
    collect_free(body, &mut bound, &mut free);
    let mut captures: Vec<LocalId> = free.into_iter().collect();
    captures.sort_by_key(|l| l.index());
    captures
}

/// Collects the free locals of `expr` (those not in `bound`).
fn collect_free(expr: &CExpr, bound: &mut FxHashSet<LocalId>, out: &mut FxHashSet<LocalId>) {
    match &expr.kind {
        K::Local(id) => {
            if !bound.contains(id) {
                out.insert(*id);
            }
        }
        K::MakeClosure { captures, .. } => {
            for c in captures {
                if !bound.contains(c) {
                    out.insert(*c);
                }
            }
        }
        K::Lit(_) | K::Global(_) | K::Error => {}
        K::Prim { args, .. } => {
            for a in args {
                collect_free(a, bound, out);
            }
        }
        K::App { func, args } => {
            collect_free(func, bound, out);
            for a in args {
                collect_free(a, bound, out);
            }
        }
        K::If { cond, then, els } => {
            collect_free(cond, bound, out);
            collect_free(then, bound, out);
            collect_free(els, bound, out);
        }
        K::Let { local, value, body } => {
            collect_free(value, bound, out);
            let fresh = bound.insert(*local);
            collect_free(body, bound, out);
            if fresh {
                bound.remove(local);
            }
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            if !bound.contains(local) {
                out.insert(*local);
            }
            collect_free(body, bound, out);
        }
    }
}
