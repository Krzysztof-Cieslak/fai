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
use fai_types::{Con, Ty, contract_body_types};

use crate::ContractInfo;

/// The standard-library testing module the harness targets.
const TEST_MODULE: &str = "Test";

/// A contract that cannot be run, with a human-readable reason.
#[derive(Debug, Clone)]
pub struct NotRunnable {
    /// Why the contract cannot be generated/executed.
    pub reason: String,
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
        return Err(NotRunnable { reason: "contract not found".to_owned() });
    };
    let (binder_pats, body_expr) = match &item.kind {
        ItemKind::Example { body } => (Vec::new(), *body),
        ItemKind::Forall { binders, body } => (binders.clone(), *body),
        _ => return Err(NotRunnable { reason: "not a contract".to_owned() }),
    };

    let types = contract_body_types(db, file, info.ordinal);
    let lowered = lower_params_body(db, file, &binder_pats, body_expr, &types);

    let test = |name: &str| -> Result<DefId, NotRunnable> {
        let m = module_file(db, ModuleName(Symbol::intern(TEST_MODULE)))
            .ok_or_else(|| NotRunnable { reason: "the std `Test` module is missing".to_owned() })?;
        Ok(DefId::new(m.source(db), Symbol::intern(name)))
    };

    // The `Arbitrary` for the binder(s): one binder's directly, or a tuple of them.
    let binder_types: Vec<Ty> =
        binder_pats.iter().map(|&p| types.pat_type(p).cloned().unwrap_or(Ty::Error)).collect();
    let arb = build_arbitrary(&test, &binder_types)?;

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

    Ok(SynthContract { entry, entry_arity: 3, prop, prop_arity })
}

/// Builds the `Arbitrary` expression for a contract's binder list: the single
/// binder's arbitrary, or a `Test.tupleN` of each.
fn build_arbitrary(
    test: &dyn Fn(&str) -> Result<DefId, NotRunnable>,
    binder_types: &[Ty],
) -> Result<CExpr, NotRunnable> {
    match binder_types {
        [] => arb_for(test, &Ty::Unit), // example: an unused Unit arbitrary
        [ty] => arb_for(test, ty),
        tys => {
            let combinator = match tys.len() {
                2 => "tuple2",
                3 => "tuple3",
                4 => "tuple4",
                n => {
                    return Err(NotRunnable {
                        reason: format!("a `forall` with {n} binders is not supported yet (max 4)"),
                    });
                }
            };
            let args: Result<Vec<CExpr>, NotRunnable> =
                tys.iter().map(|ty| arb_for(test, ty)).collect();
            Ok(app(global(test(combinator)?), args?))
        }
    }
}

/// The `Arbitrary` expression for one (monomorphic) type, or a reason it has no
/// value generator.
fn arb_for(
    test: &dyn Fn(&str) -> Result<DefId, NotRunnable>,
    ty: &Ty,
) -> Result<CExpr, NotRunnable> {
    let unsupported =
        |what: String| NotRunnable { reason: format!("cannot generate values of type {what}") };
    match ty {
        Ty::Con(Con::Int) => Ok(global(test("int")?)),
        Ty::Con(Con::Bool) => Ok(global(test("bool")?)),
        Ty::Con(Con::Float) => Ok(global(test("float")?)),
        Ty::Con(Con::String) => Ok(global(test("string")?)),
        Ty::Unit => Ok(global(test("unit")?)),
        Ty::App(f, a) => match &**f {
            Ty::Con(Con::List) => Ok(app(global(test("list")?), vec![arb_for(test, a)?])),
            Ty::Adt(adt) if adt.name.as_str() == "Option" => {
                Ok(app(global(test("option")?), vec![arb_for(test, a)?]))
            }
            Ty::App(g, ok) => match &**g {
                Ty::Adt(adt) if adt.name.as_str() == "Result" => {
                    Ok(app(global(test("result")?), vec![arb_for(test, ok)?, arb_for(test, a)?]))
                }
                _ => Err(unsupported(render(ty))),
            },
            _ => Err(unsupported(render(ty))),
        },
        Ty::Tuple(elems) => {
            let combinator = match elems.len() {
                2 => "tuple2",
                3 => "tuple3",
                4 => "tuple4",
                _ => return Err(unsupported(render(ty))),
            };
            let args: Result<Vec<CExpr>, NotRunnable> =
                elems.iter().map(|e| arb_for(test, e)).collect();
            Ok(app(global(test(combinator)?), args?))
        }
        _ => Err(unsupported(render(ty))),
    }
}

fn render(ty: &Ty) -> String {
    format!("`{}`", fai_types::render(ty, &fai_types::VarNames::new()))
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
