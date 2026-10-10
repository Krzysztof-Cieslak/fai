//! Removes representation-preserving local aliases after borrow inference.

use fai_core::ir::{CExpr, ExprKind as K, FieldIndex};
use fai_db::Db;
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

use crate::{is_boxed_data_ty, reuse_sig::e_children};

/// Coalesces uniform aliases without changing the already inferred call ownership
/// ABI. Conflicting type uses and niche/scalar conversions retain their binding.
pub(crate) fn coalesce(db: &dyn Db, body: CExpr) -> CExpr {
    let mut types = FxHashMap::default();
    collect_types(&body, &mut types);
    let mut aliases =
        Aliases { db, types, locals: FxHashMap::default(), retained_types: FxHashMap::default() };
    aliases.rewrite(body)
}

fn note(types: &mut FxHashMap<LocalId, Option<Ty>>, local: LocalId, ty: &Ty) {
    if *ty == Ty::Error {
        return;
    }
    types
        .entry(local)
        .and_modify(|old| {
            if !old.as_ref().is_some_and(|old| same_representation_type(old, ty)) {
                *old = None;
            }
        })
        .or_insert_with(|| Some(ty.clone()));
}

fn same_representation_type(a: &Ty, b: &Ty) -> bool {
    a == b || matches!((a, b), (Ty::Var(_), Ty::Var(_)))
}

fn uniform_alias(ty: &Ty) -> bool {
    is_boxed_data_ty(ty)
        || matches!(ty, Ty::Var(_) | Ty::Con(Con::String | Con::Bytes))
        || matches!(ty, Ty::App(head, _) if matches!(head.as_ref(), Ty::Con(Con::Array)))
}

fn collect_types(body: &CExpr, types: &mut FxHashMap<LocalId, Option<Ty>>) {
    match &body.kind {
        K::Local(local) => note(types, *local, &body.ty),
        K::Let { local, value, .. } => note(types, *local, &value.ty),
        _ => {}
    }
    e_children(body, &mut |child| collect_types(child, types));
}

struct Aliases<'db> {
    db: &'db dyn Db,
    types: FxHashMap<LocalId, Option<Ty>>,
    locals: FxHashMap<LocalId, LocalId>,
    retained_types: FxHashMap<LocalId, Ty>,
}

impl Aliases<'_> {
    fn local(&self, local: LocalId) -> LocalId {
        self.locals.get(&local).copied().unwrap_or(local)
    }

    fn rewrite(&mut self, expression: CExpr) -> CExpr {
        let CExpr { kind, mut ty } = expression;
        let kind = match kind {
            K::Local(local) => {
                let target = self.local(local);
                if ty == Ty::Error
                    && let Some(known) = self.retained_types.get(&target)
                {
                    ty = known.clone();
                }
                K::Local(target)
            }
            K::Let { local, value, body } => {
                let value = self.rewrite(*value);
                if let K::Local(original) = value.kind
                    && uniform_alias(&value.ty)
                    && fai_core::niche_scheme(self.db, &value.ty).is_none()
                    && self
                        .types
                        .get(&local)
                        .and_then(Option::as_ref)
                        .is_some_and(|ty| same_representation_type(ty, &value.ty))
                    && self
                        .types
                        .get(&original)
                        .and_then(Option::as_ref)
                        .is_some_and(|ty| same_representation_type(ty, &value.ty))
                {
                    let original = self.local(original);
                    self.locals.insert(local, original);
                    // The removed binding may be the only type anchor for a
                    // match scrutinee. Retain it on marker-typed uses so native
                    // tag/projection/drop specialization keeps the same shape.
                    self.retained_types.insert(original, value.ty.clone());
                    return self.rewrite(*body);
                }
                K::Let { local, value: Box::new(value), body: Box::new(self.rewrite(*body)) }
            }
            K::If { cond, then, els } => K::If {
                cond: Box::new(self.rewrite(*cond)),
                then: Box::new(self.rewrite(*then)),
                els: Box::new(self.rewrite(*els)),
            },
            K::Prim { op, args } => {
                K::Prim { op, args: args.into_iter().map(|a| self.rewrite(a)).collect() }
            }
            K::Foreign { symbol, args, marshalled } => K::Foreign {
                symbol,
                args: args.into_iter().map(|a| self.rewrite(a)).collect(),
                marshalled,
            },
            K::App { func, args, reuse, alloc } => K::App {
                func: Box::new(self.rewrite(*func)),
                args: args.into_iter().map(|a| self.rewrite(a)).collect(),
                reuse: reuse.into_iter().map(|l| l.map(|l| self.local(l))).collect(),
                alloc,
            },
            K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
                tag,
                args: args.into_iter().map(|a| self.rewrite(a)).collect(),
                reuse: reuse.map(|l| self.local(l)),
                scalars,
                niche,
            },
            K::MakeClosure { func, captures, alloc } => K::MakeClosure {
                func,
                captures: captures.into_iter().map(|l| self.local(l)).collect(),
                alloc,
            },
            K::DataField { base, index, scalar, niche } => K::DataField {
                base: Box::new(self.rewrite(*base)),
                index: match index {
                    FieldIndex::Dyn { base, evidence } => {
                        FieldIndex::Dyn { base, evidence: self.local(evidence) }
                    }
                    other => other,
                },
                scalar,
                niche,
            },
            K::DataTag { base, niche } => K::DataTag { base: Box::new(self.rewrite(*base)), niche },
            K::Spread { components } => {
                K::Spread { components: components.into_iter().map(|c| self.rewrite(c)).collect() }
            }
            K::LetMany { locals, value, body } => K::LetMany {
                locals,
                value: Box::new(self.rewrite(*value)),
                body: Box::new(self.rewrite(*body)),
            },
            K::Reset { value, token, body } => K::Reset {
                value: Box::new(self.rewrite(*value)),
                token,
                body: Box::new(self.rewrite(*body)),
            },
            K::FreeReuse { token, body } => {
                K::FreeReuse { token: self.local(token), body: Box::new(self.rewrite(*body)) }
            }
            K::Dup { local, body } => {
                K::Dup { local: self.local(local), body: Box::new(self.rewrite(*body)) }
            }
            K::Drop { local, body } => {
                K::Drop { local: self.local(local), body: Box::new(self.rewrite(*body)) }
            }
            K::Join { params, body } => K::Join { params, body: Box::new(self.rewrite(*body)) },
            K::Recur { args } => {
                K::Recur { args: args.into_iter().map(|a| self.rewrite(a)).collect() }
            }
            K::HoleStart { hole, body } => {
                K::HoleStart { hole, body: Box::new(self.rewrite(*body)) }
            }
            K::HoleFill { hole, cell, field } => {
                K::HoleFill { hole: self.local(hole), cell: Box::new(self.rewrite(*cell)), field }
            }
            K::HoleClose { hole, base } => {
                K::HoleClose { hole: self.local(hole), base: Box::new(self.rewrite(*base)) }
            }
            K::Lit(_) | K::Global(_) | K::Error => kind,
        };
        CExpr::new(kind, ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{ClosureAlloc, FnId};

    fn binding(body: CExpr) -> CExpr {
        CExpr::new(
            K::Let {
                local: LocalId::from_index(1),
                value: Box::new(CExpr::new(
                    K::Local(LocalId::from_index(0)),
                    Ty::Tuple(vec![Ty::bool()]),
                )),
                body: Box::new(body),
            },
            Ty::Error,
        )
    }

    #[test]
    fn removing_a_binding_retains_its_only_type_anchor() {
        let body = binding(CExpr::new(
            K::DataTag {
                base: Box::new(CExpr::new(K::Local(LocalId::from_index(1)), Ty::Error)),
                niche: None,
            },
            Ty::int(),
        ));
        let out = coalesce(&fai_db::FaiDatabase::new(), body);
        let K::DataTag { base, .. } = out.kind else { panic!("{out:?}") };
        assert!(matches!(base.kind, K::Local(local) if local == LocalId::from_index(0)));
        assert_eq!(base.ty, Ty::Tuple(vec![Ty::bool()]));
    }

    #[test]
    fn conflicting_representations_keep_the_binding() {
        let body =
            binding(CExpr::new(K::Local(LocalId::from_index(1)), Ty::Tuple(vec![Ty::int()])));
        assert!(matches!(coalesce(&fai_db::FaiDatabase::new(), body).kind, K::Let { .. }));
    }

    #[test]
    fn a_capture_keeps_its_position_with_the_original_value() {
        let body = binding(CExpr::new(
            K::MakeClosure {
                func: FnId(1),
                captures: vec![LocalId::from_index(1)],
                alloc: ClosureAlloc::Heap,
            },
            Ty::Error,
        ));
        let out = coalesce(&fai_db::FaiDatabase::new(), body);
        let K::MakeClosure { captures, .. } = out.kind else { panic!("{out:?}") };
        assert_eq!(captures, [LocalId::from_index(0)]);
    }

    #[test]
    fn generic_aliases_share_their_uniform_representation() {
        let first = Ty::Var(fai_types::TyVarId(0));
        let second = Ty::Var(fai_types::TyVarId(1));
        let body = CExpr::new(
            K::Let {
                local: LocalId::from_index(1),
                value: Box::new(CExpr::new(K::Local(LocalId::from_index(0)), first)),
                body: Box::new(CExpr::new(K::Local(LocalId::from_index(1)), second)),
            },
            Ty::Error,
        );
        let result = coalesce(&fai_db::FaiDatabase::new(), body);
        assert!(matches!(result.kind, K::Local(local) if local == LocalId::from_index(0)));
    }

    #[test]
    fn scalar_to_generic_aliases_keep_their_conversion() {
        let body = CExpr::new(
            K::Let {
                local: LocalId::from_index(1),
                value: Box::new(CExpr::new(K::Local(LocalId::from_index(0)), Ty::int())),
                body: Box::new(CExpr::new(
                    K::Local(LocalId::from_index(1)),
                    Ty::Var(fai_types::TyVarId(0)),
                )),
            },
            Ty::Error,
        );
        assert!(matches!(coalesce(&fai_db::FaiDatabase::new(), body).kind, K::Let { .. }));
    }

    #[test]
    fn generic_aliases_preserve_the_number_of_owned_results() {
        let source = "module M\npublic pair : 'a -> ('a * 'a)\nlet pair value =\n  let alias = value\n  (value, alias)\n";
        crate::tests::check_program(source, "pair").unwrap();
        let body = crate::tests::rc_checked(source, "pair");
        assert_eq!(body.matches("dup ").count(), 1, "{body}");
    }
}
