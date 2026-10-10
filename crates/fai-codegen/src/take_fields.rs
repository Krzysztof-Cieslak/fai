//! Identifies consecutive field extractions immediately followed by owner death.

use fai_core::ir::{CExpr, ExprKind as K, FieldIndex};
use fai_resolve::LocalId;
use rustc_hash::{FxHashMap, FxHashSet};

fn group(mut e: &CExpr) -> Option<(LocalId, Vec<u32>)> {
    let K::Let { value, .. } = &e.kind else { return None };
    let K::DataField { base, niche: None, .. } = &value.kind else { return None };
    let K::Local(root) = base.kind else { return None };
    let mut fields = Vec::new();
    while let K::Let { value, body, .. } = &e.kind {
        let K::DataField { base, index: FieldIndex::Const(index), niche: None, .. } = &value.kind
        else {
            return None;
        };
        if !matches!(base.kind, K::Local(local) if local == root)
            || fields.contains(index)
            || fields.len() == 8
        {
            return None;
        }
        fields.push(*index);
        e = body;
    }
    match &e.kind {
        K::Drop { local, .. } if *local == root => Some((root, fields)),
        K::Reset { value, .. } if matches!(value.kind, K::Local(local) if local == root) => {
            Some((root, fields))
        }
        _ => None,
    }
}

/// The marked fields are never observed through their owner after extraction.
pub(crate) fn collect(body: &CExpr) -> FxHashSet<(LocalId, u32)> {
    fn visit(
        e: &CExpr,
        result: &mut FxHashSet<(LocalId, u32)>,
        reads: &mut FxHashMap<(LocalId, u32), usize>,
        budget: &mut usize,
    ) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        if let K::DataField { base, index: FieldIndex::Const(index), .. } = &e.kind
            && let K::Local(root) = base.kind
        {
            *reads.entry((root, *index)).or_default() += 1;
        }
        if let Some((root, fields)) = group(e) {
            result.extend(fields.into_iter().map(|index| (root, index)));
        }
        let mut next = |child: &CExpr| visit(child, result, reads, budget);
        match &e.kind {
            K::Let { value, body, .. }
            | K::LetMany { value, body, .. }
            | K::Reset { value, body, .. } => {
                next(value);
                next(body);
            }
            K::If { cond, then, els } => {
                next(cond);
                next(then);
                next(els);
            }
            K::Prim { args, .. }
            | K::Foreign { args, .. }
            | K::MakeData { args, .. }
            | K::Recur { args }
            | K::Spread { components: args } => args.iter().for_each(next),
            K::App { func, args, .. } => {
                next(func);
                args.iter().for_each(next);
            }
            K::DataField { base, .. } | K::DataTag { base, .. } | K::HoleClose { base, .. } => {
                next(base)
            }
            K::Dup { body, .. }
            | K::Drop { body, .. }
            | K::FreeReuse { body, .. }
            | K::Join { body, .. }
            | K::HoleStart { body, .. } => next(body),
            K::HoleFill { cell, .. } => next(cell),
            _ => {}
        }
    }
    let mut result = FxHashSet::default();
    let mut reads = FxHashMap::default();
    let mut budget = 1024;
    visit(body, &mut result, &mut reads, &mut budget);
    result.retain(|field| budget > 0 && reads.get(field) == Some(&1));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{Lit, Prim};
    use fai_types::Ty;

    fn root() -> LocalId {
        LocalId::from_index(0)
    }
    fn value() -> CExpr {
        CExpr::new(K::Local(root()), Ty::Tuple(vec![Ty::int(), Ty::int()]))
    }
    fn field(local: usize, index: u32, body: CExpr) -> CExpr {
        CExpr::new(
            K::Let {
                local: LocalId::from_index(local),
                value: Box::new(CExpr::new(
                    K::DataField {
                        base: Box::new(value()),
                        index: FieldIndex::Const(index),
                        scalar: false,
                        niche: None,
                    },
                    Ty::int(),
                )),
                body: Box::new(body),
            },
            Ty::Unit,
        )
    }
    fn released() -> CExpr {
        CExpr::new(
            K::Drop { local: root(), body: Box::new(CExpr::new(K::Lit(Lit::Unit), Ty::Unit)) },
            Ty::Unit,
        )
    }

    #[test]
    fn distinct_fields_before_owner_death_can_move() {
        let body = field(1, 0, field(2, 1, released()));
        assert_eq!(collect(&body), FxHashSet::from_iter([(root(), 0), (root(), 1)]));
    }

    #[test]
    fn repeated_field_reads_are_not_cleared_early() {
        let body = field(1, 0, field(2, 0, released()));
        assert!(collect(&body).is_empty());
    }

    #[test]
    fn returning_the_owner_keeps_its_contents() {
        let body = field(1, 0, value());
        assert!(collect(&body).is_empty());
    }

    #[test]
    fn intervening_computation_keeps_the_earlier_projection_ordinary() {
        let compute = CExpr::new(
            K::Let {
                local: LocalId::from_index(2),
                value: Box::new(CExpr::new(
                    K::Prim {
                        op: Prim::Not,
                        args: vec![CExpr::new(K::Lit(Lit::Bool(false)), Ty::bool())],
                    },
                    Ty::bool(),
                )),
                body: Box::new(released()),
            },
            Ty::Unit,
        );
        assert!(collect(&field(1, 0, compute)).is_empty());
    }
}
