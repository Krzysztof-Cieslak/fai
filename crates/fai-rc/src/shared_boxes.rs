//! Shares immutable Int boxes used by multiple straight-line uniform boundaries.

use fai_core::ir::{CExpr, ExprKind as K, Prim};
use fai_db::Db;
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

fn common(
    mut e: CExpr,
    mut seen: Vec<(CExpr, LocalId)>,
    mut aliases: FxHashMap<LocalId, LocalId>,
) -> CExpr {
    if let K::Local(local) = &mut e.kind {
        if let Some(prior) = aliases.get(local) {
            *local = *prior;
        }
        return e;
    }
    if let K::Let { local, value, body } = e.kind {
        let value = common(*value, seen.clone(), aliases.clone());
        if matches!(
            &value.kind,
            K::Prim {
                op: Prim::IntAdd
                    | Prim::IntSub
                    | Prim::IntMul
                    | Prim::IntAnd
                    | Prim::IntOr
                    | Prim::IntXor,
                ..
            }
        ) && value.ty == Ty::int()
        {
            if let Some((_, prior)) = seen.iter().find(|(other, _)| other == &value) {
                aliases.insert(local, *prior);
                return common(*body, seen, aliases);
            }
            seen.push((value.clone(), local));
        }
        let body = common(*body, seen, aliases);
        return CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, e.ty);
    }
    crate::borrow_slots::children(&mut e, &mut |child| {
        *child = common(child.clone(), seen.clone(), aliases.clone());
    });
    e
}

fn linear(e: &CExpr) -> bool {
    if matches!(e.kind, K::If { .. } | K::Join { .. } | K::Recur { .. } | K::MakeClosure { .. }) {
        return false;
    }
    let mut result = true;
    crate::reuse_sig::e_children(e, &mut |child| {
        result &= linear(child);
    });
    result
}

fn uniform_positions(db: &dyn Db, e: &CExpr) -> Vec<usize> {
    match &e.kind {
        K::MakeData { args, .. } => (0..args.len()).collect(),
        K::App { func, args, .. } => {
            let abi =
                if let K::Global(def) = func.kind { Some(fai_core::abi_of(db, def)) } else { None };
            let arity = args.len();
            (0..arity)
                .filter(|i| {
                    !abi.as_ref().is_some_and(|abi| {
                        abi.register_abi && arity >= abi.params.len() && abi.int_param(*i)
                    })
                })
                .collect()
        }
        K::Prim { op: Prim::ArrayPush | Prim::ArrayRepeat, args } => {
            (args.len() > 1).then_some(1).into_iter().collect()
        }
        K::Prim { op: Prim::ArraySet | Prim::ArrayPut | Prim::RecordUpdate, args } => {
            (args.len() > 2).then_some(2).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn uniform_args(db: &dyn Db, e: &mut CExpr, visit: &mut impl FnMut(&mut CExpr)) {
    let positions = uniform_positions(db, e);
    if let K::App { args, .. } | K::Prim { args, .. } | K::MakeData { args, .. } = &mut e.kind {
        for position in positions {
            visit(&mut args[position]);
        }
    }
}

fn uses(db: &dyn Db, e: &CExpr, local: LocalId) -> usize {
    let mut count = 0;
    if let K::App { args, .. } | K::Prim { args, .. } | K::MakeData { args, .. } = &e.kind {
        for position in uniform_positions(db, e) {
            let arg = &args[position];
            if arg.ty == Ty::Con(Con::Int) && matches!(arg.kind, K::Local(id) if id == local) {
                count += 1;
            }
        }
    }
    crate::reuse_sig::e_children(e, &mut |child| {
        count += uses(db, child, local);
    });
    count
}

fn candidates(db: &dyn Db, e: &CExpr, peers: &mut FxHashMap<LocalId, LocalId>, next: &mut usize) {
    if let K::Let { local, value, body } = &e.kind
        && value.ty == Ty::int()
        && linear(body)
        && uses(db, body, *local) >= 2
    {
        peers.insert(*local, LocalId::from_index(*next));
        *next += 1;
    }
    crate::reuse_sig::e_children(e, &mut |child| candidates(db, child, peers, next));
}

fn substitute(db: &dyn Db, e: &mut CExpr, peers: &FxHashMap<LocalId, LocalId>) {
    uniform_args(db, e, &mut |arg| {
        if arg.ty == Ty::int()
            && let K::Local(local) = &mut arg.kind
            && let Some(peer) = peers.get(local)
        {
            *local = *peer;
        }
    });
    crate::borrow_slots::children(e, &mut |child| substitute(db, child, peers));
}

fn bind(e: &mut CExpr, peers: &FxHashMap<LocalId, LocalId>) {
    crate::borrow_slots::children(e, &mut |child| bind(child, peers));
    if let K::Let { local, body, .. } = &mut e.kind
        && let Some(peer) = peers.get(local)
    {
        let value = CExpr::new(
            K::Prim { op: Prim::IntBox, args: vec![CExpr::new(K::Local(*local), Ty::int())] },
            Ty::int(),
        );
        let old = std::mem::replace(body, Box::new(CExpr::new(K::Error, Ty::Unit)));
        let ty = old.ty.clone();
        **body = CExpr::new(K::Let { local: *peer, value: Box::new(value), body: old }, ty);
    }
}

/// Introduces boxed peers only when a small straight-line region shares them.
pub(crate) fn rewrite(db: &dyn Db, body: CExpr, next: &mut usize) -> CExpr {
    if fai_core::helper_inline::node_count(&body) > 256 {
        return body;
    }
    fn has_external_locals(e: &CExpr) -> bool {
        if matches!(
            e.kind,
            K::MakeClosure { .. }
                | K::DataField { index: fai_core::ir::FieldIndex::Dyn { .. }, .. }
        ) {
            return true;
        }
        let mut found = false;
        crate::reuse_sig::e_children(e, &mut |child| {
            found |= has_external_locals(child);
        });
        found
    }
    if has_external_locals(&body) {
        return body;
    }
    let mut candidate = common(body.clone(), Vec::new(), FxHashMap::default());
    let mut peers = FxHashMap::default();
    candidates(db, &candidate, &mut peers, next);
    if peers.is_empty() {
        return body;
    }
    substitute(db, &mut candidate, &peers);
    bind(&mut candidate, &peers);
    candidate
}

#[cfg(test)]
mod tests {
    use crate::tests::{check_program, rc_checked};

    #[test]
    fn repeated_uniform_results_share_one_explicit_box() {
        let source = "module M\npublic pair : Int -> (Int * Int)\nlet pair x = (x + 1, x + 1)\n";
        check_program(source, "pair").unwrap();
        let body = rc_checked(source, "pair");
        assert_eq!(body.matches("intBox").count(), 1, "{body}");
        assert_eq!(body.matches("(+ ").count(), 1, "{body}");
    }

    #[test]
    fn branch_only_uses_keep_boxing_inside_the_branch() {
        let source = "module M\npublic pair : Bool -> Int -> (Int * Int)\nlet pair yes x =\n  let value = x + 1\n  if yes then (value, value) else (0, 0)\n";
        check_program(source, "pair").unwrap();
        let body = rc_checked(source, "pair");
        assert!(!body.contains("intBox"), "{body}");
    }
}
