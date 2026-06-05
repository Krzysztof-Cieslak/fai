//! The salsa queries that drive type checking.
//!
//! `infer_scc_query` is the cache unit (a definition or SCC). `def_type` reads a
//! single definition's scheme out of its SCC. `check_file` walks every definition
//! and contract, emitting the required-signature and contract diagnostics. The
//! firewall holds because an out-of-SCC reference resolves through
//! [`declared_or_inferred_scheme`], which uses a declared signature where present
//! and only otherwise reaches the callee's inferred type.

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::{Diagnostic, Label};
use fai_resolve::{DefId, module_defs, module_sccs, resolve};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::ExprId;
use rustc_hash::FxHashMap;

use crate::ty::Ty;

use crate::infer::{declared_scheme, error_scheme, infer_scc};
use crate::prelude;
use crate::ty::Scheme;
use crate::{MISSING_PUBLIC_SIGNATURE, SIGNATURE_MISMATCH};

/// The inferred schemes of the SCC at `scc_index` in `file`.
#[salsa::tracked]
pub fn infer_scc_query(db: &dyn Db, file: SourceFile, scc_index: usize) -> Arc<SccTypes> {
    let sccs = module_sccs(db, file);
    let Some(scc) = sccs.sccs.get(scc_index) else {
        return Arc::new(SccTypes::default());
    };
    let resolved = resolve(db, file);

    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |name: Symbol| prelude::builtin_scheme(name);

    let inference = infer_scc(db, file, &scc.members, &resolved, &def_schemes, &builtins);
    Arc::new(SccTypes {
        schemes: inference.schemes.into_iter().collect(),
        mismatches: inference.mismatches,
    })
}

/// The schemes inferred for one SCC's members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SccTypes {
    /// Each member's generalized scheme.
    pub schemes: Vec<(DefId, Scheme)>,
    /// Members whose body disagreed with its declared signature.
    pub mismatches: Vec<DefId>,
}

impl SccTypes {
    /// The scheme for `def`, if present.
    #[must_use]
    pub fn get(&self, def: DefId) -> Option<&Scheme> {
        self.schemes.iter().find(|(d, _)| *d == def).map(|(_, s)| s)
    }

    /// Whether `def`'s body disagreed with its declared signature.
    #[must_use]
    pub fn is_mismatch(&self, def: DefId) -> bool {
        self.mismatches.contains(&def)
    }
}

/// The lowered scheme of a definition's *declared signature*, if it has one.
///
/// This is a tracked query so its (body-edit-stable) value enables early cutoff:
/// editing a private body re-runs this query but yields the same scheme, so
/// dependents (other modules' inference) are cut off — the firewall.
#[salsa::tracked]
pub fn signature_scheme(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<Scheme> {
    declared_scheme(db, file, name)
}

/// The type scheme of a single definition.
#[salsa::tracked]
pub fn def_type(db: &dyn Db, file: SourceFile, name: Symbol) -> Scheme {
    let def = DefId::new(file.source(db), name);
    let sccs = module_sccs(db, file);
    let Some(&idx) = sccs.index_of.get(&def) else {
        return error_scheme();
    };
    infer_scc_query(db, file, idx).get(def).cloned().unwrap_or_else(error_scheme)
}

/// The scheme used for an out-of-SCC reference: a declared signature when the
/// callee has one (cutting the dependency on its body — the firewall), else the
/// callee's inferred type.
fn declared_or_inferred_scheme(db: &dyn Db, def: DefId) -> Option<Scheme> {
    let file = db.source_file(def.file)?;
    if let Some(scheme) = signature_scheme(db, file, def.name) {
        return Some(scheme);
    }
    // Signature-less: reach the inferred type. (For a *cross-module* callee this
    // never happens for a well-formed program, because public bindings require a
    // signature; a signature-less public binding is an error and falls back here
    // only in the error state.)
    Some(def_type(db, file, def.name))
}

/// Test/introspection helper: the inferred types of the *local* bindings in
/// `name`'s body, as `(variable-name, type)` pairs.
///
/// This exercises inference directly (parameters, `let` locals, lambda binders),
/// independent of any declared signature — useful for testing local type
/// inference rather than just public-signature rendering. The returned [`Ty`]s
/// share one variable numbering, so a variable shared between locals (e.g.
/// tuple-destructuring components) renders consistently.
#[must_use]
pub fn def_local_types(
    db: &dyn Db,
    file: SourceFile,
    name: Symbol,
) -> Vec<(String, crate::ty::Ty)> {
    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |n: Symbol| prelude::builtin_scheme(n);
    crate::infer::infer_local_types(db, file, name, &def_schemes, &builtins)
        .into_iter()
        .map(|(sym, ty)| (sym.as_str().to_owned(), ty))
        .collect()
}

/// The inferred type of every expression in a definition's body.
///
/// A salsa value (so Core lowering depends on it for early cutoff). Mirrors the
/// firewall of [`def_type`]: out-of-SCC references resolve through
/// declared-or-inferred schemes, never bodies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyTypes {
    /// Each expression's reified type, keyed by `ExprId`.
    pub types: FxHashMap<ExprId, Ty>,
}

impl BodyTypes {
    /// The type recorded for `expr`, if any.
    #[must_use]
    pub fn get(&self, expr: ExprId) -> Option<&Ty> {
        self.types.get(&expr)
    }
}

/// The per-expression types of `name`'s body (the input to Core lowering).
#[salsa::tracked]
pub fn body_types(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<BodyTypes> {
    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |n: Symbol| prelude::builtin_scheme(n);
    let pairs = crate::infer::infer_body_types(db, file, name, &def_schemes, &builtins);
    Arc::new(BodyTypes { types: pairs.into_iter().collect() })
}

/// Type-checks every definition and contract in `file`, emitting diagnostics.
#[salsa::tracked]
pub fn check_file(db: &dyn Db, file: SourceFile) {
    let defs = module_defs(db, file);
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;

    for d in &defs.defs {
        let def = DefId::new(file.source(db), d.name);
        let inferred = def_type(db, file, d.name);

        match d.signature {
            None => {
                // A public binding must have a signature.
                if d.visibility == fai_syntax::ast::Visibility::Public {
                    let span = module.items[d.binding.index()].span;
                    let suggestion = crate::ty::render_scheme(&inferred);
                    emit(
                        db,
                        Diagnostic::error(
                            MISSING_PUBLIC_SIGNATURE,
                            format!("public binding `{}` needs a signature", d.name),
                            Span::new(file.source(db), span),
                        )
                        .with_help(format!("add a signature, e.g. `{} : {suggestion}`", d.name)),
                    );
                }
            }
            Some(sig_item) => {
                // The body was checked against the declared type during
                // inference; a recorded mismatch becomes FAI3004.
                let sccs = module_sccs(db, file);
                let is_mismatch = sccs
                    .index_of
                    .get(&def)
                    .map(|&idx| infer_scc_query(db, file, idx).is_mismatch(def))
                    .unwrap_or(false);
                if is_mismatch {
                    let sig_span = module.items[sig_item.index()].span;
                    let bind_span = module.items[d.binding.index()].span;
                    let declared = declared_scheme(db, file, d.name).unwrap_or_else(error_scheme);
                    emit(
                        db,
                        Diagnostic::error(
                            SIGNATURE_MISMATCH,
                            format!(
                                "the body of `{}` does not match its declared type `{}`",
                                d.name,
                                crate::ty::render_scheme(&declared),
                            ),
                            Span::new(file.source(db), bind_span),
                        )
                        .with_label(Label::new(
                            Span::new(file.source(db), sig_span),
                            "declared here",
                        )),
                    );
                }
            }
        }
    }

    crate::contracts::check_contracts(db, file);
}
