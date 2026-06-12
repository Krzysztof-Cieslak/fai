//! Whether calling a function is **pure and total** — free of observable effects,
//! of aborts, and of non-termination.
//!
//! This drives the tail-call transform's reorder-safety check: when the recursive
//! call is not the last constructor argument, the later arguments are hoisted ahead
//! of the back-edge, which is observable only if they have effects, can abort, or
//! can diverge. A pure, total later argument is safe to hoist.
//!
//! In Fai the only unbounded construct is recursion (there are no loops), so an
//! **acyclic call graph implies termination**. A function is therefore pure and
//! total when its body — and every function it transitively calls — performs no
//! capability effect, no aborting integer division/remainder (a non-zero literal
//! divisor cannot abort), and **no recursion**. Recursion is excluded
//! conservatively: proving a recursive function terminates is undecidable, so a
//! function reachable from itself is treated as not-total. That falls out of the
//! salsa cycle below — a cycle's members resolve to `false`.
//!
//! The analysis is intentionally conservative: an indirect or curried call (whose
//! target is not a statically known top-level function) is assumed impure, as is
//! any unresolved or error body. Over-approximating "impure" only ever leaves a
//! function as ordinary recursion; it never admits an unsafe reorder.

use fai_core::core;
use fai_core::ir::{CExpr, ExprKind as K, Lit, Prim};
use fai_db::{Db, SourceFile};
use fai_syntax::Symbol;

/// Whether calling `name` (fully applied) is pure and total.
///
/// Mutual recursion forms a salsa cycle resolved to `false` (a recursive function
/// is conservatively not-total). Because the result is a single `bool`, early
/// cutoff bounds the ripple: editing a callee's body re-runs a caller's analysis
/// only when the callee's purity actually flips.
#[salsa::tracked(cycle_fn = pure_total_recover, cycle_initial = pure_total_initial)]
pub(crate) fn is_pure_total(db: &dyn Db, file: SourceFile, name: Symbol) -> bool {
    // Only the entry body runs when the function is called; a lifted lambda runs
    // only if applied, which appears as an (indirect) call and is rejected there.
    expr_pure_total(db, &core(db, file, name).entry().body)
}

/// A recursive function is conservatively not pure and total (its termination is
/// undecidable), so a cycle starts — and stays — `false` (`false` absorbs the `&&`
/// over callees, so the fixpoint converges immediately).
fn pure_total_initial(_db: &dyn Db, _id: salsa::Id, _file: SourceFile, _name: Symbol) -> bool {
    false
}

/// Cycle recovery: accept the converged value (`false` for any recursion cluster).
fn pure_total_recover(
    _db: &dyn Db,
    _cycle: &salsa::Cycle,
    _last: &bool,
    value: bool,
    _file: SourceFile,
    _name: Symbol,
) -> bool {
    value
}

/// Whether evaluating `e` is pure and total.
fn expr_pure_total(db: &dyn Db, e: &CExpr) -> bool {
    match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) => true,
        // Building a closure is pure; applying it would be a (rejected) call.
        K::MakeClosure { .. } => true,
        K::Prim { op, args } => {
            !op_unsafe_to_reorder(*op, args) && args.iter().all(|a| expr_pure_total(db, a))
        }
        // A call is pure and total only when its target is a statically known
        // top-level function that is itself pure and total.
        K::App { func, args, .. } => {
            let target_ok = match &func.kind {
                K::Global(def) => {
                    db.source_file(def.file).is_some_and(|f| is_pure_total(db, f, def.name))
                }
                _ => false,
            };
            target_ok && args.iter().all(|a| expr_pure_total(db, a))
        }
        K::MakeData { args, .. } => args.iter().all(|a| expr_pure_total(db, a)),
        K::DataTag(base) => expr_pure_total(db, base),
        K::DataField { base, .. } => expr_pure_total(db, base),
        K::If { cond, then, els } => {
            expr_pure_total(db, cond) && expr_pure_total(db, then) && expr_pure_total(db, els)
        }
        K::Let { value, body, .. } => expr_pure_total(db, value) && expr_pure_total(db, body),
        // A lowering error never reaches a runnable program; treat it as impure so
        // an erroneous callee never enables a reorder.
        K::Error => false,
        // The reference-counting and tail-call nodes do not exist in the pre-count
        // body this analysis runs on; handled for exhaustiveness.
        K::Reset { value, body, .. } => expr_pure_total(db, value) && expr_pure_total(db, body),
        K::FreeReuse { body, .. } => expr_pure_total(db, body),
        K::Dup { body, .. } | K::Drop { body, .. } => expr_pure_total(db, body),
        K::Join { body, .. } | K::HoleStart { body, .. } => expr_pure_total(db, body),
        K::Recur { args } => args.iter().all(|a| expr_pure_total(db, a)),
        K::HoleFill { cell, .. } => expr_pure_total(db, cell),
        K::HoleClose { base, .. } => expr_pure_total(db, base),
    }
}

/// Whether a primitive is unsafe to hoist ahead of the recursion: a capability
/// effect, or an integer division/remainder that could abort (a non-zero literal
/// divisor cannot, so it is safe).
pub(crate) fn op_unsafe_to_reorder(op: Prim, args: &[CExpr]) -> bool {
    match op {
        Prim::IntDiv | Prim::IntRem => !divisor_is_nonzero_literal(args),
        Prim::ConsoleWriteLine
        | Prim::ClockNow
        | Prim::RandomNextInt
        | Prim::FileRead
        | Prim::FileWrite
        | Prim::EnvGet
        | Prim::EnvArgs => true,
        _ => false,
    }
}

/// Whether the divisor (second operand) is a literal integer other than zero, so
/// the division cannot abort.
fn divisor_is_nonzero_literal(args: &[CExpr]) -> bool {
    matches!(args.get(1).map(|a| &a.kind), Some(K::Lit(Lit::Int(n))) if *n != 0)
}
