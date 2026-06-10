//! Lowering written type expressions (the AST) into the internal [`Ty`].
//!
//! Surface type variables (`'a`) map to fresh [`TyVarId`]s, shared by name within
//! one lowering. Type-constructor names resolve against the built-in set, then the
//! current module's and the prelude's `type` declarations: a discriminated union
//! lowers to a nominal [`Ty::Adt`] applied to its arguments, a transparent alias
//! is expanded (with a cycle check). A whole signature lowers to a [`Scheme`]
//! quantifying every variable it mentions.

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{
    AdtRef, InterfaceInfo, InterfaceRef, ModuleName, TypeDeclInfo, interface_decls, module_defs,
    module_file, prelude_exports, qualify, type_decls,
};
use fai_span::{Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemKind, Module, RowTail, TypeDef, TypeId, TypeKind};
use rustc_hash::FxHashMap;

use crate::ty::{RecordRow, RowEnd, RowVarId, Scheme, Ty, TyVarId};
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
            TypeKind::Arrow { from, to } => {
                let f = self.lower(*from, vars);
                let t = self.lower(*to, vars);
                Ty::arrow(f, t)
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
            TypeKind::Error => Ty::Error,
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
                            "`{name}` takes {} type argument(s), but {} were given",
                            info.params.len(),
                            args.len()
                        ),
                        Span::new(self.file.source(self.db), span),
                    ),
                );
            }
            // Identify the interface by its canonical (resolved) qualified name,
            // not the as-written reference, so nested/cross-file spellings unify.
            let mut t = Ty::Interface(InterfaceRef::new(decl_file.source(self.db), info.name));
            for &a in args {
                t = Ty::App(Arc::new(t), Arc::new(self.lower(a, vars)));
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
        // its arguments.
        let mut t = Ty::Adt(AdtRef::new(decl_file.source(self.db), info.name));
        for &a in args {
            t = Ty::App(Arc::new(t), Arc::new(self.lower(a, vars)));
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
        // Lower the arguments in the *current* variable space.
        let arg_tys: Vec<Ty> = args.iter().map(|&a| self.lower(a, vars)).collect();

        // Fetch the alias body from its declaring module.
        let parsed = fai_syntax::parse(self.db, decl_file);
        let decl_module = &parsed.module;
        let ItemKind::Type { def: TypeDef::Alias(body), .. } =
            &decl_module.items[info.item.index()].kind
        else {
            return Ty::Error;
        };
        let body = *body;

        // Lower the body in the declaring module with its own variable space, then
        // substitute the parameters' ids with the supplied argument types.
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
        let body_ty = body_lowerer.lower(body, &mut body_vars);
        self.expanding = std::mem::take(&mut body_lowerer.expanding);
        self.expanding.pop();

        let mut subst: FxHashMap<TyVarId, Ty> = FxHashMap::default();
        for (i, &param) in info.params.iter().enumerate() {
            if let Some(&id) = body_vars.by_name.get(&param)
                && let Some(arg) = arg_tys.get(i)
            {
                subst.insert(id, arg.clone());
            }
        }
        subst_ty(&body_ty, &subst)
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
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => ty.clone(),
    }
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
    let param_ids: Vec<TyVarId> = params.iter().map(|&p| vars.var(p)).collect();
    // The constructor's field types resolve in its type's module scope.
    let mut lowerer =
        Lowerer { db, file, module, scope: scope_of(info.adt), expanding: Vec::new() };
    let field_tys: Vec<Ty> = variant.fields.iter().map(|&f| lowerer.lower(f, &mut vars)).collect();

    let mut result = Ty::Adt(AdtRef::new(file.source(db), info.adt));
    for &pid in &param_ids {
        result = Ty::App(Arc::new(result), Arc::new(Ty::Var(pid)));
    }
    let body = Ty::arrows(field_tys, result);

    Some(
        Scheme::new(type_vars(&vars), body)
            .with_names(type_names(&vars))
            .with_rows(row_vars(&vars), row_names(&vars)),
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

/// The number of type parameters of the interface `iref`.
#[must_use]
pub fn interface_param_count(db: &dyn Db, iref: InterfaceRef) -> usize {
    db.source_file(iref.file)
        .and_then(|file| {
            interface_decls(db, file).interface_named(iref.name).map(|i| i.params.len())
        })
        .unwrap_or(0)
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
    // variables, in declaration order.
    let _: Vec<TyVarId> = params.iter().map(|&p| vars.var(p)).collect();
    let mut lowerer =
        Lowerer { db, file, module, scope: scope_of(iref.name), expanding: Vec::new() };
    let body = lowerer.lower(msig.ty, &mut vars);

    Some(
        Scheme::new(type_vars(&vars), body)
            .with_names(type_names(&vars))
            .with_rows(row_vars(&vars), row_names(&vars)),
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
