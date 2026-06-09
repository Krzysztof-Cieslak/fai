//! Synthesizing a runnable harness from a contract.
//!
//! Each contract becomes two lowered definitions: a *property*
//! (`contract#k$prop`) — the contract body as a function of its binders (or a
//! tuple of them, projected back out) — and an *entry* (`contract#k`) that calls
//! the standard-library driver `Test.checkForall`/`Test.checkExample` with a
//! type-directed `Arbitrary` composed from `Test` combinators. The entry takes
//! `(seed, trials, maxSize)` so the runner controls them.

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, LoweredDef};
use fai_core::lower_params_body;
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId, ModuleName, module_file};
use fai_syntax::Symbol;
use fai_syntax::ast::ItemKind;
use fai_types::{Ty, contract_body_types};

use crate::ContractInfo;
use crate::arb::ArbBuilder;

/// The standard-library testing module the harness targets.
const TEST_MODULE: &str = "Test";

/// A contract that cannot be run, with a human-readable reason and the
/// diagnostic code to report it under.
#[derive(Debug, Clone)]
pub struct NotRunnable {
    /// Why the contract cannot be generated/executed.
    pub reason: String,
    /// The diagnostic code (defaults to [`crate::CONTRACT_NOT_RUNNABLE`]; a
    /// non-groundable type or an ambiguous generator use a more specific code).
    pub code: fai_diagnostics::DiagnosticCode,
}

impl NotRunnable {
    /// A generic "cannot be run" reason, reported as `FAI6002`.
    #[must_use]
    pub fn reason(reason: impl Into<String>) -> Self {
        NotRunnable { reason: reason.into(), code: crate::CONTRACT_NOT_RUNNABLE }
    }

    /// A "cannot be run" reason under a specific diagnostic code.
    #[must_use]
    pub fn coded(code: fai_diagnostics::DiagnosticCode, reason: impl Into<String>) -> Self {
        NotRunnable { reason: reason.into(), code }
    }
}

/// A contract lowered to a runnable harness: the entry to call plus the property
/// it drives, with their runtime arities (the entry is not an ordinary binding,
/// so the backend cannot derive these from a signature).
#[derive(Debug, Clone)]
pub struct SynthContract {
    /// The harness entry (`contract#k`): `Seed -> Int -> Size -> TestResult`.
    pub entry: LoweredDef,
    /// The entry's arity (always 3).
    pub entry_arity: usize,
    /// The property (`contract#k$prop`).
    pub prop: LoweredDef,
    /// The property's arity (0 for an `example`, else 1).
    pub prop_arity: usize,
    /// Synthesized `Arbitrary`/setter definitions for the binders' user types
    /// (records/ADTs), with their runtime arities. Empty for built-in binders.
    pub extra: Vec<(LoweredDef, usize)>,
}

/// Lowers `info` to a harness, or reports why it cannot be run.
pub fn synthesize(
    db: &dyn Db,
    file: SourceFile,
    info: &ContractInfo,
) -> Result<SynthContract, NotRunnable> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let source = file.source(db);
    let Some(item) = module.contract(info.ordinal) else {
        return Err(NotRunnable::reason("contract not found"));
    };
    let (binder_pats, body_expr) = match &item.kind {
        ItemKind::Example { body } => (Vec::new(), *body),
        ItemKind::Forall { binders, body } => (binders.clone(), *body),
        _ => return Err(NotRunnable::reason("not a contract")),
    };

    let types = contract_body_types(db, file, info.ordinal);
    let lowered = lower_params_body(db, file, &binder_pats, body_expr, &types);

    let test = |name: &str| -> Result<DefId, NotRunnable> {
        let m = module_file(db, ModuleName(Symbol::intern(TEST_MODULE)))
            .ok_or_else(|| NotRunnable::reason("the std `Test` module is missing"))?;
        Ok(DefId::new(m.source(db), Symbol::intern(name)))
    };

    // The `Arbitrary` for the binder(s): one binder's directly, or a tuple of
    // them. Records/ADTs synthesize supporting definitions, collected in `extra`.
    let binder_types: Vec<Ty> =
        binder_pats.iter().map(|&p| types.pat_type(p).cloned().unwrap_or(Ty::Error)).collect();
    let mut builder = ArbBuilder::new(db, file, format!("contract#{}", info.ordinal));
    // Discover user-defined `Arbitrary` overrides and rank the reachable types
    // (for the recursion base case) before composing the binders' generators.
    builder.prepare(&binder_types);
    let arb = build_arbitrary(&mut builder, &binder_types)?;
    let extra = std::mem::take(&mut builder.defs);

    let n = binder_pats.len();
    let prop_def = DefId::new(source, Symbol::intern(&format!("contract#{}$prop", info.ordinal)));
    let entry_def = DefId::new(source, Symbol::intern(&format!("contract#{}", info.ordinal)));

    // Build the property's entry function.
    let mut next = lowered.next_local;
    let (prop_params, prop_body) = if n <= 1 {
        // 0 binders (example) or 1 binder: the body uses the param locals directly.
        (lowered.param_locals.clone(), lowered.body)
    } else {
        // 2+ binders: take one tuple and project each binder out of it.
        let tuple = fresh(&mut next);
        let mut body = lowered.body;
        for (i, &local) in lowered.param_locals.iter().enumerate().rev() {
            let field = CExpr::new(
                K::DataField {
                    base: Box::new(local_expr(tuple)),
                    index: FieldIndex::Const(u32::try_from(i).unwrap_or(0)),
                },
                Ty::Error,
            );
            body = CExpr::new(
                K::Let { local, value: Box::new(field), body: Box::new(body) },
                Ty::Error,
            );
        }
        (vec![tuple], body)
    };
    let prop_arity = prop_params.len();
    let mut prop_fns = vec![CoreFn { params: prop_params, captures: Vec::new(), body: prop_body }];
    prop_fns.extend(lowered.lifted);
    let prop = LoweredDef { def: prop_def, fns: prop_fns, entry_borrowed: Vec::new() };

    // Build the harness entry `fun seed trials size -> Test.check… …`.
    let seed = fresh(&mut next);
    let trials = fresh(&mut next);
    let size = fresh(&mut next);
    let entry_body = if n == 0 {
        // example: `Test.checkExample <body>` (the property is a forced value).
        app(global(test("checkExample")?), vec![global(prop_def)])
    } else {
        // forall: `Test.checkForall seed trials size <arb> <prop>`.
        app(
            global(test("checkForall")?),
            vec![local_expr(seed), local_expr(trials), local_expr(size), arb, global(prop_def)],
        )
    };
    let entry = LoweredDef {
        def: entry_def,
        fns: vec![CoreFn {
            params: vec![seed, trials, size],
            captures: Vec::new(),
            body: entry_body,
        }],
        entry_borrowed: Vec::new(),
    };

    Ok(SynthContract { entry, entry_arity: 3, prop, prop_arity, extra })
}

/// Builds the `Arbitrary` for a contract's binder list: the single binder's
/// arbitrary, or a tuple of them (`>4` binders is unsupported, via the tuple
/// combinators). Records/ADTs synthesize supporting defs into the builder.
fn build_arbitrary(builder: &mut ArbBuilder, binder_types: &[Ty]) -> Result<CExpr, NotRunnable> {
    match binder_types {
        [] => builder.arb_for(&Ty::Unit), // example: an unused Unit arbitrary
        [ty] => builder.arb_for(ty),
        tys => builder.arb_for(&Ty::Tuple(tys.to_vec())),
    }
}

/// A reference to a top-level definition as a value.
fn global(def: DefId) -> CExpr {
    CExpr::new(K::Global(def), Ty::Error)
}

/// A use of a local.
fn local_expr(local: LocalId) -> CExpr {
    CExpr::new(K::Local(local), Ty::Error)
}

/// A (possibly over-saturated) application, routed through `apply_n`.
fn app(func: CExpr, args: Vec<CExpr>) -> CExpr {
    CExpr::new(K::App { func: Box::new(func), args }, Ty::Error)
}

/// Allocates a fresh local slot.
fn fresh(next: &mut usize) -> LocalId {
    let id = LocalId::from_index(*next);
    *next += 1;
    id
}
