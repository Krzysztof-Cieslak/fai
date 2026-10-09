//! Proves allocation-free searches can borrow descendants of one retained owner.

use fai_core::ir::{CExpr, CoreFn, DataShape, ExprKind as K, FieldIndex, Prim};
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// Proven descendant cursors of one resource-free input value.
pub(crate) struct Plan {
    /// The input parameter whose owner outlives the complete scan.
    pub(crate) root: LocalId,
    /// Root, descendant projections and representation-preserving cursor aliases.
    pub(crate) cursors: FxHashSet<LocalId>,
    /// Field reads that borrow a descendant rather than acquire an owner.
    pub(crate) projections: FxHashSet<(LocalId, u32)>,
}

fn scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char))
}

fn scalar_list(ty: &Ty) -> bool {
    matches!(ty, Ty::App(head, element) if matches!(head.as_ref(), Ty::Con(Con::List)) && scalar(element))
}

/// Recognizes an allocation-free search over data known to contain no resources.
/// A niche result may return a scalar payload without constructing a data cell.
pub(crate) fn plan(
    function: &CoreFn,
    types: &FxHashMap<usize, Ty>,
    shapes: &[(LocalId, DataShape)],
    niche_result: bool,
) -> Option<Plan> {
    let K::Join { params, body } = &function.body.kind else { return None };
    if params != &function.params || (!scalar(&body.ty) && !niche_result) {
        return None;
    }
    let mut candidates: FxHashSet<_> = types
        .iter()
        .filter(|(_, ty)| scalar_list(ty))
        .map(|(local, _)| LocalId::from_index(*local))
        .collect();
    candidates
        .extend(shapes.iter().filter(|(_, shape)| shape.resource_free).map(|(local, _)| *local));
    let roots: Vec<_> = params.iter().copied().filter(|local| candidates.contains(local)).collect();
    let [root] = roots.as_slice() else { return None };
    if params.iter().any(|local| local != root && !types.get(&local.index()).is_some_and(scalar)) {
        return None;
    }
    let mut cursors = FxHashSet::from_iter([*root]);
    let mut projections = FxHashSet::default();
    if !collect(body, &candidates, &mut cursors, &mut projections, &mut 1024) || cursors.len() < 2 {
        return None;
    }
    let position = params.iter().position(|local| local == root)?;
    if !valid(body, &cursors, position, false) {
        return None;
    }
    Some(Plan { root: *root, cursors, projections })
}

fn cursor_source(value: &CExpr, cursors: &FxHashSet<LocalId>) -> bool {
    match &value.kind {
        K::Local(local) => cursors.contains(local),
        K::DataField { base, index: FieldIndex::Const(_), scalar: false, niche: None } => {
            matches!(base.kind, K::Local(local) if cursors.contains(&local))
        }
        K::Dup { body, .. } | K::Drop { body, .. } => cursor_source(body, cursors),
        _ => false,
    }
}

fn note_projection(e: &CExpr, projections: &mut FxHashSet<(LocalId, u32)>) {
    match &e.kind {
        K::DataField { base, index: FieldIndex::Const(index), .. } => {
            if let K::Local(local) = base.kind {
                projections.insert((local, *index));
            }
        }
        K::Dup { body, .. } | K::Drop { body, .. } => note_projection(body, projections),
        _ => {}
    }
}

fn collect(
    e: &CExpr,
    candidates: &FxHashSet<LocalId>,
    cursors: &mut FxHashSet<LocalId>,
    projections: &mut FxHashSet<(LocalId, u32)>,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &e.kind {
        K::Let { local, value, body } => {
            if !collect(value, candidates, cursors, projections, budget) {
                return false;
            }
            if candidates.contains(local) && cursor_source(value, cursors) {
                note_projection(value, projections);
                cursors.insert(*local);
            }
            collect(body, candidates, cursors, projections, budget)
        }
        K::If { cond, then, els } => {
            collect(cond, candidates, cursors, projections, budget)
                && collect(then, candidates, cursors, projections, budget)
                && collect(els, candidates, cursors, projections, budget)
        }
        K::Prim { args, .. }
        | K::Recur { args }
        | K::MakeData { args, niche: Some(_), reuse: None, .. } => {
            args.iter().all(|e| collect(e, candidates, cursors, projections, budget))
        }
        K::DataTag { base, .. } | K::DataField { base, .. } => {
            collect(base, candidates, cursors, projections, budget)
        }
        K::Dup { body, .. } | K::Drop { body, .. } => {
            collect(body, candidates, cursors, projections, budget)
        }
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
        K::DataField { base, index: FieldIndex::Const(_), niche: None, .. } => {
            matches!(base.kind, K::Local(local) if cursors.contains(&local))
                && (cursor_value || scalar(&e.ty))
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
        K::MakeData { args, niche: Some(_), reuse: None, .. } => {
            args.len() <= 1 && args.iter().all(|arg| scalar(&arg.ty) && ordinary(arg))
        }
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
