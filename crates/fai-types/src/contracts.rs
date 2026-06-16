//! Type-checking `example`/`forall` contracts.
//!
//! A contract body is resolved in module scope and must have type `Bool`
//! ([`CONTRACT_NOT_BOOL`]). `forall` binders are bound as fresh monomorphic type
//! variables (exactly like function parameters), so a reference to a binder in
//! the body is typed from its uses; a repeated binder is
//! [`DUPLICATE_BINDER`](fai_resolve). Contracts are checked per file and have no
//! exported type; they use referenced definitions' schemes (declared where
//! present), consistent with the firewall.
//!
//! Contracts must also be **pure**: a contract has no `Runtime` in scope, so the
//! only way to reach a host capability (`Console`, `Clock`, `Random`,
//! `FileSystem`, `Env`) is to reference an effectful binding whose type carries
//! one. Such a reference is reported directly as [`CONTRACT_IMPURE`] at the
//! offending expression, instead of surfacing as a downstream type mismatch.

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::{CONTRACT_IMPURE, Diagnostic};
use fai_resolve::{DefId, InterfaceRef, resolve};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{ExprId, ItemKind, Module, PatId, PatKind};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::CONTRACT_NOT_BOOL;
use crate::infer::{InferCtx, SolveTy, Walker, contract_env};
use crate::std_lib;
use crate::ty::{Scheme, Ty};

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
    walker.enable_type_recording();
    // Bind each `forall` binder as a fresh monomorphic parameter, so references
    // to it in the body are typed from use (and a misuse is a real type error).
    for &pat in binders {
        let _ = walker.bind_param(pat);
    }
    let body_ty = walker.infer_expr(body);
    let expr_types = walker.collect_expr_types();

    // A contract is pure by construction (no `Runtime` is in scope), so any
    // capability-typed expression means it references an effectful binding.
    // Report that at the offending reference and skip the `Bool` check, which
    // would otherwise pile a confusing mismatch on top of the real problem.
    if let Some((id, caps)) = leftmost_capability(db, module, &expr_types) {
        emit(db, impure_diagnostic(db, file, module.expr(id).span, &caps));
        return;
    }

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

/// Whether `iref` is a capability interface: one that declares at least one
/// **effect-carrying** method (a method whose arrow performs an effect). Effect
/// rows make this general — the host capabilities (`Console`, …) qualify because
/// their methods are annotated `/ { Console }` …, and a *user-declared* capability
/// (e.g. `interface Logger 'e = log : String -> Unit / 'e`) is recognized for
/// free. A plain interface whose methods are all pure (e.g. `Greeter`) is not a
/// capability, so a contract may build and exercise it.
fn is_capability_interface(db: &dyn Db, iref: InterfaceRef) -> bool {
    let Some(file) = db.source_file(iref.file) else {
        return false;
    };
    let decls = fai_resolve::interface_decls(db, file);
    let Some(info) = decls.interface_named(iref.name) else {
        return false;
    };
    info.methods.clone().into_iter().any(|m| {
        crate::lower::build_interface_method_scheme(db, iref, m)
            .is_some_and(|scheme| ty_performs_effect(&scheme.ty))
    })
}

/// Whether any arrow within `ty` performs an effect (a non-pure effect row),
/// anywhere in its structure — so a method type `String -> Unit / { Console }`
/// (and a nested `… -> … / { FileSystem }`) counts.
fn ty_performs_effect(ty: &Ty) -> bool {
    match ty {
        Ty::Arrow(from, to, eff) => {
            !eff.is_pure() || ty_performs_effect(from) || ty_performs_effect(to)
        }
        Ty::App(f, a) => ty_performs_effect(f) || ty_performs_effect(a),
        Ty::Tuple(ts) => ts.iter().any(ty_performs_effect),
        Ty::Record(row) => row.fields.iter().any(|(_, t)| ty_performs_effect(t)),
        Ty::Var(_)
        | Ty::Con(_)
        | Ty::Adt(_)
        | Ty::Interface(_)
        | Ty::EffectArg(_)
        | Ty::Unit
        | Ty::Error => false,
    }
}

/// The distinct host capabilities mentioned anywhere in `ty`, sorted by name. A
/// non-empty result means a value of this type carries a capability. `Runtime`
/// is a transparent record alias, so its capability fields are found here too.
fn capabilities_in_ty(db: &dyn Db, ty: &Ty) -> Vec<Symbol> {
    fn walk(db: &dyn Db, ty: &Ty, out: &mut Vec<Symbol>) {
        match ty {
            Ty::Interface(iref) if is_capability_interface(db, *iref) => out.push(iref.name),
            Ty::App(f, a) | Ty::Arrow(f, a, _) => {
                walk(db, f, out);
                walk(db, a, out);
            }
            Ty::Tuple(ts) => ts.iter().for_each(|t| walk(db, t, out)),
            Ty::Record(row) => row.fields.iter().for_each(|(_, t)| walk(db, t, out)),
            // An effect *argument* is a type-level effect row (erased), not a
            // capability value the runtime must provide.
            Ty::Var(_)
            | Ty::Con(_)
            | Ty::Adt(_)
            | Ty::Interface(_)
            | Ty::EffectArg(_)
            | Ty::Unit
            | Ty::Error => {}
        }
    }
    let mut out = Vec::new();
    walk(db, ty, &mut out);
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    out.dedup();
    out
}

/// The leftmost-innermost expression whose type carries a host capability,
/// paired with the distinct capabilities it mentions. `None` for a pure body.
fn leftmost_capability(
    db: &dyn Db,
    module: &Module,
    expr_types: &[(ExprId, Ty)],
) -> Option<(ExprId, Vec<Symbol>)> {
    let key = |id: ExprId| {
        let span = module.expr(id).span;
        (span.start().to_usize(), span.end().to_usize())
    };
    let mut best: Option<(ExprId, Vec<Symbol>)> = None;
    for (id, ty) in expr_types {
        let caps = capabilities_in_ty(db, ty);
        if caps.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| key(*id) < key(*b)) {
            best = Some((*id, caps));
        }
    }
    best
}

/// Builds the [`CONTRACT_IMPURE`] diagnostic for a capability reference: it names
/// the single capability, or lists several (e.g. a whole `Runtime`).
fn impure_diagnostic(
    db: &dyn Db,
    file: SourceFile,
    span: fai_span::TextRange,
    caps: &[Symbol],
) -> Diagnostic {
    let names: Vec<String> = caps.iter().map(|c| format!("`{c}`")).collect();
    let message = if let [only] = names.as_slice() {
        format!("a contract must be pure, but this references the {only} capability")
    } else {
        format!("a contract must be pure, but this references capabilities ({})", names.join(", "))
    };
    Diagnostic::error(CONTRACT_IMPURE, message, Span::new(file.source(db), span)).with_help(
        "Contracts are checked by `fai check` and run by `fai test`, so they must be deterministic \
         and pure — they cannot use the host capabilities (Console, Clock, Random, FileSystem, \
         Env) or the `Runtime` that bundles them. Express the law over pure values instead.",
    )
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
