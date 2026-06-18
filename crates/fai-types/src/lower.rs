//! Lowering written type expressions (the AST) into the internal [`Ty`].
//!
//! Surface type variables (`'a`) map to fresh [`TyVarId`]s, shared by name within
//! one lowering. Type-constructor names resolve against the built-in set, then the
//! current module's and the prelude's `type` declarations: a discriminated union
//! lowers to a nominal [`Ty::Adt`] applied to its arguments, a transparent alias
//! is expanded (with a cycle check). A whole signature lowers to a [`Scheme`]
//! quantifying every variable it mentions.

use std::cell::RefCell;
use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{
    AdtRef, InterfaceInfo, InterfaceRef, ModuleName, TypeDeclInfo, interface_decls, module_defs,
    module_file, prelude_exports, qualify, type_decls,
};
use fai_span::{SourceId, Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{EffectAnnot, ItemKind, Module, RowTail, TypeDef, TypeId, TypeKind};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ty::{EffEnd, EffRowVarId, EffectRow, RecordRow, RowEnd, RowVarId, Scheme, Ty, TyVarId};
use crate::{RECURSIVE_ALIAS, TYPE_ARITY, UNKNOWN_TYPE_CONSTRUCTOR};

/// A scratch map from surface type-variable names to assigned ids during one
/// lowering, plus a fresh-id counter local to that lowering.
#[derive(Default)]
pub struct LowerVars {
    by_name: FxHashMap<Symbol, TyVarId>,
    next: u32,
    rows_by_name: FxHashMap<Symbol, RowVarId>,
    row_next: u32,
    /// Row variables in allocation order, with their spellings (`_` anonymous).
    row_order: Vec<(RowVarId, String)>,
    effs_by_name: FxHashMap<Symbol, EffRowVarId>,
    eff_next: u32,
    /// Effect-row variables in allocation order, with their spellings.
    eff_order: Vec<(EffRowVarId, String)>,
}

impl LowerVars {
    pub(crate) fn var(&mut self, name: Symbol) -> TyVarId {
        if let Some(id) = self.by_name.get(&name) {
            return *id;
        }
        let id = TyVarId(self.next);
        self.next += 1;
        self.by_name.insert(name, id);
        id
    }

    /// A named open tail `'r` (shared by name, so `setX`'s tail threads through).
    fn row_named(&mut self, name: Symbol) -> RowVarId {
        if let Some(id) = self.rows_by_name.get(&name) {
            return *id;
        }
        let id = RowVarId(self.row_next);
        self.row_next += 1;
        self.rows_by_name.insert(name, id);
        self.row_order.push((id, name.as_str().to_owned()));
        id
    }

    /// A fresh anonymous open tail `_` (never shared).
    fn row_anon(&mut self) -> RowVarId {
        let id = RowVarId(self.row_next);
        self.row_next += 1;
        self.row_order.push((id, "_".to_owned()));
        id
    }

    /// A named open effect tail `'e` (shared by name across one signature).
    fn eff_named(&mut self, name: Symbol) -> EffRowVarId {
        if let Some(id) = self.effs_by_name.get(&name) {
            return *id;
        }
        let id = EffRowVarId(self.eff_next);
        self.eff_next += 1;
        self.effs_by_name.insert(name, id);
        self.eff_order.push((id, name.as_str().to_owned()));
        id
    }

    /// A fresh anonymous open effect tail `_` (never shared).
    fn eff_anon(&mut self) -> EffRowVarId {
        let id = EffRowVarId(self.eff_next);
        self.eff_next += 1;
        self.eff_order.push((id, "_".to_owned()));
        id
    }

    /// The effect-row variables in allocation order, with their spellings.
    fn effects(&self) -> Vec<(EffRowVarId, String)> {
        self.eff_order.clone()
    }

    /// The `(var, source-name)` pairs, ordered by var id. Names keep the written
    /// `'a` spelling.
    fn named(&self) -> Vec<(TyVarId, String)> {
        let mut pairs: Vec<(TyVarId, String)> =
            self.by_name.iter().map(|(name, id)| (*id, name.as_str().to_owned())).collect();
        pairs.sort_by_key(|(id, _)| *id);
        pairs
    }

    /// The row variables in allocation order, with their spellings.
    fn rows(&self) -> Vec<(RowVarId, String)> {
        self.row_order.clone()
    }
}

/// The type-lowering context for one file's module, tracking the alias-expansion
/// stack so a recursive transparent alias is detected rather than looping.
struct Lowerer<'a> {
    db: &'a dyn Db,
    file: SourceFile,
    module: &'a Module,
    /// The module path where the lowered type appears, for lexical (outward)
    /// resolution of a bare type/interface name.
    scope: Vec<Symbol>,
    /// Names of aliases currently being expanded (cycle detection).
    expanding: Vec<Symbol>,
}

impl Lowerer<'_> {
    fn lower(&mut self, ty: TypeId, vars: &mut LowerVars) -> Ty {
        let (head, args) = peel_spine(self.module, ty);
        let head_node = self.module.ty(head);
        if let TypeKind::Con(name) = &head_node.kind {
            return self.lower_con_app(*name, &args, head_node.span, vars);
        }
        // A non-constructor head: lower it, then fold any arguments as `App`.
        let mut t = self.lower_leaf(head, vars);
        for arg in args {
            t = Ty::App(Arc::new(t), Arc::new(self.lower(arg, vars)));
        }
        t
    }

    /// Lowers a node that is not a constructor application head.
    fn lower_leaf(&mut self, ty: TypeId, vars: &mut LowerVars) -> Ty {
        match &self.module.ty(ty).kind {
            TypeKind::Var(name) => Ty::Var(vars.var(*name)),
            TypeKind::Arrow { from, to, effect } => {
                let f = self.lower(*from, vars);
                let t = self.lower(*to, vars);
                let eff = self.lower_effect(effect.as_ref(), vars);
                Ty::arrow_eff(f, t, eff)
            }
            TypeKind::Tuple(elems) => {
                Ty::Tuple(elems.iter().map(|&e| self.lower(e, vars)).collect())
            }
            TypeKind::Record { fields, tail } => {
                let mut row: Vec<(Symbol, Ty)> =
                    fields.iter().map(|f| (f.name, self.lower(f.ty, vars))).collect();
                row.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                let row_tail = match tail {
                    RowTail::Closed => RowEnd::Closed,
                    RowTail::Open => RowEnd::Open(vars.row_anon()),
                    RowTail::Named(name) => RowEnd::Open(vars.row_named(*name)),
                };
                Ty::Record(RecordRow { fields: row, tail: row_tail })
            }
            TypeKind::Unit => Ty::Unit,
            TypeKind::Paren(inner) => self.lower(*inner, vars),
            // `Con` is handled by `lower`; `App` was peeled into the spine.
            TypeKind::Con(name) => self.lower_con_app(*name, &[], self.module.ty(ty).span, vars),
            TypeKind::App { .. } => self.lower(ty, vars),
            // An effect row is meaningful only as an interface effect argument,
            // which interface-application lowering consumes directly; reaching a
            // bare one here means it was written in a non-argument position.
            TypeKind::EffectRow { .. } => {
                emit(
                    self.db,
                    Diagnostic::error(
                        crate::EFFECT_ARG_KIND,
                        "an effect row is only valid as an interface effect argument",
                        Span::new(self.file.source(self.db), self.module.ty(ty).span),
                    ),
                );
                Ty::Error
            }
            TypeKind::Error => Ty::Error,
        }
    }

    /// Lowers a written arrow effect annotation into an [`EffectRow`], resolving
    /// each atom (a capability interface name) to its canonical [`InterfaceRef`].
    /// `None` is the pure effect. An atom that does not name a visible interface
    /// is dropped for now (a dedicated diagnostic lands with enforcement).
    fn lower_effect(&mut self, annot: Option<&EffectAnnot>, vars: &mut LowerVars) -> EffectRow {
        match annot {
            None => EffectRow::pure(),
            Some(annot) => self.lower_effect_row(&annot.labels, annot.tail, vars),
        }
    }

    /// Lowers an effect row's atoms and tail (shared by an arrow's effect
    /// annotation and an interface effect argument). Each atom resolves to its
    /// canonical [`InterfaceRef`]; an atom that names no visible interface is
    /// dropped for now (a dedicated diagnostic lands with enforcement).
    fn lower_effect_row(
        &mut self,
        labels: &[Symbol],
        tail: RowTail,
        vars: &mut LowerVars,
    ) -> EffectRow {
        let mut out = Vec::new();
        for &name in labels {
            if let Some((decl_file, info)) = self.lookup_interface(name) {
                out.push(InterfaceRef::new(decl_file.source(self.db), info.name));
            }
        }
        out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        out.dedup();
        let tail = match tail {
            RowTail::Closed => EffEnd::Closed,
            RowTail::Open => EffEnd::Open(vars.eff_anon()),
            RowTail::Named(name) => EffEnd::Open(vars.eff_named(name)),
        };
        EffectRow { labels: out, tail }
    }

    /// Lowers one type/interface argument according to its parameter's
    /// [`ParamKind`]: a type parameter takes an ordinary type; an effect parameter
    /// takes an effect row (an `{ … }` literal, a lone variable `'e`, or `{}` for
    /// the pure effect), reified as a [`Ty::EffectArg`]. A kind mismatch is
    /// `FAI3020`.
    fn lower_kinded_arg(&mut self, arg: TypeId, kind: ParamKind, vars: &mut LowerVars) -> Ty {
        let node = self.module.ty(arg);
        match kind {
            ParamKind::Type => {
                if let TypeKind::EffectRow { .. } = &node.kind {
                    emit(
                        self.db,
                        Diagnostic::error(
                            crate::EFFECT_ARG_KIND,
                            "expected a type argument, found an effect row",
                            Span::new(self.file.source(self.db), node.span),
                        ),
                    );
                    return Ty::Error;
                }
                self.lower(arg, vars)
            }
            ParamKind::Effect => {
                let eff = match &node.kind {
                    TypeKind::EffectRow { labels, tail } => {
                        self.lower_effect_row(labels, *tail, vars)
                    }
                    // A lone variable `'e` is an open effect tail.
                    TypeKind::Var(name) => {
                        EffectRow { labels: Vec::new(), tail: EffEnd::Open(vars.eff_named(*name)) }
                    }
                    // `{}` (an empty closed record) reads as the pure effect.
                    TypeKind::Record { fields, tail: RowTail::Closed } if fields.is_empty() => {
                        EffectRow::pure()
                    }
                    _ => {
                        emit(
                            self.db,
                            Diagnostic::error(
                                crate::EFFECT_ARG_KIND,
                                "expected an effect row for an effect parameter",
                                Span::new(self.file.source(self.db), node.span),
                            ),
                        );
                        EffectRow::pure()
                    }
                };
                Ty::EffectArg(eff)
            }
        }
    }

    /// Lowers a (possibly applied) type-constructor name.
    fn lower_con_app(
        &mut self,
        name: Symbol,
        args: &[TypeId],
        span: TextRange,
        vars: &mut LowerVars,
    ) -> Ty {
        // Built-in constructors (Int/Float/.../List/Unit) apply their arguments
        // directly.
        if let Some(builtin) = crate::ty::con_or_unit(name.as_str()) {
            let mut t = builtin;
            for &a in args {
                t = Ty::App(Arc::new(t), Arc::new(self.lower(a, vars)));
            }
            return t;
        }
        // An interface name → a nominal interface type.
        if let Some((decl_file, info)) = self.lookup_interface(name) {
            if args.len() != info.params.len() {
                emit(
                    self.db,
                    Diagnostic::error(
                        TYPE_ARITY,
                        format!(
                            "`{name}` takes {} argument(s), but {} were given",
                            info.params.len(),
                            args.len()
                        ),
                        Span::new(self.file.source(self.db), span),
                    ),
                );
            }
            // Identify the interface by its canonical (resolved) qualified name,
            // not the as-written reference, so nested/cross-file spellings unify.
            let iref = InterfaceRef::new(decl_file.source(self.db), info.name);
            // Each argument is lowered according to its parameter's kind: a type
            // for a type parameter, an effect row (as a `Ty::EffectArg`) for an
            // effect parameter. The application spine keeps them in order.
            let kinds = interface_param_kinds(self.db, iref);
            let mut t = Ty::Interface(iref);
            for (i, &a) in args.iter().enumerate() {
                let kind = kinds.get(i).copied().unwrap_or(ParamKind::Type);
                t = Ty::App(Arc::new(t), Arc::new(self.lower_kinded_arg(a, kind, vars)));
            }
            return t;
        }
        // A user/prelude type.
        let Some((decl_file, info)) = self.lookup_type(name) else {
            emit(
                self.db,
                Diagnostic::error(
                    UNKNOWN_TYPE_CONSTRUCTOR,
                    format!("unknown type `{name}`"),
                    Span::new(self.file.source(self.db), span),
                ),
            );
            return Ty::Error;
        };
        if args.len() != info.params.len() {
            emit(
                self.db,
                Diagnostic::error(
                    TYPE_ARITY,
                    format!(
                        "`{name}` takes {} type argument(s), but {} were given",
                        info.params.len(),
                        args.len()
                    ),
                    Span::new(self.file.source(self.db), span),
                ),
            );
        }
        // An opaque alias is transparent only within its declaring file; from
        // another file it stays nominal (its underlying type is hidden), so it
        // falls through to the `Ty::Adt` head below rather than expanding.
        let opaque_cross_file = info.opaque && decl_file != self.file;
        if info.is_alias && !opaque_cross_file {
            return self.expand_alias(decl_file, &info, args, span, vars);
        }
        // A discriminated union, or an opaque alias seen from another file: a
        // nominal head (identified by its canonical qualified name) applied to
        // its arguments, each lowered by its parameter's kind (an effect
        // parameter takes an effect row, reified as a `Ty::EffectArg`).
        let aref = AdtRef::new(decl_file.source(self.db), info.name);
        let kinds = adt_param_kinds(self.db, aref);
        let mut t = Ty::Adt(aref);
        for (i, &a) in args.iter().enumerate() {
            let kind = kinds.get(i).copied().unwrap_or(ParamKind::Type);
            t = Ty::App(Arc::new(t), Arc::new(self.lower_kinded_arg(a, kind, vars)));
        }
        t
    }

    /// Expands a transparent alias `Name args…` by substituting its arguments for
    /// its parameters in the (recursively lowered) alias body.
    fn expand_alias(
        &mut self,
        decl_file: SourceFile,
        info: &TypeDeclInfo,
        args: &[TypeId],
        span: TextRange,
        vars: &mut LowerVars,
    ) -> Ty {
        if self.expanding.contains(&info.name) {
            emit(
                self.db,
                Diagnostic::error(
                    RECURSIVE_ALIAS,
                    format!("the type alias `{}` refers to itself", info.name),
                    Span::new(self.file.source(self.db), span),
                ),
            );
            return Ty::Error;
        }
        // Lower the arguments in the *current* variable space, each by its
        // parameter's kind (an effect parameter takes an effect row).
        let kinds = adt_param_kinds(self.db, AdtRef::new(decl_file.source(self.db), info.name));
        let arg_tys: Vec<Ty> = args
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                let kind = kinds.get(i).copied().unwrap_or(ParamKind::Type);
                self.lower_kinded_arg(a, kind, vars)
            })
            .collect();

        // Fetch the alias body from its declaring module.
        let parsed = fai_syntax::parse(self.db, decl_file);
        let decl_module = &parsed.module;
        let ItemKind::Type { def: TypeDef::Alias(body), .. } =
            &decl_module.items[info.item.index()].kind
        else {
            return Ty::Error;
        };
        let body = *body;

        // Lower the body in the declaring module with its own variable space (its
        // parameters seeded by kind so an effect parameter is an effect-row
        // variable), then substitute the parameters with the supplied arguments.
        let mut body_lowerer = Lowerer {
            db: self.db,
            file: decl_file,
            module: decl_module,
            scope: scope_of(info.name),
            expanding: Vec::new(),
        };
        body_lowerer.expanding = std::mem::take(&mut self.expanding);
        body_lowerer.expanding.push(info.name);
        let mut body_vars = LowerVars::default();
        seed_kinded_params(&mut body_vars, &info.params, &kinds);
        let body_ty = body_lowerer.lower(body, &mut body_vars);
        self.expanding = std::mem::take(&mut body_lowerer.expanding);
        self.expanding.pop();

        let mut type_subst: FxHashMap<TyVarId, Ty> = FxHashMap::default();
        let mut eff_subst: FxHashMap<EffRowVarId, EffectRow> = FxHashMap::default();
        for (i, &param) in info.params.iter().enumerate() {
            match kinds.get(i).copied().unwrap_or(ParamKind::Type) {
                ParamKind::Type => {
                    if let Some(&id) = body_vars.by_name.get(&param)
                        && let Some(arg) = arg_tys.get(i)
                    {
                        type_subst.insert(id, arg.clone());
                    }
                }
                ParamKind::Effect => {
                    if let Some(&id) = body_vars.effs_by_name.get(&param)
                        && let Some(Ty::EffectArg(row)) = arg_tys.get(i)
                    {
                        eff_subst.insert(id, row.clone());
                    }
                }
            }
        }
        subst_ty_eff(&body_ty, &type_subst, &eff_subst)
    }

    /// Finds the declaration of type `name`: same-file (up the lexical scope
    /// chain, qualifying the possibly-dotted name with each scope prefix), then
    /// the auto-imported prelude (bare names), then a cross-file qualified path.
    fn lookup_type(&self, name: Symbol) -> Option<(SourceFile, TypeDeclInfo)> {
        let decls = type_decls(self.db, self.file);
        for k in (0..=self.scope.len()).rev() {
            let cand = qualify(&self.scope[..k], name);
            if let Some(info) = decls.type_named(cand) {
                return Some((self.file, info.clone()));
            }
        }
        if !name.as_str().contains('.') {
            let exports = prelude_exports(self.db);
            if let Some(&(_, decl_file)) = exports.types.iter().find(|(n, _)| *n == name)
                && decl_file != self.file
                && let Some(info) = type_decls(self.db, decl_file).type_named(name)
                && info.visibility == fai_syntax::ast::Visibility::Public
            {
                return Some((decl_file, info.clone()));
            }
            return None;
        }
        // A cross-file qualified type `File.[Nested.]Type`.
        let (target, member) = self.cross_file_member(name)?;
        let decls = type_decls(self.db, target);
        let info = decls.type_named(member)?;
        (info.visibility == fai_syntax::ast::Visibility::Public).then(|| (target, info.clone()))
    }

    /// Looks up an interface name: same-file lexical scope chain, then the
    /// auto-imported prelude (bare names), then a cross-file qualified path.
    fn lookup_interface(&self, name: Symbol) -> Option<(SourceFile, InterfaceInfo)> {
        let decls = interface_decls(self.db, self.file);
        for k in (0..=self.scope.len()).rev() {
            let cand = qualify(&self.scope[..k], name);
            if let Some(info) = decls.interface_named(cand) {
                return Some((self.file, info.clone()));
            }
        }
        if !name.as_str().contains('.') {
            let exports = prelude_exports(self.db);
            if let Some(&(_, decl_file)) = exports.interfaces.iter().find(|(n, _)| *n == name)
                && decl_file != self.file
                && let Some(info) = interface_decls(self.db, decl_file).interface_named(name)
                && info.visibility == fai_syntax::ast::Visibility::Public
            {
                return Some((decl_file, info.clone()));
            }
            return None;
        }
        let (target, member) = self.cross_file_member(name)?;
        let decls = interface_decls(self.db, target);
        let info = decls.interface_named(member)?;
        (info.visibility == fai_syntax::ast::Visibility::Public).then(|| (target, info.clone()))
    }

    /// Resolves the file and within-file qualified member name of a cross-file
    /// dotted path `File.[Nested.]Member` (the head names a workspace module; the
    /// inner segments name nested modules within it).
    fn cross_file_member(&self, name: Symbol) -> Option<(SourceFile, Symbol)> {
        let segments: Vec<Symbol> = name.as_str().split('.').map(Symbol::intern).collect();
        let target = module_file(self.db, ModuleName(segments[0]))?;
        let target_defs = module_defs(self.db, target);
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
        if consumed != segments.len() - 1 {
            return None;
        }
        Some((target, qualify(&inner, segments[consumed])))
    }
}

/// Peels an application spine, returning the head node and its arguments in order.
fn peel_spine(module: &Module, ty: TypeId) -> (TypeId, Vec<TypeId>) {
    let mut args = Vec::new();
    let mut cur = ty;
    while let TypeKind::App { func, arg } = &module.ty(cur).kind {
        args.push(*arg);
        cur = *func;
    }
    args.reverse();
    (cur, args)
}

/// Substitutes type variables in `ty` according to `map`.
fn subst_ty(ty: &Ty, map: &FxHashMap<TyVarId, Ty>) -> Ty {
    match ty {
        Ty::Var(v) => map.get(v).cloned().unwrap_or(Ty::Var(*v)),
        Ty::App(f, a) => Ty::App(Arc::new(subst_ty(f, map)), Arc::new(subst_ty(a, map))),
        Ty::Arrow(f, a, e) => Ty::arrow_eff(subst_ty(f, map), subst_ty(a, map), e.clone()),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| subst_ty(e, map)).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, subst_ty(t, map))).collect(),
            tail: row.tail,
        }),
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::EffectArg(_) | Ty::Unit | Ty::Error => {
            ty.clone()
        }
    }
}

/// Substitutes both type variables and effect-row variables in `ty` — the latter
/// for an alias that threads an effect parameter into its body (e.g. the
/// `Prelude` re-export `type Stream 'a 'e = Stream.Stream 'a 'e`).
fn subst_ty_eff(
    ty: &Ty,
    type_map: &FxHashMap<TyVarId, Ty>,
    eff_map: &FxHashMap<EffRowVarId, EffectRow>,
) -> Ty {
    match ty {
        Ty::Var(v) => type_map.get(v).cloned().unwrap_or(Ty::Var(*v)),
        Ty::App(f, a) => Ty::App(
            Arc::new(subst_ty_eff(f, type_map, eff_map)),
            Arc::new(subst_ty_eff(a, type_map, eff_map)),
        ),
        Ty::Arrow(f, a, e) => Ty::arrow_eff(
            subst_ty_eff(f, type_map, eff_map),
            subst_ty_eff(a, type_map, eff_map),
            subst_effect(e, eff_map),
        ),
        Ty::Tuple(elems) => {
            Ty::Tuple(elems.iter().map(|e| subst_ty_eff(e, type_map, eff_map)).collect())
        }
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row
                .fields
                .iter()
                .map(|(l, t)| (*l, subst_ty_eff(t, type_map, eff_map)))
                .collect(),
            tail: row.tail,
        }),
        Ty::EffectArg(e) => Ty::EffectArg(subst_effect(e, eff_map)),
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => ty.clone(),
    }
}

/// Substitutes an effect row's open tail variable per `eff_map`, merging any
/// fixed labels with the substituted row's (sorted, deduplicated).
fn subst_effect(row: &EffectRow, eff_map: &FxHashMap<EffRowVarId, EffectRow>) -> EffectRow {
    let EffEnd::Open(v) = row.tail else { return row.clone() };
    let Some(repl) = eff_map.get(&v) else { return row.clone() };
    let mut labels = row.labels.clone();
    labels.extend(repl.labels.iter().copied());
    labels.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    labels.dedup();
    EffectRow { labels, tail: repl.tail }
}

/// Lowers a single written type to a [`Ty`], assigning variables via `vars`.
///
/// Emits [`UNKNOWN_TYPE_CONSTRUCTOR`] for any constructor name that is not known.
pub fn lower_type(
    db: &dyn Db,
    file: SourceFile,
    module: &Module,
    ty: TypeId,
    vars: &mut LowerVars,
) -> Ty {
    lower_type_in(db, file, module, &[], ty, vars)
}

/// Lowers a type in a given module scope (for lexical resolution of bare names).
pub fn lower_type_in(
    db: &dyn Db,
    file: SourceFile,
    module: &Module,
    scope: &[Symbol],
    ty: TypeId,
    vars: &mut LowerVars,
) -> Ty {
    let mut lowerer = Lowerer { db, file, module, scope: scope.to_vec(), expanding: Vec::new() };
    lowerer.lower(ty, vars)
}

/// Lowers a written signature type to a generalized [`Scheme`], quantifying every
/// type variable it mentions.
pub fn lower_signature(db: &dyn Db, file: SourceFile, module: &Module, ty: TypeId) -> Scheme {
    lower_signature_in(db, file, module, &[], ty)
}

/// Lowers a signature type in a given module scope.
pub fn lower_signature_in(
    db: &dyn Db,
    file: SourceFile,
    module: &Module,
    scope: &[Symbol],
    ty: TypeId,
) -> Scheme {
    let mut vars = LowerVars::default();
    let body = lower_type_in(db, file, module, scope, ty, &mut vars);
    Scheme::new(type_vars(&vars), body)
        .with_names(type_names(&vars))
        .with_rows(row_vars(&vars), row_names(&vars))
        .with_effects(effect_vars(&vars), effect_names(&vars))
}

/// The module path of a qualified name (everything but the final segment).
fn scope_of(qualified: Symbol) -> Vec<Symbol> {
    let mut segs: Vec<&str> = qualified.as_str().split('.').collect();
    segs.pop();
    segs.into_iter().map(Symbol::intern).collect()
}

fn type_vars(vars: &LowerVars) -> Vec<TyVarId> {
    vars.named().into_iter().map(|(id, _)| id).collect()
}
fn type_names(vars: &LowerVars) -> Vec<String> {
    vars.named().into_iter().map(|(_, n)| n).collect()
}
fn row_vars(vars: &LowerVars) -> Vec<RowVarId> {
    vars.rows().into_iter().map(|(id, _)| id).collect()
}
fn row_names(vars: &LowerVars) -> Vec<String> {
    vars.rows().into_iter().map(|(_, n)| n).collect()
}
fn effect_vars(vars: &LowerVars) -> Vec<EffRowVarId> {
    vars.effects().into_iter().map(|(id, _)| id).collect()
}
fn effect_names(vars: &LowerVars) -> Vec<String> {
    vars.effects().into_iter().map(|(_, n)| n).collect()
}

/// Builds the scheme of a data constructor `name` declared in `file`, e.g.
/// `Some : 'a -> Option 'a`. Returns `None` if it is not a known constructor.
pub fn build_constructor_scheme(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<Scheme> {
    let decls = type_decls(db, file);
    let info = decls.ctor(name)?;
    let tinfo = decls.type_named(info.adt)?;
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let ItemKind::Type { params, def: TypeDef::Union(variants), .. } =
        &module.items[tinfo.item.index()].kind
    else {
        return None;
    };
    let variant = variants.get(info.variant_index)?;

    let mut vars = LowerVars::default();
    // Seed the type's parameters by kind so an effect parameter (one used only in
    // `/ 'e` position) becomes an effect-row variable shared with the fields'
    // arrows, rather than a decoupled type variable.
    let kinds = adt_param_kinds(db, AdtRef::new(file.source(db), info.adt));
    seed_kinded_params(&mut vars, params, &kinds);
    // The constructor's field types resolve in its type's module scope.
    let mut lowerer =
        Lowerer { db, file, module, scope: scope_of(info.adt), expanding: Vec::new() };
    let field_tys: Vec<Ty> = variant.fields.iter().map(|&f| lowerer.lower(f, &mut vars)).collect();

    let mut result = Ty::Adt(AdtRef::new(file.source(db), info.adt));
    for (i, &p) in params.iter().enumerate() {
        let arg = match kinds.get(i).copied().unwrap_or(ParamKind::Type) {
            ParamKind::Type => Ty::Var(vars.var(p)),
            ParamKind::Effect => Ty::EffectArg(EffectRow {
                labels: Vec::new(),
                tail: EffEnd::Open(vars.eff_named(p)),
            }),
        };
        result = Ty::App(Arc::new(result), Arc::new(arg));
    }
    let body = Ty::arrows(field_tys, result);

    Some(
        Scheme::new(type_vars(&vars), body)
            .with_names(type_names(&vars))
            .with_rows(row_vars(&vars), row_names(&vars))
            .with_effects(effect_vars(&vars), effect_names(&vars)),
    )
}

/// Expands an alias's body with concrete type arguments substituted for its
/// parameters, yielding the underlying type. This deliberately peeks past
/// opacity, for the few places the compiler legitimately needs an opaque type's
/// representation (e.g. synthesizing a generator for a property test). Returns
/// `None` if `adt` is not an alias in a loadable file.
pub fn expand_alias_ty(db: &dyn Db, adt: AdtRef, args: &[Ty]) -> Option<Ty> {
    let file = db.source_file(adt.file)?;
    let decls = type_decls(db, file);
    let info = decls.type_named(adt.name).filter(|i| i.is_alias)?;
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let ItemKind::Type { def: TypeDef::Alias(body), .. } = &module.items[info.item.index()].kind
    else {
        return None;
    };
    let body = *body;
    // The body lowers in its own (declaring) file, where the alias is transparent.
    let mut lowerer =
        Lowerer { db, file, module, scope: scope_of(info.name), expanding: Vec::new() };
    let mut body_vars = LowerVars::default();
    let body_ty = lowerer.lower(body, &mut body_vars);

    let mut subst: FxHashMap<TyVarId, Ty> = FxHashMap::default();
    for (i, param) in info.params.iter().enumerate() {
        if let Some(&id) = body_vars.by_name.get(param)
            && let Some(arg) = args.get(i)
        {
            subst.insert(id, arg.clone());
        }
    }
    Some(subst_ty(&body_ty, &subst))
}

/// Resolves an interface name to its [`InterfaceRef`] in the context of `file`
/// (this module's interfaces, then the auto-imported prelude's).
#[must_use]
pub fn resolve_interface(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<InterfaceRef> {
    if interface_decls(db, file).interface_named(name).is_some() {
        return Some(InterfaceRef::new(file.source(db), name));
    }
    let exports = prelude_exports(db);
    if let Some(&(_, decl_file)) = exports.interfaces.iter().find(|(n, _)| *n == name)
        && decl_file != file
        && interface_decls(db, decl_file)
            .interface_named(name)
            .is_some_and(|i| i.visibility == fai_syntax::ast::Visibility::Public)
    {
        return Some(InterfaceRef::new(decl_file.source(db), name));
    }
    None
}

/// The kind of an interface parameter, inferred from how the interface's methods
/// use it: as an ordinary type (`'a`) or as an effect row (`/ 'e`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// A type parameter (`Box 'a`).
    Type,
    /// An effect-row parameter (`Logger 'e`), appearing after `/` in a method.
    Effect,
}

/// Each interface parameter's kind, inferred from its use across the methods. A
/// parameter used only as an effect-row tail (`/ 'e`) is [`ParamKind::Effect`];
/// otherwise (type position, a record-row tail, or unused) it is
/// [`ParamKind::Type`]. A parameter used as *both* is ill-kinded — resolved here
/// to `Type` and reported as `FAI3019` at the declaration.
#[must_use]
pub fn interface_param_kinds(db: &dyn Db, iref: InterfaceRef) -> Vec<ParamKind> {
    interface_param_usage(db, iref)
        .iter()
        .map(
            |&(ty_used, eff_used)| {
                if eff_used && !ty_used { ParamKind::Effect } else { ParamKind::Type }
            },
        )
        .collect()
}

/// Per-parameter `(used-as-type, used-as-effect)`, scanning the method
/// signatures of `iref`. Backs both kind inference and the `FAI3019` check.
#[must_use]
pub(crate) fn interface_param_usage(db: &dyn Db, iref: InterfaceRef) -> Vec<(bool, bool)> {
    let Some(file) = db.source_file(iref.file) else { return Vec::new() };
    let decls = interface_decls(db, file);
    let Some(info) = decls.interface_named(iref.name) else { return Vec::new() };
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let ItemKind::Interface { params, methods, .. } = &module.items[info.item.index()].kind else {
        return Vec::new();
    };
    let mut type_used: FxHashSet<Symbol> = FxHashSet::default();
    let mut eff_used: FxHashSet<Symbol> = FxHashSet::default();
    for m in methods {
        scan_var_usage(module, m.ty, &mut type_used, &mut eff_used);
    }
    params.iter().map(|p| (type_used.contains(p), eff_used.contains(p))).collect()
}

/// Records, for each type-variable name in `ty`, whether it appears in type
/// position (`type_used`) or as an effect-row tail after `/` (`eff_used`).
fn scan_var_usage(
    module: &Module,
    ty: TypeId,
    type_used: &mut FxHashSet<Symbol>,
    eff_used: &mut FxHashSet<Symbol>,
) {
    match &module.ty(ty).kind {
        TypeKind::Var(name) => {
            type_used.insert(*name);
        }
        TypeKind::App { func, arg } => {
            scan_var_usage(module, *func, type_used, eff_used);
            scan_var_usage(module, *arg, type_used, eff_used);
        }
        TypeKind::Arrow { from, to, effect } => {
            scan_var_usage(module, *from, type_used, eff_used);
            scan_var_usage(module, *to, type_used, eff_used);
            if let Some(annot) = effect
                && let RowTail::Named(name) = annot.tail
            {
                eff_used.insert(name);
            }
        }
        TypeKind::Tuple(elems) => {
            for &e in elems {
                scan_var_usage(module, e, type_used, eff_used);
            }
        }
        TypeKind::Record { fields, tail } => {
            for f in fields {
                scan_var_usage(module, f.ty, type_used, eff_used);
            }
            if let RowTail::Named(name) = tail {
                type_used.insert(*name);
            }
        }
        TypeKind::Paren(inner) => scan_var_usage(module, *inner, type_used, eff_used),
        // An effect row written as an interface argument: its tail is an effect
        // variable use.
        TypeKind::EffectRow { tail, .. } => {
            if let RowTail::Named(name) = tail {
                eff_used.insert(*name);
            }
        }
        TypeKind::Con(_) | TypeKind::Unit | TypeKind::Error => {}
    }
}

/// Whether a parameter slot of a type/interface takes a type, an effect row, or
/// is not yet classified (only during the ADT kind-inference fixpoint).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Type,
    Effect,
    /// A recursive reference whose kind the fixpoint has not settled yet. An
    /// immediate variable in such a slot is deferred to a later iteration rather
    /// than counted as a type use, so an effect signal is never contaminated.
    Pending,
}

impl SlotKind {
    fn from_param_kind(kind: ParamKind) -> Self {
        match kind {
            ParamKind::Effect => SlotKind::Effect,
            ParamKind::Type => SlotKind::Type,
        }
    }
}

thread_local! {
    /// Files whose type-parameter kinds are currently being computed, so a
    /// cross-file type cycle breaks (conservatively, all `Type`) instead of
    /// recursing forever. Intra-file (mutual) recursion is handled by the
    /// fixpoint itself, not this guard.
    static KINDS_IN_PROGRESS: RefCell<FxHashSet<SourceId>> = RefCell::new(FxHashSet::default());
}

/// Each parameter's kind for a user `type`/alias `adt`, inferred exactly as for
/// interfaces: a parameter used only in effect position (an arrow's `/ 'e`, an
/// effect-row tail, or the effect slot of another type/interface it is applied
/// to) is [`ParamKind::Effect`]; otherwise [`ParamKind::Type`].
#[must_use]
pub fn adt_param_kinds(db: &dyn Db, adt: AdtRef) -> Vec<ParamKind> {
    adt_param_usage(db, adt)
        .iter()
        .map(
            |&(ty_used, eff_used)| {
                if eff_used && !ty_used { ParamKind::Effect } else { ParamKind::Type }
            },
        )
        .collect()
}

/// Per-parameter `(used-as-type, used-as-effect)` for a user `type`/alias. Backs
/// both ADT kind inference and the `FAI3019` "used as both" check.
#[must_use]
pub fn adt_param_usage(db: &dyn Db, adt: AdtRef) -> Vec<(bool, bool)> {
    let Some(file) = db.source_file(adt.file) else { return Vec::new() };
    file_type_param_usage(db, file).get(&adt.name).cloned().unwrap_or_default()
}

/// Computes, for every `type`/alias in `file` at once, each parameter's
/// `(type-used, effect-used)` flags via a monotone fixpoint. Doing the whole file
/// jointly classifies mutually-recursive types (e.g. `Stream`/`Step`, which both
/// thread an effect parameter) correctly: a recursive reference routes its
/// arguments by the referenced type's *current* kinds, and the unambiguous
/// effect anchors (`/ 'e` tails) seed the iteration.
fn file_type_param_usage(db: &dyn Db, file: SourceFile) -> FxHashMap<Symbol, Vec<(bool, bool)>> {
    let sid = file.source(db);
    // A cross-file type cycle: break conservatively (every parameter a type).
    let reentered = KINDS_IN_PROGRESS.with(|s| !s.borrow_mut().insert(sid));
    if reentered {
        return type_decls(db, file)
            .types
            .iter()
            .map(|(n, i)| (*n, vec![(false, true); i.params.len()]))
            .collect();
    }
    let result = file_type_param_usage_inner(db, file);
    KINDS_IN_PROGRESS.with(|s| {
        s.borrow_mut().remove(&sid);
    });
    result
}

fn file_type_param_usage_inner(
    db: &dyn Db,
    file: SourceFile,
) -> FxHashMap<Symbol, Vec<(bool, bool)>> {
    let decls = type_decls(db, file);
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;

    // Each type's fields (a union) or body (an alias), its parameters, and its
    // module scope — in a stable (name-sorted) order so the fixpoint is
    // deterministic.
    struct TypeScan {
        name: Symbol,
        params: Vec<Symbol>,
        fields: Vec<TypeId>,
        scope: Vec<Symbol>,
    }
    let mut scans: Vec<TypeScan> = decls
        .types
        .values()
        .map(|info| {
            let fields = match &module.items[info.item.index()].kind {
                ItemKind::Type { def: TypeDef::Union(variants), .. } => {
                    variants.iter().flat_map(|v| v.fields.iter().copied()).collect()
                }
                ItemKind::Type { def: TypeDef::Alias(body), .. } => vec![*body],
                _ => Vec::new(),
            };
            TypeScan {
                name: info.name,
                params: info.params.clone(),
                fields,
                scope: scope_of(info.name),
            }
        })
        .collect();
    scans.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    let mut state: FxHashMap<Symbol, Vec<(bool, bool)>> =
        scans.iter().map(|s| (s.name, vec![(false, false); s.params.len()])).collect();

    loop {
        let mut changed = false;
        for scan in &scans {
            let lowerer =
                Lowerer { db, file, module, scope: scan.scope.clone(), expanding: Vec::new() };
            let mut type_used: FxHashSet<Symbol> = FxHashSet::default();
            let mut eff_used: FxHashSet<Symbol> = FxHashSet::default();
            for &field in &scan.fields {
                scan_kinded(&lowerer, &state, field, SlotKind::Type, &mut type_used, &mut eff_used);
            }
            if let Some(entry) = state.get_mut(&scan.name) {
                for (i, p) in scan.params.iter().enumerate() {
                    let merged =
                        (entry[i].0 || type_used.contains(p), entry[i].1 || eff_used.contains(p));
                    if merged != entry[i] {
                        entry[i] = merged;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    state
}

/// Scans a written type, recording for each variable whether it appears in type
/// position (`type_used`) or effect position (`eff_used`). `slot` is the kind of
/// the position this node occupies: an immediate variable in an
/// [`SlotKind::Effect`] slot is an effect use, in a [`SlotKind::Type`] slot a type
/// use, and in a [`SlotKind::Pending`] slot deferred. Nested structure (arrows,
/// effect rows) is always scanned so an unambiguous `/ 'e` is never missed.
fn scan_kinded(
    lowerer: &Lowerer,
    state: &FxHashMap<Symbol, Vec<(bool, bool)>>,
    ty: TypeId,
    slot: SlotKind,
    type_used: &mut FxHashSet<Symbol>,
    eff_used: &mut FxHashSet<Symbol>,
) {
    match &lowerer.module.ty(ty).kind {
        TypeKind::Var(name) => match slot {
            SlotKind::Effect => {
                eff_used.insert(*name);
            }
            SlotKind::Type => {
                type_used.insert(*name);
            }
            SlotKind::Pending => {}
        },
        TypeKind::EffectRow { tail, .. } => {
            if let RowTail::Named(name) = tail {
                eff_used.insert(*name);
            }
        }
        TypeKind::Arrow { from, to, effect } => {
            scan_kinded(lowerer, state, *from, SlotKind::Type, type_used, eff_used);
            scan_kinded(lowerer, state, *to, SlotKind::Type, type_used, eff_used);
            if let Some(annot) = effect
                && let RowTail::Named(name) = annot.tail
            {
                eff_used.insert(name);
            }
        }
        TypeKind::Tuple(elems) => {
            for &e in elems {
                scan_kinded(lowerer, state, e, SlotKind::Type, type_used, eff_used);
            }
        }
        TypeKind::Record { fields, tail } => {
            for f in fields {
                scan_kinded(lowerer, state, f.ty, SlotKind::Type, type_used, eff_used);
            }
            if let RowTail::Named(name) = tail {
                type_used.insert(*name);
            }
        }
        TypeKind::Paren(inner) => {
            scan_kinded(lowerer, state, *inner, slot, type_used, eff_used);
        }
        // An application (or a bare constructor): route each argument by the
        // head's parameter kinds. An application is always a type, so the
        // incoming `slot` does not constrain it.
        TypeKind::App { .. } | TypeKind::Con(_) => {
            let (head, args) = peel_spine(lowerer.module, ty);
            if let TypeKind::Con(name) = &lowerer.module.ty(head).kind {
                let slots = slot_kinds_of(lowerer, state, *name, args.len());
                for (i, &arg) in args.iter().enumerate() {
                    let s = slots.get(i).copied().unwrap_or(SlotKind::Type);
                    scan_kinded(lowerer, state, arg, s, type_used, eff_used);
                }
            } else {
                scan_kinded(lowerer, state, head, SlotKind::Type, type_used, eff_used);
                for &arg in &args {
                    scan_kinded(lowerer, state, arg, SlotKind::Type, type_used, eff_used);
                }
            }
        }
        TypeKind::Unit | TypeKind::Error => {}
    }
}

/// The per-slot kinds of the type/interface named `name` (applied with `arity`
/// arguments), used to route an application's arguments while scanning.
/// Built-ins take all type arguments; an interface uses its inferred kinds; a
/// same-file user type uses the fixpoint's current `state` (a not-yet-settled
/// slot is [`SlotKind::Pending`]); a cross-file user type uses its computed kinds.
fn slot_kinds_of(
    lowerer: &Lowerer,
    state: &FxHashMap<Symbol, Vec<(bool, bool)>>,
    name: Symbol,
    arity: usize,
) -> Vec<SlotKind> {
    if crate::ty::con_or_unit(name.as_str()).is_some() {
        return vec![SlotKind::Type; arity];
    }
    if let Some((decl_file, info)) = lowerer.lookup_interface(name) {
        let iref = InterfaceRef::new(decl_file.source(lowerer.db), info.name);
        return interface_param_kinds(lowerer.db, iref)
            .into_iter()
            .map(SlotKind::from_param_kind)
            .collect();
    }
    if let Some((decl_file, info)) = lowerer.lookup_type(name) {
        if decl_file == lowerer.file {
            if let Some(flags) = state.get(&info.name) {
                return flags
                    .iter()
                    .map(|&(ty_used, eff_used)| {
                        if ty_used {
                            SlotKind::Type
                        } else if eff_used {
                            SlotKind::Effect
                        } else {
                            SlotKind::Pending
                        }
                    })
                    .collect();
            }
        } else {
            return adt_param_kinds(
                lowerer.db,
                AdtRef::new(decl_file.source(lowerer.db), info.name),
            )
            .into_iter()
            .map(SlotKind::from_param_kind)
            .collect();
        }
    }
    vec![SlotKind::Type; arity]
}

/// Seeds a type's/interface's parameters into `vars` in declaration order, each
/// into its kind's namespace (a type variable or an effect-row variable), so they
/// occupy the leading scheme variables for positional sharing across the body.
pub(crate) fn seed_kinded_params(vars: &mut LowerVars, params: &[Symbol], kinds: &[ParamKind]) {
    for (i, &p) in params.iter().enumerate() {
        match kinds.get(i).copied().unwrap_or(ParamKind::Type) {
            ParamKind::Type => {
                vars.var(p);
            }
            ParamKind::Effect => {
                vars.eff_named(p);
            }
        }
    }
}

/// Builds the scheme of interface `iref`'s method `method`, quantified over the
/// interface's type parameters **first** (so positional sharing works), then any
/// method-local variables. Returns `None` if the method is not declared.
pub fn build_interface_method_scheme(
    db: &dyn Db,
    iref: InterfaceRef,
    method: Symbol,
) -> Option<Scheme> {
    let file = db.source_file(iref.file)?;
    let decls = interface_decls(db, file);
    let info = decls.interface_named(iref.name)?;
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let ItemKind::Interface { params, methods, .. } = &module.items[info.item.index()].kind else {
        return None;
    };
    let msig = methods.iter().find(|m| m.name == method)?;

    let mut vars = LowerVars::default();
    // Seed the interface's parameters first so they occupy the leading scheme
    // variables, in declaration order — each in its kind's namespace (a type
    // variable, or an effect-row variable for an effect parameter `'e`).
    let kinds = interface_param_kinds(db, iref);
    seed_kinded_params(&mut vars, params, &kinds);
    let mut lowerer =
        Lowerer { db, file, module, scope: scope_of(iref.name), expanding: Vec::new() };
    let body = lowerer.lower(msig.ty, &mut vars);

    Some(
        Scheme::new(type_vars(&vars), body)
            .with_names(type_names(&vars))
            .with_rows(row_vars(&vars), row_names(&vars))
            .with_effects(effect_vars(&vars), effect_names(&vars)),
    )
}

#[cfg(test)]
mod tests {
    use fai_db::{Db, FaiDatabase};
    use fai_syntax::ast::ItemKind;

    use super::*;

    fn sig_scheme(src: &str) -> (FaiDatabase, Scheme) {
        let mut db = FaiDatabase::new();
        let id = db.add_source("M.fai".into(), src.to_owned());
        let file = db.source_file(id).unwrap();
        let parsed = fai_syntax::parse(&db, file);
        // The first signature item in the module.
        let module = &parsed.module;
        let ty = module
            .items
            .iter()
            .find_map(|it| match &it.kind {
                ItemKind::Signature { ty, .. } => Some(*ty),
                _ => None,
            })
            .expect("a signature");
        let scheme = lower_signature(&db, file, module, ty);
        drop(parsed);
        (db, scheme)
    }

    #[test]
    fn lowers_arrow_signature() {
        let (_db, scheme) = sig_scheme("module M\n\npublic f : Int -> Bool\nlet f x = true\n");
        assert_eq!(crate::ty::render_scheme(&scheme), "Int -> Bool");
        assert!(scheme.vars.is_empty());
    }

    #[test]
    fn lowers_polymorphic_signature() {
        let (_db, scheme) = sig_scheme("module M\n\npublic id : 'a -> 'a\nlet id x = x\n");
        assert_eq!(scheme.vars.len(), 1);
        assert_eq!(crate::ty::render_scheme(&scheme), "'a -> 'a");
    }

    #[test]
    fn lowers_list_application() {
        let (_db, scheme) = sig_scheme("module M\n\npublic len : List 'a -> Int\nlet len x = 0\n");
        assert_eq!(crate::ty::render_scheme(&scheme), "List 'a -> Int");
    }
}
