//! Lowering written type expressions (the AST) into the internal [`Ty`].
//!
//! Surface type variables (`'a`) map to fresh [`TyVarId`]s, shared by name within
//! one lowering; type constructors are recognized against the known set (unknown
//! constructors emit [`UNKNOWN_TYPE_CONSTRUCTOR`]). A whole signature lowers to a
//! [`Scheme`] quantifying every variable it mentions.

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{Module, Type, TypeId, TypeKind};
use rustc_hash::FxHashMap;

use crate::UNKNOWN_TYPE_CONSTRUCTOR;
use crate::ty::{Scheme, Ty, TyVarId};

/// A scratch map from surface type-variable names to assigned ids during one
/// lowering, plus a fresh-id counter local to that lowering.
#[derive(Default)]
pub struct LowerVars {
    by_name: FxHashMap<Symbol, TyVarId>,
    next: u32,
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

    /// The ids assigned so far, in ascending order (a scheme's quantified set).
    fn assigned(&self) -> Vec<TyVarId> {
        let mut vars: Vec<TyVarId> = self.by_name.values().copied().collect();
        vars.sort();
        vars
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
    let Type { kind, span } = module.ty(ty);
    match kind {
        TypeKind::Var(name) => Ty::Var(vars.var(*name)),
        TypeKind::Con(name) => match crate::ty::con_or_unit(name.as_str()) {
            Some(ty) => ty,
            None => {
                emit(
                    db,
                    Diagnostic::error(
                        UNKNOWN_TYPE_CONSTRUCTOR,
                        format!("unknown type `{name}`"),
                        Span::new(file.source(db), *span),
                    ),
                );
                Ty::Error
            }
        },
        TypeKind::App { func, arg } => {
            let f = lower_type(db, file, module, *func, vars);
            let a = lower_type(db, file, module, *arg, vars);
            Ty::App(std::sync::Arc::new(f), std::sync::Arc::new(a))
        }
        TypeKind::Arrow { from, to } => {
            let f = lower_type(db, file, module, *from, vars);
            let t = lower_type(db, file, module, *to, vars);
            Ty::arrow(f, t)
        }
        TypeKind::Tuple(elems) => {
            Ty::Tuple(elems.iter().map(|&e| lower_type(db, file, module, e, vars)).collect())
        }
        TypeKind::Unit => Ty::Unit,
        TypeKind::Paren(inner) => lower_type(db, file, module, *inner, vars),
        TypeKind::Error => Ty::Error,
    }
}

/// Lowers a written signature type to a generalized [`Scheme`], quantifying every
/// type variable it mentions.
pub fn lower_signature(db: &dyn Db, file: SourceFile, module: &Module, ty: TypeId) -> Scheme {
    let mut vars = LowerVars::default();
    let body = lower_type(db, file, module, ty, &mut vars);
    Scheme { vars: vars.assigned(), ty: body }
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
