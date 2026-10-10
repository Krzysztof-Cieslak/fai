//! Reduces bounded constructor/match intermediates without reordering fields.
//!
//! Field initializers remain strict and ordered. Only resource-free fields with
//! matching type views can lose their parent cell. Small continuations may move
//! into constructor-producing branches, with every copied binder freshened.

use super::*;
use crate::ir::FieldIndex;
use fai_resolve::AdtRef;
use fai_types::RowEnd;
use rustc_hash::FxHashSet;

pub(super) fn reduce(db: &dyn Db, base: Arc<LoweredDef>) -> Arc<LoweredDef> {
    let mut cx =
        Reducer { db, next: crate::inline::next_free_local(&base), budget: 4096, changed: false };
    let mut output = (*base).clone();
    for function in &mut output.fns {
        function.body = cx.fold(function.body.clone());
    }
    if cx.changed { Arc::new(output) } else { base }
}

struct Reducer<'a> {
    db: &'a dyn Db,
    next: usize,
    budget: usize,
    changed: bool,
}

#[cfg(test)]
mod tests;

fn children(e: &mut CExpr, f: &mut impl FnMut(&mut CExpr)) {
    match &mut e.kind {
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
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => args.iter_mut().for_each(f),
        K::App { func, args, .. } => {
            f(func);
            args.iter_mut().for_each(f);
        }
        K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => f(base),
        K::Drop { body, .. }
        | K::Dup { body, .. }
        | K::FreeReuse { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => f(body),
        K::HoleFill { cell, .. } => f(cell),
        _ => {}
    }
}

fn resource_free(
    db: &dyn Db,
    ty: &Ty,
    visiting: &mut FxHashSet<AdtRef>,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match ty {
        Ty::Unit
        | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char | Con::String | Con::Bytes) => true,
        Ty::App(head, item) if matches!(head.as_ref(), Ty::Con(Con::List | Con::Array)) => {
            resource_free(db, item, visiting, budget)
        }
        Ty::Tuple(fields) => fields.iter().all(|field| resource_free(db, field, visiting, budget)),
        Ty::Record(row) if row.tail == RowEnd::Closed => {
            row.fields.iter().all(|(_, field)| resource_free(db, field, visiting, budget))
        }
        Ty::Adt(adt) => {
            if visiting.contains(adt) {
                return true;
            }
            let Some(file) = db.source_file(adt.file) else { return false };
            let decls = fai_resolve::type_decls(db, file);
            let Some(info) = decls.type_named(adt.name) else { return false };
            if info.opaque || info.is_alias || !info.params.is_empty() {
                return false;
            }
            visiting.insert(*adt);
            let result = info.ctors.iter().all(|name| {
                let Some(ctor) = decls.ctor(*name) else { return false };
                let Some(scheme) = fai_types::constructor_scheme(db, file, *name) else {
                    return false;
                };
                let repr = crate::representation::runtime_type(db, &scheme.ty);
                let mut ty = &repr;
                for _ in 0..ctor.arity {
                    let Ty::Arrow(input, output, _) = ty else { return false };
                    if !resource_free(db, input, visiting, budget) {
                        return false;
                    }
                    ty = output;
                }
                true
            });
            visiting.remove(adt);
            result
        }
        _ => false,
    }
}

fn bind(local: LocalId, value: CExpr, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, ty)
}

fn replace_atom(mut e: CExpr, local: LocalId, atom: &CExpr) -> CExpr {
    if matches!(e.kind, K::Local(id) if id == local) {
        return atom.clone();
    }
    children(&mut e, &mut |child| *child = replace_atom(child.clone(), local, atom));
    e
}

fn atom_uses_match(body: &CExpr, local: LocalId, ty: &Ty) -> bool {
    let mut safe = true;
    walk_pre(body, &mut |e| match &e.kind {
        K::Local(id) if *id == local => safe &= e.ty == *ty || e.ty == Ty::Error,
        K::MakeClosure { captures, .. } if captures.contains(&local) => safe = false,
        K::DataField { index: FieldIndex::Dyn { evidence, .. }, .. } if *evidence == local => {
            safe = false
        }
        _ => {}
    });
    safe
}

fn project(mut body: CExpr, local: LocalId, tag: u32, fields: &[CExpr]) -> Option<CExpr> {
    match &body.kind {
        K::Local(id) if *id == local => return None,
        K::MakeClosure { captures, .. } if captures.contains(&local) => return None,
        K::DataTag { base, .. } if matches!(base.kind, K::Local(id) if id == local) => {
            return Some(CExpr::new(K::Lit(Lit::Int(i64::from(tag))), body.ty));
        }
        K::DataField { base, index: FieldIndex::Const(index), .. } if matches!(base.kind, K::Local(id) if id == local) =>
        {
            let field = fields.get(*index as usize)?;
            return (field.ty == body.ty).then(|| field.clone());
        }
        K::If { cond, then, els } => {
            let cond = project((**cond).clone(), local, tag, fields)?;
            if let K::Lit(Lit::Bool(yes)) = cond.kind {
                return project(
                    if yes { (**then).clone() } else { (**els).clone() },
                    local,
                    tag,
                    fields,
                );
            }
        }
        _ => {}
    }
    let mut valid = true;
    children(&mut body, &mut |child| match project(child.clone(), local, tag, fields) {
        Some(rewritten) => *child = rewritten,
        None => valid = false,
    });
    if valid {
        if let K::Prim { op: Prim::Eq, args } = &body.kind
            && let [
                CExpr { kind: K::Lit(Lit::Int(a)), .. },
                CExpr { kind: K::Lit(Lit::Int(b)), .. },
            ] = args.as_slice()
        {
            body.kind = K::Lit(Lit::Bool(a == b));
        }
        Some(body)
    } else {
        None
    }
}

impl Reducer<'_> {
    fn constructor_tree(&self, value: &CExpr, leaves: &mut usize) -> bool {
        if *leaves >= 4 {
            return false;
        }
        match &value.kind {
            K::Let { body, .. } => self.constructor_tree(body, leaves),
            K::If { then, els, .. } => {
                self.constructor_tree(then, leaves) && self.constructor_tree(els, leaves)
            }
            K::MakeData { args, reuse: None, .. } => {
                *leaves += 1;
                args.iter()
                    .all(|arg| resource_free(self.db, &arg.ty, &mut FxHashSet::default(), &mut 128))
            }
            _ => false,
        }
    }

    fn fresh_continuation(&mut self, local: LocalId, body: &CExpr, value: CExpr) -> CExpr {
        let mut subst = FxHashMap::default();
        let mut bound = Vec::new();
        walk_pre(body, &mut |e| match &e.kind {
            K::Local(id) => {
                subst.insert(*id, *id);
            }
            K::MakeClosure { captures, .. } => {
                for &id in captures {
                    subst.insert(id, id);
                }
            }
            K::DataField { index: FieldIndex::Dyn { evidence, .. }, .. } => {
                subst.insert(*evidence, *evidence);
            }
            K::Let { local, .. } => bound.push(*local),
            _ => {}
        });
        for id in bound {
            subst.insert(id, crate::inline::fresh_local(&mut self.next));
        }
        let parameter = crate::inline::fresh_local(&mut self.next);
        subst.insert(local, parameter);
        let body =
            crate::inline::remap_expr(body, &mut subst, &FxHashMap::default(), &mut self.next);
        bind(parameter, value, body)
    }

    fn fold(&mut self, mut e: CExpr) -> CExpr {
        if self.budget == 0 {
            return e;
        }
        self.budget -= 1;
        children(&mut e, &mut |child| *child = self.fold(child.clone()));
        match &e.kind {
            K::Prim { op: Prim::Eq, args } => {
                if let [CExpr { kind: K::Lit(a), .. }, CExpr { kind: K::Lit(b), .. }] =
                    args.as_slice()
                    && matches!((a, b), (Lit::Int(_), Lit::Int(_)) | (Lit::Bool(_), Lit::Bool(_)))
                {
                    return CExpr::new(K::Lit(Lit::Bool(a == b)), e.ty);
                }
            }
            K::If { cond, then, els } if matches!(cond.kind, K::Lit(Lit::Bool(_))) => {
                return if matches!(cond.kind, K::Lit(Lit::Bool(true))) {
                    (**then).clone()
                } else {
                    (**els).clone()
                };
            }
            K::Let { local, value, body } => match &value.kind {
                K::Let { local: inner, value: first, body: rest } => {
                    return self.fold(bind(
                        *inner,
                        (**first).clone(),
                        bind(*local, (**rest).clone(), (**body).clone()),
                    ));
                }
                K::Local(_)
                | K::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Bool(_) | Lit::Char(_) | Lit::Unit)
                    if atom_uses_match(body, *local, &value.ty) =>
                {
                    return self.fold(replace_atom((**body).clone(), *local, value));
                }
                K::MakeData { tag, args, reuse: None, .. }
                    if args.len() <= 8
                        && args.iter().all(|arg| {
                            resource_free(self.db, &arg.ty, &mut FxHashSet::default(), &mut 128)
                        }) =>
                {
                    let fields: Vec<_> = args
                        .iter()
                        .map(|arg| {
                            CExpr::new(
                                K::Local(crate::inline::fresh_local(&mut self.next)),
                                arg.ty.clone(),
                            )
                        })
                        .collect();
                    if let Some(mut result) = project((**body).clone(), *local, *tag, &fields) {
                        for (field, value) in fields.iter().zip(args).rev() {
                            let K::Local(id) = field.kind else { unreachable!() };
                            result = bind(id, value.clone(), result);
                        }
                        self.changed = true;
                        return self.fold(result);
                    }
                }
                K::If { cond, then, els }
                    if crate::helper_inline::node_count(body) <= 64
                        && self.constructor_tree(value, &mut 0) =>
                {
                    let left = self.fresh_continuation(*local, body, (**then).clone());
                    let right = self.fresh_continuation(*local, body, (**els).clone());
                    return self.fold(CExpr::new(
                        K::If { cond: cond.clone(), then: Box::new(left), els: Box::new(right) },
                        e.ty,
                    ));
                }
                _ => {}
            },
            _ => {}
        }
        e
    }
}
