//! Bounded, field-count-aware selection of local reuse targets.

use fai_core::ir::{CExpr, ExprKind as K};
use fai_db::Db;
use fai_resolve::LocalId;
use fai_types::{Con, RowEnd, Ty};
use rustc_hash::FxHashMap;

use crate::reuse_sig::e_children;

const SEARCH_BUDGET: usize = 256;

/// Zero means a known wrapper-free value; absent/unknown classes keep the
/// runtime-checked selection. Classes describe field counts, as reuse signatures
/// do; the runtime still checks the complete allocation size and header form.
pub(crate) fn collect(db: &dyn Db, body: &CExpr) -> FxHashMap<LocalId, Option<usize>> {
    fn walk(
        db: &dyn Db,
        e: &CExpr,
        types: &mut FxHashMap<Ty, Option<usize>>,
        locals: &mut FxHashMap<LocalId, Option<usize>>,
    ) {
        let observation = match &e.kind {
            K::Local(local) => Some((*local, &e.ty)),
            K::Let { local, value, .. } => Some((*local, &value.ty)),
            _ => None,
        };
        if let Some((local, ty)) = observation.filter(|(_, ty)| **ty != Ty::Error) {
            let class = *types.entry(ty.clone()).or_insert_with(|| class(db, ty));
            locals
                .entry(local)
                .and_modify(|old| {
                    if *old != class {
                        *old = None;
                    }
                })
                .or_insert(class);
        }
        e_children(e, &mut |child| walk(db, child, types, locals));
    }
    let mut locals = FxHashMap::default();
    walk(db, body, &mut FxHashMap::default(), &mut locals);
    locals
}

fn class(db: &dyn Db, ty: &Ty) -> Option<usize> {
    if fai_core::niche::niche_scheme(db, ty).is_some() {
        return Some(0);
    }
    match ty {
        Ty::Tuple(fields) => Some(fields.len()),
        Ty::Record(row) if row.tail == RowEnd::Closed => Some(row.fields.len()),
        Ty::Con(Con::List) => Some(2),
        Ty::App(head, _) => class(db, head),
        Ty::Adt(adt) => {
            let file = db.source_file(adt.file)?;
            let decls = fai_resolve::type_decls(db, file);
            let info = decls.type_named(adt.name)?;
            if info.is_alias || info.opaque {
                return None;
            }
            let mut fields = None;
            for name in &info.ctors {
                let arity = decls.ctor(*name)?.arity;
                if arity > 0 {
                    if fields.is_some_and(|old| old != arity) {
                        return None;
                    }
                    fields = Some(arity);
                }
            }
            Some(fields.unwrap_or(0))
        }
        _ => None,
    }
}

fn target(e: &CExpr, fields: usize) -> bool {
    crate::is_reuse_target(e) && matches!(&e.kind, K::MakeData { args, .. } if args.len() == fields)
}

/// Calls and unknown work cannot extend a token past a construction that would
/// previously have consumed it. Budget exhaustion also ends that search.
fn simple(e: &CExpr, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    if matches!(e.kind, K::App { .. } | K::Foreign { .. } | K::Prim { .. }) {
        return false;
    }
    let mut result = true;
    e_children(e, &mut |child| {
        if result {
            result = simple(child, budget);
        }
    });
    result
}

fn reaches(e: &CExpr, fields: usize, skipped: bool, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &e.kind {
        K::MakeData { .. } => target(e, fields),
        K::Let { value, body, .. } => {
            if skipped && !simple(value, budget) {
                return false;
            }
            target(value, fields)
                || reaches(body, fields, skipped || crate::is_reuse_target(value), budget)
        }
        K::If { cond, then, els } => {
            (!skipped || simple(cond, budget))
                && (reaches(then, fields, skipped, budget) || reaches(els, fields, skipped, budget))
        }
        K::Dup { body, .. } | K::Drop { body, .. } | K::Reset { body, .. } => {
            reaches(body, fields, skipped, budget)
        }
        _ => false,
    }
}

fn thread(e: CExpr, token: LocalId, fields: usize, skipped: bool) -> CExpr {
    let mut budget = SEARCH_BUDGET;
    if !reaches(&e, fields, skipped, &mut budget) {
        return crate::free_reuse_(token, e);
    }
    let CExpr { kind, ty } = e;
    let kind = match kind {
        K::MakeData { .. } => return crate::attach_reuse(CExpr::new(kind, ty), token),
        K::Let { local, value, body } => {
            if target(&value, fields) {
                K::Let { local, value: Box::new(crate::attach_reuse(*value, token)), body }
            } else {
                let skipped = skipped || crate::is_reuse_target(&value);
                K::Let { local, value, body: Box::new(thread(*body, token, fields, skipped)) }
            }
        }
        K::If { cond, then, els } => K::If {
            cond,
            then: Box::new(thread(*then, token, fields, skipped)),
            els: Box::new(thread(*els, token, fields, skipped)),
        },
        K::Dup { local, body } => {
            K::Dup { local, body: Box::new(thread(*body, token, fields, skipped)) }
        }
        K::Drop { local, body } => {
            K::Drop { local, body: Box::new(thread(*body, token, fields, skipped)) }
        }
        K::Reset { value, token: other, body } => {
            K::Reset { value, token: other, body: Box::new(thread(*body, token, fields, skipped)) }
        }
        _ => unreachable!("a reachable reuse target follows a supported continuation"),
    };
    CExpr::new(kind, ty)
}

pub(crate) fn release(local: LocalId, body: CExpr, fields: usize, next: &mut usize) -> CExpr {
    let mut budget = SEARCH_BUDGET;
    if fields == 0 || !reaches(&body, fields, false, &mut budget) {
        return crate::drop_(local, body);
    }
    let token = crate::fresh(next);
    crate::reset_(local, token, thread(body, token, fields, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{ClosureAlloc, Lit};

    fn data(fields: usize) -> CExpr {
        CExpr::new(
            K::MakeData {
                tag: 0,
                args: vec![CExpr::new(K::Lit(Lit::Unit), Ty::Unit); fields],
                reuse: None,
                scalars: 0,
                niche: None,
            },
            Ty::Tuple(vec![Ty::Unit; fields]),
        )
    }

    fn bind(value: CExpr, body: CExpr) -> CExpr {
        let ty = body.ty.clone();
        CExpr::new(
            K::Let { local: LocalId::from_index(1), value: Box::new(value), body: Box::new(body) },
            ty,
        )
    }

    #[test]
    fn mismatched_construction_keeps_the_original_drop() {
        let source = LocalId::from_index(0);
        let body = data(3);
        assert_eq!(release(source, body.clone(), 2, &mut 10), crate::drop_(source, body));
    }

    #[test]
    fn matching_later_construction_receives_the_token() {
        let result = release(LocalId::from_index(0), bind(data(2), data(3)), 3, &mut 10);
        let K::Reset { token, body, .. } = result.kind else { panic!("expected reset") };
        let K::Let { value, body, .. } = body.kind else { panic!("expected binding") };
        assert!(matches!(value.kind, K::MakeData { reuse: None, .. }));
        assert!(matches!(body.kind, K::MakeData { reuse: Some(t), .. } if t == token));
    }

    #[test]
    fn skipping_a_construction_does_not_extend_a_token_across_a_call() {
        let source = LocalId::from_index(0);
        let call = CExpr::new(
            K::App {
                func: Box::new(CExpr::new(K::Local(LocalId::from_index(5)), Ty::Error)),
                args: Vec::new(),
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            Ty::Unit,
        );
        let body = bind(data(2), bind(call, data(3)));
        assert_eq!(release(source, body.clone(), 3, &mut 10), crate::drop_(source, body));
    }

    #[test]
    fn a_call_before_the_first_construction_keeps_death_point_reset() {
        let call = CExpr::new(
            K::App {
                func: Box::new(CExpr::new(K::Local(LocalId::from_index(5)), Ty::Error)),
                args: Vec::new(),
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            Ty::Unit,
        );
        assert!(matches!(
            release(LocalId::from_index(0), bind(call, data(3)), 3, &mut 10).kind,
            K::Reset { .. }
        ));
    }

    #[test]
    fn a_nonmatching_branch_releases_its_token_before_construction() {
        let body = CExpr::new(
            K::If {
                cond: Box::new(CExpr::new(K::Lit(Lit::Bool(true)), Ty::Con(Con::Bool))),
                then: Box::new(data(3)),
                els: Box::new(data(2)),
            },
            Ty::Error,
        );
        let K::Reset { body, token, .. } = release(LocalId::from_index(0), body, 3, &mut 10).kind
        else {
            panic!("expected reset")
        };
        let K::If { then, els, .. } = body.kind else { panic!("expected branch") };
        assert!(matches!(then.kind, K::MakeData { reuse: Some(t), .. } if t == token));
        assert!(matches!(els.kind, K::FreeReuse { token: t, .. } if t == token));
    }

    #[test]
    fn exhausted_search_keeps_an_ordinary_drop() {
        let source = LocalId::from_index(0);
        let mut body = data(3);
        for _ in 0..SEARCH_BUDGET {
            body = bind(CExpr::new(K::Lit(Lit::Unit), Ty::Unit), body);
        }
        assert!(matches!(release(source, body, 3, &mut 10).kind, K::Drop { .. }));
    }

    #[test]
    fn same_arity_union_alternatives_have_one_local_class() {
        let (db, file) = crate::tests::db_with("module M\ntype T = | A Int | B String\n");
        let ty =
            Ty::Adt(fai_resolve::AdtRef::new(file.source(&db), fai_syntax::Symbol::intern("T")));
        assert_eq!(class(&db, &ty), Some(1));
    }

    #[test]
    fn different_union_arities_retain_dynamic_selection() {
        let (db, file) = crate::tests::db_with("module M\ntype T = | A Int | B Int Int\n");
        let ty =
            Ty::Adt(fai_resolve::AdtRef::new(file.source(&db), fai_syntax::Symbol::intern("T")));
        assert_eq!(class(&db, &ty), None);
    }
}
