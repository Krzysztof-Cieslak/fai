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
use fai_resolve::{AdtRef, TypeDeclInfo, prelude_exports, type_decls};
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
    fn var(&mut self, name: Symbol) -> TyVarId {
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
        if info.is_alias {
            return self.expand_alias(decl_file, &info, args, span, vars);
        }
        // A discriminated union: a nominal head applied to its arguments.
        let mut t = Ty::Adt(AdtRef::new(decl_file.source(self.db), name));
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
        let mut body_lowerer =
            Lowerer { db: self.db, file: decl_file, module: decl_module, expanding: Vec::new() };
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

    /// Finds the declaration of type `name` in this module, then the auto-imported
    /// core (the merged `Prelude` interface).
    fn lookup_type(&self, name: Symbol) -> Option<(SourceFile, TypeDeclInfo)> {
        if let Some(info) = type_decls(self.db, self.file).type_named(name) {
            return Some((self.file, info.clone()));
        }
        let exports = prelude_exports(self.db);
        if let Some(&(_, decl_file)) = exports.types.iter().find(|(n, _)| *n == name)
            && decl_file != self.file
            && let Some(info) = type_decls(self.db, decl_file).type_named(name)
            && info.visibility == fai_syntax::ast::Visibility::Public
        {
            return Some((decl_file, info.clone()));
        }
        None
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
        Ty::Arrow(f, a) => Ty::arrow(subst_ty(f, map), subst_ty(a, map)),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| subst_ty(e, map)).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, subst_ty(t, map))).collect(),
            tail: row.tail,
        }),
        Ty::Con(_) | Ty::Adt(_) | Ty::Unit | Ty::Error => ty.clone(),
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
    let mut lowerer = Lowerer { db, file, module, expanding: Vec::new() };
    lowerer.lower(ty, vars)
}

/// Lowers a written signature type to a generalized [`Scheme`], quantifying every
/// type variable it mentions.
pub fn lower_signature(db: &dyn Db, file: SourceFile, module: &Module, ty: TypeId) -> Scheme {
    let mut vars = LowerVars::default();
    let body = lower_type(db, file, module, ty, &mut vars);
    Scheme::new(type_vars(&vars), body)
        .with_names(type_names(&vars))
        .with_rows(row_vars(&vars), row_names(&vars))
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
    let mut lowerer = Lowerer { db, file, module, expanding: Vec::new() };
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
