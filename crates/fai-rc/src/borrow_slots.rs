//! Borrows nonescaping array data slots when their owner's lifetime is proven.

use fai_core::ir::{CExpr, ExprKind as K, Prim};
use fai_db::Db;
use fai_resolve::LocalId;

use crate::is_boxed_data_ty;

pub(crate) fn rewrite(db: &dyn Db, body: &mut CExpr) {
    children(body, &mut |child| rewrite(db, child));
    let K::Let { local, value, body } = &mut body.kind else { return };
    let K::Prim { op: Prim::ArrayGet, args } = &value.kind else { return };
    let Some(K::Local(parent)) = args.first().map(|arg| &arg.kind) else { return };
    if !is_boxed_data_ty(&value.ty) || fai_core::niche_scheme(db, &value.ty).is_some() {
        return;
    }
    let mut state = State { parent_alive: true, slot_live: true };
    if inspect(body, *local, *parent, &mut state, &mut 1024) {
        let K::Prim { op, .. } = &mut value.kind else { unreachable!() };
        *op = Prim::ArrayPeek;
        remove_drops(body, *local);
    }
}

#[derive(Clone, Copy)]
struct State {
    parent_alive: bool,
    slot_live: bool,
}

/// Checks evaluation order, including both arms. A consumed owner invalidates
/// later slot inspections. A trailing drop chain has no intervening operation
/// that can observe the redundant reference's lifetime.
fn inspect(
    e: &CExpr,
    slot: LocalId,
    parent: LocalId,
    state: &mut State,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    if !state.parent_alive && state.slot_live && !matches!(e.kind, K::Drop { .. }) {
        return false;
    }
    match &e.kind {
        K::Local(local) => {
            if *local == slot {
                return false;
            }
            if *local == parent {
                state.parent_alive = false;
            }
            true
        }
        K::DataTag { base, .. } | K::DataField { base, .. } if matches!(base.kind, K::Local(local) if local == slot) => {
            state.parent_alive && state.slot_live
        }
        K::Dup { local, body } => *local != slot && inspect(body, slot, parent, state, budget),
        K::Drop { local, body } => {
            if *local == slot {
                state.slot_live = false;
            }
            if *local == parent {
                state.parent_alive = false;
            }
            inspect(body, slot, parent, state, budget)
        }
        K::Let { value, body, .. }
        | K::Reset { value, body, .. }
        | K::LetMany { value, body, .. } => {
            inspect(value, slot, parent, state, budget)
                && inspect(body, slot, parent, state, budget)
        }
        K::If { cond, then, els } => {
            if !inspect(cond, slot, parent, state, budget) {
                return false;
            }
            let mut left = *state;
            let mut right = *state;
            let ok = inspect(then, slot, parent, &mut left, budget)
                && inspect(els, slot, parent, &mut right, budget);
            state.parent_alive = left.parent_alive && right.parent_alive;
            state.slot_live = left.slot_live || right.slot_live;
            ok
        }
        K::MakeClosure { captures, .. } => {
            if captures.contains(&slot) {
                return false;
            }
            if captures.contains(&parent) {
                state.parent_alive = false;
            }
            true
        }
        K::Prim { op, args } => {
            let borrowed = args.first().is_some_and(|arg| op.borrows_operand(&arg.ty));
            args.iter().all(|arg| {
                if borrowed && matches!(arg.kind, K::Local(local) if local == parent) {
                    true
                } else {
                    inspect(arg, slot, parent, state, budget)
                }
            })
        }
        K::App { func, args, .. } => {
            inspect(func, slot, parent, state, budget)
                && args.iter().all(|arg| inspect(arg, slot, parent, state, budget))
        }
        K::MakeData { args, .. }
        | K::Foreign { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => {
            args.iter().all(|arg| inspect(arg, slot, parent, state, budget))
        }
        K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
            inspect(base, slot, parent, state, budget)
        }
        K::FreeReuse { body, .. } | K::HoleStart { body, .. } => {
            inspect(body, slot, parent, state, budget)
        }
        K::HoleFill { cell, .. } => inspect(cell, slot, parent, state, budget),
        K::Join { .. } => false,
        K::Lit(_) | K::Global(_) | K::Error => true,
    }
}

fn remove_drops(e: &mut CExpr, slot: LocalId) {
    if matches!(e.kind, K::Drop { local, .. } if local == slot) {
        let K::Drop { body, .. } = std::mem::replace(&mut e.kind, K::Error) else { unreachable!() };
        *e = *body;
    }
    children(e, &mut |child| remove_drops(child, slot));
}

fn children(e: &mut CExpr, f: &mut impl FnMut(&mut CExpr)) {
    match &mut e.kind {
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => args.iter_mut().for_each(f),
        K::App { func, args, .. } => {
            f(func);
            args.iter_mut().for_each(f);
        }
        K::Let { value, body, .. }
        | K::LetMany { value, body, .. }
        | K::Reset { value, body, .. } => {
            f(value);
            f(body);
        }
        K::If { cond, then, els } => {
            f(cond);
            f(then);
            f(els);
        }
        K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => f(base),
        K::FreeReuse { body, .. }
        | K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => f(body),
        K::HoleFill { cell, .. } => f(cell),
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{CoreFn, Lit, LoweredDef};
    use fai_resolve::DefId;
    use fai_syntax::Symbol;
    use fai_types::Ty;

    fn lowered(source: &str, name: &str) -> String {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let result = crate::rc(&db, db.source_file(id).unwrap(), Symbol::intern(name));
        let borrows = |def: DefId, count: usize| {
            let Some(file) = db.source_file(def.file) else { return vec![false; count] };
            let sig = crate::borrow_signature(&db, file, def.name);
            if sig.exploitable_at(count) { sig.0.clone() } else { vec![false; count] }
        };
        crate::check_rc(&result, &borrows).unwrap();
        fai_core::pretty_def(&result)
    }

    #[test]
    fn inspected_data_slot_is_borrowed_from_a_live_array() {
        let out = lowered(
            "module M\ntype Slot = | Empty | Full String\nlet probe i xs = match Array.unsafeGet i xs with | Empty -> Array.length xs + i | Full s -> String.length s + Array.length xs + i\n",
            "probe",
        );
        assert!(out.contains("arrayPeek"), "{out}");
    }

    #[test]
    fn an_escaping_data_slot_keeps_ownership() {
        let out = lowered(
            "module M\ntype Slot = | Empty | Full String\nfirst : Array Slot -> Slot\nlet first xs = Array.unsafeGet 0 xs\n",
            "first",
        );
        assert!(out.contains("arrayGet") && !out.contains("arrayPeek"), "{out}");
    }

    #[test]
    fn a_slot_used_after_array_update_keeps_ownership() {
        let out = lowered(
            "module M\ntype Slot = | Full String\nlet field slot = match slot with | Full s -> s\nlet update xs =\n  let old = Array.unsafeGet 0 xs\n  let changed = Array.unsafeSet 0 (Full \"new\") xs\n  (field old, changed)\n",
            "update",
        );
        assert!(out.contains("arrayGet"), "{out}");
    }

    #[test]
    fn an_unknown_element_representation_is_not_borrowed() {
        let out =
            lowered("module M\nlet probe xs = (Array.unsafeGet 0 xs, Array.length xs)\n", "probe");
        assert!(!out.contains("arrayPeek"), "{out}");
    }

    fn invalid_peek(body: CExpr) -> LoweredDef {
        let array = LocalId::from_index(0);
        let slot = LocalId::from_index(1);
        let value = CExpr::new(
            K::Prim {
                op: Prim::ArrayPeek,
                args: vec![
                    CExpr::new(K::Local(array), Ty::array(Ty::Tuple(vec![Ty::bool()]))),
                    CExpr::new(K::Lit(Lit::Int(0)), Ty::int()),
                ],
            },
            Ty::Tuple(vec![Ty::bool()]),
        );
        LoweredDef {
            def: DefId::new(fai_span::SourceId::new(0), Symbol::intern("invalid")),
            fns: vec![CoreFn {
                params: vec![array],
                captures: Vec::new(),
                body: CExpr::new(
                    K::Let { local: slot, value: Box::new(value), body: Box::new(body) },
                    Ty::int(),
                ),
            }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
            data_shapes: Vec::new(),
        }
    }

    #[test]
    fn verifier_rejects_peeking_after_the_array_dies() {
        let body = CExpr::new(
            K::Drop {
                local: LocalId::from_index(0),
                body: Box::new(CExpr::new(
                    K::DataTag {
                        base: Box::new(CExpr::new(K::Local(LocalId::from_index(1)), Ty::Error)),
                        niche: None,
                    },
                    Ty::int(),
                )),
            },
            Ty::int(),
        );
        let error = crate::check_rc(&invalid_peek(body), &|_, _| Vec::new()).unwrap_err();
        assert!(error.contains("borrow of released"), "{error}");
    }

    #[test]
    fn verifier_rejects_transferring_a_borrowed_slot() {
        let body = CExpr::new(K::Local(LocalId::from_index(1)), Ty::Error);
        let error = crate::check_rc(&invalid_peek(body), &|_, _| Vec::new()).unwrap_err();
        assert!(error.contains("consumption of borrowed slot"), "{error}");
    }
}
