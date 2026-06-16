//! The native calling-convention shape ([`FnAbi`]) of a definition.
//!
//! [`abi`] derives a definition's [`FnAbi`] from its **declared-or-inferred type
//! signature** and its syntactic source-parameter count — both body-edit-stable,
//! so a caller's compiled object (and the SROA rewrite that marshals a call)
//! depends on a callee's *signature*, never its body (the codegen firewall). It is
//! the single source of truth shared by the SROA pass (`fai-rc`), the reference
//! counter, and code generation (the driver threads it in as `signature_of`).

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, module_defs};
use fai_syntax::Symbol;
use fai_syntax::ast::ItemKind;

use crate::ir::FnAbi;
use crate::niche::niche_scheme;

/// `name`'s native calling-convention shape (see [`FnAbi`]). Tracked so its
/// memoization boundary keeps a dependent's recompute independent of unrelated
/// edits (a callee body edit that does not change its signature does not ripple).
#[salsa::tracked]
pub fn abi(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<FnAbi> {
    let def = DefId::new(file.source(db), name);
    let Some(scheme) = fai_types::declared_or_inferred_scheme(db, def) else {
        return Arc::new(FnAbi::default());
    };
    // A niche `Option` (and a spread float aggregate) parameter/result is carried
    // wrapper-free / exploded across a direct call.
    let niche = |ty: &fai_types::Ty| niche_scheme(db, ty);
    Arc::new(FnAbi::from_scheme(&scheme, source_param_count(db, file, name), &niche))
}

/// `name`'s ABI located by [`DefId`], or the default (uniform) ABI for a
/// definition with no source file (a synthetic combined loop).
#[must_use]
pub fn abi_of(db: &dyn Db, def: DefId) -> Arc<FnAbi> {
    db.source_file(def.file).map_or_else(|| Arc::new(FnAbi::default()), |f| abi(db, f, def.name))
}

/// The number of syntactic source parameters of `name`'s binding (`let f a b = …`
/// has two), or zero if it is not a function binding.
fn source_param_count(db: &dyn Db, file: SourceFile, name: Symbol) -> usize {
    let parsed = fai_syntax::parse(db, file);
    module_defs(db, file)
        .get(name)
        .and_then(|d| match &parsed.module.items[d.binding.index()].kind {
            ItemKind::Binding { params, .. } => Some(params.len()),
            // A `foreign` decl has no parameter patterns; its parameter count is
            // the arrow arity of its declared type.
            ItemKind::Foreign { ty, .. } => Some(parsed.module.arrow_arity(*ty)),
            _ => None,
        })
        .unwrap_or(0)
}
