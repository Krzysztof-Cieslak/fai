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

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{ExprId, ExprKind, ItemKind, Module, Pat, PatId, PatKind, Visibility};
use rustc_hash::FxHashMap;

use crate::ids::{DefId, LocalId, Res, is_upper};
use crate::module::{ModuleName, emit_duplicate_module_errors, module_defs, module_file};
use crate::prelude;
use crate::{PRIVATE_REFERENCE, SHADOWS_PRELUDE, UNBOUND_NAME, UNRESOLVED_MODULE};

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

/// Resolves all bodies in `file`, emitting resolution diagnostics.
#[salsa::tracked]
pub fn resolve(db: &dyn Db, file: SourceFile) -> Arc<ResolvedBodies> {
    emit_duplicate_module_errors(db, file);

    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let defs = module_defs(db, file);

    // Names defined at this module's top level (binding present).
    let top_level: FxHashMap<Symbol, DefId> =
        defs.defs.iter().map(|d| (d.name, DefId::new(file.source(db), d.name))).collect();

    let mut cx = Resolver {
        db,
        file,
        module,
        top_level: &top_level,
        scope: Scope::default(),
        by_expr: FxHashMap::default(),
        deps: Vec::new(),
        dep_seen: FxHashMap::default(),
        current_def: None,
        deps_by_def: FxHashMap::default(),
        pat_locals: FxHashMap::default(),
    };

    // Warn when a top-level binding shadows a prelude name — except inside the
    // prelude module itself, whose bindings *define* those names.
    let is_prelude_module = module.name.is_some_and(|n| n.as_str() == prelude::PRELUDE_MODULE);
    for d in &defs.defs {
        if !is_prelude_module && prelude::is_prelude_name(d.name) {
            let span = module.items[d.binding.index()].span;
            emit(
                db,
                Diagnostic::warning(
                    SHADOWS_PRELUDE,
                    format!("`{}` shadows a prelude name", d.name),
                    Span::new(file.source(db), span),
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
            ItemKind::Signature { .. } | ItemKind::Error => {}
        }
    }

    Arc::new(ResolvedBodies {
        by_expr: cx.by_expr,
        deps: cx.deps,
        deps_by_def: cx.deps_by_def,
        pat_locals: cx.pat_locals,
    })
}

/// The per-file resolution walker.
struct Resolver<'a> {
    db: &'a dyn Db,
    file: SourceFile,
    module: &'a Module,
    top_level: &'a FxHashMap<Symbol, DefId>,
    scope: Scope,
    by_expr: FxHashMap<ExprId, Res>,
    deps: Vec<DefId>,
    dep_seen: FxHashMap<DefId, ()>,
    current_def: Option<DefId>,
    deps_by_def: FxHashMap<DefId, Vec<DefId>>,
    pat_locals: FxHashMap<PatId, LocalId>,
}

impl Resolver<'_> {
    fn span(&self, range: fai_span::TextRange) -> Span {
        Span::new(self.file.source(self.db), range)
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
        let Pat { kind, .. } = self.module.pat(pat);
        match kind {
            PatKind::Var(name) => {
                let slot = self.scope.bind(*name);
                self.pat_locals.insert(pat, slot);
            }
            PatKind::Wildcard => {
                let slot = self.scope.bind_anonymous();
                self.pat_locals.insert(pat, slot);
            }
            PatKind::Tuple(elems) => {
                for &e in elems {
                    self.bind_pattern(e);
                }
            }
            PatKind::Paren(inner) => self.bind_pattern(*inner),
            PatKind::Unit | PatKind::Error => {}
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
                    emit(
                        self.db,
                        Diagnostic::error(
                            UNBOUND_NAME,
                            format!("cannot find `{name}` in scope"),
                            self.span(node.span),
                        ),
                    );
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
            ExprKind::Binary { lhs, rhs, .. } => {
                self.resolve_expr(*lhs);
                self.resolve_expr(*rhs);
            }
            ExprKind::Unary { operand, .. } => self.resolve_expr(*operand),
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
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Char(_)
            | ExprKind::Unit
            | ExprKind::Error => {}
        }
    }

    /// Resolves a bare name: literals (`true`/`false`), local scope, this
    /// module's top level, then the prelude.
    fn resolve_name(&self, name: Symbol) -> Res {
        // `true`/`false` are keyword-like boolean literals parsed as `Var`.
        if matches!(name.as_str(), "true" | "false") {
            return Res::Builtin(name);
        }
        if let Some(local) = self.scope.lookup(name) {
            return Res::Local(local);
        }
        if let Some(def) = self.top_level.get(&name) {
            return Res::Def(*def);
        }
        if prelude::is_prelude_name(name) {
            return Res::Builtin(name);
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
        // Built-in capability modules (M3): `Console.writeLine` resolves to a
        // qualified builtin. A placeholder until interfaces/records (M5) make
        // capabilities ordinary values reached through a `Runtime` record.
        if module_sym.as_str() == "Console" && field.as_str() == "writeLine" {
            return Res::Builtin(field);
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
        let defs = module_defs(self.db, target_file);
        match defs.get(field) {
            Some(def) if def.visibility == Visibility::Public => {
                Res::Def(DefId::new(target_file.source(self.db), field))
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
                Res::Error
            }
            None => {
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
    }
}
