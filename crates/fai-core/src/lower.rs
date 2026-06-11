//! Lowering the surface AST to Core IR.
//!
//! [`core`] is a per-definition salsa query: it lowers one top-level binding
//! (its parameters and body) into a [`LoweredDef`], lambda-lifting nested
//! lambdas and desugaring operators, pipes, composition, and short-circuit
//! booleans. A few constructs are not yet supported by the native backend (a
//! destructuring function parameter, and the structural/short-circuit operators
//! used as first-class values); these are reported as
//! [`crate::UNSUPPORTED_NATIVE`] and lowered to an error placeholder, so an
//! unused such definition never blocks a build (only the reachable closure is
//! lowered).

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{
    CtorRef, DefId, InterfaceRef, LocalId, Res, ResolvedBodies, interface_decls, resolve,
    type_decls,
};
use fai_span::{Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{
    BinOp, ExprId, ExprKind, FieldInit, MatchArm, MethodImpl, Module, PatId, PatKind, UnOp,
    classify_op, classify_prefix,
};
use fai_types::{
    BodyTypes, Con, RecordRow, RowEnd, RowVarId, Scheme, Ty, body_types,
    declared_or_inferred_scheme, evidence_requirements,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, FnId, Lit, LoweredDef, Prim};
use crate::{ROW_POLY_UNSUPPORTED, UNSUPPORTED_NATIVE};

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

    let Some((params, body)) = binding_body(db, file, &parsed.module, name) else {
        return Arc::new(LoweredDef {
            def,
            fns: vec![CoreFn { params: Vec::new(), captures: Vec::new(), body: error_expr() }],
            entry_borrowed: Vec::new(),
        });
    };

    let mut lowerer = Lowerer {
        db,
        file,
        module: &parsed.module,
        resolved: &resolved,
        types: &types,
        next_local: first_free_local(&resolved),
        fns: vec![placeholder_fn()],
        evidence: FxHashMap::default(),
        aliases: FxHashMap::default(),
        emit_unsupported: true,
    };

    let param_locals: Vec<LocalId> = params.iter().map(|&p| lowerer.param_local(p)).collect();
    // Offset-evidence parameters precede the real parameters; allocate and bind
    // them before lowering the body so row-polymorphic field accesses can use
    // them, and so calls supply them in the type-derived canonical order.
    let evidence_params = lowerer.bind_evidence_params(&params, body);
    let body = lowerer.lower_expr(body);
    let mut all_params = evidence_params;
    all_params.extend(param_locals);
    lowerer.fns[0] = CoreFn { params: all_params, captures: Vec::new(), body };
    Arc::new(LoweredDef { def, fns: lowerer.fns, entry_borrowed: Vec::new() })
}

/// The lowered pieces of a `(params, body)` form, for callers that assemble their
/// own enclosing function(s) — e.g. the contract harness synthesizer. The body's
/// nested lambdas are `lifted` (a `MakeClosure { func: FnId(i) }` in `body` refers
/// to `lifted[i - 1]`, so placing `lifted` at `fns[1..]` keeps the indices valid).
pub struct LoweredBody {
    /// The lowered body expression.
    pub body: CExpr,
    /// The body's lifted lambdas, in `FnId` order starting at 1.
    pub lifted: Vec<CoreFn>,
    /// The local slot bound by each parameter, in order.
    pub param_locals: Vec<LocalId>,
    /// The first local index free after lowering (for synthesizing more locals).
    pub next_local: usize,
}

/// Lowers a `(params, body)` form (parameters as monomorphic locals, no
/// offset-evidence) to its body expression plus lifted lambdas. Used to lower a
/// contract body, which the contract synthesizer wraps in a harness; `types` is
/// the contract's [`fai_types::contract_body_types`].
#[must_use]
pub fn lower_params_body(
    db: &dyn Db,
    file: SourceFile,
    params: &[PatId],
    body: ExprId,
    types: &BodyTypes,
) -> LoweredBody {
    let parsed = fai_syntax::parse(db, file);
    let resolved = resolve(db, file);
    let mut lowerer = Lowerer {
        db,
        file,
        module: &parsed.module,
        resolved: &resolved,
        types,
        next_local: first_free_local(&resolved),
        fns: vec![placeholder_fn()],
        evidence: FxHashMap::default(),
        aliases: FxHashMap::default(),
        // The contract synthesizer runs outside a tracked query, so it must not
        // accumulate diagnostics; unsupported constructs become error nodes the
        // caller detects (and reports as not-runnable).
        emit_unsupported: false,
    };
    let param_locals: Vec<LocalId> = params.iter().map(|&p| lowerer.param_local(p)).collect();
    let body = lowerer.lower_expr(body);
    let lifted = lowerer.fns.split_off(1);
    LoweredBody { body, lifted, param_locals, next_local: lowerer.next_local }
}

/// The body item (params + body expr) of a definition with qualified `name`,
/// located by its binding item (so a nested definition is found by its
/// module-qualified name, not the local name in the AST).
fn binding_body(
    db: &dyn Db,
    file: SourceFile,
    module: &Module,
    name: Symbol,
) -> Option<(Vec<PatId>, ExprId)> {
    let binding = fai_resolve::module_defs(db, file).get(name)?.binding;
    match &module.items[binding.index()].kind {
        fai_syntax::ast::ItemKind::Binding { params, body, .. } => Some((params.clone(), *body)),
        _ => None,
    }
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

/// An immediate `Int` literal (used for statically known offset evidence).
fn int_lit(n: i64) -> CExpr {
    CExpr::new(K::Lit(Lit::Int(n)), Ty::int())
}

/// Recovers each scheme row variable's instantiation by matching a (general)
/// scheme type against an actual instantiated type in parallel. For an open
/// record `{ known | 'r }` matched against `{ actual… | tail }`, `'r` is bound to
/// the actual fields not named in the scheme record, plus the actual tail — so an
/// offset can be split into a static part and (when `tail` is another row
/// variable) threaded caller evidence.
fn match_rows(scheme: &Ty, actual: &Ty, out: &mut FxHashMap<RowVarId, RecordRow>) {
    match (scheme, actual) {
        (Ty::Record(s), Ty::Record(a)) => {
            if let RowEnd::Open(r) = s.tail {
                let known: FxHashSet<Symbol> = s.fields.iter().map(|(l, _)| *l).collect();
                let extra: Vec<(Symbol, Ty)> =
                    a.fields.iter().filter(|(l, _)| !known.contains(l)).cloned().collect();
                out.insert(r, RecordRow { fields: extra, tail: a.tail });
            }
            for (label, st) in &s.fields {
                if let Some((_, at)) = a.fields.iter().find(|(m, _)| m == label) {
                    match_rows(st, at, out);
                }
            }
        }
        (Ty::Arrow(sf, st, _), Ty::Arrow(af, at, _)) | (Ty::App(sf, st), Ty::App(af, at)) => {
            match_rows(sf, af, out);
            match_rows(st, at, out);
        }
        (Ty::Tuple(ss), Ty::Tuple(aa)) => {
            for (s, a) in ss.iter().zip(aa) {
                match_rows(s, a, out);
            }
        }
        _ => {}
    }
}

/// The per-definition lowering state.
struct Lowerer<'a> {
    db: &'a dyn Db,
    file: SourceFile,
    module: &'a Module,
    resolved: &'a ResolvedBodies,
    types: &'a BodyTypes,
    next_local: usize,
    fns: Vec<CoreFn>,
    /// Offset evidence for the definition's row variables: the integer local
    /// holding the count of the row's hidden fields before each lacked label.
    /// Keyed by the row variable's body-numbered id and the label.
    evidence: FxHashMap<(RowVarId, Symbol), LocalId>,
    /// Function-valued `let` aliases: a local bound directly to a top-level
    /// function value (`let g = f`, `f` a non-row-polymorphic function) maps to
    /// that function, so every use of the local is copy-propagated to `Global f`.
    /// This turns `g x` into a direct call and drops the redundant binding. Keyed by
    /// the (unique) `LocalId`, so it is scope-exact and append-only.
    aliases: FxHashMap<LocalId, DefId>,
    /// Whether to accumulate unsupported-construct diagnostics. The per-definition
    /// `core` query does (it runs inside salsa); a caller outside a tracked query
    /// (the contract synthesizer) suppresses them and detects the resulting error
    /// placeholders instead, so it never accumulates outside an active query.
    emit_unsupported: bool,
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
        if self.emit_unsupported {
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
        }
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
            ExprKind::Char(raw) => {
                K::Lit(Lit::Char(crate::lit::decode_char(raw.as_str()).unwrap_or('\0')))
            }
            ExprKind::Var(_) => return self.lower_ref(expr),
            ExprKind::Field { base, field } => {
                // A qualified `Module.x` is recorded in resolution; otherwise it
                // is ordinary record field access.
                if self.resolved.get(expr).is_some() {
                    return self.lower_ref(expr);
                }
                return self.lower_field_access(*base, *field, node.span, ty);
            }
            ExprKind::Record(fields) => return self.lower_record(fields, &ty, node.span),
            ExprKind::RecordUpdate { base, fields } => {
                return self.lower_record_update(*base, fields, &ty, node.span);
            }
            ExprKind::Instance { methods, .. } => return self.lower_instance(methods, ty),
            ExprKind::App { .. } => {
                let (head, args) = self.app_spine(expr);
                return self.lower_application(head, &args, ty);
            }
            ExprKind::Infix { op, lhs, rhs } => return self.lower_infix(*op, *lhs, *rhs, ty),
            ExprKind::Prefix { op, operand } => return self.lower_prefix(*op, *operand, ty),
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
                K::MakeData { tag: 0, args, reuse: None }
            }
            ExprKind::List(elems) => return self.lower_list(elems, ty),
            ExprKind::Array(elems) => return self.lower_array(elems, ty),
            ExprKind::Error => K::Error,
        };
        CExpr::new(kind, ty)
    }

    /// Lowers a name/field reference (in value position).
    fn lower_ref(&mut self, expr: ExprId) -> CExpr {
        let ty = self.ty_of(expr);
        let span = self.module.expr(expr).span;
        match self.resolved.get(expr) {
            // A local aliasing a top-level function (`let g = f`) is copy-propagated
            // to `Global f`, so a saturated use becomes a direct call and a value use
            // is `f`'s closure — exactly as if `f` had been named directly.
            Some(Res::Local(id)) => match self.aliases.get(&id) {
                Some(&def) => CExpr::new(K::Global(def), ty),
                None => CExpr::new(K::Local(id), ty),
            },
            Some(Res::Def(def)) => self.def_value(def, ty),
            Some(Res::Ctor(ctor)) => self.lower_ctor_value(ctor, ty),
            Some(Res::Builtin(name)) => self.lower_builtin_ref(name, ty, span),
            Some(Res::Error) | None => error_expr(),
        }
    }

    /// A reference to a top-level definition as a value. A row-polymorphic
    /// definition takes leading offset evidence; partially apply it here so the
    /// resulting value — used first-class or applied to the real arguments —
    /// already carries it. (A saturated call thus completes the partial
    /// application; `apply_n` handles both uniformly.)
    fn def_value(&mut self, def: DefId, ty: Ty) -> CExpr {
        let args = self.evidence_args(def, &ty);
        let global = CExpr::new(K::Global(def), ty.clone());
        if args.is_empty() {
            global
        } else {
            CExpr::new(K::App { func: Box::new(global), args }, ty)
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
            return CExpr::new(K::MakeData { tag, args: Vec::new(), reuse: None }, ty);
        }
        let params: Vec<LocalId> = (0..arity).map(|_| self.fresh_local()).collect();
        let args = params.iter().map(|&p| CExpr::new(K::Local(p), Ty::Error)).collect();
        let body = CExpr::new(K::MakeData { tag, args, reuse: None }, Ty::Error);
        let fn_id = self.push_fn(CoreFn { params, captures: Vec::new(), body });
        CExpr::new(K::MakeClosure { func: fn_id, captures: Vec::new() }, ty)
    }

    /// Reports a row-polymorphic record operation (deferred to a later milestone)
    /// and yields an error placeholder.
    fn unsupported_row_poly(&self, range: TextRange, feature: &str) -> CExpr {
        if self.emit_unsupported {
            emit(
                self.db,
                Diagnostic::error(
                    ROW_POLY_UNSUPPORTED,
                    format!("{feature} is not supported by the native backend yet"),
                    self.span(range),
                )
                .with_help("give the value a closed record type so the field offsets are known"),
            );
        }
        error_expr()
    }

    /// Lowers `r.x`: a constant-offset projection for a monomorphic record, or a
    /// method projection (also a constant offset) for a nominal interface.
    fn lower_field_access(
        &mut self,
        base: ExprId,
        field: Symbol,
        span: TextRange,
        ty: Ty,
    ) -> CExpr {
        let base_ty = self.ty_of(base);
        // Interface method access: the dictionary stores method closures sorted
        // by name, so the method's index is a constant offset.
        if let Some(iref) = interface_head(&base_ty) {
            return match self.interface_method_index(iref, field) {
                Some(index) => {
                    let b = self.lower_expr(base);
                    CExpr::new(
                        K::DataField { base: Box::new(b), index: FieldIndex::Const(index) },
                        ty,
                    )
                }
                None => error_expr(),
            };
        }
        match record_field_index(&base_ty, field) {
            Some(index) => {
                let b = self.lower_expr(base);
                CExpr::new(K::DataField { base: Box::new(b), index: FieldIndex::Const(index) }, ty)
            }
            None => self.row_poly_field(base, &base_ty, field, span, ty),
        }
    }

    /// Lowers a field access on a *row-polymorphic* record: the slot is the count
    /// of the record's statically known fields that precede `field` plus the
    /// row's offset evidence (a leading parameter).
    fn row_poly_field(
        &mut self,
        base: ExprId,
        base_ty: &Ty,
        field: Symbol,
        span: TextRange,
        ty: Ty,
    ) -> CExpr {
        if let Ty::Record(row) = base_ty
            && let RowEnd::Open(r) = row.tail
            && let Some(&evidence) = self.evidence.get(&(r, field))
        {
            let preceding = row.fields.iter().filter(|(l, _)| l.as_str() < field.as_str()).count();
            let index = FieldIndex::Dyn { base: u32::try_from(preceding).unwrap_or(0), evidence };
            let b = self.lower_expr(base);
            return CExpr::new(K::DataField { base: Box::new(b), index }, ty);
        }
        self.unsupported_row_poly(span, "row-polymorphic record field access")
    }

    /// Allocates a leading offset-evidence parameter for each row lacks-constraint
    /// in the definition's (body-reconstructed) type, recording the local for
    /// every `(row variable, label)` so field accesses can find it. Returns the
    /// evidence parameters in canonical order (matching what callers supply).
    fn bind_evidence_params(&mut self, params: &[PatId], body: ExprId) -> Vec<LocalId> {
        let mut fn_ty = self.ty_of(body);
        for &p in params.iter().rev() {
            let pt = self.types.pat_type(p).cloned().unwrap_or(Ty::Error);
            fn_ty = Ty::arrow(pt, fn_ty);
        }
        let mut locals = Vec::new();
        for req in evidence_requirements(&Scheme::mono(fn_ty)) {
            let local = self.fresh_local();
            self.evidence.insert((req.row_var, req.label), local);
            locals.push(local);
        }
        locals
    }

    /// The offset-evidence arguments to supply when referencing `def` (whose
    /// reference has instantiated type `ref_ty`): one integer per the callee's row
    /// lacks-constraints, in the same canonical order the callee binds them.
    fn evidence_args(&self, def: DefId, ref_ty: &Ty) -> Vec<CExpr> {
        let Some(scheme) = declared_or_inferred_scheme(self.db, def) else {
            return Vec::new();
        };
        let reqs = evidence_requirements(&scheme);
        if reqs.is_empty() {
            return Vec::new();
        }
        let mut inst: FxHashMap<RowVarId, RecordRow> = FxHashMap::default();
        match_rows(&scheme.ty, ref_ty, &mut inst);
        reqs.iter().map(|req| self.evidence_value(req.row_var, req.label, &inst)).collect()
    }

    /// The evidence value for `(row_var, label)` under a call's row instantiation:
    /// the statically known count of preceding fields plus, when the row resolves
    /// to an extension of one of *this* function's row variables, that variable's
    /// own evidence (threaded through).
    fn evidence_value(
        &self,
        row_var: RowVarId,
        label: Symbol,
        inst: &FxHashMap<RowVarId, RecordRow>,
    ) -> CExpr {
        let Some(row) = inst.get(&row_var) else {
            return int_lit(0);
        };
        let preceding = row.fields.iter().filter(|(l, _)| l.as_str() < label.as_str()).count();
        let preceding = i64::try_from(preceding).unwrap_or(0);
        match row.tail {
            RowEnd::Closed => int_lit(preceding),
            RowEnd::Open(s) => match self.evidence.get(&(s, label)).copied() {
                Some(local) => {
                    let caller = CExpr::new(K::Local(local), Ty::int());
                    if preceding == 0 {
                        caller
                    } else {
                        CExpr::new(
                            K::Prim { op: Prim::IntAdd, args: vec![int_lit(preceding), caller] },
                            Ty::int(),
                        )
                    }
                }
                None => int_lit(preceding),
            },
        }
    }

    /// The index of `method` in interface `iref`'s dictionary (its methods sorted
    /// by name), or `None` if the method is unknown.
    fn interface_method_index(&self, iref: InterfaceRef, method: Symbol) -> Option<u32> {
        let file = self.db.source_file(iref.file)?;
        let decls = interface_decls(self.db, file);
        let info = decls.interface_named(iref.name)?;
        let mut names: Vec<Symbol> = info.methods.clone();
        names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        names.iter().position(|&n| n == method).map(|i| u32::try_from(i).unwrap_or(0))
    }

    /// Lowers an interface instance to a dictionary `MakeData{tag:0, …}` whose
    /// method closures are stored sorted by name (matching method access).
    fn lower_instance(&mut self, methods: &[MethodImpl], ty: Ty) -> CExpr {
        let mut sorted: Vec<&MethodImpl> = methods.iter().collect();
        sorted.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let args: Vec<CExpr> = sorted
            .iter()
            .map(|m| {
                if m.params.is_empty() {
                    // A value-shaped method stores its value directly.
                    self.lower_expr(m.body)
                } else {
                    let param_locals: Vec<LocalId> =
                        m.params.iter().map(|&p| self.param_local(p)).collect();
                    let body = self.lower_expr(m.body);
                    self.lift_lambda(param_locals, body, Ty::Error)
                }
            })
            .collect();
        CExpr::new(K::MakeData { tag: 0, args, reuse: None }, ty)
    }

    /// Lowers a record literal to a tagless composite, fields in canonical
    /// (sorted-label) order so projections line up.
    fn lower_record(&mut self, fields: &[FieldInit], ty: &Ty, _span: TextRange) -> CExpr {
        let mut sorted: Vec<&FieldInit> = fields.iter().collect();
        sorted.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let args = sorted.iter().map(|f| self.lower_expr(f.value)).collect();
        CExpr::new(K::MakeData { tag: 0, args, reuse: None }, ty.clone())
    }

    /// Lowers `{ base with x = v, … }` to a fresh record copying the unchanged
    /// fields (monomorphic only; a row-polymorphic update is deferred).
    fn lower_record_update(
        &mut self,
        base: ExprId,
        fields: &[FieldInit],
        ty: &Ty,
        span: TextRange,
    ) -> CExpr {
        let Ty::Record(row) = ty else {
            return self.unsupported_row_poly(span, "record update");
        };
        if row.tail != RowEnd::Closed {
            return self.lower_row_poly_update(base, fields, ty, span);
        }
        let base_c = self.lower_expr(base);
        // Project the unchanged fields from a single base local. When the base is
        // already a local, use it directly rather than binding an alias: an alias
        // would split the base's reference count (the new-value expressions read
        // the original), preventing the record from being recognized as unique and
        // reused in place. A compound base is bound once to avoid re-evaluating it.
        let (base_local, wrap) = match base_c.kind {
            K::Local(l) => (l, None),
            _ => (self.fresh_local(), Some(base_c)),
        };
        let row_fields = row.fields.clone();
        let mut args = Vec::with_capacity(row_fields.len());
        for (index, (label, _)) in row_fields.iter().enumerate() {
            if let Some(f) = fields.iter().find(|f| f.name == *label) {
                args.push(self.lower_expr(f.value));
            } else {
                let i = u32::try_from(index).unwrap_or(0);
                let base = Box::new(CExpr::new(K::Local(base_local), Ty::Error));
                args.push(CExpr::new(
                    K::DataField { base, index: FieldIndex::Const(i) },
                    Ty::Error,
                ));
            }
        }
        let make = CExpr::new(K::MakeData { tag: 0, args, reuse: None }, ty.clone());
        match wrap {
            Some(base_c) => CExpr::new(
                K::Let { local: base_local, value: Box::new(base_c), body: Box::new(make) },
                ty.clone(),
            ),
            None => make,
        }
    }

    /// Lowers `{ base with l = v, … }` on a *row-polymorphic* record: the field
    /// count is unknown, so clone the record at runtime (by its object size) once
    /// per updated field, replacing the field at its offset-evidence slot.
    fn lower_row_poly_update(
        &mut self,
        base: ExprId,
        fields: &[FieldInit],
        ty: &Ty,
        span: TextRange,
    ) -> CExpr {
        let base_ty = self.ty_of(base);
        let Ty::Record(brow) = &base_ty else {
            return self.unsupported_row_poly(span, "record update");
        };
        let RowEnd::Open(r) = brow.tail else {
            return self.unsupported_row_poly(span, "record update");
        };
        let mut cur = self.lower_expr(base);
        for f in fields {
            let Some(&evidence) = self.evidence.get(&(r, f.name)) else {
                return self.unsupported_row_poly(span, "row-polymorphic record update");
            };
            let preceding =
                brow.fields.iter().filter(|(l, _)| l.as_str() < f.name.as_str()).count();
            let offset = self.offset_expr(i64::try_from(preceding).unwrap_or(0), evidence);
            let value = self.lower_expr(f.value);
            cur = CExpr::new(
                K::Prim { op: Prim::RecordUpdate, args: vec![cur, offset, value] },
                ty.clone(),
            );
        }
        cur
    }

    /// An `Int`-valued slot expression `preceding + evidence` for a
    /// row-polymorphic field (just the evidence when no fields precede it).
    fn offset_expr(&self, preceding: i64, evidence: LocalId) -> CExpr {
        let ev = CExpr::new(K::Local(evidence), Ty::int());
        if preceding == 0 {
            ev
        } else {
            CExpr::new(K::Prim { op: Prim::IntAdd, args: vec![int_lit(preceding), ev] }, Ty::int())
        }
    }

    /// Lowers a list literal `[a, b, …]` to nested `Cons`/`Nil` data.
    fn lower_list(&mut self, elems: &[ExprId], ty: Ty) -> CExpr {
        let mut list =
            CExpr::new(K::MakeData { tag: NIL_TAG, args: Vec::new(), reuse: None }, ty.clone());
        for &e in elems.iter().rev() {
            let head = self.lower_expr(e);
            list = CExpr::new(
                K::MakeData { tag: CONS_TAG, args: vec![head, list], reuse: None },
                ty.clone(),
            );
        }
        list
    }

    /// Lowers an array literal `[| a, b, … |]` to a pre-sized builder: one
    /// `withCapacity n` followed by an in-place `push` per element (no
    /// intermediate `List`). The capacity matches the length, so the pushes never
    /// reallocate; building a fresh array, they are all in place.
    fn lower_array(&mut self, elems: &[ExprId], ty: Ty) -> CExpr {
        let n = i64::try_from(elems.len()).unwrap_or(0);
        let mut arr =
            CExpr::new(K::Prim { op: Prim::ArrayWithCapacity, args: vec![int_lit(n)] }, ty.clone());
        for &e in elems {
            let value = self.lower_expr(e);
            arr = CExpr::new(K::Prim { op: Prim::ArrayPush, args: vec![arr, value] }, ty.clone());
        }
        arr
    }

    /// Lowers a builtin reference used as a value: booleans become literals and
    /// intrinsics (`Prim.*`, the boolean ops, `Console.writeLine`) are
    /// eta-expanded into a closure. Auto-imported core functions are ordinary
    /// `Res::Def` globals, so they never reach here.
    fn lower_builtin_ref(&mut self, name: Symbol, ty: Ty, span: TextRange) -> CExpr {
        match name.as_str() {
            "true" => return CExpr::new(K::Lit(Lit::Bool(true)), ty),
            "false" => return CExpr::new(K::Lit(Lit::Bool(false)), ty),
            _ => {}
        }
        if let Some(prim) = Prim::from_builtin(name.as_str()) {
            return self.eta_expand_prim(prim, ty);
        }
        // A built-in operator used as a value (`(+)`, `(=)`, …): eta-expand it to
        // its primitive at the use-site type.
        if classify_op(name).is_some() || classify_prefix(name).is_some() {
            return self.eta_expand_operator(name, ty, span);
        }
        self.unsupported(span, "this intrinsic")
    }

    /// Eta-expands a built-in operator used in value position to a closure over
    /// its primitive. The operand type selects the Int vs Float primitive. The
    /// structural/short-circuit operators are not yet available as values.
    fn eta_expand_operator(&mut self, name: Symbol, ty: Ty, span: TextRange) -> CExpr {
        let float = matches!(first_arg_ty(&ty), Some(Ty::Con(Con::Float)));
        let prim = match name.as_str() {
            "+" if float => Prim::FloatAdd,
            "+" => Prim::IntAdd,
            "-" if float => Prim::FloatSub,
            "-" => Prim::IntSub,
            "*" if float => Prim::FloatMul,
            "*" => Prim::IntMul,
            "/" if float => Prim::FloatDiv,
            "/" => Prim::IntDiv,
            "%" => Prim::IntRem,
            "=" => Prim::Eq,
            "++" => Prim::StrConcat,
            _ => return self.unsupported(span, "this operator as a value"),
        };
        self.eta_expand_prim(prim, ty)
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
            return CExpr::new(K::MakeData { tag, args, reuse: None }, ty);
        }
        let func = Box::new(self.lower_expr(head));
        let args = args.iter().map(|&a| self.lower_expr(a)).collect();
        CExpr::new(K::App { func, args }, ty)
    }

    /// The operator symbol held in an operator `Var` node.
    fn op_symbol(&self, op: ExprId) -> Symbol {
        match &self.module.expr(op).kind {
            ExprKind::Var(s) => *s,
            _ => Symbol::intern(""),
        }
    }

    /// Whether the operator node resolved to the built-in operator (not shadowed).
    fn is_builtin_op(&self, op: ExprId) -> bool {
        matches!(self.resolved.get(op), Some(Res::Builtin(_)))
    }

    /// Lowers an infix application. A non-shadowed built-in operator lowers to its
    /// dedicated form; otherwise it is an ordinary curried application of the
    /// resolved operator function.
    fn lower_infix(&mut self, op: ExprId, lhs: ExprId, rhs: ExprId, ty: Ty) -> CExpr {
        let sym = self.op_symbol(op);
        if self.is_builtin_op(op)
            && let Some(binop) = classify_op(sym)
        {
            return self.lower_builtin_binary(binop, lhs, rhs, ty);
        }
        self.lower_application(op, &[lhs, rhs], ty)
    }

    /// Lowers a prefix application: built-in negation, or an ordinary one-argument
    /// application of the resolved operator function.
    fn lower_prefix(&mut self, op: ExprId, operand: ExprId, ty: Ty) -> CExpr {
        let sym = self.op_symbol(op);
        if self.is_builtin_op(op) && matches!(classify_prefix(sym), Some(UnOp::Neg)) {
            let is_float = matches!(self.ty_of(operand), Ty::Con(Con::Float));
            let operand = self.lower_expr(operand);
            let kind = if is_float {
                let zero = CExpr::new(K::Lit(Lit::Float(0f64.to_bits())), Ty::Con(Con::Float));
                K::Prim { op: Prim::FloatSub, args: vec![zero, operand] }
            } else {
                let zero = CExpr::new(K::Lit(Lit::Int(0)), Ty::int());
                K::Prim { op: Prim::IntSub, args: vec![zero, operand] }
            };
            return CExpr::new(kind, ty);
        }
        self.lower_application(op, &[operand], ty)
    }

    fn lower_builtin_binary(&mut self, op: BinOp, lhs: ExprId, rhs: ExprId, ty: Ty) -> CExpr {
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
            // `x :: xs` builds a `Cons` cell.
            BinOp::Cons => {
                let args = vec![self.lower_expr(lhs), self.lower_expr(rhs)];
                CExpr::new(K::MakeData { tag: CONS_TAG, args, reuse: None }, ty)
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
            PatKind::Char(raw) => {
                let c = crate::lit::decode_char(raw.as_str()).unwrap_or('\0');
                self.test_lit(value(), Lit::Char(c), success, fail)
            }
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
            PatKind::As { pat: inner, .. } => {
                // Bind the alias name to the whole matched value, then match the
                // inner pattern against the same value.
                let inner = *inner;
                let local = self.resolved.local_of(pat).unwrap_or_else(|| self.fresh_local());
                let bound = CExpr::new(
                    K::Let { local, value: Box::new(value()), body: Box::new(success) },
                    Ty::Error,
                );
                self.compile_pattern(value_local, inner, bound, fail)
            }
            PatKind::Record { fields, .. } => {
                let fields: Vec<(Symbol, PatId)> = fields.iter().map(|f| (f.name, f.pat)).collect();
                self.compile_record_pattern(value_local, pat, &fields, success, fail)
            }
        }
    }

    /// Compiles a record pattern: project each named field at its constant offset
    /// (its index in the record type's sorted fields) and match the sub-pattern.
    /// Records are single-shape, so there is no tag test.
    fn compile_record_pattern(
        &mut self,
        value_local: LocalId,
        pat: PatId,
        fields: &[(Symbol, PatId)],
        success: CExpr,
        fail: CExpr,
    ) -> CExpr {
        let Some(Ty::Record(row)) = self.types.pat_type(pat).cloned() else {
            return self.unsupported_row_poly(self.module.pat(pat).span, "record pattern");
        };
        if row.tail != RowEnd::Closed {
            return self
                .unsupported_row_poly(self.module.pat(pat).span, "row-polymorphic record pattern");
        }
        let mut inner = success;
        for &(name, fpat) in fields.iter().rev() {
            let Some(index) = row.fields.iter().position(|(l, _)| *l == name) else {
                continue;
            };
            let index = u32::try_from(index).unwrap_or(0);
            // Carry the field's real type so a scalar `Float` field stays unboxed.
            let field_ty = self.types.pat_type(fpat).cloned().unwrap_or(Ty::Error);
            let projection = || {
                CExpr::new(
                    K::DataField {
                        base: Box::new(CExpr::new(K::Local(value_local), Ty::Error)),
                        index: FieldIndex::Const(index),
                    },
                    field_ty.clone(),
                )
            };
            match &self.module.pat(fpat).kind {
                PatKind::Wildcard => {}
                PatKind::Var(_) => {
                    let local = self.resolved.local_of(fpat).unwrap_or_else(|| self.fresh_local());
                    inner = CExpr::new(
                        K::Let { local, value: Box::new(projection()), body: Box::new(inner) },
                        Ty::Error,
                    );
                }
                _ => {
                    let f = self.fresh_local();
                    let matched = self.compile_pattern(f, fpat, inner, fail.clone());
                    inner = CExpr::new(
                        K::Let { local: f, value: Box::new(projection()), body: Box::new(matched) },
                        Ty::Error,
                    );
                }
            }
        }
        inner
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
            // The projection carries the field's real type (the bound pattern's
            // type), so a scalar `Float` field flows unboxed rather than being
            // reclassified as a boxed value downstream.
            let field_ty = self.types.pat_type(fp).cloned().unwrap_or(Ty::Error);
            let projection = || {
                CExpr::new(
                    K::DataField {
                        base: Box::new(CExpr::new(K::Local(value_local), Ty::Error)),
                        index: FieldIndex::Const(index),
                    },
                    field_ty.clone(),
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
                        index: FieldIndex::Const(1),
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
        // A simple `let g = f` binding `g` directly to a non-row-polymorphic
        // top-level function value (a bare `Global` of arrow type — a row-polymorphic
        // `f` lowers to a `Global` partial application, and a nullary value to a
        // non-arrow type, so both are excluded) is recorded as an alias and the
        // binding dropped: every use of `g` copy-propagates to `Global f`, turning a
        // saturated `g …` into a direct call. The discarded value is a pure
        // immortal-closure fetch, so this is sound and changes no observable result.
        if is_simple_binder(self.module, stmt.pat)
            && matches!(value.ty, Ty::Arrow(_, _, _))
            && let K::Global(def) = &value.kind
        {
            let def = *def;
            let local = self.param_local(stmt.pat);
            self.aliases.insert(local, def);
            return self.lower_stmts(rest, tail);
        }
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

/// The interface at the head of a (possibly applied) type, if any.
fn interface_head(ty: &Ty) -> Option<InterfaceRef> {
    match ty {
        Ty::Interface(iref) => Some(*iref),
        Ty::App(f, _) => interface_head(f),
        _ => None,
    }
}

/// The first argument type of a function type `a -> …` (used to pick the Int vs
/// Float primitive when eta-expanding an operator value).
fn first_arg_ty(ty: &Ty) -> Option<&Ty> {
    match ty {
        Ty::Arrow(from, _, _) => Some(from),
        _ => None,
    }
}

/// The constant field offset (index) of `field` in a *monomorphic* (closed)
/// record type, or `None` for a row-polymorphic record (offset unknown).
fn record_field_index(ty: &Ty, field: Symbol) -> Option<u32> {
    if let Ty::Record(row) = ty
        && row.tail == RowEnd::Closed
    {
        return row
            .fields
            .iter()
            .position(|(l, _)| *l == field)
            .map(|i| u32::try_from(i).unwrap_or(0));
    }
    None
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
        // The following are inserted after lowering (reset/reuse and the tail-call
        // transform), so they never reach here; handled defensively for
        // exhaustiveness.
        K::Reset { value, body, .. } => {
            collect_free(value, bound, out);
            collect_free(body, bound, out);
        }
        K::FreeReuse { body, .. } => collect_free(body, bound, out),
        K::Join { params, body } => {
            let fresh: Vec<LocalId> = params.iter().copied().filter(|p| bound.insert(*p)).collect();
            collect_free(body, bound, out);
            for p in fresh {
                bound.remove(&p);
            }
        }
        K::Recur { args } => {
            for a in args {
                collect_free(a, bound, out);
            }
        }
        K::HoleStart { hole, body } => {
            let fresh = bound.insert(*hole);
            collect_free(body, bound, out);
            if fresh {
                bound.remove(hole);
            }
        }
        K::HoleFill { hole, cell, .. } => {
            if !bound.contains(hole) {
                out.insert(*hole);
            }
            collect_free(cell, bound, out);
        }
        K::HoleClose { hole, base } => {
            if !bound.contains(hole) {
                out.insert(*hole);
            }
            collect_free(base, bound, out);
        }
    }
}
