//! Borrows uniform descendants of parameters retained by the caller.

use fai_core::ir::{CExpr, ExprKind as K, FieldIndex, Lit, Prim};
use fai_db::Db;
use fai_types::{Con, Ty};

use crate::Locals;

/// A borrowed parameter's owner outlives the call. Uniform descendants can stay
/// borrowed too; ordinary ownership insertion duplicates them only when consumed.
pub(crate) fn rewrite(db: &dyn Db, body: &mut CExpr, borrowed: &mut Locals) {
    if borrowed.is_empty() {
        return;
    }
    fn uniform(db: &dyn Db, ty: &Ty) -> bool {
        !matches!(ty, Ty::Error | Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char))
            && fai_core::niche_scheme(db, ty).is_none()
            && fai_core::ir::ffa_arity(ty).is_none()
    }
    fn visit(
        db: &dyn Db,
        e: &mut CExpr,
        borrowed: &mut Locals,
        shapes: &[(fai_resolve::LocalId, fai_core::ir::DataShape)],
    ) {
        let K::Let { local, value, body } = &mut e.kind else {
            crate::borrow_slots::children(e, &mut |child| visit(db, child, borrowed, shapes));
            return;
        };
        visit(db, value, borrowed, shapes);
        if uniform(db, &value.ty) {
            match &value.kind {
                K::Prim { op: Prim::ArrayGet, args }
                    if matches!(args.first().map(|arg| &arg.kind), Some(K::Local(parent)) if borrowed.contains(parent))
                        && crate::is_boxed_data_ty(&value.ty) =>
                {
                    let K::Prim { op, .. } = &mut value.kind else { unreachable!() };
                    *op = Prim::ArrayPeek;
                    borrowed.insert(*local);
                }
                K::DataField {
                    base,
                    index: FieldIndex::Const(index),
                    scalar: false,
                    niche: None,
                } if matches!(base.kind, K::Local(parent) if borrowed.contains(&parent)) => {
                    let K::Local(parent) = base.kind else { unreachable!() };
                    if shapes
                        .binary_search_by_key(&parent.index(), |(local, _)| local.index())
                        .ok()
                        .is_some_and(|position| {
                            *index >= 64 || shapes[position].1.scalars & (1u64 << index) == 0
                        })
                    {
                        value.kind = K::Prim {
                            op: Prim::DataPeek,
                            args: vec![
                                (**base).clone(),
                                CExpr::new(K::Lit(Lit::Int(i64::from(*index))), Ty::int()),
                            ],
                        };
                        borrowed.insert(*local);
                    }
                }
                _ => {}
            }
        }
        visit(db, body, borrowed, shapes);
    }
    let shapes = crate::data_shapes::collect(db, body);
    visit(db, body, borrowed, &shapes);
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_borrowed_array_field_is_acquired_only_for_the_result() {
        let source = "module M\ntype Entry 'a = | Entry 'a\nlet read xs = match Array.unsafeGet 0 xs with | Entry value -> Some value\n";
        assert_eq!(crate::tests::borrow_sig(source, "read"), vec![true]);
        crate::tests::check_program(source, "read").unwrap();
        let body = crate::tests::rc_checked(source, "read");
        assert!(
            body.contains("arrayPeek") && body.contains("dataPeek") && body.contains("dup "),
            "{body}"
        );
    }

    #[test]
    fn a_borrowed_descendant_can_be_duplicated_for_an_owned_local_binding() {
        let source = "module M\nlet f0 xs = match xs with | [] -> 0 | x :: rest -> x + f1 rest\nlet f1 xs = match xs with | [] -> 0 | _ :: _ -> 0\n";
        crate::tests::check_program(source, "f0").unwrap();
    }
}
