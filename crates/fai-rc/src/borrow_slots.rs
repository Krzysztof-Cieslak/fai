//! Borrows nonescaping array data slots when their owner's lifetime is proven.

use fai_core::ir::{CExpr, DataShape, ExprKind as K, FieldIndex, Lit, Prim};
use fai_db::Db;
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

use crate::is_boxed_data_ty;

pub(crate) fn rewrite(db: &dyn Db, body: &mut CExpr) {
    children(body, &mut |child| rewrite(db, child));
    let K::Let { local, value, body } = &mut body.kind else { return };
    let K::Prim { op: Prim::ArrayGet, args } = &value.kind else { return };
    let Some(K::Local(parent)) = args.first().map(|arg| &arg.kind) else { return };
    if !is_boxed_data_ty(&value.ty) || fai_core::niche_scheme(db, &value.ty).is_some() {
        return;
    }
    let mut state = State { parent_alive: true, slot_live: true, strict: false };
    if inspect(body, *local, *parent, &mut state, &mut 1024) {
        let K::Prim { op, .. } = &mut value.kind else { unreachable!() };
        *op = Prim::ArrayPeek;
        remove_drops(body, *local);
    }
}

/// Removes an acquire/release pair after borrowed-slot rewriting has fixed the
/// owner's lifetime. An intervening parent release keeps the projection intact.
pub(crate) fn remove_discarded_projections(e: &mut CExpr) {
    children(e, &mut remove_discarded_projections);
    let K::Let { local, value, body } = &e.kind else { return };
    if matches!(&value.kind, K::DataField { base, .. } if matches!(base.kind, K::Local(_)))
        && matches!(body.kind, K::Drop { local: dropped, .. } if dropped == *local)
    {
        let K::Let { body, .. } = std::mem::replace(&mut e.kind, K::Error) else { unreachable!() };
        let K::Drop { body, .. } = body.kind else { unreachable!() };
        *e = *body;
    }
}

/// Borrows statically uniform fields when every use fits within the containing
/// value's lifetime, following borrowed array slots back to their true owner.
pub(crate) fn borrow_fields(body: &mut CExpr, shapes: &[(LocalId, DataShape)]) {
    fn walk(
        e: &mut CExpr,
        shapes: &[(LocalId, DataShape)],
        owners: &mut FxHashMap<LocalId, LocalId>,
    ) {
        let K::Let { local, value, body } = &mut e.kind else {
            children(e, &mut |child| walk(child, shapes, owners));
            return;
        };
        walk(value, shapes, owners);
        if let K::Prim { op: Prim::ArrayPeek | Prim::DataPeek, args } = &value.kind
            && let Some(K::Local(parent)) = args.first().map(|arg| &arg.kind)
        {
            owners.insert(*local, owners.get(parent).copied().unwrap_or(*parent));
        }
        if let K::DataField { base, index: FieldIndex::Const(index), scalar: false, niche: None } =
            &value.kind
            && let K::Local(parent) = base.kind
            && !matches!(
                value.ty,
                Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char)
            )
            && let Ok(shape) = shapes.binary_search_by_key(&parent.index(), |(id, _)| id.index())
            && (*index >= 64 || shapes[shape].1.scalars & (1u64 << index) == 0)
        {
            let owner = owners.get(&parent).copied().unwrap_or(parent);
            let mut state = State { parent_alive: true, slot_live: true, strict: true };
            if inspect(body, *local, owner, &mut state, &mut 1024) {
                let args = vec![
                    (**base).clone(),
                    CExpr::new(K::Lit(Lit::Int(i64::from(*index))), Ty::int()),
                ];
                value.kind = K::Prim { op: Prim::DataPeek, args };
                remove_drops(body, *local);
                owners.insert(*local, owner);
            }
        }
        walk(body, shapes, owners);
    }
    walk(body, shapes, &mut FxHashMap::default());
}

#[derive(Clone, Copy)]
struct State {
    parent_alive: bool,
    slot_live: bool,
    /// A field can contain resources: retain its exact release order relative to
    /// siblings by requiring the owner to survive even its final drop.
    strict: bool,
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
    if !state.parent_alive && state.slot_live && (state.strict || !matches!(e.kind, K::Drop { .. }))
    {
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
                } else if borrowed && matches!(arg.kind, K::Local(local) if local == slot) {
                    state.parent_alive && state.slot_live
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

    #[test]
    fn discarded_field_of_a_borrowed_slot_needs_no_owner() {
        let out = lowered(
            "module M\ntype Slot 'a 'b = | Full 'a 'b\nlet probe i xs = match Array.unsafeGet i xs with | Full k v -> (k, xs)\n",
            "probe",
        );
        assert!(out.contains("arrayPeek") && out.contains("(field 0"), "{out}");
        assert!(!out.contains("(field 1"), "{out}");
    }

    #[test]
    fn used_field_of_a_borrowed_slot_keeps_its_owner() {
        let out = lowered(
            "module M\ntype Slot 'a 'b = | Full 'a 'b\nlet probe i xs = match Array.unsafeGet i xs with | Full k v -> (v, xs)\n",
            "probe",
        );
        assert!(out.contains("(field 1"), "{out}");
    }

    #[test]
    fn releasing_the_parent_before_the_field_keeps_release_order() {
        let out = lowered(
            "module M\ntype Slot 'a 'b = | Full 'a 'b\nlet probe slot = match slot with | Full k v -> Some k\n",
            "probe",
        );
        assert!(out.contains("(field 1"), "{out}");
    }

    #[test]
    fn compared_uniform_field_borrows_its_arrays_owner() {
        let out = lowered(
            "module M\ntype Slot 'a = | Full 'a String\nlet probe key i xs = match Array.unsafeGet i xs with | Full stored _ -> if stored = key then Array.length xs else 0\n",
            "probe",
        );
        assert!(out.contains("arrayPeek") && out.contains("dataPeek"), "{out}");
    }

    #[test]
    fn field_used_after_array_update_keeps_ownership() {
        let out = lowered(
            "module M\ntype Slot 'a = | Full 'a String\nlet probe key xs =\n  let Full stored _ = Array.unsafeGet 0 xs\n  let changed = Array.unsafeSet 0 (Full key \"new\") xs\n  (stored = key, changed)\n",
            "probe",
        );
        assert!(!out.contains("dataPeek"), "{out}");
    }

    #[test]
    fn captured_field_keeps_ownership() {
        let out = lowered(
            "module M\ntype Slot 'a = | Full 'a\nlet probe xs =\n  let Full stored = Array.unsafeGet 0 xs\n  (fun key -> stored = key, xs)\n",
            "probe",
        );
        assert!(!out.contains("dataPeek"), "{out}");
    }

    #[test]
    fn generic_tuple_fields_keep_their_possible_scalar_conversion() {
        let out = lowered(
            "module M\nlet probe key pair = match pair with | (stored, _) -> (stored = key, pair)\n",
            "probe",
        );
        assert!(!out.contains("dataPeek"), "{out}");
    }

    #[test]
    fn nested_borrowed_fields_follow_the_ultimate_array_owner() {
        let out = lowered(
            "module M\ntype Key 'a = | Key 'a\ntype Slot 'a = | Full (Key 'a)\nlet probe key xs =\n  let Full (Key stored) = Array.unsafeGet 0 xs\n  let changed = Array.unsafeSet 0 (Full (Key key)) xs\n  (stored = key, changed)\n",
            "probe",
        );
        assert!(out.contains("(field 0"), "{out}");
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

    #[test]
    fn verifier_tracks_borrowed_fields_through_their_borrowed_parent() {
        let field = LocalId::from_index(2);
        let peek = CExpr::new(
            K::Prim {
                op: Prim::DataPeek,
                args: vec![
                    CExpr::new(K::Local(LocalId::from_index(1)), Ty::Error),
                    CExpr::new(K::Lit(Lit::Int(0)), Ty::int()),
                ],
            },
            Ty::Con(Con::String),
        );
        let body = CExpr::new(
            K::Let {
                local: field,
                value: Box::new(peek),
                body: Box::new(CExpr::new(
                    K::Drop {
                        local: LocalId::from_index(0),
                        body: Box::new(CExpr::new(
                            K::Prim {
                                op: Prim::StringLength,
                                args: vec![CExpr::new(K::Local(field), Ty::Con(Con::String))],
                            },
                            Ty::int(),
                        )),
                    },
                    Ty::int(),
                )),
            },
            Ty::int(),
        );
        let error = crate::check_rc(&invalid_peek(body), &|_, _| Vec::new()).unwrap_err();
        assert!(error.contains("borrow of released"), "{error}");
    }
}
