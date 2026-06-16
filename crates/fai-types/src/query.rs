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
use fai_diagnostics::{Diagnostic, Label, Suggestion};
use fai_resolve::{DefId, InterfaceRef, module_defs, module_sccs, qualify, resolve};
use fai_span::{ByteOffset, Span, TextRange};
use fai_syntax::Symbol;
use fai_syntax::ast::{ExprId, ItemKind};
use rustc_hash::FxHashMap;

use crate::ty::Ty;

use crate::infer::{declared_scheme, error_scheme, infer_scc};
use crate::lower::{ParamKind, interface_param_usage, seed_interface_params};
use crate::std_lib;
use crate::ty::Scheme;
use crate::{
    EFFECT_MISMATCH, FOREIGN_EFFECT_REQUIRED, INTERFACE_PARAM_KIND, MISSING_PUBLIC_SIGNATURE,
    OPAQUE_ACCESS, SIGNATURE_MISMATCH,
};

/// Whether a `foreign` declaration's scheme names at least one capability in the
/// effect a full application performs — the effect on the innermost arrow of its
/// (curried) function type. A foreign that names none would launder its native
/// side effect as pure (rejected as [`FOREIGN_EFFECT_REQUIRED`]).
fn foreign_names_a_capability(scheme: &Scheme) -> bool {
    fn innermost(ty: &Ty) -> Option<&crate::ty::EffectRow> {
        match ty {
            // A deeper arrow's effect is the saturating one; this arrow's effect is
            // used only when its result is not itself a function.
            Ty::Arrow(_, to, eff) => Some(innermost(to).unwrap_or(eff)),
            _ => None,
        }
    }
    innermost(&scheme.ty).is_some_and(|e| !e.labels.is_empty())
}

/// The inferred schemes of the SCC at `scc_index` in `file`.
#[salsa::tracked]
pub fn infer_scc_query(db: &dyn Db, file: SourceFile, scc_index: usize) -> Arc<SccTypes> {
    let sccs = module_sccs(db, file);
    let Some(scc) = sccs.sccs.get(scc_index) else {
        return Arc::new(SccTypes::default());
    };
    let resolved = resolve(db, file);

    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |name: Symbol| std_lib::builtin_scheme(name);

    let inference = infer_scc(db, file, &scc.members, &resolved, &def_schemes, &builtins);
    Arc::new(SccTypes {
        schemes: inference.schemes.into_iter().collect(),
        mismatches: inference.mismatches,
        opaque_mismatches: inference.opaque_mismatches,
        effect_mismatches: inference.effect_mismatches,
    })
}

/// The schemes inferred for one SCC's members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SccTypes {
    /// Each member's generalized scheme.
    pub schemes: Vec<(DefId, Scheme)>,
    /// Members whose body disagreed with its declared signature.
    pub mismatches: Vec<DefId>,
    /// Members whose mismatch is a structural value against an opaque signature,
    /// paired with that opaque type's name.
    pub opaque_mismatches: Vec<(DefId, Symbol)>,
    /// Members whose declared concrete effect disagreed with the inferred effect,
    /// as `(def, declared rendered, inferred rendered)` (for FAI5001).
    pub effect_mismatches: Vec<(DefId, String, String)>,
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

    /// The opaque type a `def`'s body tried to build structurally, if its
    /// mismatch is an opaque-access rather than a plain signature mismatch.
    #[must_use]
    pub fn opaque_mismatch(&self, def: DefId) -> Option<Symbol> {
        self.opaque_mismatches.iter().find(|(d, _)| *d == def).map(|(_, n)| *n)
    }

    /// The `(declared, inferred)` rendered effects for `def` if its declared
    /// concrete effect disagreed with the inferred one.
    #[must_use]
    pub fn effect_mismatch(&self, def: DefId) -> Option<(&str, &str)> {
        self.effect_mismatches
            .iter()
            .find(|(d, _, _)| *d == def)
            .map(|(_, decl, inf)| (decl.as_str(), inf.as_str()))
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

/// The scheme of a data constructor declared in `file` (e.g. `Some : 'a ->
/// Option 'a`). Tracked so it is computed once and stays a body-edit-stable part
/// of the module's public interface.
#[salsa::tracked]
pub fn constructor_scheme(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<Scheme> {
    crate::lower::build_constructor_scheme(db, file, name)
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
/// callee's inferred type. Also drives offset-evidence elaboration in lowering,
/// where caller and callee must agree on a function's evidence from its type.
pub fn declared_or_inferred_scheme(db: &dyn Db, def: DefId) -> Option<Scheme> {
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
    let builtins = |n: Symbol| std_lib::builtin_scheme(n);
    crate::infer::infer_local_types(db, file, name, &def_schemes, &builtins)
        .into_iter()
        .map(|(sym, ty)| (sym.as_str().to_owned(), ty))
        .collect()
}

/// The inferred latent effect of a definition's body — the host capabilities it
/// uses (directly or via the lambdas/methods it calls). A function that merely
/// *holds* or *builds an effectful closure* is pure; the effect is recorded
/// where a capability method is actually invoked.
#[must_use]
pub fn def_effect(db: &dyn Db, file: SourceFile, name: Symbol) -> crate::ty::EffectRow {
    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |n: Symbol| std_lib::builtin_scheme(n);
    crate::infer::infer_def_effect(db, file, name, &def_schemes, &builtins)
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
    /// Each pattern's reified type, keyed by `PatId`.
    pub pat_types: FxHashMap<fai_syntax::ast::PatId, Ty>,
}

impl BodyTypes {
    /// The type recorded for `expr`, if any.
    #[must_use]
    pub fn get(&self, expr: ExprId) -> Option<&Ty> {
        self.types.get(&expr)
    }

    /// The type recorded for a pattern, if any.
    #[must_use]
    pub fn pat_type(&self, pat: fai_syntax::ast::PatId) -> Option<&Ty> {
        self.pat_types.get(&pat)
    }
}

/// The per-expression and per-pattern types of `name`'s body (the input to Core
/// lowering).
#[salsa::tracked]
pub fn body_types(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<BodyTypes> {
    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |n: Symbol| std_lib::builtin_scheme(n);
    let (exprs, pats) = crate::infer::infer_body_types(db, file, name, &def_schemes, &builtins);
    Arc::new(BodyTypes {
        types: exprs.into_iter().collect(),
        pat_types: pats.into_iter().collect(),
    })
}

/// The per-expression and per-pattern types of the `ordinal`-th contract in
/// `file` (a peer of [`body_types`], but for an `example`/`forall` body). Binders
/// are bound as fresh parameters and residual type variables default to `Int`, so
/// the result is fully monomorphic and ready for harness lowering.
#[salsa::tracked]
pub fn contract_body_types(db: &dyn Db, file: SourceFile, ordinal: usize) -> Arc<BodyTypes> {
    let parsed = fai_syntax::parse(db, file);
    let Some(item) = parsed.module.contract(ordinal) else {
        return Arc::new(BodyTypes::default());
    };
    let (binders, body) = match &item.kind {
        fai_syntax::ast::ItemKind::Example { body } => (Vec::new(), *body),
        fai_syntax::ast::ItemKind::Forall { binders, body } => (binders.clone(), *body),
        _ => return Arc::new(BodyTypes::default()),
    };
    let def_schemes = |db: &dyn Db, def: DefId| declared_or_inferred_scheme(db, def);
    let builtins = |n: Symbol| std_lib::builtin_scheme(n);
    let (exprs, pats) =
        crate::infer::infer_contract_body_types(db, file, &binders, body, &def_schemes, &builtins);
    Arc::new(BodyTypes {
        types: exprs.into_iter().collect(),
        pat_types: pats.into_iter().collect(),
    })
}

/// Validates the `type`/`interface` declarations of one module scope (lowering
/// alias bodies, constructor schemes, and method signatures), recursing into
/// nested modules so each declaration is checked under its own scope path.
fn validate_decls(
    db: &dyn Db,
    file: SourceFile,
    module: &fai_syntax::ast::Module,
    scope: &mut Vec<fai_syntax::Symbol>,
    items: &[fai_syntax::ast::ItemId],
) {
    for &id in items {
        match &module.items[id.index()].kind {
            fai_syntax::ast::ItemKind::Type { def, .. } => match def {
                fai_syntax::ast::TypeDef::Alias(ty) => {
                    let mut vars = crate::lower::LowerVars::default();
                    let _ = crate::lower::lower_type_in(db, file, module, scope, *ty, &mut vars);
                }
                fai_syntax::ast::TypeDef::Union(variants) => {
                    for v in variants {
                        let _ = constructor_scheme(db, file, fai_resolve::qualify(scope, v.name));
                    }
                }
            },
            fai_syntax::ast::ItemKind::Interface { methods, params, name, .. } => {
                let iref = InterfaceRef::new(file.source(db), qualify(scope, *name));
                // Infer each parameter's kind from its use across the methods; a
                // parameter used as both a type and an effect row is ill-kinded.
                let usage = interface_param_usage(db, iref);
                let kinds: Vec<ParamKind> = usage
                    .iter()
                    .map(|&(t, e)| if e && !t { ParamKind::Effect } else { ParamKind::Type })
                    .collect();
                for (i, &(ty_used, eff_used)) in usage.iter().enumerate() {
                    if ty_used && eff_used {
                        let span = module.items[id.index()].span;
                        emit(
                            db,
                            Diagnostic::error(
                                INTERFACE_PARAM_KIND,
                                format!(
                                    "interface parameter `{}` of `{name}` is used as both a type \
                                     and an effect",
                                    params[i]
                                ),
                                Span::new(file.source(db), span),
                            ),
                        );
                    }
                }
                for m in methods {
                    let mut vars = crate::lower::LowerVars::default();
                    seed_interface_params(&mut vars, params, &kinds);
                    let _ = crate::lower::lower_type_in(db, file, module, scope, m.ty, &mut vars);
                }
            }
            fai_syntax::ast::ItemKind::Module { name, body } => {
                scope.push(*name);
                validate_decls(db, file, module, scope, body);
                scope.pop();
            }
            _ => {}
        }
    }
}

/// Type-checks every definition and contract in `file`, emitting diagnostics.
#[salsa::tracked]
pub fn check_file(db: &dyn Db, file: SourceFile) {
    let defs = module_defs(db, file);
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;

    // Validate `type` and `interface` declarations (in source order, descending
    // into nested modules so each declaration is checked in its own scope).
    let mut scope: Vec<fai_syntax::Symbol> = Vec::new();
    validate_decls(db, file, module, &mut scope, &module.roots);

    for d in &defs.defs {
        let def = DefId::new(file.source(db), d.name);
        let inferred = def_type(db, file, d.name);

        match d.signature {
            None => {
                // A public binding must have a signature.
                if d.visibility == fai_syntax::ast::Visibility::Public {
                    let span = module.items[d.binding.index()].span;
                    let rendered = crate::ty::render_scheme(&inferred);
                    let mut diag = Diagnostic::error(
                        MISSING_PUBLIC_SIGNATURE,
                        format!("public binding `{}` needs a signature", d.name),
                        Span::new(file.source(db), span),
                    )
                    .with_help(format!("add a signature, e.g. `{} : {rendered}`", d.name));
                    // Offer the inferred signature as a machine-applicable fix:
                    // move `public` onto a new signature line above the binding.
                    if let Some(fix) = missing_signature_fix(db, file, span, d.name, &rendered) {
                        diag = diag.with_suggestion(fix);
                    }
                    emit(db, diag);
                }
            }
            Some(sig_item) => {
                // The body was checked against the declared type during
                // inference; a recorded mismatch becomes FAI3004 — or, when the
                // body builds an opaque type structurally, the more specific
                // FAI3018.
                let sccs = module_sccs(db, file);
                let scc_types = sccs.index_of.get(&def).map(|&idx| infer_scc_query(db, file, idx));
                let is_mismatch = scc_types.as_ref().is_some_and(|t| t.is_mismatch(def));
                if is_mismatch {
                    let bind_span = module.items[d.binding.index()].span;
                    if let Some(name) = scc_types.and_then(|t| t.opaque_mismatch(def)) {
                        emit(
                            db,
                            Diagnostic::error(
                                OPAQUE_ACCESS,
                                format!(
                                    "the type `{name}` is opaque; its values cannot be built \
                                     by record construction from this file"
                                ),
                                Span::new(file.source(db), bind_span),
                            ),
                        );
                    } else {
                        let sig_span = module.items[sig_item.index()].span;
                        let declared =
                            declared_scheme(db, file, d.name).unwrap_or_else(error_scheme);
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

        // Opt-in effect enforcement (FAI5001): a signature that declares a
        // concrete effect must match the capabilities the body actually uses.
        if let Some(&idx) = module_sccs(db, file).index_of.get(&def)
            && let Some((declared, used)) = infer_scc_query(db, file, idx).effect_mismatch(def)
        {
            let bind_span = module.items[d.binding.index()].span;
            emit(
                db,
                Diagnostic::error(
                    EFFECT_MISMATCH,
                    format!(
                        "the declared effect of `{}` is `{declared}`, but its body uses `{used}`",
                        d.name
                    ),
                    Span::new(file.source(db), bind_span),
                ),
            );
        }

        // A `foreign` declaration must name a capability in its effect row (FAI5002),
        // so a native side effect cannot be laundered as pure: any caller of the
        // foreign then surfaces that capability through the ordinary effect check.
        if let ItemKind::Foreign { .. } = &module.items[d.binding.index()].kind
            && !foreign_names_a_capability(
                &declared_scheme(db, file, d.name).unwrap_or_else(error_scheme),
            )
        {
            emit(
                db,
                Diagnostic::error(
                    FOREIGN_EFFECT_REQUIRED,
                    format!(
                        "the foreign declaration `{}` must name a capability in its effect row, \
                         e.g. `/ {{ Console }}`",
                        d.name
                    ),
                    Span::new(file.source(db), module.items[d.binding.index()].span),
                ),
            );
        }
    }

    crate::exhaustive::check_matches(db, file);
    crate::contracts::check_contracts(db, file);
}

/// The machine-applicable fix for a missing public signature: replace the
/// binding's leading `public ` keyword with a `public name : type` signature line
/// (so visibility moves to the signature) followed by the binding's original
/// indentation, leaving the binding itself a plain `let`.
fn missing_signature_fix(
    db: &dyn Db,
    file: SourceFile,
    binding_span: TextRange,
    name: Symbol,
    rendered_type: &str,
) -> Option<Suggestion> {
    let text = file.text(db);
    let start = binding_span.start().to_usize();
    let end = binding_span.end().to_usize();
    let binding_text = text.get(start..end)?;
    // The `let` keyword sits right after the `public ` prefix to replace.
    let let_off = binding_text.find("let")?;
    // Repeat the binding's own indentation on the inserted signature line.
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line_start..start];
    let prefix = Span::new(
        file.source(db),
        TextRange::new(ByteOffset::from_usize(start), ByteOffset::from_usize(start + let_off)),
    );
    // The signature uses the binding's own (bare) name, even for a nested member
    // whose qualified `name` carries a module path.
    let bare = name.as_str().rsplit('.').next().unwrap_or(name.as_str());
    Some(Suggestion::new(prefix, format!("public {bare} : {rendered_type}\n{indent}")))
}
