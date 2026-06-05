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
use fai_resolve::{CtorRef, DefId, LocalId, Res, ResolvedBodies, resolve, type_decls};
use fai_span::{Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{BinOp, ExprId, ExprKind, MatchArm, Module, PatId, PatKind, UnOp};
use fai_types::{BodyTypes, Con, Ty, body_types};
use rustc_hash::FxHashSet;

use crate::UNSUPPORTED_NATIVE;
use crate::ir::{CExpr, CoreFn, ExprKind as K, FnId, Lit, LoweredDef, Prim};

/// The built-in `List` constructor tags: `[]` is `Nil`, `x :: xs` is `Cons`.
const NIL_TAG: u32 = 0;
const CONS_TAG: u32 = 1;

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
            ExprKind::Float(raw) => K::Lit(Lit::Float(crate::lit::decode_float(raw.as_str()))),
            ExprKind::Char(_) => return self.unsupported(node.span, "the Char type"),
            ExprKind::Var(_) => return self.lower_ref(expr),
            ExprKind::Field { .. } => return self.lower_ref(expr),
            ExprKind::App { .. } => {
                let (head, args) = self.app_spine(expr);
                return self.lower_application(head, &args, ty);
            }
            ExprKind::Binary { op, lhs, rhs } => return self.lower_binary(*op, *lhs, *rhs, ty),
            ExprKind::Unary { op, operand } => {
                let UnOp::Neg = op;
                let is_float = matches!(self.ty_of(*operand), Ty::Con(Con::Float));
                let operand = self.lower_expr(*operand);
                if is_float {
                    let zero = CExpr::new(K::Lit(Lit::Float(0f64.to_bits())), Ty::Con(Con::Float));
                    K::Prim { op: Prim::FloatSub, args: vec![zero, operand] }
                } else {
                    let zero = CExpr::new(K::Lit(Lit::Int(0)), Ty::int());
                    K::Prim { op: Prim::IntSub, args: vec![zero, operand] }
                }
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
            ExprKind::Match { scrutinee, arms } => return self.lower_match(*scrutinee, arms, ty),
            ExprKind::Block { stmts, tail } => return self.lower_block(stmts, *tail),
            ExprKind::Paren(inner) => return self.lower_expr(*inner),
            ExprKind::Tuple(elems) => {
                let args = elems.iter().map(|&e| self.lower_expr(e)).collect();
                K::MakeData { tag: 0, args }
            }
            ExprKind::List(elems) => return self.lower_list(elems, ty),
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
            Some(Res::Ctor(ctor)) => self.lower_ctor_value(ctor, ty),
            Some(Res::Builtin(name)) => self.lower_builtin_ref(name, ty, span),
            Some(Res::Error) | None => error_expr(),
        }
    }

    /// The tag and arity of a data constructor, from its declaring file.
    fn ctor_tag_arity(&self, ctor: CtorRef) -> Option<(u32, usize)> {
        let file = self.db.source_file(ctor.file)?;
        let decls = type_decls(self.db, file);
        let info = decls.ctor(ctor.name)?;
        Some((info.tag, info.arity))
    }

    /// Lowers a constructor used as a *value*: a nullary constructor is its data
    /// immediately; an n-ary one becomes a closure `fun a0 … -> MakeData …`.
    fn lower_ctor_value(&mut self, ctor: CtorRef, ty: Ty) -> CExpr {
        let (tag, arity) = self.ctor_tag_arity(ctor).unwrap_or((0, 0));
        if arity == 0 {
            return CExpr::new(K::MakeData { tag, args: Vec::new() }, ty);
        }
        let params: Vec<LocalId> = (0..arity).map(|_| self.fresh_local()).collect();
        let args = params.iter().map(|&p| CExpr::new(K::Local(p), Ty::Error)).collect();
        let body = CExpr::new(K::MakeData { tag, args }, Ty::Error);
        let fn_id = self.push_fn(CoreFn { params, captures: Vec::new(), body });
        CExpr::new(K::MakeClosure { func: fn_id, captures: Vec::new() }, ty)
    }

    /// Lowers a list literal `[a, b, …]` to nested `Cons`/`Nil` data.
    fn lower_list(&mut self, elems: &[ExprId], ty: Ty) -> CExpr {
        let mut list = CExpr::new(K::MakeData { tag: NIL_TAG, args: Vec::new() }, ty.clone());
        for &e in elems.iter().rev() {
            let head = self.lower_expr(e);
            list = CExpr::new(K::MakeData { tag: CONS_TAG, args: vec![head, list] }, ty.clone());
        }
        list
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
        // A saturated constructor application builds its data directly.
        if let Some(Res::Ctor(ctor)) = self.resolved.get(head)
            && let Some((tag, arity)) = self.ctor_tag_arity(ctor)
            && arity == args.len()
        {
            let args = args.iter().map(|&a| self.lower_expr(a)).collect();
            return CExpr::new(K::MakeData { tag, args }, ty);
        }
        let func = Box::new(self.lower_expr(head));
        let args = args.iter().map(|&a| self.lower_expr(a)).collect();
        CExpr::new(K::App { func, args }, ty)
    }

    fn lower_binary(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId, ty: Ty) -> CExpr {
        let float = matches!(self.ty_of(lhs), Ty::Con(Con::Float));
        // Arithmetic: pick the Int or Float primitive from the operand type.
        let arith = match (op, float) {
            (BinOp::Add, false) => Some(Prim::IntAdd),
            (BinOp::Sub, false) => Some(Prim::IntSub),
            (BinOp::Mul, false) => Some(Prim::IntMul),
            (BinOp::Div, false) => Some(Prim::IntDiv),
            (BinOp::Rem, false) => Some(Prim::IntRem),
            (BinOp::Add, true) => Some(Prim::FloatAdd),
            (BinOp::Sub, true) => Some(Prim::FloatSub),
            (BinOp::Mul, true) => Some(Prim::FloatMul),
            (BinOp::Div, true) => Some(Prim::FloatDiv),
            _ => None,
        };
        if let Some(op) = arith {
            let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
            return CExpr::new(K::Prim { op, args }, ty);
        }
        // Comparison: Int/Float primitives, else structural `Compare` against 0.
        if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
            return self.lower_comparison(op, lhs, rhs, float, ty);
        }
        if matches!(op, BinOp::Eq) {
            let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
            return CExpr::new(K::Prim { op: Prim::Eq, args }, ty);
        }
        if matches!(op, BinOp::Concat) {
            let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
            return CExpr::new(K::Prim { op: Prim::StrConcat, args }, ty);
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
            // `x :: xs` builds a `Cons` cell.
            BinOp::Cons => {
                let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
                CExpr::new(K::MakeData { tag: CONS_TAG, args }, ty)
            }
            _ => error_expr(),
        }
    }

    /// Lowers `a < b` (and `<=`/`>`/`>=`): an Int/Float primitive, or structural
    /// `Compare` (returning `-1`/`0`/`1`) tested against `0`.
    fn lower_comparison(
        &mut self,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
        float: bool,
        ty: Ty,
    ) -> CExpr {
        let int_prim = match op {
            BinOp::Lt => Prim::IntLt,
            BinOp::Le => Prim::IntLe,
            BinOp::Gt => Prim::IntGt,
            _ => Prim::IntGe,
        };
        if matches!(self.ty_of(lhs), Ty::Con(Con::Int)) || self.ty_of(lhs) == Ty::Error {
            let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
            return CExpr::new(K::Prim { op: int_prim, args }, ty);
        }
        if float {
            let fprim = match op {
                BinOp::Lt => Prim::FloatLt,
                BinOp::Le => Prim::FloatLe,
                BinOp::Gt => Prim::FloatGt,
                _ => Prim::FloatGe,
            };
            let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
            return CExpr::new(K::Prim { op: fprim, args }, ty);
        }
        // Structural: `compare a b <op> 0`.
        let cmp = CExpr::new(
            K::Prim { op: Prim::Compare, args: vec![self.lower_expr(lhs), self.lower_expr(rhs)] },
            Ty::int(),
        );
        let zero = CExpr::new(K::Lit(Lit::Int(0)), Ty::int());
        CExpr::new(K::Prim { op: int_prim, args: vec![cmp, zero] }, ty)
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

    /// Lowers `match scrutinee with | p -> b …` to a decision over the scrutinee:
    /// the value is bound once, then each arm is tried in order. A failed arm
    /// falls through to the next; exhaustiveness (checked in the type phase)
    /// guarantees the final fallthrough is unreachable.
    fn lower_match(&mut self, scrutinee: ExprId, arms: &[MatchArm], ty: Ty) -> CExpr {
        let sval = self.lower_expr(scrutinee);
        let s = self.fresh_local();
        let mut chain = error_expr();
        for arm in arms.iter().rev() {
            let body = self.lower_expr(arm.body);
            chain = self.compile_pattern(s, arm.pat, body, chain);
        }
        CExpr::new(K::Let { local: s, value: Box::new(sval), body: Box::new(chain) }, ty)
    }

    /// Compiles a single pattern match of `value_local` against `pat`: on success
    /// it binds the pattern's variables and evaluates `success`; otherwise `fail`.
    fn compile_pattern(
        &mut self,
        value_local: LocalId,
        pat: PatId,
        success: CExpr,
        fail: CExpr,
    ) -> CExpr {
        let value = || CExpr::new(K::Local(value_local), Ty::Error);
        match &self.module.pat(pat).kind {
            PatKind::Wildcard | PatKind::Error => success,
            PatKind::Var(_) => {
                let local = self.resolved.local_of(pat).unwrap_or_else(|| self.fresh_local());
                CExpr::new(
                    K::Let { local, value: Box::new(value()), body: Box::new(success) },
                    Ty::Error,
                )
            }
            PatKind::Paren(inner) => {
                let inner = *inner;
                self.compile_pattern(value_local, inner, success, fail)
            }
            PatKind::Unit => success,
            PatKind::Bool(b) => self.test_lit(value(), Lit::Bool(*b), success, fail),
            PatKind::Int(raw) => {
                let n = crate::lit::decode_int(raw.as_str()).unwrap_or(0);
                self.test_lit(value(), Lit::Int(n), success, fail)
            }
            PatKind::Float(raw) => {
                let bits = crate::lit::decode_float(raw.as_str());
                self.test_lit(value(), Lit::Float(bits), success, fail)
            }
            PatKind::String(raw) => {
                let bytes = crate::lit::decode_string(raw.as_str());
                self.test_lit(value(), Lit::Str(bytes), success, fail)
            }
            PatKind::Char(_) => self.unsupported(self.module.pat(pat).span, "a character pattern"),
            PatKind::Tuple(elems) => {
                let elems = elems.clone();
                self.compile_fields(value_local, &elems, success, &fail)
            }
            PatKind::List(elems) => {
                let elems = elems.clone();
                self.compile_list_pattern(value_local, &elems, success, fail)
            }
            PatKind::Cons { head, tail } => {
                let fields = [*head, *tail];
                let bind = self.compile_fields(value_local, &fields, success, &fail);
                self.test_tag(value_local, CONS_TAG, bind, fail)
            }
            PatKind::Constructor { args, .. } => {
                let tag = match self.resolved.pat_res(pat) {
                    Some(Res::Ctor(ctor)) => self.ctor_tag_arity(ctor).map_or(0, |(t, _)| t),
                    _ => 0,
                };
                let args = args.clone();
                let bind = self.compile_fields(value_local, &args, success, &fail);
                self.test_tag(value_local, tag, bind, fail)
            }
            PatKind::Or(alts) => {
                let alts = alts.clone();
                let mut chain = fail;
                for &alt in alts.iter().rev() {
                    chain = self.compile_pattern(value_local, alt, success.clone(), chain);
                }
                chain
            }
        }
    }

    /// `if <value> = <lit> then success else fail`.
    fn test_lit(&mut self, value: CExpr, lit: Lit, success: CExpr, fail: CExpr) -> CExpr {
        let lit = CExpr::new(K::Lit(lit), Ty::Error);
        let cond = CExpr::new(K::Prim { op: Prim::Eq, args: vec![value, lit] }, Ty::bool());
        CExpr::new(
            K::If { cond: Box::new(cond), then: Box::new(success), els: Box::new(fail) },
            Ty::Error,
        )
    }

    /// `if tag(value_local) = <tag> then success else fail`.
    fn test_tag(&mut self, value_local: LocalId, tag: u32, success: CExpr, fail: CExpr) -> CExpr {
        let read = CExpr::new(
            K::DataTag(Box::new(CExpr::new(K::Local(value_local), Ty::Error))),
            Ty::int(),
        );
        let tag_lit = CExpr::new(K::Lit(Lit::Int(i64::from(tag))), Ty::int());
        let cond = CExpr::new(K::Prim { op: Prim::Eq, args: vec![read, tag_lit] }, Ty::bool());
        CExpr::new(
            K::If { cond: Box::new(cond), then: Box::new(success), els: Box::new(fail) },
            Ty::Error,
        )
    }

    /// Projects and matches each field of a data value, threading `success`
    /// through the fields and `fail` out of any sub-match.
    fn compile_fields(
        &mut self,
        value_local: LocalId,
        fields: &[PatId],
        success: CExpr,
        fail: &CExpr,
    ) -> CExpr {
        let mut inner = success;
        for (i, &fp) in fields.iter().enumerate().rev() {
            let index = u32::try_from(i).unwrap_or(0);
            let projection = || {
                CExpr::new(
                    K::DataField {
                        base: Box::new(CExpr::new(K::Local(value_local), Ty::Error)),
                        index,
                    },
                    Ty::Error,
                )
            };
            match &self.module.pat(fp).kind {
                // A wildcard field needs no projection (the scrutinee's drop frees it).
                PatKind::Wildcard => {}
                PatKind::Var(_) => {
                    let local = self.resolved.local_of(fp).unwrap_or_else(|| self.fresh_local());
                    inner = CExpr::new(
                        K::Let { local, value: Box::new(projection()), body: Box::new(inner) },
                        Ty::Error,
                    );
                }
                _ => {
                    let f = self.fresh_local();
                    let matched = self.compile_pattern(f, fp, inner, fail.clone());
                    inner = CExpr::new(
                        K::Let { local: f, value: Box::new(projection()), body: Box::new(matched) },
                        Ty::Error,
                    );
                }
            }
        }
        inner
    }

    /// Matches a list pattern `[p0, p1, …]` as nested `Cons`/`Nil`.
    fn compile_list_pattern(
        &mut self,
        value_local: LocalId,
        elems: &[PatId],
        success: CExpr,
        fail: CExpr,
    ) -> CExpr {
        let Some((&head, rest)) = elems.split_first() else {
            // `[]` matches the `Nil` tag.
            return self.test_tag(value_local, NIL_TAG, success, fail);
        };
        // A `Cons` cell: head = field 0, tail matches the rest of the list.
        let tail_local = self.fresh_local();
        let tail_match = self.compile_list_pattern(tail_local, rest, success, fail.clone());
        let tail_bind = CExpr::new(
            K::Let {
                local: tail_local,
                value: Box::new(CExpr::new(
                    K::DataField {
                        base: Box::new(CExpr::new(K::Local(value_local), Ty::Error)),
                        index: 1,
                    },
                    Ty::Error,
                )),
                body: Box::new(tail_match),
            },
            Ty::Error,
        );
        let head_bind = self.compile_fields(value_local, &[head], tail_bind, &fail);
        self.test_tag(value_local, CONS_TAG, head_bind, fail)
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
        let body = self.lower_stmts(rest, tail);
        let ty = body.ty.clone();
        // A simple `let v = …`/`let _ = …` binds directly; a destructuring
        // `let (x, y) = …` (or any irrefutable pattern) matches the value, with an
        // unreachable failure branch.
        if is_simple_binder(self.module, stmt.pat) {
            let local = self.param_local(stmt.pat);
            CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, ty)
        } else {
            let s = self.fresh_local();
            let matched = self.compile_pattern(s, stmt.pat, body, error_expr());
            CExpr::new(K::Let { local: s, value: Box::new(value), body: Box::new(matched) }, ty)
        }
    }
}

/// Whether a binding pattern is a plain variable or wildcard (so it binds a slot
/// directly, without destructuring).
fn is_simple_binder(module: &Module, pat: PatId) -> bool {
    match &module.pat(pat).kind {
        PatKind::Var(_) | PatKind::Wildcard => true,
        PatKind::Paren(inner) => is_simple_binder(module, *inner),
        _ => false,
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
        K::MakeData { args, .. } => {
            for a in args {
                collect_free(a, bound, out);
            }
        }
        K::DataTag(base) => collect_free(base, bound, out),
        K::DataField { base, .. } => collect_free(base, bound, out),
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
