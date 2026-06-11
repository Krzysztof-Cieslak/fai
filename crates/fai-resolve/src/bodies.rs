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
    ExprId, ExprKind, ItemId, ItemKind, Module, PatId, PatKind, TypeDef, TypeId, TypeKind,
    Visibility, classify_op,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::decls::type_decls;
use crate::ids::{CtorRef, DefId, LocalId, Res, is_upper, qualify};
use crate::intrinsics;
use crate::module::{
    ModuleName, emit_duplicate_module_errors, emit_duplicate_prelude_export_errors, module_defs,
    module_file, module_interface, prelude_exports,
};
use crate::{
    INTRINSIC_OUTSIDE_STD, MODULE_AS_VALUE, OPAQUE_CONSTRUCTOR, PRIVATE_REFERENCE,
    PRIVATE_TYPE_IN_PUBLIC_SIGNATURE, SHADOWS_PRELUDE, UNBOUND_CONSTRUCTOR, UNBOUND_NAME,
    UNRESOLVED_MODULE,
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
        TypeKind::Arrow { from, to, .. } => {
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
        // An effect row's atoms are capability names resolved during lowering (as
        // for an arrow's effect annotation), not type-constructor references.
        TypeKind::Var(_) | TypeKind::EffectRow { .. } | TypeKind::Unit | TypeKind::Error => {}
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
            // An opaque type's definition (alias body / constructor fields) is not
            // cross-file-visible, so it cannot leak a private type — skip it.
            ItemKind::Type { visibility: Visibility::Public, opaque: false, def, .. } => {
                match def {
                    TypeDef::Alias(ty) => check(module, *ty, &mut refs),
                    TypeDef::Union(variants) => {
                        for variant in variants {
                            for &field in &variant.fields {
                                check(module, field, &mut refs);
                            }
                        }
                    }
                }
            }
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

    // Every definition in the file, keyed by its qualified name.
    let def_ids: FxHashMap<Symbol, DefId> =
        defs.defs.iter().map(|d| (d.name, DefId::new(source, d.name))).collect();

    // The nested module paths in this file (qualified), used to recognize a
    // module segment during qualified-path resolution.
    let modules: FxHashSet<Symbol> = defs.modules.iter().copied().collect();

    // Constructors declared in this module (qualified by their module path).
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
        defs: &def_ids,
        ctors: &local_ctors,
        modules: &modules,
        prelude_values: &prelude_values,
        prelude_ctors: &prelude_ctors,
        scope: Scope::default(),
        current_scope: Vec::new(),
        by_expr: FxHashMap::default(),
        by_pat: FxHashMap::default(),
        deps: Vec::new(),
        dep_seen: FxHashMap::default(),
        current_def: None,
        deps_by_def: FxHashMap::default(),
        pat_locals: FxHashMap::default(),
    };

    resolve_items(&mut cx, module, &module.roots);

    Arc::new(ResolvedBodies {
        by_expr: cx.by_expr,
        by_pat: cx.by_pat,
        deps: cx.deps,
        deps_by_def: cx.deps_by_def,
        pat_locals: cx.pat_locals,
    })
}

/// Resolves the bodies of a module scope (`items`), descending into nested
/// modules so each body resolves with its lexical scope path active.
fn resolve_items(cx: &mut Resolver, module: &Module, items: &[ItemId]) {
    for &id in items {
        match &module.items[id.index()].kind {
            ItemKind::Binding { name, params, body, .. } => {
                // A binding whose local name shadows an auto-imported name (except
                // in standard-library modules, whose bindings *define* them).
                if !cx.is_std
                    && (cx.prelude_values.contains_key(name) || cx.prelude_ctors.contains_key(name))
                {
                    emit(
                        cx.db,
                        Diagnostic::warning(
                            SHADOWS_PRELUDE,
                            format!("`{name}` shadows a prelude name"),
                            Span::new(cx.source, module.items[id.index()].span),
                        ),
                    );
                }
                cx.current_def = Some(DefId::new(cx.source, qualify(&cx.current_scope, *name)));
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
                for &p in binders {
                    cx.bind_pattern(p);
                }
                cx.resolve_expr(*body);
                cx.scope.pop();
            }
            ItemKind::Module { name, body } => {
                cx.current_scope.push(*name);
                resolve_items(cx, module, body);
                cx.current_scope.pop();
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
}

/// The per-file resolution walker.
struct Resolver<'a> {
    db: &'a dyn Db,
    module: &'a Module,
    source: fai_span::SourceId,
    /// Whether this is a standard-library module (may use `Prim`).
    is_std: bool,
    /// Every definition in the file, keyed by qualified name.
    defs: &'a FxHashMap<Symbol, DefId>,
    /// Constructors declared in this file, keyed by qualified name.
    ctors: &'a FxHashMap<Symbol, CtorRef>,
    /// Qualified paths of the nested modules declared in this file.
    modules: &'a FxHashSet<Symbol>,
    prelude_values: &'a FxHashMap<Symbol, DefId>,
    prelude_ctors: &'a FxHashMap<Symbol, CtorRef>,
    scope: Scope,
    /// The module path of the definition currently being resolved (empty at the
    /// top level), for lexical (outward) bare-name resolution.
    current_scope: Vec<Symbol>,
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
                // A dotted name is a qualified constructor path; the path resolver
                // reports its own errors.
                let res = if name.as_str().contains('.') {
                    let segments: Vec<Symbol> =
                        name.as_str().split('.').map(Symbol::intern).collect();
                    match self.resolve_path(&segments, node.span) {
                        Some((res, _)) => res,
                        None => {
                            emit(
                                self.db,
                                Diagnostic::error(
                                    UNBOUND_CONSTRUCTOR,
                                    format!("cannot find constructor `{name}` in scope"),
                                    self.span(node.span),
                                ),
                            );
                            Res::Error
                        }
                    }
                } else {
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
                    res
                };
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
            PatKind::As { pat: inner, name } => {
                // Bind the alias name (keyed by the as-pattern node), then bind the
                // inner pattern's own variables.
                let slot = self.scope.bind(*name);
                self.pat_locals.insert(pat, slot);
                self.bind_pattern(*inner);
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
                    let (code, msg) = if !is_upper(*name) {
                        (UNBOUND_NAME, format!("cannot find `{name}` in scope"))
                    } else if self.same_file_module_anchor(*name).is_some()
                        || module_file(self.db, ModuleName(*name)).is_some()
                    {
                        // A bare module name used where a value is expected.
                        (MODULE_AS_VALUE, format!("`{name}` is a module, not a value"))
                    } else {
                        (UNBOUND_CONSTRUCTOR, format!("cannot find constructor `{name}` in scope"))
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

    /// Resolves a bare name. `true`/`false` and the built-in operators are
    /// builtins; otherwise local scope, then the lexical module-scope chain
    /// (inner → outer), then the auto-imported core. Upper-case names are
    /// constructors. (Intrinsics are reached only as `Prim.<name>` inside
    /// standard-library modules, never as a bare name.)
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
        if let Some(def) = self.lookup_value(name) {
            return Res::Def(def);
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

    /// Looks a bare value name up the lexical module-scope chain (innermost scope
    /// first), returning the qualified definition it binds to.
    fn lookup_value(&self, local: Symbol) -> Option<DefId> {
        for k in (0..=self.current_scope.len()).rev() {
            let cand = qualify(&self.current_scope[..k], local);
            if let Some(def) = self.defs.get(&cand) {
                return Some(*def);
            }
        }
        None
    }

    /// Resolves a bare upper-case constructor name up the lexical scope chain,
    /// then the auto-imported core. (Qualified constructors `M.Ctor` go through
    /// the path resolver.)
    fn resolve_ctor(&self, name: Symbol) -> Res {
        for k in (0..=self.current_scope.len()).rev() {
            let cand = qualify(&self.current_scope[..k], name);
            if let Some(ctor) = self.ctors.get(&cand) {
                return Res::Ctor(*ctor);
            }
        }
        if let Some(ctor) = self.prelude_ctors.get(&name) {
            return Res::Ctor(*ctor);
        }
        Res::Error
    }

    /// Resolves a `Field` chain. A chain whose innermost base is an upper-case
    /// `Var` may be a qualified reference (`Outer.Inner.member`); otherwise it is
    /// ordinary record-field access (the field is handled by the types phase).
    fn resolve_field(&mut self, expr: ExprId, base: ExprId, _field: Symbol) {
        let Some(chain) = self.flatten_chain(expr) else {
            // The head is a complex expression: ordinary record-field access.
            self.resolve_expr(base);
            return;
        };
        let head = chain[0].0;
        if !is_upper(head) {
            // Record-field access on a local/value: resolve the head; the types
            // phase handles the fields.
            self.resolve_expr(chain[0].1);
            return;
        }
        let segments: Vec<Symbol> = chain.iter().map(|&(s, _)| s).collect();
        let span = self.module.expr(expr).span;
        match self.resolve_path(&segments, span) {
            Some((res, consumed)) => {
                // The member reference is the chain node at `consumed - 1`; any
                // trailing segments are record-field accesses (no resolution).
                let member_expr = chain[consumed - 1].1;
                if let Res::Def(def) = res {
                    self.record_dep(def);
                }
                self.by_expr.insert(member_expr, res);
            }
            None => {
                // An upper head in a field chain is a qualified reference; if it
                // names no module (nested or cross-file), it is unresolved.
                let head_span = self.module.expr(chain[0].1).span;
                emit(
                    self.db,
                    Diagnostic::error(
                        UNRESOLVED_MODULE,
                        format!("no module named `{head}` in scope"),
                        self.span(head_span),
                    ),
                );
                self.by_expr.insert(expr, Res::Error);
            }
        }
    }

    /// Flattens a `Field`/`Var` spine into its segments, innermost first, each
    /// paired with the expression node that ends at that segment. Returns `None`
    /// when the head is not a simple name (so the chain is record access on a
    /// complex expression).
    fn flatten_chain(&self, expr: ExprId) -> Option<Vec<(Symbol, ExprId)>> {
        let mut rev: Vec<(Symbol, ExprId)> = Vec::new();
        let mut cur = expr;
        loop {
            match &self.module.expr(cur).kind {
                ExprKind::Field { base, field } => {
                    rev.push((*field, cur));
                    cur = *base;
                }
                ExprKind::Var(sym) => {
                    rev.push((*sym, cur));
                    break;
                }
                _ => return None,
            }
        }
        rev.reverse();
        Some(rev)
    }

    /// Resolves a qualified dotted path `segments` (the head is upper-case).
    /// Returns the member's resolution plus the number of leading segments it
    /// spans (the module path plus the member); the remaining segments are
    /// record-field accesses. Returns `None` when the head is not a module at all
    /// (the caller falls back to constructor/record handling). Emits the
    /// appropriate diagnostic on a genuine path error.
    fn resolve_path(&self, segments: &[Symbol], span: TextRange) -> Option<(Res, usize)> {
        let head = segments[0];

        // `Prim.<name>`: prelude-private intrinsics, only inside std modules.
        if head.as_str() == intrinsics::PRIM_MODULE {
            if !self.is_std {
                emit(
                    self.db,
                    Diagnostic::error(
                        INTRINSIC_OUTSIDE_STD,
                        format!(
                            "the intrinsics module `{}` is only available inside \
                             standard-library modules",
                            intrinsics::PRIM_MODULE
                        ),
                        self.span(span),
                    ),
                );
                return Some((Res::Error, 1));
            }
            if segments.len() >= 2 && intrinsics::is_intrinsic(segments[1]) {
                return Some((Res::Builtin(segments[1]), 2));
            }
            emit(
                self.db,
                Diagnostic::error(
                    UNBOUND_NAME,
                    format!(
                        "`{}` has no intrinsic `{}`",
                        intrinsics::PRIM_MODULE,
                        segments.get(1).map_or("", |s| s.as_str())
                    ),
                    self.span(span),
                ),
            );
            return Some((Res::Error, segments.len().clamp(1, 2)));
        }

        // A nested module visible in the current lexical scope chain.
        if let Some(anchor) = self.same_file_module_anchor(head) {
            return Some(self.walk_same_file(anchor, segments, span));
        }

        // A workspace file module (the current file's own name resolves here too,
        // so a self-qualified reference is treated like any cross-module one).
        if let Some(target) = module_file(self.db, ModuleName(head)) {
            return Some(self.walk_cross_file(target, segments, span));
        }

        None
    }

    /// The innermost qualified module path for which `head` names a nested module
    /// visible in the current scope chain, if any.
    fn same_file_module_anchor(&self, head: Symbol) -> Option<Symbol> {
        for k in (0..=self.current_scope.len()).rev() {
            let cand = qualify(&self.current_scope[..k], head);
            if self.modules.contains(&cand) {
                return Some(cand);
            }
        }
        None
    }

    /// Resolves the member of a same-file nested-module path. Same-file access has
    /// no visibility gate (the enclosing file sees every nested member).
    fn walk_same_file(&self, anchor: Symbol, segments: &[Symbol], span: TextRange) -> (Res, usize) {
        let mut module_path = anchor;
        let mut consumed = 1;
        while consumed < segments.len() {
            let cand = extend(module_path, segments[consumed]);
            if self.modules.contains(&cand) {
                module_path = cand;
                consumed += 1;
            } else {
                break;
            }
        }
        if consumed >= segments.len() {
            self.emit_module_as_value(module_path, span);
            return (Res::Error, consumed);
        }
        let member = segments[consumed];
        let member_qual = extend(module_path, member);
        consumed += 1;
        let res = if is_upper(member) {
            match self.ctors.get(&member_qual) {
                Some(ctor) => Res::Ctor(*ctor),
                None => self.emit_no_member(module_path, member, span),
            }
        } else {
            match self.defs.get(&member_qual) {
                Some(def) => Res::Def(*def),
                None => self.emit_no_member(module_path, member, span),
            }
        };
        (res, consumed)
    }

    /// Resolves the member of a cross-file (possibly nested) module path. Only
    /// `public` members are visible across files.
    fn walk_cross_file(
        &self,
        target: SourceFile,
        segments: &[Symbol],
        span: TextRange,
    ) -> (Res, usize) {
        let target_source = target.source(self.db);
        let target_defs = module_defs(self.db, target);
        // Walk nested-module segments within the target file.
        let mut inner: Vec<Symbol> = Vec::new();
        let mut consumed = 1;
        while consumed < segments.len() {
            let cand = qualify(&inner, segments[consumed]);
            if target_defs.is_module(cand) {
                inner.push(segments[consumed]);
                consumed += 1;
            } else {
                break;
            }
        }
        if consumed >= segments.len() {
            self.emit_module_as_value(join_segments(&segments[..consumed]), span);
            return (Res::Error, consumed);
        }
        let member = segments[consumed];
        let member_qual = qualify(&inner, member);
        consumed += 1;
        match target_defs.get(member_qual) {
            Some(def) if def.visibility == Visibility::Public => {
                return (Res::Def(DefId::new(target_source, member_qual)), consumed);
            }
            Some(_) => {
                emit(
                    self.db,
                    Diagnostic::error(
                        PRIVATE_REFERENCE,
                        format!("`{member}` is private to module `{}`", segments[0]),
                        self.span(span),
                    ),
                );
                return (Res::Error, consumed);
            }
            None => {}
        }
        if module_interface(self.db, target).has_ctor(member_qual) {
            return (Res::Ctor(CtorRef::new(target_source, member_qual)), consumed);
        }
        let target_decls = type_decls(self.db, target);
        if let Some(ctor) = target_decls.ctor(member_qual) {
            // The constructor exists but is not exported. Distinguish an opaque
            // type (name exported, constructors hidden) from a plain private type.
            let opaque = target_decls.type_named(ctor.adt).is_some_and(|t| t.opaque);
            let diagnostic = if opaque {
                Diagnostic::error(
                    OPAQUE_CONSTRUCTOR,
                    format!("`{member}` is a constructor of the opaque type `{}`", ctor.adt),
                    self.span(span),
                )
                .with_help(format!(
                    "the type is opaque outside its module; build and match values through \
                     `{}`'s operations instead",
                    segments[0]
                ))
            } else {
                Diagnostic::error(
                    PRIVATE_REFERENCE,
                    format!("constructor `{member}` is private to module `{}`", segments[0]),
                    self.span(span),
                )
            };
            emit(self.db, diagnostic);
            return (Res::Error, consumed);
        }
        (self.emit_no_member_named(segments[0], member, span), consumed)
    }

    fn emit_module_as_value(&self, module_path: Symbol, span: TextRange) {
        emit(
            self.db,
            Diagnostic::error(
                MODULE_AS_VALUE,
                format!("`{module_path}` is a module, not a value or type"),
                self.span(span),
            )
            .with_help("name a member of the module (e.g. `Module.value`)"),
        );
    }

    fn emit_no_member(&self, module_path: Symbol, member: Symbol, span: TextRange) -> Res {
        self.emit_no_member_named(module_path, member, span)
    }

    fn emit_no_member_named(&self, module_path: Symbol, member: Symbol, span: TextRange) -> Res {
        emit(
            self.db,
            Diagnostic::error(
                UNBOUND_NAME,
                format!("module `{module_path}` has no member `{member}`"),
                self.span(span),
            ),
        );
        Res::Error
    }
}

/// Extends a qualified module path with one more segment (`A.B` + `c` → `A.B.c`).
fn extend(base: Symbol, seg: Symbol) -> Symbol {
    Symbol::intern(&format!("{}.{}", base.as_str(), seg.as_str()))
}

/// Joins path segments into one dotted symbol (for diagnostic messages).
fn join_segments(segments: &[Symbol]) -> Symbol {
    let mut s = String::new();
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            s.push('.');
        }
        s.push_str(seg.as_str());
    }
    Symbol::intern(&s)
}
