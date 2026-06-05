//! Inference over a single definition or SCC.
//!
//! Members of an SCC are inferred together: each gets a fresh monomorphic type,
//! references *within* the SCC use those monomorphic types (so mutual recursion
//! is monomorphic), and references *outside* go through declared/inferred
//! schemes — never bodies. After solving, each member is generalized.

mod ctx;
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
    let (reified, vars) = cx.reify_with_vars(ty);
    Scheme::new(vars, reified)
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
        Some(crate::lower::lower_signature(db, file, module, *ty))
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
}

/// The body item (params + body expr) of a definition, located in `module`.
fn binding_body(
    module: &Module,
    name: Symbol,
) -> Option<(&[fai_syntax::ast::PatId], fai_syntax::ast::ExprId)> {
    module.items.iter().find_map(|it| match &it.kind {
        ItemKind::Binding { name: n, params, body, .. } if *n == name => {
            Some((params.as_slice(), *body))
        }
        _ => None,
    })
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

    // Fresh monomorphic type for each member. If a member has a declared
    // signature, instantiate it as the member's type (so the body is checked
    // against the signature and recursive calls use the declared type).
    let mut scc_types: FxHashMap<DefId, SolveTy> = FxHashMap::default();
    let mut declared: FxHashMap<DefId, Scheme> = FxHashMap::default();
    let mut declared_vars: FxHashMap<DefId, Vec<crate::ty::TyVarId>> = FxHashMap::default();
    for m in members {
        if let Some(scheme) = declared_scheme(db, file, m.name) {
            let (mono, vars) = cx.instantiate_tracked(&scheme);
            scc_types.insert(*m, mono);
            declared_vars.insert(*m, vars);
            declared.insert(*m, scheme);
        } else {
            scc_types.insert(*m, cx.fresh());
        }
    }

    // Infer each member's body, unifying with its monomorphic type.
    for m in members {
        let Some((params, body)) = binding_body(module, m.name) else {
            continue;
        };
        let member_ty = scc_types[m].clone();

        let env_scc = scc_types.clone();
        let mut env = SccEnv { db, scc_types: &env_scc, def_schemes, builtins };
        let mut walker = Walker::new(db, file, module, resolved, &mut cx, &mut env);

        // Parameters introduce fresh local types; the body's type is the result.
        let param_tys: Vec<SolveTy> = params.iter().map(|&p| walker.bind_param(p)).collect();
        let body_ty = walker.infer_expr(body);
        let fn_ty = SolveTy::arrows_solver(param_tys, body_ty);

        let unify = cx.unify(&fn_ty, &member_ty);
        // A failed unification (the body conflicts with the signature) is an
        // immediate mismatch.
        if declared.contains_key(m) && unify != UnifyResult::Ok {
            mismatches.push(*m);
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
    // to concrete types or collapsed together.
    for m in members {
        if declared.contains_key(m) && !mismatches.contains(m) {
            let over_general = declared_vars.get(m).is_some_and(|vars| !cx.all_distinct_free(vars));
            if over_general {
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
    SccInference { schemes: result, mismatches }
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

    let Some((params, body)) = binding_body(module, name) else {
        return Vec::new();
    };
    let params: Vec<fai_syntax::ast::PatId> = params.to_vec();

    let mut cx = InferCtx::new();
    let member_ty = match declared_scheme(db, file, name) {
        Some(scheme) => cx.instantiate(&scheme),
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

    let locals = {
        let mut env = SccEnv::new(db, &scc_types, def_schemes, builtins);
        let mut walker = Walker::new(db, file, module, &resolved, &mut cx, &mut env);
        let param_tys: Vec<SolveTy> = params.iter().map(|&p| walker.bind_param(p)).collect();
        let body_ty = walker.infer_expr(body);
        let fn_ty = SolveTy::arrows_solver(param_tys, body_ty);
        let _ = walker.cx.unify(&fn_ty, &member_ty);
        walker.collect_local_types()
    };

    locals.into_iter().filter_map(|(id, ty)| local_names.get(&id).map(|name| (*name, ty))).collect()
}

/// The error scheme (monomorphic error type).
#[must_use]
pub fn error_scheme() -> Scheme {
    Scheme::mono(Ty::Error)
}
