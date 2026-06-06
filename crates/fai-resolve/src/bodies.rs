//! Body resolution: bind every reference in every body to a local, a definition,
//! or a builtin.
//!
//! [`resolve`] is a per-file `salsa` query. Its value, [`ResolvedBodies`], keys
//! references by position-independent [`ExprId`] (never byte offsets), so it is
//! firewall-safe: reformatting does not change it, and the spans needed for
//! diagnostics/queries are looked up late from `parse`.
//!
//! Resolution order for a bare name is local scope, then this module's
//! top-level, then the prelude. Cross-module access is *only* via a qualified
//! `Foo.bar` (an `UpperIdent` base in a `Field`), which resolves to a public
//! binding of module `Foo`.

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit, is_std_path};
use fai_diagnostics::Diagnostic;
use fai_span::{SourceId, Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{
    ExprId, ExprKind, ItemKind, Module, PatId, PatKind, TypeDef, TypeId, TypeKind, Visibility,
    classify_op,
};
use rustc_hash::FxHashMap;

use crate::decls::type_decls;
use crate::ids::{CtorRef, DefId, LocalId, Res, is_upper};
use crate::intrinsics;
use crate::module::{
    ModuleName, emit_duplicate_module_errors, emit_duplicate_prelude_export_errors, module_defs,
    module_file, module_interface, prelude_exports,
};
use crate::{
    INTRINSIC_OUTSIDE_STD, PRIVATE_REFERENCE, PRIVATE_TYPE_IN_PUBLIC_SIGNATURE, SHADOWS_PRELUDE,
    UNBOUND_CONSTRUCTOR, UNBOUND_NAME, UNRESOLVED_MODULE,
};

/// The resolved references of one file's bodies.
///
/// `by_expr` maps each *referencing* expression (a `Var`, or the head of a
/// qualified `Field`) to what it resolved to. `deps` lists the distinct
/// definitions referenced from this file (for the dependency graph / SCCs and
/// for `dependents`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedBodies {
    /// Resolution of each referencing expression, keyed by `ExprId`.
    pub by_expr: FxHashMap<ExprId, Res>,
    /// Resolution of each constructor pattern's head, keyed by `PatId`.
    pub by_pat: FxHashMap<PatId, Res>,
    /// Distinct outbound definition references, in first-seen order (whole file).
    pub deps: Vec<DefId>,
    /// Distinct outbound definition references, per owning top-level definition.
    /// Contracts' references are not attributed to any def (they are checked
    /// per-file), so they appear only in `deps`.
    pub deps_by_def: FxHashMap<DefId, Vec<DefId>>,
    /// The local slot bound by each variable/wildcard pattern, so inference can
    /// type locals by the same `LocalId` resolution assigned to their uses.
    pub pat_locals: FxHashMap<PatId, LocalId>,
}

impl ResolvedBodies {
    /// The resolution recorded for `expr`, if any.
    #[must_use]
    pub fn get(&self, expr: ExprId) -> Option<Res> {
        self.by_expr.get(&expr).copied()
    }

    /// The distinct definitions referenced by `def`'s body.
    #[must_use]
    pub fn deps_of(&self, def: DefId) -> &[DefId] {
        self.deps_by_def.get(&def).map_or(&[], Vec::as_slice)
    }

    /// The local slot bound by a variable/wildcard pattern, if any.
    #[must_use]
    pub fn local_of(&self, pat: PatId) -> Option<LocalId> {
        self.pat_locals.get(&pat).copied()
    }

    /// The resolution recorded for a constructor pattern, if any.
    #[must_use]
    pub fn pat_res(&self, pat: PatId) -> Option<Res> {
        self.by_pat.get(&pat).copied()
    }
}

/// A lexical scope frame mapping local names to slots.
#[derive(Default)]
struct Scope {
    frames: Vec<FxHashMap<Symbol, LocalId>>,
    next_slot: usize,
}

impl Scope {
    fn push(&mut self) {
        self.frames.push(FxHashMap::default());
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    fn bind(&mut self, name: Symbol) -> LocalId {
        let id = LocalId::from_index(self.next_slot);
        self.next_slot += 1;
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name, id);
        }
        id
    }

    /// Allocates a slot with no name (a wildcard binding).
    fn bind_anonymous(&mut self) -> LocalId {
        let id = LocalId::from_index(self.next_slot);
        self.next_slot += 1;
        id
    }

    fn lookup(&self, name: Symbol) -> Option<LocalId> {
        self.frames.iter().rev().find_map(|frame| frame.get(&name).copied())
    }
}

/// Collects every type-constructor reference (`Con`) reachable from `ty`, with
/// its span, into `out`. Type variables, `Unit`, and error nodes contribute none.
fn collect_con_refs(module: &Module, ty: TypeId, out: &mut Vec<(Symbol, TextRange)>) {
    let node = module.ty(ty);
    match &node.kind {
        TypeKind::Con(name) => out.push((*name, node.span)),
        TypeKind::App { func, arg } => {
            collect_con_refs(module, *func, out);
            collect_con_refs(module, *arg, out);
        }
        TypeKind::Arrow { from, to } => {
            collect_con_refs(module, *from, out);
            collect_con_refs(module, *to, out);
        }
        TypeKind::Tuple(items) => {
            for &item in items {
                collect_con_refs(module, item, out);
            }
        }
        TypeKind::Record { fields, .. } => {
            for field in fields {
                collect_con_refs(module, field.ty, out);
            }
        }
        TypeKind::Paren(inner) => collect_con_refs(module, *inner, out),
        TypeKind::Var(_) | TypeKind::Unit | TypeKind::Error => {}
    }
}

/// Emits [`PRIVATE_TYPE_IN_PUBLIC_SIGNATURE`] for any public surface that names a
/// same-module private type: public binding signatures, public type-alias bodies,
/// and public union constructors' field types. Built-in, prelude, and other
/// modules' types are never local here, so they never trip the check; a private
/// type reached only from private surfaces is fine.
fn emit_privacy_leaks(db: &dyn Db, file: SourceFile, module: &Module, source: SourceId) {
    let decls = type_decls(db, file);
    // Fast path: with no private types there is nothing to leak.
    if decls.types.values().all(|t| t.visibility == Visibility::Public) {
        return;
    }

    let mut refs: Vec<(Symbol, TextRange)> = Vec::new();
    let check = |module: &Module, ty: TypeId, refs: &mut Vec<(Symbol, TextRange)>| {
        refs.clear();
        collect_con_refs(module, ty, refs);
        for &(name, span) in refs.iter() {
            if decls.types.get(&name).is_some_and(|t| t.visibility == Visibility::Private) {
                emit(
                    db,
                    Diagnostic::error(
                        PRIVATE_TYPE_IN_PUBLIC_SIGNATURE,
                        format!("the public signature exposes the private type `{name}`"),
                        Span::new(source, span),
                    )
                    .with_help(format!("make `{name}` public, or make this binding private")),
                );
            }
        }
    };

    for item in &module.items {
        match &item.kind {
            ItemKind::Signature { visibility: Visibility::Public, ty, .. } => {
                check(module, *ty, &mut refs);
            }
            ItemKind::Type { visibility: Visibility::Public, def, .. } => match def {
                TypeDef::Alias(ty) => check(module, *ty, &mut refs),
                TypeDef::Union(variants) => {
                    for variant in variants {
                        for &field in &variant.fields {
                            check(module, field, &mut refs);
                        }
                    }
                }
            },
            ItemKind::Interface { visibility: Visibility::Public, methods, .. } => {
                for m in methods {
                    check(module, m.ty, &mut refs);
                }
            }
            _ => {}
        }
    }
}

/// Resolves all bodies in `file`, emitting resolution diagnostics.
#[salsa::tracked]
pub fn resolve(db: &dyn Db, file: SourceFile) -> Arc<ResolvedBodies> {
    emit_duplicate_module_errors(db, file);
    emit_duplicate_prelude_export_errors(db, file);

    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let defs = module_defs(db, file);
    let source = file.source(db);

    // A public surface must not expose a same-module private type.
    emit_privacy_leaks(db, file, module, source);

    // A standard-library module: it may reach the `Prim` intrinsics, and its own
    // bindings *define* the auto-imported names (so they never "shadow").
    let is_std = is_std_path(file.path(db));

    // Names defined at this module's top level (binding present).
    let top_level: FxHashMap<Symbol, DefId> =
        defs.defs.iter().map(|d| (d.name, DefId::new(source, d.name))).collect();

    // Constructors declared in this module.
    let local_ctors: FxHashMap<Symbol, CtorRef> =
        type_decls(db, file).ctors.keys().map(|&name| (name, CtorRef::new(source, name))).collect();

    // The auto-imported core (the merged `Prelude` interface), visible unqualified.
    let exports = prelude_exports(db);
    let prelude_values: FxHashMap<Symbol, DefId> = exports.values.iter().copied().collect();
    let prelude_ctors: FxHashMap<Symbol, CtorRef> = exports.ctors.iter().copied().collect();

    let mut cx = Resolver {
        db,
        module,
        source,
        is_std,
        top_level: &top_level,
        local_ctors: &local_ctors,
        prelude_values: &prelude_values,
        prelude_ctors: &prelude_ctors,
        scope: Scope::default(),
        by_expr: FxHashMap::default(),
        by_pat: FxHashMap::default(),
        deps: Vec::new(),
        dep_seen: FxHashMap::default(),
        current_def: None,
        deps_by_def: FxHashMap::default(),
        pat_locals: FxHashMap::default(),
    };

    // Warn when a top-level binding shadows an auto-imported name — except inside
    // standard-library modules, whose bindings *define* those names.
    for d in &defs.defs {
        let shadows = prelude_values.contains_key(&d.name) || prelude_ctors.contains_key(&d.name);
        if !is_std && shadows {
            let span = module.items[d.binding.index()].span;
            emit(
                db,
                Diagnostic::warning(
                    SHADOWS_PRELUDE,
                    format!("`{}` shadows a prelude name", d.name),
                    Span::new(source, span),
                ),
            );
        }
    }

    for item in &module.items {
        match &item.kind {
            ItemKind::Binding { name, params, body, .. } => {
                cx.current_def = Some(DefId::new(file.source(db), *name));
                cx.scope.push();
                for &p in params {
                    cx.bind_pattern(p);
                }
                cx.resolve_expr(*body);
                cx.scope.pop();
                cx.current_def = None;
            }
            ItemKind::Example { body } => {
                cx.scope.push();
                cx.resolve_expr(*body);
                cx.scope.pop();
            }
            ItemKind::Forall { binders, body } => {
                cx.scope.push();
                for &b in binders {
                    cx.scope.bind(b);
                }
                cx.resolve_expr(*body);
                cx.scope.pop();
            }
            // Type and interface declarations introduce type-level names and
            // (for unions) constructors, resolved via `type_decls`/the types
            // phase; they have no value body to resolve here.
            ItemKind::Type { .. }
            | ItemKind::Interface { .. }
            | ItemKind::Signature { .. }
            | ItemKind::Error => {}
        }
    }

    Arc::new(ResolvedBodies {
        by_expr: cx.by_expr,
        by_pat: cx.by_pat,
        deps: cx.deps,
        deps_by_def: cx.deps_by_def,
        pat_locals: cx.pat_locals,
    })
}

/// The per-file resolution walker.
struct Resolver<'a> {
    db: &'a dyn Db,
    module: &'a Module,
    source: fai_span::SourceId,
    /// Whether this is a standard-library module (may use `Prim`).
    is_std: bool,
    top_level: &'a FxHashMap<Symbol, DefId>,
    local_ctors: &'a FxHashMap<Symbol, CtorRef>,
    prelude_values: &'a FxHashMap<Symbol, DefId>,
    prelude_ctors: &'a FxHashMap<Symbol, CtorRef>,
    scope: Scope,
    by_expr: FxHashMap<ExprId, Res>,
    by_pat: FxHashMap<PatId, Res>,
    deps: Vec<DefId>,
    dep_seen: FxHashMap<DefId, ()>,
    current_def: Option<DefId>,
    deps_by_def: FxHashMap<DefId, Vec<DefId>>,
    pat_locals: FxHashMap<PatId, LocalId>,
}

impl Resolver<'_> {
    fn span(&self, range: fai_span::TextRange) -> Span {
        Span::new(self.source, range)
    }

    fn record_dep(&mut self, def: DefId) {
        if self.dep_seen.insert(def, ()).is_none() {
            self.deps.push(def);
        }
        if let Some(owner) = self.current_def {
            let edges = self.deps_by_def.entry(owner).or_default();
            if !edges.contains(&def) {
                edges.push(def);
            }
        }
    }

    fn bind_pattern(&mut self, pat: PatId) {
        let node = self.module.pat(pat);
        match &node.kind {
            PatKind::Var(name) => {
                let slot = self.scope.bind(*name);
                self.pat_locals.insert(pat, slot);
            }
            PatKind::Wildcard => {
                let slot = self.scope.bind_anonymous();
                self.pat_locals.insert(pat, slot);
            }
            PatKind::Tuple(elems) | PatKind::List(elems) => {
                for &e in elems {
                    self.bind_pattern(e);
                }
            }
            PatKind::Cons { head, tail } => {
                self.bind_pattern(*head);
                self.bind_pattern(*tail);
            }
            PatKind::Constructor { name, args } => {
                // Resolve the constructor head (a reference), then bind its args.
                let res = self.resolve_ctor(*name);
                if matches!(res, Res::Error) {
                    emit(
                        self.db,
                        Diagnostic::error(
                            UNBOUND_CONSTRUCTOR,
                            format!("cannot find constructor `{name}` in scope"),
                            self.span(node.span),
                        ),
                    );
                }
                self.by_pat.insert(pat, res);
                for &a in args {
                    self.bind_pattern(a);
                }
            }
            PatKind::Or(alts) => {
                // Each alternative must bind the same variables; the types phase
                // checks that. Bind each so its variables are in scope.
                for &a in alts {
                    self.bind_pattern(a);
                }
            }
            PatKind::Record { fields, .. } => {
                // Each field's sub-pattern binds (punning's is a `Var(name)`).
                let pats: Vec<PatId> = fields.iter().map(|f| f.pat).collect();
                for p in pats {
                    self.bind_pattern(p);
                }
            }
            PatKind::Paren(inner) => self.bind_pattern(*inner),
            PatKind::Int(_)
            | PatKind::Float(_)
            | PatKind::String(_)
            | PatKind::Char(_)
            | PatKind::Bool(_)
            | PatKind::Unit
            | PatKind::Error => {}
        }
    }

    fn resolve_expr(&mut self, expr: ExprId) {
        let node = self.module.expr(expr);
        match &node.kind {
            ExprKind::Var(name) => {
                let res = self.resolve_name(*name);
                if let Res::Def(def) = res {
                    self.record_dep(def);
                }
                if matches!(res, Res::Error) {
                    let (code, msg) = if is_upper(*name) {
                        (UNBOUND_CONSTRUCTOR, format!("cannot find constructor `{name}` in scope"))
                    } else {
                        (UNBOUND_NAME, format!("cannot find `{name}` in scope"))
                    };
                    emit(self.db, Diagnostic::error(code, msg, self.span(node.span)));
                }
                self.by_expr.insert(expr, res);
            }
            ExprKind::Field { base, field } => {
                self.resolve_field(expr, *base, *field);
            }
            ExprKind::App { func, arg } => {
                self.resolve_expr(*func);
                self.resolve_expr(*arg);
            }
            ExprKind::Infix { op, lhs, rhs } => {
                // `op` is a `Var` node, resolved like any name (built-in, user, or
                // a shadowing local/top-level binding).
                self.resolve_expr(*op);
                self.resolve_expr(*lhs);
                self.resolve_expr(*rhs);
            }
            ExprKind::Prefix { op, operand } => {
                self.resolve_expr(*op);
                self.resolve_expr(*operand);
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                self.resolve_expr(*cond);
                self.resolve_expr(*then_branch);
                self.resolve_expr(*else_branch);
            }
            ExprKind::Lambda { params, body } => {
                self.scope.push();
                for &p in params {
                    self.bind_pattern(p);
                }
                self.resolve_expr(*body);
                self.scope.pop();
            }
            ExprKind::Match { scrutinee, arms } => {
                self.resolve_expr(*scrutinee);
                for arm in arms {
                    self.scope.push();
                    self.bind_pattern(arm.pat);
                    self.resolve_expr(arm.body);
                    self.scope.pop();
                }
            }
            ExprKind::Block { stmts, tail } => {
                self.scope.push();
                for stmt in stmts {
                    // A local function binding's params scope over its value.
                    self.scope.push();
                    for &p in &stmt.params {
                        self.bind_pattern(p);
                    }
                    self.resolve_expr(stmt.value);
                    self.scope.pop();
                    // The bound pattern is in scope for subsequent statements.
                    self.bind_pattern(stmt.pat);
                }
                self.resolve_expr(*tail);
                self.scope.pop();
            }
            ExprKind::Paren(inner) => self.resolve_expr(*inner),
            ExprKind::Tuple(elems) | ExprKind::List(elems) => {
                for &e in elems {
                    self.resolve_expr(e);
                }
            }
            ExprKind::Record(fields) => {
                for f in fields {
                    self.resolve_expr(f.value);
                }
            }
            ExprKind::RecordUpdate { base, fields } => {
                self.resolve_expr(*base);
                for f in fields {
                    self.resolve_expr(f.value);
                }
            }
            // An interface instance `{ Name with m args = body, … }`. The
            // interface name is resolved in the types phase; each method body is
            // resolved with its own parameters in scope but *without* sibling
            // methods (record semantics).
            ExprKind::Instance { methods, .. } => {
                for m in methods {
                    self.scope.push();
                    for &p in &m.params {
                        self.bind_pattern(p);
                    }
                    self.resolve_expr(m.body);
                    self.scope.pop();
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Char(_)
            | ExprKind::Unit
            | ExprKind::Error => {}
        }
    }

    /// Resolves a bare name. Upper-case names are constructors (this module, then
    /// the auto-imported core); lower-case names are `true`/`false`, then local
    /// scope, this module's top level, then the auto-imported core's values.
    /// (Intrinsics are reached only as `Prim.<name>` inside standard-library
    /// modules, never as a bare name.)
    fn resolve_name(&self, name: Symbol) -> Res {
        // `true`/`false` are keyword-like boolean literals parsed as `Var`.
        if matches!(name.as_str(), "true" | "false") {
            return Res::Builtin(name);
        }
        // The short-circuit operators and the list-cons constructor are built-in
        // syntax — never shadowed by a user binding.
        if matches!(name.as_str(), "&&" | "||" | "::") {
            return Res::Builtin(name);
        }
        if let Some(local) = self.scope.lookup(name) {
            return Res::Local(local);
        }
        if is_upper(name) {
            return self.resolve_ctor(name);
        }
        if let Some(def) = self.top_level.get(&name) {
            return Res::Def(*def);
        }
        if let Some(def) = self.prelude_values.get(&name) {
            return Res::Def(*def);
        }
        // The built-in operators are reachable as bare names (the standard-library
        // interfaces will own them); a same-named user binding shadows them above.
        if classify_op(name).is_some() {
            return Res::Builtin(name);
        }
        Res::Error
    }

    /// Resolves an upper-case constructor name: this module's constructors, then
    /// the prelude module's.
    fn resolve_ctor(&self, name: Symbol) -> Res {
        if let Some(ctor) = self.local_ctors.get(&name) {
            return Res::Ctor(*ctor);
        }
        if let Some(ctor) = self.prelude_ctors.get(&name) {
            return Res::Ctor(*ctor);
        }
        Res::Error
    }

    /// Resolves a `Field`. A `Var(Upper)` base is a qualified cross-module
    /// reference `Foo.bar`; any other base is record field access (unimplemented
    /// in M2 — the types phase reports it). Depth-1 only.
    fn resolve_field(&mut self, expr: ExprId, base: ExprId, field: Symbol) {
        let base_node = self.module.expr(base);
        if let ExprKind::Var(module_sym) = &base_node.kind
            && is_upper(*module_sym)
        {
            let res = self.resolve_qualified(
                *module_sym,
                field,
                base_node.span,
                self.module.expr(expr).span,
            );
            if let Res::Def(def) = res {
                self.record_dep(def);
            }
            self.by_expr.insert(expr, res);
            return;
        }
        // Record field access: resolve the base; the field is handled by types.
        self.resolve_expr(base);
    }

    fn resolve_qualified(
        &self,
        module_sym: Symbol,
        field: Symbol,
        base_span: fai_span::TextRange,
        whole_span: fai_span::TextRange,
    ) -> Res {
        // The prelude-private intrinsics module: `Prim.<name>` is reachable only
        // from standard-library modules, and only for a known intrinsic.
        if module_sym.as_str() == intrinsics::PRIM_MODULE {
            if !self.is_std {
                emit(
                    self.db,
                    Diagnostic::error(
                        INTRINSIC_OUTSIDE_STD,
                        format!(
                            "the intrinsics module `{}` is only available inside standard-library \
                             modules",
                            intrinsics::PRIM_MODULE
                        ),
                        self.span(base_span),
                    ),
                );
                return Res::Error;
            }
            if intrinsics::is_intrinsic(field) {
                return Res::Builtin(field);
            }
            emit(
                self.db,
                Diagnostic::error(
                    UNBOUND_NAME,
                    format!("`{}` has no intrinsic `{field}`", intrinsics::PRIM_MODULE),
                    self.span(whole_span),
                ),
            );
            return Res::Error;
        }
        let Some(target_file) = module_file(self.db, ModuleName(module_sym)) else {
            emit(
                self.db,
                Diagnostic::error(
                    UNRESOLVED_MODULE,
                    format!("no module named `{module_sym}` in the workspace"),
                    self.span(base_span),
                ),
            );
            return Res::Error;
        };
        let target_source = target_file.source(self.db);
        let defs = module_defs(self.db, target_file);
        match defs.get(field) {
            Some(def) if def.visibility == Visibility::Public => {
                return Res::Def(DefId::new(target_source, field));
            }
            Some(_) => {
                emit(
                    self.db,
                    Diagnostic::error(
                        PRIVATE_REFERENCE,
                        format!("`{field}` is private to module `{module_sym}`"),
                        self.span(whole_span),
                    ),
                );
                return Res::Error;
            }
            None => {}
        }
        // Not a value: try a constructor of the target module.
        if module_interface(self.db, target_file).has_ctor(field) {
            return Res::Ctor(CtorRef::new(target_source, field));
        }
        if let Some(info) = type_decls(self.db, target_file).ctor(field) {
            // The constructor exists but its type is private.
            let _ = info;
            emit(
                self.db,
                Diagnostic::error(
                    PRIVATE_REFERENCE,
                    format!("constructor `{field}` is private to module `{module_sym}`"),
                    self.span(whole_span),
                ),
            );
            return Res::Error;
        }
        emit(
            self.db,
            Diagnostic::error(
                UNBOUND_NAME,
                format!("module `{module_sym}` has no member `{field}`"),
                self.span(whole_span),
            ),
        );
        Res::Error
    }
}
