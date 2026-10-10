//! Whether calling a function is **pure and total** — free of observable effects,
//! of aborts, and of non-termination.
//!
//! Shared by pipeline fusion, combinator reduction, and tail-call transformation.
//! Work may be reordered or skipped only when it has no effects, cannot abort,
//! and cannot diverge. Resource exhaustion from ordinary finite allocation is
//! outside this classification, but explicit checked operations remain barriers.
//!
//! In Fai the only unbounded construct is recursion (there are no loops), so an
//! **acyclic call graph implies termination**. A function is therefore pure and
//! total when its body — and every function it transitively calls — performs no
//! capability effect, no potentially aborting primitive, and **no recursion**.
//! Division by a non-zero literal is total. Recursion is excluded
//! conservatively: proving a recursive function terminates is undecidable, so a
//! function reachable from itself is treated as not-total. That falls out of the
//! salsa cycle below — a cycle's members resolve to `false`.
//!
//! The analysis is intentionally conservative: an indirect or curried call (whose
//! target is not a statically known top-level function) is assumed impure, as is
//! any unresolved or error body. Over-approximating "impure" only ever leaves a
//! function as ordinary recursion; it never admits an unsafe reorder.

use fai_db::{Db, SourceFile};
use fai_resolve::DefId;
use fai_syntax::Symbol;

use crate::core;
use crate::ir::{CExpr, ExprKind as K, Lit, Prim};

/// Whether calling `name` (fully applied) is pure and total.
///
/// Mutual recursion forms a salsa cycle resolved to `false` (a recursive function
/// is conservatively not-total). Because the result is a single `bool`, early
/// cutoff bounds the ripple: editing a callee's body re-runs a caller's analysis
/// only when the callee's purity actually flips.
pub fn is_pure_total(db: &dyn Db, file: SourceFile, name: Symbol) -> bool {
    let arity = core(db, file, name).entry().params.len();
    application_pure_total(db, file, name, arity)
}

/// Whether applying a definition to `arity` arguments is pure and total. A
/// partial application only builds a closure; over-application may invoke an
/// unknown returned function and is conservatively rejected. The boolean query
/// firewalls callers from body edits that leave reorder safety unchanged.
#[salsa::tracked(cycle_fn = pure_total_recover, cycle_initial = pure_total_initial)]
pub fn application_pure_total(db: &dyn Db, file: SourceFile, name: Symbol, arity: usize) -> bool {
    let lowered = core(db, file, name);
    match arity.cmp(&lowered.entry().params.len()) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => expr_pure_total(db, &lowered.entry().body),
        std::cmp::Ordering::Greater => false,
    }
}

/// A recursive function is conservatively not pure and total (its termination is
/// undecidable), so a cycle starts — and stays — `false` (`false` absorbs the `&&`
/// over callees, so the fixpoint converges immediately).
fn pure_total_initial(
    _db: &dyn Db,
    _id: salsa::Id,
    _file: SourceFile,
    _name: Symbol,
    _arity: usize,
) -> bool {
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
    _arity: usize,
) -> bool {
    value
}

/// Whether evaluating `e` is pure and total.
pub fn expr_pure_total(db: &dyn Db, e: &CExpr) -> bool {
    match &e.kind {
        K::Lit(_) | K::Local(_) => true,
        // A nullary global is evaluated when referenced; it is not merely a
        // static closure address and may itself trap, diverge, or perform effects.
        K::Global(def) => global_application_pure_total(db, *def, 0),
        // Building a closure is pure; applying it would be a (rejected) call.
        K::MakeClosure { .. } => true,
        K::Prim { op, args } => {
            !op_unsafe_to_reorder(*op, args) && args.iter().all(|a| expr_pure_total(db, a))
        }
        // A foreign call performs a host capability (and may abort), so it is never
        // pure-and-total — it must not be hoisted ahead of the recursion.
        K::Foreign { .. } => false,
        // A call is pure and total only when its target is a statically known
        // top-level function that is itself pure and total.
        K::App { func, args, .. } => {
            let target_ok = match &func.kind {
                K::Global(def) => global_application_pure_total(db, *def, args.len()),
                _ => false,
            };
            target_ok && args.iter().all(|a| expr_pure_total(db, a))
        }
        K::MakeData { args, .. } => args.iter().all(|a| expr_pure_total(db, a)),
        K::DataTag { base, .. } => expr_pure_total(db, base),
        K::DataField { base, .. } => expr_pure_total(db, base),
        K::If { cond, then, els } => {
            expr_pure_total(db, cond) && expr_pure_total(db, then) && expr_pure_total(db, els)
        }
        K::Let { value, body, .. } => expr_pure_total(db, value) && expr_pure_total(db, body),
        K::Spread { components } => components.iter().all(|a| expr_pure_total(db, a)),
        K::LetMany { value, body, .. } => expr_pure_total(db, value) && expr_pure_total(db, body),
        // A lowering error never reaches a runnable program; treat it as impure so
        // an erroneous callee never enables a reorder.
        K::Error => false,
        // The reference-counting and tail-call nodes do not exist in the pre-count
        // body this analysis runs on; handled for exhaustiveness.
        K::Reset { value, body, .. } => expr_pure_total(db, value) && expr_pure_total(db, body),
        K::FreeReuse { body, .. } => expr_pure_total(db, body),
        K::Dup { body, .. } | K::Drop { body, .. } => expr_pure_total(db, body),
        K::Join { body, .. } | K::HoleStart { body, .. } => expr_pure_total(db, body),
        K::Recur { .. } => false,
        K::HoleFill { cell, .. } => expr_pure_total(db, cell),
        K::HoleClose { base, .. } => expr_pure_total(db, base),
    }
}

/// The resolved-definition form of [`application_pure_total`].
pub fn global_application_pure_total(db: &dyn Db, def: DefId, arity: usize) -> bool {
    db.source_file(def.file).is_some_and(|file| application_pure_total(db, file, def.name, arity))
}

/// Whether a primitive is unsafe to hoist ahead of the recursion: an integer
/// division/remainder that could abort, a checked buffer operation, or an
/// unchecked conversion whose validity is not established. Host capabilities are
/// [`crate::ir::ExprKind::Foreign`] calls, treated as impure where this is consulted.
pub fn op_unsafe_to_reorder(op: Prim, args: &[CExpr]) -> bool {
    match op {
        Prim::IntDiv | Prim::IntRem => !divisor_is_nonzero_literal(args),
        Prim::Eq | Prim::Compare | Prim::Hash => {
            args.is_empty() || args.iter().any(|arg| !comparison_is_total(&arg.ty))
        }
        Prim::ArrayWithCapacity => {
            !matches!(args.first().map(|e| &e.kind), Some(K::Lit(Lit::Int(0))))
        }
        Prim::ArrayGet
        | Prim::ArrayPeek
        | Prim::DataPeek
        | Prim::ArraySet
        | Prim::ArrayUnique
        | Prim::ArrayRepeat
        | Prim::ArrayTake
        | Prim::ArrayPut
        | Prim::ArrayPush
        | Prim::BytesGet
        | Prim::CharFromCode
        | Prim::BytesToString => true,
        Prim::IntAdd
        | Prim::IntSub
        | Prim::IntMul
        | Prim::IntAnd
        | Prim::IntOr
        | Prim::IntXor
        | Prim::IntShl
        | Prim::IntShr
        | Prim::IntShrLogical
        | Prim::IntComplement
        | Prim::IntLt
        | Prim::IntLe
        | Prim::IntGt
        | Prim::IntGe
        | Prim::FloatAdd
        | Prim::FloatSub
        | Prim::FloatNeg
        | Prim::FloatMul
        | Prim::FloatDiv
        | Prim::FloatLt
        | Prim::FloatLe
        | Prim::FloatGt
        | Prim::FloatGe
        | Prim::StrConcat
        | Prim::IntToString
        | Prim::FloatToString
        | Prim::IntToFloat
        | Prim::FloatToInt
        | Prim::Sqrt
        | Prim::FloatFromBits
        | Prim::FloatToBits
        | Prim::CharToString
        | Prim::CharToCode
        | Prim::IsValidCharCode
        | Prim::StringLength
        | Prim::ToUpper
        | Prim::ToLower
        | Prim::Trim
        | Prim::StringContains
        | Prim::StringSplit
        | Prim::StringJoin
        | Prim::StringSubstring
        | Prim::StringTake
        | Prim::StringDrop
        | Prim::Not
        | Prim::RecordUpdate
        | Prim::ArrayLength
        | Prim::ArraySplit
        | Prim::ArrayJoin
        | Prim::BytesLength
        | Prim::BytesConcat
        | Prim::BytesSlice
        | Prim::BytesFromList
        | Prim::BytesToList
        | Prim::BytesFromString
        | Prim::BytesIsUtf8
        | Prim::ListReversePrefix => false,
    }
}

/// Structural operations can trap on a function hidden behind polymorphism or
/// an ADT. Only closed, recursively known comparable shapes establish totality.
fn comparison_is_total(ty: &fai_types::Ty) -> bool {
    use fai_types::{Con, RowEnd, Ty};
    match ty {
        Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char | Con::String | Con::Bytes)
        | Ty::Unit => true,
        Ty::App(head, elem) if matches!(head.as_ref(), Ty::Con(Con::List | Con::Array)) => {
            comparison_is_total(elem)
        }
        Ty::Tuple(fields) => fields.iter().all(comparison_is_total),
        Ty::Record(row) => {
            row.tail == RowEnd::Closed && row.fields.iter().all(|(_, ty)| comparison_is_total(ty))
        }
        _ => false,
    }
}

/// Whether the divisor (second operand) is a literal integer other than zero, so
/// the division cannot abort.
fn divisor_is_nonzero_literal(args: &[CExpr]) -> bool {
    matches!(args.get(1).map(|a| &a.kind), Some(K::Lit(Lit::Int(n))) if *n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: &str, name: &str, arity: usize) -> bool {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        application_pure_total(&db, db.source_file(id).unwrap(), Symbol::intern(name), arity)
    }

    #[test]
    fn division_by_nonzero_literal_is_total() {
        assert!(check("module M\nlet f x = x / 2\n", "f", 1));
    }

    #[test]
    fn checked_array_access_is_not_total() {
        assert!(!check("module M\nlet f xs = Array.unsafeGet 0 xs\n", "f", 1));
    }

    #[test]
    fn polymorphic_equality_is_not_total() {
        assert!(!check("module M\nlet same x = x = x\n", "same", 1));
    }

    #[test]
    fn monomorphic_integer_equality_is_total() {
        assert!(check("module M\nsame : Int -> Bool\nlet same x = x = x\n", "same", 1));
    }

    #[test]
    fn closed_scalar_record_comparison_is_total() {
        assert!(check(
            "module M\nsame : { x : Int, y : Float } -> Bool\nlet same r = r = r\n",
            "same",
            1
        ));
    }

    #[test]
    fn unknown_adt_fields_cannot_prove_comparison_totality() {
        assert!(!check(
            "module M\ntype Box = | Box (Int -> Int)\nsame : Box -> Bool\nlet same x = x = x\n",
            "same",
            1
        ));
    }

    #[test]
    fn hashing_a_type_variable_is_not_total() {
        let value = CExpr::new(
            K::Local(fai_resolve::LocalId::from_index(0)),
            fai_types::Ty::Var(fai_types::TyVarId(0)),
        );
        assert!(op_unsafe_to_reorder(Prim::Hash, &[value]));
    }

    #[test]
    fn checked_bytes_access_is_not_total() {
        assert!(!check("module M\nlet f bytes = Bytes.unsafeGet 0 bytes\n", "f", 1));
    }

    #[test]
    fn forcing_a_partial_caf_is_not_total() {
        assert!(!check("module M\nlet value = 1 / 0\nlet f x = value + x\n", "f", 1));
    }

    #[test]
    fn partial_application_does_not_run_a_recursive_body() {
        assert!(check("module M\nlet f x y = f x y\n", "f", 1));
    }

    #[test]
    fn saturated_recursive_application_is_not_total() {
        assert!(!check("module M\nlet f x y = f x y\n", "f", 2));
    }

    #[test]
    fn over_application_must_not_assume_a_returned_closure_is_total() {
        assert!(!check("module M\nlet f x = fun y -> x / y\n", "f", 2));
    }
}
