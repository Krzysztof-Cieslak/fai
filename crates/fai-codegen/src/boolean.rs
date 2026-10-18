//! Finds single-use Boolean values whose next operation immediately tests them.
//!
//! These bindings may pass control directly to their consumer. No operation may
//! move between producing and testing the value, so effects and releases retain
//! their order. Shared, captured and loop-carried values remain materialized.

use fai_core::ir::{CExpr, ExprKind as K, FieldIndex, Prim};
use fai_resolve::LocalId;
use rustc_hash::{FxHashMap, FxHashSet};

const NODE_LIMIT: usize = 8192;
/// Maximum total expression nodes retained for forwarding within one function.
pub(crate) const COPY_BUDGET: usize = 1024;

fn children<'a>(e: &'a CExpr, work: &mut Vec<&'a CExpr>) {
    match &e.kind {
        K::Let { value, body, .. }
        | K::LetMany { value, body, .. }
        | K::Reset { value, body, .. } => work.extend([&**value, &**body]),
        K::If { cond, then, els } => work.extend([&**cond, &**then, &**els]),
        K::App { func, args, .. } => {
            work.push(func);
            work.extend(args);
        }
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => work.extend(args),
        K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
            work.push(base);
        }
        K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::FreeReuse { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => work.push(body),
        K::HoleFill { cell, .. } => work.push(cell),
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

/// Exactly-once references, with no candidates if the bounded scan is incomplete.
pub(crate) fn single_uses(body: &CExpr) -> FxHashSet<LocalId> {
    let mut uses = FxHashMap::<LocalId, u8>::default();
    let mut work = vec![body];
    let mut budget = NODE_LIMIT;
    while let Some(e) = work.pop() {
        if budget == 0 {
            return FxHashSet::default();
        }
        budget -= 1;
        let mut note = |local| {
            let count = uses.entry(local).or_default();
            *count = (*count + 1).min(2);
        };
        match &e.kind {
            K::Local(local) | K::Dup { local, .. } | K::Drop { local, .. } => note(*local),
            K::MakeClosure { captures, .. } => captures.iter().copied().for_each(&mut note),
            K::App { reuse, .. } => reuse.iter().flatten().copied().for_each(&mut note),
            K::MakeData { reuse: Some(token), .. } | K::FreeReuse { token, .. } => note(*token),
            K::DataField { index: FieldIndex::Dyn { evidence, .. }, .. } => note(*evidence),
            K::HoleFill { hole, .. } | K::HoleClose { hole, .. } => note(*hole),
            K::Join { params, .. } => {
                for local in params {
                    note(*local);
                    note(*local);
                }
            }
            _ => {}
        }
        children(e, &mut work);
    }
    uses.into_iter().filter_map(|(local, uses)| (uses == 1).then_some(local)).collect()
}

/// Accept an immediate test or forwarding alias, without an intervening operation.
pub(crate) fn immediate_test(body: &CExpr, local: LocalId) -> bool {
    let tested = match &body.kind {
        K::Let { value, .. } => &**value,
        _ => body,
    };
    match &tested.kind {
        K::If { cond, .. } => matches!(cond.kind, K::Local(value) if value == local),
        K::Prim { op: Prim::Not, args } => {
            matches!(args.as_slice(), [arg] if matches!(arg.kind, K::Local(value) if value == local))
        }
        K::Local(value) => *value == local,
        _ => false,
    }
}

/// Bound both the retained expression copies and their traversal cost.
pub(crate) fn take_budget(value: &CExpr, budget: &mut usize) -> bool {
    let mut work = vec![value];
    let mut remaining = *budget;
    while let Some(e) = work.pop() {
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        children(e, &mut work);
    }
    *budget = remaining;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{ClosureAlloc, FnId, Lit};
    use fai_types::Ty;

    fn local(index: usize) -> CExpr {
        CExpr::new(K::Local(LocalId::from_index(index)), Ty::bool())
    }

    fn literal(value: bool) -> CExpr {
        CExpr::new(K::Lit(Lit::Bool(value)), Ty::bool())
    }

    fn choice(cond: CExpr, then: CExpr, els: CExpr) -> CExpr {
        CExpr::new(
            K::If { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) },
            Ty::bool(),
        )
    }

    #[test]
    fn a_single_immediate_condition_can_forward() {
        let body = choice(local(0), literal(true), literal(false));
        let id = LocalId::from_index(0);
        assert!(single_uses(&body).contains(&id));
        assert!(immediate_test(&body, id));
    }

    #[test]
    fn a_condition_used_in_its_branch_stays_materialized() {
        let body = choice(local(0), local(0), literal(false));
        assert!(!single_uses(&body).contains(&LocalId::from_index(0)));
    }

    #[test]
    fn a_condition_captured_by_a_branch_stays_materialized() {
        let id = LocalId::from_index(0);
        let closure = CExpr::new(
            K::MakeClosure { func: FnId(0), captures: vec![id], alloc: ClosureAlloc::Heap },
            Ty::Error,
        );
        let body = choice(local(0), closure, literal(false));
        assert!(!single_uses(&body).contains(&id));
    }

    #[test]
    fn an_intervening_binding_is_not_an_immediate_test() {
        let body = CExpr::new(
            K::Let {
                local: LocalId::from_index(1),
                value: Box::new(literal(false)),
                body: Box::new(choice(local(0), literal(true), literal(false))),
            },
            Ty::bool(),
        );
        assert!(!immediate_test(&body, LocalId::from_index(0)));
    }

    #[test]
    fn an_intervening_release_is_not_an_immediate_test() {
        let body = CExpr::new(
            K::Drop {
                local: LocalId::from_index(1),
                body: Box::new(choice(local(0), literal(true), literal(false))),
            },
            Ty::bool(),
        );
        assert!(!immediate_test(&body, LocalId::from_index(0)));
    }

    #[test]
    fn rejected_expression_copy_leaves_the_budget_intact() {
        let mut budget = 2;
        assert!(!take_budget(&choice(local(0), literal(true), literal(false)), &mut budget));
        assert_eq!(budget, 2);
    }

    #[test]
    fn an_incomplete_use_scan_cannot_mark_any_value_single_use() {
        let mut args = vec![literal(false); NODE_LIMIT];
        args.push(local(0));
        let body = CExpr::new(
            K::MakeData { tag: 0, args, reuse: None, scalars: 0, niche: None },
            Ty::Error,
        );
        assert!(single_uses(&body).is_empty());
    }
}
