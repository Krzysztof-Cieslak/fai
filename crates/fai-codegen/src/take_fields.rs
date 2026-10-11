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

/// Every read of a marked field belongs to a group immediately followed by its
/// owner's release. Alternative branches may each contain such a group, but a
/// single read outside one keeps the field ordinary at every site.
pub(crate) fn collect(body: &CExpr) -> FxHashSet<(LocalId, u32)> {
    fn visit(
        e: &CExpr,
        reads: &mut FxHashMap<(LocalId, u32), bool>,
        grouped: bool,
        budget: &mut usize,
    ) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        if let K::DataField { base, index: FieldIndex::Const(index), .. } = &e.kind
            && let K::Local(root) = base.kind
        {
            *reads.entry((root, *index)).or_insert(true) &= grouped;
        }
        if let K::Let { value, body, .. } = &e.kind {
            visit(value, reads, group(e).is_some(), budget);
            visit(body, reads, false, budget);
            return;
        }
        let mut next = |child: &CExpr| visit(child, reads, false, budget);
        match &e.kind {
            K::LetMany { value, body, .. } | K::Reset { value, body, .. } => {
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
    let mut reads = FxHashMap::default();
    let mut budget = 1024;
    visit(body, &mut reads, false, &mut budget);
    reads
        .into_iter()
        .filter_map(|(field, grouped)| (budget > 0 && grouped).then_some(field))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{Lit, Prim};
    use fai_types::Ty;

    #[test]
    fn recursive_tree_branches_expose_their_dying_children() {
        use fai_db::{Db, FaiDatabase};
        use fai_syntax::Symbol;
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), "module M\ntype Tree = | Leaf Int | Add Tree Tree | Mul Tree Tree\nlet eval tree = match tree with | Leaf n -> n | Add a b -> eval a + eval b | Mul a b -> eval a * eval b\n".into());
        let lowered = fai_rc::rc(&db, db.source_file(id).unwrap(), Symbol::intern("eval"));
        let fields = collect(&lowered.entry().body);
        assert!(fields.iter().any(|(_, field)| *field == 0), "{fields:?}");
        assert!(fields.iter().any(|(_, field)| *field == 1), "{fields:?}");
    }

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
    fn alternative_death_groups_can_move_the_same_fields() {
        let body = CExpr::new(
            K::If {
                cond: Box::new(CExpr::new(K::Lit(Lit::Bool(true)), Ty::bool())),
                then: Box::new(field(1, 0, field(2, 1, released()))),
                els: Box::new(field(3, 0, field(4, 1, released()))),
            },
            Ty::Unit,
        );
        assert_eq!(collect(&body), FxHashSet::from_iter([(root(), 0), (root(), 1)]));
    }

    #[test]
    fn a_nonterminal_read_on_another_path_keeps_the_field_ordinary() {
        let body = CExpr::new(
            K::If {
                cond: Box::new(CExpr::new(K::Lit(Lit::Bool(true)), Ty::bool())),
                then: Box::new(field(1, 0, released())),
                els: Box::new(field(2, 0, value())),
            },
            Ty::Unit,
        );
        assert!(collect(&body).is_empty());
    }

    #[test]
    fn an_earlier_condition_read_is_not_moved() {
        let condition = match field(1, 0, released()).kind {
            K::Let { value, .. } => value,
            _ => unreachable!(),
        };
        let body = CExpr::new(
            K::If {
                cond: condition,
                then: Box::new(field(2, 0, released())),
                els: Box::new(field(3, 1, released())),
            },
            Ty::Unit,
        );
        assert_eq!(collect(&body), FxHashSet::from_iter([(root(), 1)]));
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
