//! Tail recursion modulo wrapping Int addition, preserving operand evaluation.

use super::*;
use fai_core::ir::Prim;
use fai_types::Con;

pub(super) fn flatten(
    body: &CExpr,
    params: &[LocalId],
    def: DefId,
    next: &mut usize,
) -> Option<CExpr> {
    if body.ty != Ty::int() || !has_add_tail(body, def, params.len()) {
        return None;
    }
    let accumulator = fresh(next);
    let rewritten = rewrite(body, accumulator, def, params.len());
    let mut loop_params = params.to_vec();
    loop_params.push(accumulator);
    let loop_body =
        CExpr::new(K::Join { params: loop_params, body: Box::new(rewritten) }, Ty::int());
    Some(CExpr::new(
        K::Let {
            local: accumulator,
            value: Box::new(CExpr::new(K::Lit(Lit::Int(0)), Ty::int())),
            body: Box::new(loop_body),
        },
        Ty::int(),
    ))
}

fn contribution(body: &CExpr, result: LocalId) -> Option<&CExpr> {
    let K::Prim { op: Prim::IntAdd, args } = &body.kind else { return None };
    let [left, right] = args.as_slice() else { return None };
    let other = if is_local(left, result) {
        right
    } else if is_local(right, result) {
        left
    } else {
        return None;
    };
    if other.ty != Ty::Con(Con::Int) {
        return None;
    }
    match other.kind {
        K::Local(local) if local != result => Some(other),
        K::Lit(Lit::Int(_)) => Some(other),
        _ => None,
    }
}

fn has_add_tail(body: &CExpr, def: DefId, arity: usize) -> bool {
    match &body.kind {
        K::If { then, els, .. } => has_add_tail(then, def, arity) || has_add_tail(els, def, arity),
        K::Let { local, value, body } => {
            (self_call_args(value, def, arity).is_some() && contribution(body, *local).is_some())
                || has_add_tail(body, def, arity)
        }
        K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Reset { body, .. }
        | K::FreeReuse { body, .. } => has_add_tail(body, def, arity),
        _ => false,
    }
}

fn accumulator(local: LocalId) -> CExpr {
    CExpr::new(K::Local(local), Ty::int())
}

fn add(left: CExpr, right: CExpr) -> CExpr {
    CExpr::new(K::Prim { op: Prim::IntAdd, args: vec![left, right] }, Ty::int())
}

fn recur(args: &[CExpr], value: CExpr) -> CExpr {
    let mut args = args.to_vec();
    args.push(value);
    CExpr::new(K::Recur { args }, Ty::int())
}

fn rewrite(e: &CExpr, acc: LocalId, def: DefId, arity: usize) -> CExpr {
    let kind = match &e.kind {
        K::If { cond, then, els } => K::If {
            cond: cond.clone(),
            then: Box::new(rewrite(then, acc, def, arity)),
            els: Box::new(rewrite(els, acc, def, arity)),
        },
        K::Let { local, value, body } => {
            if let Some(args) = self_call_args(value, def, arity)
                && let Some(other) = contribution(body, *local)
            {
                return recur(args, add(accumulator(acc), other.clone()));
            }
            K::Let {
                local: *local,
                value: value.clone(),
                body: Box::new(rewrite(body, acc, def, arity)),
            }
        }
        K::Dup { local, body } => {
            K::Dup { local: *local, body: Box::new(rewrite(body, acc, def, arity)) }
        }
        K::Drop { local, body } => {
            K::Drop { local: *local, body: Box::new(rewrite(body, acc, def, arity)) }
        }
        K::Reset { value, token, body } => K::Reset {
            value: value.clone(),
            token: *token,
            body: Box::new(rewrite(body, acc, def, arity)),
        },
        K::FreeReuse { token, body } => {
            K::FreeReuse { token: *token, body: Box::new(rewrite(body, acc, def, arity)) }
        }
        _ => {
            if let Some(args) = self_call_args(e, def, arity) {
                return recur(args, accumulator(acc));
            }
            return add(accumulator(acc), e.clone());
        }
    };
    CExpr::new(kind, Ty::int())
}
