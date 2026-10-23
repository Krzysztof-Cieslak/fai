//! Removes private data cells while keeping their fields' explicit ownership lifetimes.
//!
//! A projected field still acquires its own reference. The former parent's drop
//! releases its field owners in the same reverse-field order as the runtime.

use fai_core::ir::{CExpr, ExprKind as K, FieldIndex, Lit};
use fai_db::Db;
use fai_resolve::LocalId;
use fai_types::Ty;

const MAX_FIELDS: usize = 8;
const USE_BUDGET: usize = 512;

fn only_observed(e: &CExpr, parent: LocalId, fields: &[Ty], budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &e.kind {
        K::Local(local) if *local == parent => return false,
        K::Dup { local, .. } if *local == parent => return false,
        K::MakeClosure { captures, .. } if captures.contains(&parent) => return false,
        K::DataField { index: FieldIndex::Dyn { evidence, .. }, .. } if *evidence == parent => {
            return false;
        }
        K::DataTag { base, niche: None } if matches!(base.kind, K::Local(local) if local == parent) =>
        {
            return true;
        }
        K::DataField { base, index: FieldIndex::Const(index), niche: None, .. } if matches!(base.kind, K::Local(local) if local == parent) =>
        {
            return fields.get(*index as usize) == Some(&e.ty);
        }
        _ => {}
    }
    let mut valid = true;
    crate::reuse_sig::e_children(e, &mut |child| {
        if valid {
            valid = only_observed(child, parent, fields, budget);
        }
    });
    valid
}

fn shape(
    db: &dyn Db,
    value: &CExpr,
    found: &mut Option<(u32, Vec<Ty>)>,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &value.kind {
        K::MakeData { tag, args, reuse: None, niche: None, .. } => {
            if args.is_empty()
                || args.len() > MAX_FIELDS
                || args.iter().any(|arg| {
                    arg.ty == Ty::Error
                        || fai_core::niche_scheme(db, &arg.ty).is_some()
                        || fai_core::ir::ffa_arity(&arg.ty).is_some()
                })
            {
                return false;
            }
            let current = (*tag, args.iter().map(|arg| arg.ty.clone()).collect());
            if let Some(expected) = found {
                *expected == current
            } else {
                *found = Some(current);
                true
            }
        }
        K::If { then, els, .. } => shape(db, then, found, budget) && shape(db, els, found, budget),
        K::Let { body, .. }
        | K::LetMany { body, .. }
        | K::Dup { body, .. }
        | K::Drop { body, .. } => shape(db, body, found, budget),
        K::Error => true,
        _ => false,
    }
}

fn components(mut value: CExpr, ty: &Ty) -> CExpr {
    value.kind = match value.kind {
        K::MakeData { args, .. } => K::Spread { components: args },
        K::If { cond, then, els } => K::If {
            cond,
            then: Box::new(components(*then, ty)),
            els: Box::new(components(*els, ty)),
        },
        K::Let { local, value, body } => {
            K::Let { local, value, body: Box::new(components(*body, ty)) }
        }
        K::LetMany { locals, value, body } => {
            K::LetMany { locals, value, body: Box::new(components(*body, ty)) }
        }
        K::Dup { local, body } => K::Dup { local, body: Box::new(components(*body, ty)) },
        K::Drop { local, body } => K::Drop { local, body: Box::new(components(*body, ty)) },
        K::Error => K::Error,
        _ => unreachable!("validated local constructor tree"),
    };
    value.ty = ty.clone();
    value
}

fn replace(e: &mut CExpr, parent: LocalId, tag: u32, fields: &[CExpr]) {
    match &e.kind {
        K::DataTag { base, .. } if matches!(base.kind, K::Local(local) if local == parent) => {
            e.kind = K::Lit(Lit::Int(i64::from(tag)));
            return;
        }
        K::DataField { base, index: FieldIndex::Const(index), .. } if matches!(base.kind, K::Local(local) if local == parent) =>
        {
            let value = fields[*index as usize].clone();
            let K::Local(local) = value.kind else { unreachable!() };
            *e = crate::dup_(local, value);
            return;
        }
        K::Drop { local, .. } if *local == parent => {
            let K::Drop { body, .. } = std::mem::replace(&mut e.kind, K::Error) else {
                unreachable!()
            };
            *e = *body;
            replace(e, parent, tag, fields);
            for field in fields {
                let K::Local(local) = field.kind else { unreachable!() };
                let body = std::mem::replace(e, CExpr::new(K::Error, Ty::Error));
                *e = crate::drop_(local, body);
            }
            return;
        }
        _ => {}
    }
    crate::borrow_slots::children(e, &mut |child| replace(child, parent, tag, fields));
}

pub(crate) fn rewrite(db: &dyn Db, mut e: CExpr, next: &mut usize) -> (CExpr, crate::Locals) {
    if fai_db::allocation_control(db) == fai_db::AllocationControl::NoScalarReplacement {
        return (e, crate::Locals::default());
    }
    fn walk(
        db: &dyn Db,
        e: &mut CExpr,
        next: &mut usize,
        budget: &mut usize,
        owners: &mut crate::Locals,
    ) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        crate::borrow_slots::children(e, &mut |child| walk(db, child, next, budget, owners));
        let K::Let { local, value, body } = &e.kind else { return };
        let mut found = None;
        if !shape(db, value, &mut found, &mut 256) {
            return;
        }
        let Some((tag, types)) = found else { return };
        let mut use_budget = USE_BUDGET;
        if !only_observed(body, *local, &types, &mut use_budget) {
            return;
        }
        let K::Let { local, value, body } = std::mem::replace(&mut e.kind, K::Error) else {
            unreachable!()
        };
        let fields: Vec<_> =
            types.iter().map(|ty| CExpr::new(K::Local(crate::fresh(next)), ty.clone())).collect();
        *e = *body;
        replace(e, local, tag, &fields);
        let locals = fields
            .iter()
            .map(|field| {
                let K::Local(local) = field.kind else { unreachable!() };
                // This owner replaces a runtime child release, not a data-cell reset.
                // In particular an opaque field can be a native resource handle.
                owners.insert(local);
                local
            })
            .collect();
        let value = components(*value, &Ty::Tuple(types));
        let body = std::mem::replace(e, CExpr::new(K::Error, Ty::Error));
        let ty = body.ty.clone();
        *e = CExpr::new(K::LetMany { locals, value: Box::new(value), body: Box::new(body) }, ty);
    }
    let mut owners = crate::Locals::default();
    walk(db, &mut e, next, &mut 4096, &mut owners);
    (e, owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_replacement_control_retains_private_cells() {
        let source = "module M\nlet run x y observe =\n  let pair = (x, y)\n  let ignored = observe ()\n  match pair with | (_, last) -> last\n";
        let (mut db, file) = crate::tests::db_with(source);
        db.set_allocation_control(fai_db::AllocationControl::NoScalarReplacement);
        let lowered = crate::rc(&db, file, fai_syntax::Symbol::intern("run"));
        let text = fai_core::pretty_def(&lowered);
        assert!(text.contains("(data "), "{text}");
    }

    #[test]
    fn generic_private_tuple_loses_its_cell_without_losing_field_owners() {
        let source = "module M\nlet run x y observe =\n  let pair = (x, y)\n  let first = match pair with | (first, _) -> first\n  let ignored = observe first\n  match pair with | (_, last) -> last\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(!body.contains("(data "), "private tuple should be scalar-replaced:\n{body}");
        assert!(
            body.contains("dup ") && body.contains("drop "),
            "field ownership remains explicit:\n{body}"
        );
    }

    #[test]
    fn escaping_generic_tuple_keeps_its_cell() {
        let source = "module M\nlet run x y = (x, y)\n";
        crate::tests::check_program(source, "run").unwrap();
        assert!(crate::tests::rc_checked(source, "run").contains("(data "));
    }

    #[test]
    fn captured_generic_tuple_keeps_its_cell() {
        let source = "module M\nlet run x y =\n  let pair = (x, y)\n  fun _ -> pair\n";
        crate::tests::check_program(source, "run").unwrap();
        assert!(crate::tests::rc_checked(source, "run").contains("(data "));
    }

    #[test]
    fn shared_fields_keep_their_preconstruction_acquisitions() {
        let source = "module M\nlet run x y observe =\n  let pair = (x, y)\n  let ignored = observe x\n  match pair with | (_, last) -> last\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(!body.contains("(data "), "{body}");
        assert!(body.contains("dup "), "the original shared-field acquisition remains:\n{body}");
    }

    #[test]
    fn each_branch_keeps_a_balanced_field_lifetime() {
        let source = "module M\nlet run yes x y observe =\n  let pair = (x, y)\n  if yes then\n    let first = match pair with | (first, _) -> first\n    let ignored = observe first\n    match pair with | (_, last) -> last\n  else\n    match pair with | (_, last) -> last\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(!body.contains("(data "), "{body}");
    }

    #[test]
    fn branch_selected_generic_fields_share_one_continuation() {
        let source = "module M\nlet run yes x y observe =\n  let pair = if yes then (x, y) else (y, x)\n  let first = match pair with | (first, _) -> first\n  let ignored = observe first\n  match pair with | (_, last) -> last\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(body.contains("letmany"), "branch results should merge as components:\n{body}");
        assert!(!body.contains("(data "), "{body}");
        assert_eq!(
            body.matches("(app %").count(),
            1,
            "the callback continuation is not duplicated:\n{body}"
        );
    }

    #[test]
    fn distinct_constructor_tags_keep_their_original_representation() {
        let source = "module M\ntype Choice 'a = | A 'a | B 'a\nlet run yes value =\n  let choice = if yes then A value else B value\n  match choice with | A x -> x | B x -> x\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(
            body.contains("(data "),
            "distinct tags keep their ordinary representation:\n{body}"
        );
    }

    #[test]
    fn native_resource_field_owners_are_never_reset_for_reuse() {
        let source = "module M\nrun : Channel Int -> (Unit -> Unit) -> List Int\nlet run channel observe =\n  let pair = (channel, 1)\n  let ignored = observe ()\n  let answer = match pair with | (_, answer) -> answer\n  [answer]\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(
            !body.contains("reset "),
            "a former resource field keeps its ordinary release:\n{body}"
        );
    }

    #[test]
    fn wider_private_tuples_keep_their_bounded_fallback() {
        let source = "module M\nlet run x observe =\n  let values = (x, x, x, x, x, x, x, x, x)\n  let ignored = observe ()\n  match values with | (first, _, _, _, _, _, _, _, _) -> first\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(body.contains("(data "), "wide cells retain the ordinary path:\n{body}");
    }

    #[test]
    fn niche_field_conversions_keep_their_explicit_container() {
        let source = "module M\nrun : Option Int -> (Unit -> Unit) -> Option Int\nlet run value observe =\n  let pair = (value, 1)\n  let ignored = observe ()\n  match pair with | (result, _) -> result\n";
        crate::tests::check_program(source, "run").unwrap();
        let body = crate::tests::rc_checked(source, "run");
        assert!(body.contains("(data "), "niche boundaries retain conversion ownership:\n{body}");
    }

    #[test]
    fn field_owners_are_released_after_the_original_intervening_operation() {
        let parent = LocalId::from_index(1);
        let a = LocalId::from_index(2);
        let b = LocalId::from_index(3);
        let done = CExpr::new(K::Lit(Lit::Unit), Ty::Unit);
        let effect = CExpr::new(
            K::Foreign {
                symbol: fai_syntax::Symbol::intern("observe"),
                args: Vec::new(),
                marshalled: true,
            },
            Ty::Unit,
        );
        let mut body = CExpr::new(
            K::Let {
                local: LocalId::from_index(4),
                value: Box::new(effect),
                body: Box::new(crate::drop_(parent, done.clone())),
            },
            Ty::Unit,
        );
        let fields = [CExpr::new(K::Local(a), Ty::Unit), CExpr::new(K::Local(b), Ty::Unit)];
        replace(&mut body, parent, 0, &fields);
        let K::Let { value, body, .. } = body.kind else {
            panic!("effect must retain its position")
        };
        assert!(matches!(value.kind, K::Foreign { .. }));
        assert_eq!(*body, crate::drop_(b, crate::drop_(a, done)));
    }
}
