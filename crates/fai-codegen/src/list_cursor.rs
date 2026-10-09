//! Proves scalar-only, allocation-free list scans can retain one root owner.

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, Prim};
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// Proven descendant cursors of one primitive-element input list.
pub(crate) struct Plan {
    /// The input parameter whose owner outlives the complete scan.
    pub(crate) root: LocalId,
    /// Root, tail projections and representation-preserving cursor aliases.
    pub(crate) cursors: FxHashSet<LocalId>,
}

fn scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char))
}

fn scalar_list(ty: &Ty) -> bool {
    matches!(ty, Ty::App(head, element) if matches!(head.as_ref(), Ty::Con(Con::List)) && scalar(element))
}

/// Recognizes an allocation-free numeric loop. Primitive elements cannot own
/// resource handles, so retaining the root changes no observable finalization.
pub(crate) fn plan(function: &CoreFn, types: &FxHashMap<usize, Ty>) -> Option<Plan> {
    let K::Join { params, body } = &function.body.kind else { return None };
    if params != &function.params || !scalar(&body.ty) {
        return None;
    }
    let roots: Vec<_> = params
        .iter()
        .copied()
        .filter(|local| types.get(&local.index()).is_some_and(scalar_list))
        .collect();
    let [root] = roots.as_slice() else { return None };
    if params.iter().any(|local| local != root && !types.get(&local.index()).is_some_and(scalar)) {
        return None;
    }
    let mut cursors = FxHashSet::from_iter([*root]);
    if !collect(body, &mut cursors, &mut 1024) || cursors.len() < 2 {
        return None;
    }
    let position = params.iter().position(|local| local == root)?;
    if !valid(body, &cursors, position, false) {
        return None;
    }
    Some(Plan { root: *root, cursors })
}

fn cursor_source(value: &CExpr, cursors: &FxHashSet<LocalId>) -> bool {
    match &value.kind {
        K::Local(local) => cursors.contains(local),
        K::DataField { base, index: FieldIndex::Const(1), scalar: false, niche: None } => {
            matches!(base.kind, K::Local(local) if cursors.contains(&local))
        }
        K::Dup { body, .. } | K::Drop { body, .. } => cursor_source(body, cursors),
        _ => false,
    }
}

fn collect(e: &CExpr, cursors: &mut FxHashSet<LocalId>, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &e.kind {
        K::Let { local, value, body } => {
            if !collect(value, cursors, budget) {
                return false;
            }
            if cursor_source(value, cursors) {
                cursors.insert(*local);
            }
            collect(body, cursors, budget)
        }
        K::If { cond, then, els } => {
            collect(cond, cursors, budget)
                && collect(then, cursors, budget)
                && collect(els, cursors, budget)
        }
        K::Prim { args, .. } | K::Recur { args } => {
            args.iter().all(|e| collect(e, cursors, budget))
        }
        K::DataTag { base, .. } | K::DataField { base, .. } => collect(base, cursors, budget),
        K::Dup { body, .. } | K::Drop { body, .. } => collect(body, cursors, budget),
        K::Lit(_) | K::Local(_) | K::Error => true,
        _ => false,
    }
}

fn valid(e: &CExpr, cursors: &FxHashSet<LocalId>, position: usize, cursor_value: bool) -> bool {
    let ordinary = |e: &CExpr| valid(e, cursors, position, false);
    match &e.kind {
        K::Local(local) => {
            if cursor_value {
                cursors.contains(local)
            } else {
                !cursors.contains(local)
            }
        }
        K::Lit(_) | K::Error => true,
        K::Let { local, value, body } => {
            valid(value, cursors, position, cursors.contains(local)) && ordinary(body)
        }
        K::If { cond, then, els } => ordinary(cond) && ordinary(then) && ordinary(els),
        K::DataTag { base, niche: None } => {
            matches!(base.kind, K::Local(local) if cursors.contains(&local))
        }
        K::DataField { base, index: FieldIndex::Const(index), niche: None, .. } => {
            matches!(base.kind, K::Local(local) if cursors.contains(&local))
                && (*index == 0 || (*index == 1 && cursor_value))
        }
        K::Dup { body, .. } | K::Drop { body, .. } => valid(body, cursors, position, cursor_value),
        K::Recur { args } => args.iter().enumerate().all(|(index, arg)| {
            if index == position {
                matches!(arg.kind, K::Local(local) if cursors.contains(&local))
            } else {
                ordinary(arg)
            }
        }),
        K::Prim { op, args } => numeric(*op) && args.iter().all(ordinary),
        _ => false,
    }
}

fn numeric(op: Prim) -> bool {
    matches!(
        op,
        Prim::IntAdd
            | Prim::IntSub
            | Prim::IntMul
            | Prim::IntDiv
            | Prim::IntRem
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
            | Prim::FloatMul
            | Prim::FloatDiv
            | Prim::FloatNeg
            | Prim::FloatLt
            | Prim::FloatLe
            | Prim::FloatGt
            | Prim::FloatGe
            | Prim::Eq
            | Prim::Compare
            | Prim::IntToFloat
            | Prim::FloatToInt
            | Prim::Sqrt
            | Prim::FloatFromBits
            | Prim::FloatToBits
            | Prim::CharToCode
            | Prim::CharFromCode
            | Prim::IsValidCharCode
            | Prim::Not
    )
}
