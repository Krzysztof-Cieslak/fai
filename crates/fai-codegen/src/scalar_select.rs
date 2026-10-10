//! Bounds the work that may be evaluated eagerly for an integer value select.

use fai_core::ir::{CExpr, ExprKind as K, Lit, Prim};
use fai_resolve::LocalId;
use fai_types::Ty;

/// Every accepted node is total, allocation-free and produces a raw Int.
/// Reference-count nodes may only name raw Ints, where they are no-ops.
pub(crate) fn integer_arm(
    expression: &CExpr,
    raw: &impl Fn(LocalId) -> bool,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &expression.kind {
        K::Local(local) => raw(*local),
        K::Lit(Lit::Int(_)) => expression.ty == Ty::int(),
        K::Let { local, value, body } => {
            raw(*local) && integer_arm(value, raw, budget) && integer_arm(body, raw, budget)
        }
        K::Dup { local, body } | K::Drop { local, body } => {
            raw(*local) && integer_arm(body, raw, budget)
        }
        K::Prim {
            op:
                Prim::IntAdd
                | Prim::IntSub
                | Prim::IntMul
                | Prim::IntAnd
                | Prim::IntOr
                | Prim::IntXor
                | Prim::IntComplement
                | Prim::IntShl
                | Prim::IntShr
                | Prim::IntShrLogical,
            args,
        } => args.iter().all(|arg| integer_arm(arg, raw, budget)),
        K::Prim { op: Prim::IntDiv | Prim::IntRem, args } if matches!(args.as_slice(), [_, CExpr { kind: K::Lit(Lit::Int(divisor)), .. }] if *divisor > 0 && (*divisor as u64).is_power_of_two()) => {
            args.iter().all(|arg| integer_arm(arg, raw, budget))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(value: i64) -> CExpr {
        CExpr::new(K::Lit(Lit::Int(value)), Ty::int())
    }

    #[test]
    fn division_by_a_positive_power_of_two_is_total() {
        let expression =
            CExpr::new(K::Prim { op: Prim::IntDiv, args: vec![int(i64::MIN), int(2)] }, Ty::int());
        assert!(integer_arm(&expression, &|_| true, &mut 12));
    }

    #[test]
    fn a_zero_divisor_cannot_be_evaluated_speculatively() {
        let expression =
            CExpr::new(K::Prim { op: Prim::IntDiv, args: vec![int(1), int(0)] }, Ty::int());
        assert!(!integer_arm(&expression, &|_| true, &mut 12));
    }

    #[test]
    fn an_unknown_representation_keeps_the_branch() {
        let expression = CExpr::new(K::Local(LocalId::from_index(0)), Ty::int());
        assert!(!integer_arm(&expression, &|_| false, &mut 12));
    }

    #[test]
    fn the_combined_work_budget_is_enforced() {
        let expression =
            CExpr::new(K::Prim { op: Prim::IntAdd, args: vec![int(1), int(2)] }, Ty::int());
        assert!(!integer_arm(&expression, &|_| true, &mut 2));
    }
}
