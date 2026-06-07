//! Type-checking `example`/`forall` contracts.
//!
//! A contract body is resolved in module scope and must have type `Bool`
//! ([`CONTRACT_NOT_BOOL`]). `forall` binders are bound as fresh monomorphic type
//! variables (exactly like function parameters), so a reference to a binder in
//! the body is typed from its uses; a repeated binder is
//! [`DUPLICATE_BINDER`](fai_resolve). Contracts are checked per file and have no
//! exported type; they use referenced definitions' schemes (declared where
//! present), consistent with the firewall.

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{DefId, resolve};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{ExprId, ItemKind, Module, PatId, PatKind};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::CONTRACT_NOT_BOOL;
use crate::infer::{InferCtx, SolveTy, Walker, contract_env};
use crate::std_lib;
use crate::ty::Scheme;

/// Type-checks all contracts in `file`.
pub fn check_contracts(db: &dyn Db, file: SourceFile) {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = resolve(db, file);

    for item in &module.items {
        match &item.kind {
            ItemKind::Example { body } => {
                check_contract_body(db, file, &resolved, &[], *body, item.span);
            }
            ItemKind::Forall { binders, body } => {
                report_duplicate_binders(db, file, module, binders, item.span);
                check_contract_body(db, file, &resolved, binders, *body, item.span);
            }
            _ => {}
        }
    }
}

/// The variable name bound by a `forall` binder pattern (always a `Var`).
fn binder_name(module: &Module, pat: PatId) -> Option<Symbol> {
    match module.pat(pat).kind {
        PatKind::Var(name) => Some(name),
        _ => None,
    }
}

fn report_duplicate_binders(
    db: &dyn Db,
    file: SourceFile,
    module: &Module,
    binders: &[PatId],
    span: fai_span::TextRange,
) {
    let mut seen: FxHashSet<Symbol> = FxHashSet::default();
    for &pat in binders {
        let Some(name) = binder_name(module, pat) else { continue };
        if !seen.insert(name) {
            emit(
                db,
                Diagnostic::error(
                    fai_resolve::DUPLICATE_BINDER,
                    format!("`forall` repeats the binder `{name}`"),
                    Span::new(file.source(db), span),
                ),
            );
        }
    }
}

fn check_contract_body(
    db: &dyn Db,
    file: SourceFile,
    resolved: &fai_resolve::ResolvedBodies,
    binders: &[PatId],
    body: ExprId,
    span: fai_span::TextRange,
) {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;

    let mut cx = InferCtx::new();
    let def_schemes = |db: &dyn Db, def: DefId| scheme_for(db, def);
    let builtins = |name: Symbol| std_lib::builtin_scheme(name);
    let scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    let mut env = contract_env(db, &scc_types, &def_schemes, &builtins);

    let mut walker = Walker::new(db, file, module, resolved, &mut cx, &mut env);
    // Bind each `forall` binder as a fresh monomorphic parameter, so references
    // to it in the body are typed from use (and a misuse is a real type error).
    for &pat in binders {
        let _ = walker.bind_param(pat);
    }
    let body_ty = walker.infer_expr(body);
    if cx.unify(&body_ty, &SolveTy::bool()) != crate::infer::UnifyResult::Ok {
        let rendered = crate::ty::render(&cx.reify(&body_ty), &crate::ty::VarNames::new());
        emit(
            db,
            Diagnostic::error(
                CONTRACT_NOT_BOOL,
                format!("a contract must have type `Bool`, but this has type `{rendered}`"),
                Span::new(file.source(db), span),
            ),
        );
    }
}

/// Convenience used by contracts to fetch a referenced definition's scheme.
#[must_use]
pub fn scheme_for(db: &dyn Db, def: DefId) -> Option<Scheme> {
    let file = db.source_file(def.file)?;
    if let Some(s) = crate::infer::declared_scheme(db, file, def.name) {
        Some(s)
    } else {
        Some(crate::query::def_type(db, file, def.name))
    }
}
