//! Inference over a single definition or SCC.
//!
//! Members of an SCC are inferred together: each gets a fresh monomorphic type,
//! references *within* the SCC use those monomorphic types (so mutual recursion
//! is monomorphic), and references *outside* go through declared/inferred
//! schemes — never bodies. After solving, each member is generalized.

pub(crate) mod ctx;
mod walk;

pub use ctx::{Constraint, InferCtx, SolveTy, UnifyResult};
pub use walk::{Env, Walker};

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, ResolvedBodies};
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemKind, Module};
use rustc_hash::FxHashMap;

use crate::ty::{Scheme, Ty};

/// Generalizes a solver type into a scheme by quantifying its free variables.
///
/// In M2 there are no enclosing monomorphic bindings to exclude at the top level
/// (each def is generalized independently), so every remaining free variable is
/// quantified — except that an unresolved constrained variable that would
/// generalize is the caller's concern (it is reported as ambiguous before
/// generalization for Numeric/Ord).
pub fn generalize(cx: &InferCtx, ty: &SolveTy) -> Scheme {
    let (reified, vars, row_vars, eff_vars) = cx.reify_with_vars(ty);
    let row_names = row_vars.iter().map(|_| "_".to_owned()).collect();
    // Effect variables read as named (`'e`, `'f`, …), not anonymous `_`, so a
    // forwarded effect that appears in several positions renders linked.
    let eff_names =
        eff_vars.iter().enumerate().map(|(i, _)| crate::ty::eff_canonical_name(i)).collect();
    Scheme::new(vars, reified).with_rows(row_vars, row_names).with_effects(eff_vars, eff_names)
}

/// Looks up the declared signature scheme of a definition in `file`, if it has
/// one (lowered from the AST). Returns `None` for signature-less definitions.
pub fn declared_scheme(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<Scheme> {
    let defs = fai_resolve::module_defs(db, file);
    let def = defs.get(name)?;
    let sig_item = def.signature?;
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    if let ItemKind::Signature { ty, .. } = &module.items[sig_item.index()].kind {
        // Resolve the signature's type names in the definition's module scope.
        let mut scope: Vec<Symbol> = name.as_str().split('.').map(Symbol::intern).collect();
        scope.pop();
        Some(crate::lower::lower_signature_in(db, file, module, &scope, *ty))
    } else {
        None
    }
}

/// The environment used while inferring one SCC: same-SCC monomorphic types plus
/// outside schemes. Also used for contracts (with an empty SCC-type map).
pub struct SccEnv<'a> {
    db: &'a dyn Db,
    /// Monomorphic solver types of the SCC's members, by def.
    scc_types: &'a FxHashMap<DefId, SolveTy>,
    /// Resolver for an out-of-SCC definition's scheme.
    def_schemes: &'a dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    /// Resolver for a builtin/prelude name's scheme.
    builtins: &'a dyn Fn(Symbol) -> Option<Scheme>,
}

impl<'a> SccEnv<'a> {
    /// Builds an environment from its parts.
    pub fn new(
        db: &'a dyn Db,
        scc_types: &'a FxHashMap<DefId, SolveTy>,
        def_schemes: &'a dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
        builtins: &'a dyn Fn(Symbol) -> Option<Scheme>,
    ) -> Self {
        Self { db, scc_types, def_schemes, builtins }
    }
}

/// Builds an environment for checking a contract (no SCC members in scope).
pub fn contract_env<'a>(
    db: &'a dyn Db,
    scc_types: &'a FxHashMap<DefId, SolveTy>,
    def_schemes: &'a dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    builtins: &'a dyn Fn(Symbol) -> Option<Scheme>,
) -> SccEnv<'a> {
    SccEnv::new(db, scc_types, def_schemes, builtins)
}

impl Env for SccEnv<'_> {
    fn def_scheme(&mut self, def: DefId) -> Option<Scheme> {
        (self.def_schemes)(self.db, def)
    }

    fn scc_type(&mut self, def: DefId) -> Option<SolveTy> {
        self.scc_types.get(&def).cloned()
    }

    fn builtin_scheme(&mut self, name: Symbol) -> Option<Scheme> {
        (self.builtins)(name)
    }

    fn ctor_scheme(&mut self, ctor: fai_resolve::CtorRef) -> Option<Scheme> {
        let file = self.db.source_file(ctor.file)?;
        crate::query::constructor_scheme(self.db, file, ctor.name)
    }
}

/// The body item (params + body expr) of a definition with qualified `name`,
/// located by its binding `ItemId` (so a nested definition is found by its
/// module-qualified name, not the local name in the AST).
fn binding_body<'a>(
    db: &dyn Db,
    file: SourceFile,
    module: &'a Module,
    name: Symbol,
) -> Option<(&'a [fai_syntax::ast::PatId], fai_syntax::ast::ExprId)> {
    let binding = fai_resolve::module_defs(db, file).get(name)?.binding;
    match &module.items[binding.index()].kind {
        ItemKind::Binding { params, body, .. } => Some((params.as_slice(), *body)),
        _ => None,
    }
}

/// Infers the schemes of an SCC's members.
///
/// `members` are the SCC's definitions (all in `file`). `def_schemes` resolves an
/// out-of-SCC reference's scheme; `builtins` resolves a prelude name's scheme.
/// Returns each member's generalized scheme.
/// The result of inferring an SCC: each member's scheme, plus the members whose
/// declared signature disagreed with the inferred body (for FAI3004).
pub struct SccInference {
    /// Each member's exported scheme (declared if signatured, else generalized).
    pub schemes: FxHashMap<DefId, Scheme>,
    /// Members whose body did not match its declared signature.
    pub mismatches: Vec<DefId>,
    /// Members whose mismatch is specifically a structural value (a record)
    /// where the signature is an opaque type, paired with that type's name —
    /// reported as the more specific opaque-access error rather than a bare
    /// signature mismatch.
    pub opaque_mismatches: Vec<(DefId, Symbol)>,
}

#[allow(clippy::too_many_arguments)]
pub fn infer_scc(
    db: &dyn Db,
    file: SourceFile,
    members: &[DefId],
    resolved: &ResolvedBodies,
    def_schemes: &dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    builtins: &dyn Fn(Symbol) -> Option<Scheme>,
) -> SccInference {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;

    let mut cx = InferCtx::new();
    let mut mismatches: Vec<DefId> = Vec::new();
    let mut opaque_mismatches: Vec<(DefId, Symbol)> = Vec::new();

    // Fresh monomorphic type for each member. If a member has a declared
    // signature, instantiate it as the member's type (so the body is checked
    // against the signature and recursive calls use the declared type).
    let mut scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    let mut declared: FxHashMap<DefId, Scheme> = FxHashMap::default();
    let mut declared_vars: FxHashMap<DefId, Vec<crate::ty::TyVarId>> = FxHashMap::default();
    let mut declared_rows: FxHashMap<DefId, Vec<crate::ty::RowVarId>> = FxHashMap::default();
    for m in members {
        if let Some(scheme) = declared_scheme(db, file, m.name) {
            let (mono, vars, rows) = cx.instantiate_tracked(&scheme);
            scc_types.insert(*m, mono);
            declared_vars.insert(*m, vars);
            declared_rows.insert(*m, rows);
            declared.insert(*m, scheme);
        } else {
            scc_types.insert(*m, cx.fresh());
        }
    }

    // Infer each member's body, unifying with its monomorphic type.
    for m in members {
        let Some((params, body)) = binding_body(db, file, module, m.name) else {
            continue;
        };
        let member_ty = scc_types[m].clone();

        // For a signatured member, the body is checked with each parameter bound
        // to its declared type (peeled from the signature). This makes
        // type-directed interface method access on a parameter work.
        let declared_params: Option<Vec<SolveTy>> = if declared.contains_key(m) {
            Some(peel_param_types(&cx, &member_ty, params.len()))
        } else {
            None
        };

        let env_scc = scc_types.clone();
        let mut env = SccEnv { db, scc_types: &env_scc, def_schemes, builtins };
        let mut walker = Walker::new(db, file, module, resolved, &mut cx, &mut env);

        // Parameters introduce local types (the declared one when known); the
        // body's type is the result.
        let param_tys: Vec<SolveTy> = params
            .iter()
            .enumerate()
            .map(|(i, &p)| match declared_params.as_ref().and_then(|d| d.get(i)).cloned() {
                Some(expected) => walker.bind_param_checked(p, &expected),
                None => walker.bind_param(p),
            })
            .collect();
        let body_ty = walker.infer_expr(body);
        // The body's latent effect rides the function's saturating arrow.
        let body_eff = walker.body_effect_solve();
        let fn_ty = SolveTy::arrows_solver_eff(param_tys, body_ty, body_eff);

        let unify = cx.unify(&fn_ty, &member_ty);
        // A failed unification (the body conflicts with the signature) is an
        // immediate mismatch.
        if declared.contains_key(m) && unify != UnifyResult::Ok {
            mismatches.push(*m);
            // A structural body (a record) against an opaque signature is the
            // construction-of-an-opaque-type case; report it specifically.
            let body_re = cx.reify(&fn_ty);
            let sig_re = cx.reify(&member_ty);
            let opaque = match (&body_re, &sig_re) {
                (crate::ty::Ty::Record(_), other) | (other, crate::ty::Ty::Record(_)) => {
                    walk::opaque_adt_head_name(db, other)
                }
                _ => None,
            };
            if let Some(name) = opaque {
                opaque_mismatches.push((*m, name));
            }
        }
    }

    // Default unresolved numeric variables, then check signature generality and
    // generalize. Defaulting must happen first so that, e.g., `f : 'a -> 'a` with
    // body `x + 1` is seen as `Int -> Int` (the quantified var was forced to Int).
    for m in members {
        let ty = scc_types[m].clone();
        default_numerics(&mut cx, &ty);
    }

    // An over-general signature is one whose quantified variables the body forced
    // to concrete types or collapsed together, or whose quantified row variable
    // the body forced to contain a field the signature does not name (which would
    // read past a caller's record).
    for m in members {
        if declared.contains_key(m) && !mismatches.contains(m) {
            let vars_over_general =
                declared_vars.get(m).is_some_and(|vars| !cx.all_distinct_free(vars));
            let rows_over_general =
                declared_rows.get(m).is_some_and(|rows| !cx.rows_gained_no_fields(rows));
            if vars_over_general || rows_over_general {
                mismatches.push(*m);
            }
        }
    }

    let mut result = FxHashMap::default();
    for m in members {
        let ty = scc_types[m].clone();
        let scheme = if let Some(decl) = declared.get(m) {
            // The exported type is the declared signature (firewall): use it
            // verbatim. The body was checked against it above.
            decl.clone()
        } else {
            generalize(&cx, &ty)
        };
        result.insert(*m, scheme);
    }
    SccInference { schemes: result, mismatches, opaque_mismatches }
}

/// Peels the first `n` parameter types from a (resolved) function type.
fn peel_param_types(cx: &InferCtx, ty: &SolveTy, n: usize) -> Vec<SolveTy> {
    let mut out = Vec::with_capacity(n);
    let mut cur = cx.resolve_shallow(ty);
    for _ in 0..n {
        match cur {
            SolveTy::Arrow(from, to, _) => {
                out.push(std::rc::Rc::unwrap_or_clone(from));
                cur = cx.resolve_shallow(&to);
            }
            _ => break,
        }
    }
    out
}

/// Recursively defaults still-free Numeric variables in a solver type to `Int`.
fn default_numerics(cx: &mut InferCtx, ty: &SolveTy) {
    cx.default_numerics_deep(ty);
}

/// Introspection for tests: the inferred type of each *local* binding in
/// `name`'s body.
///
/// Unlike [`infer_scc`]/`def_type`, which return the *declared* scheme for a
/// signatured binding, this exposes the types inference actually computed for
/// parameters, `let`-bound locals, and lambda binders. Returns
/// `(variable-name, generalized-scheme)` pairs in local-allocation order; the
/// closures resolve out-of-body and prelude references (mirroring the real
/// inference environment). Self-recursion uses the def's own (declared-or-fresh)
/// type.
pub fn infer_local_types(
    db: &dyn Db,
    file: SourceFile,
    name: Symbol,
    def_schemes: &dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    builtins: &dyn Fn(Symbol) -> Option<Scheme>,
) -> Vec<(Symbol, Ty)> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = fai_resolve::resolve(db, file);

    let Some((params, body)) = binding_body(db, file, module, name) else {
        return Vec::new();
    };
    let params: Vec<fai_syntax::ast::PatId> = params.to_vec();

    let mut cx = InferCtx::new();
    let declared = declared_scheme(db, file, name);
    let member_ty = match &declared {
        Some(scheme) => cx.instantiate(scheme),
        None => cx.fresh(),
    };
    let mut scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    scc_types.insert(DefId::new(file.source(db), name), member_ty.clone());

    // Map each local slot to its source variable name (for var/wildcard patterns).
    let mut local_names: FxHashMap<fai_resolve::LocalId, Symbol> = FxHashMap::default();
    for (pat, local) in &resolved.pat_locals {
        if let fai_syntax::ast::PatKind::Var(sym) = &module.pat(*pat).kind {
            local_names.insert(*local, *sym);
        }
    }

    // Bind signatured parameters to their declared types up front (see
    // [`infer_body_types`]), so method access on a parameter resolves.
    let declared_params: Option<Vec<SolveTy>> =
        declared.is_some().then(|| peel_param_types(&cx, &member_ty, params.len()));

    let locals = {
        let mut env = SccEnv::new(db, &scc_types, def_schemes, builtins);
        let mut walker = Walker::new(db, file, module, &resolved, &mut cx, &mut env);
        let param_tys: Vec<SolveTy> = params
            .iter()
            .enumerate()
            .map(|(i, &p)| match declared_params.as_ref().and_then(|d| d.get(i)).cloned() {
                Some(expected) => walker.bind_param_checked(p, &expected),
                None => walker.bind_param(p),
            })
            .collect();
        let body_ty = walker.infer_expr(body);
        let fn_ty = SolveTy::arrows_solver(param_tys, body_ty);
        let _ = walker.cx.unify(&fn_ty, &member_ty);
        walker.collect_local_types()
    };

    locals.into_iter().filter_map(|(id, ty)| local_names.get(&id).map(|name| (*name, ty))).collect()
}

/// Infers the latent effect of `name`'s body — the capabilities it uses. Runs
/// the same walk as [`infer_local_types`] but returns the body's accumulated
/// effect row (after numeric defaulting). The closure-capturing case is handled
/// by the walker: a lambda's effect lands on the lambda's arrow, so a function
/// that only *builds* an effectful closure is itself pure.
pub fn infer_def_effect(
    db: &dyn Db,
    file: SourceFile,
    name: Symbol,
    def_schemes: &dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    builtins: &dyn Fn(Symbol) -> Option<Scheme>,
) -> crate::ty::EffectRow {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = fai_resolve::resolve(db, file);

    let Some((params, body)) = binding_body(db, file, module, name) else {
        return crate::ty::EffectRow::pure();
    };
    let params: Vec<fai_syntax::ast::PatId> = params.to_vec();

    let mut cx = InferCtx::new();
    let declared = declared_scheme(db, file, name);
    let member_ty = match &declared {
        Some(scheme) => cx.instantiate(scheme),
        None => cx.fresh(),
    };
    let mut scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    scc_types.insert(DefId::new(file.source(db), name), member_ty.clone());

    let declared_params: Option<Vec<SolveTy>> =
        declared.is_some().then(|| peel_param_types(&cx, &member_ty, params.len()));

    let mut env = SccEnv::new(db, &scc_types, def_schemes, builtins);
    let mut walker = Walker::new(db, file, module, &resolved, &mut cx, &mut env);
    let param_tys: Vec<SolveTy> = params
        .iter()
        .enumerate()
        .map(|(i, &p)| match declared_params.as_ref().and_then(|d| d.get(i)).cloned() {
            Some(expected) => walker.bind_param_checked(p, &expected),
            None => walker.bind_param(p),
        })
        .collect();
    let _ = param_tys;
    let _ = walker.infer_expr(body);
    walker.body_effect()
}

/// Infers the type of every expression in `name`'s body, as `(ExprId, Ty)`
/// pairs sharing one variable numbering.
///
/// This backs the `body_types` query consumed by Core lowering. Like
/// [`infer_local_types`], it re-runs a walk over the body (independent of the
/// SCC cache) with self-recursion bound to the def's declared-or-fresh type, and
/// reifies the recorded solver types after defaulting.
#[allow(clippy::type_complexity)]
pub fn infer_body_types(
    db: &dyn Db,
    file: SourceFile,
    name: Symbol,
    def_schemes: &dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    builtins: &dyn Fn(Symbol) -> Option<Scheme>,
) -> (Vec<(fai_syntax::ast::ExprId, Ty)>, Vec<(fai_syntax::ast::PatId, Ty)>) {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = fai_resolve::resolve(db, file);

    let Some((params, body)) = binding_body(db, file, module, name) else {
        return (Vec::new(), Vec::new());
    };
    let params: Vec<fai_syntax::ast::PatId> = params.to_vec();

    let mut cx = InferCtx::new();
    let declared = declared_scheme(db, file, name);
    let member_ty = match &declared {
        Some(scheme) => cx.instantiate(scheme),
        None => cx.fresh(),
    };
    let mut scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    scc_types.insert(DefId::new(file.source(db), name), member_ty.clone());

    // For a signatured binding, bind each parameter to its declared type *before*
    // checking the body, so type-directed interface method access on a parameter
    // (e.g. `runtime.console.writeLine`) sees the parameter's real type.
    let declared_params: Option<Vec<SolveTy>> =
        declared.is_some().then(|| peel_param_types(&cx, &member_ty, params.len()));

    let mut env = SccEnv::new(db, &scc_types, def_schemes, builtins);
    let mut walker = Walker::new(db, file, module, &resolved, &mut cx, &mut env);
    walker.enable_type_recording();
    let param_tys: Vec<SolveTy> = params
        .iter()
        .enumerate()
        .map(|(i, &p)| match declared_params.as_ref().and_then(|d| d.get(i)).cloned() {
            Some(expected) => walker.bind_param_checked(p, &expected),
            None => walker.bind_param(p),
        })
        .collect();
    let body_ty = walker.infer_expr(body);
    let fn_ty = SolveTy::arrows_solver(param_tys, body_ty);
    let _ = walker.cx.unify(&fn_ty, &member_ty);
    let exprs = walker.collect_expr_types();
    let pats = walker.collect_pat_types();
    (exprs, pats)
}

/// Infers the per-expression and per-pattern types of a contract body, with the
/// `forall` binders bound as fresh monomorphic parameters and every residual
/// (unconstrained) type variable defaulted to `Int` — so the synthesized harness
/// lowers to monomorphic code and the value generators know each binder's shape.
#[allow(clippy::type_complexity)]
pub fn infer_contract_body_types(
    db: &dyn Db,
    file: SourceFile,
    binders: &[fai_syntax::ast::PatId],
    body: fai_syntax::ast::ExprId,
    def_schemes: &dyn Fn(&dyn Db, DefId) -> Option<Scheme>,
    builtins: &dyn Fn(Symbol) -> Option<Scheme>,
) -> (Vec<(fai_syntax::ast::ExprId, Ty)>, Vec<(fai_syntax::ast::PatId, Ty)>) {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = fai_resolve::resolve(db, file);

    let mut cx = InferCtx::new();
    let scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    let mut env = contract_env(db, &scc_types, def_schemes, builtins);
    let mut walker = Walker::new(db, file, module, &resolved, &mut cx, &mut env);
    walker.enable_type_recording();
    for &p in binders {
        let _ = walker.bind_param(p);
    }
    let _ = walker.infer_expr(body);
    let exprs = walker.collect_expr_types();
    let pats = walker.collect_pat_types();
    let exprs = exprs.into_iter().map(|(e, t)| (e, monomorphize(&t))).collect();
    let pats = pats.into_iter().map(|(p, t)| (p, monomorphize(&t))).collect();
    (exprs, pats)
}

/// Replaces every residual (unconstrained) type variable with `Int`, recursing
/// through compound types. A row tail is left intact (an open-row binder is
/// unsupported by generation and reported as not-runnable, not silently changed).
fn monomorphize(ty: &Ty) -> Ty {
    use std::sync::Arc;
    match ty {
        Ty::Var(_) => Ty::int(),
        Ty::App(f, a) => Ty::App(Arc::new(monomorphize(f)), Arc::new(monomorphize(a))),
        Ty::Arrow(f, t, e) => {
            Ty::Arrow(Arc::new(monomorphize(f)), Arc::new(monomorphize(t)), e.clone())
        }
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(monomorphize).collect()),
        Ty::Record(row) => Ty::Record(crate::ty::RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, monomorphize(t))).collect(),
            tail: row.tail,
        }),
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => ty.clone(),
    }
}

/// The error scheme (monomorphic error type).
#[must_use]
pub fn error_scheme() -> Scheme {
    Scheme::mono(Ty::Error)
}
