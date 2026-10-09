//! Type-derived bounds for data headers, retained after nominal type erasure.

use fai_core::ir::{CExpr, DataShape, ExprKind as K};
use fai_db::Db;
use fai_resolve::{LocalId, type_decls};
use fai_types::{Con, RowEnd, Ty};
use rustc_hash::FxHashMap;

use crate::reuse_sig::e_children;

/// Collects conservative shape bounds in local-id order. A local with an
/// unknown non-marker type retains dynamic layout access.
pub(crate) fn collect(db: &dyn Db, body: &CExpr) -> Vec<(LocalId, DataShape)> {
    let mut collector = Collector { db, types: FxHashMap::default(), locals: FxHashMap::default() };
    collector.walk(body);
    let mut result: Vec<_> = collector
        .locals
        .into_iter()
        .filter_map(|(local, shape)| shape.map(|shape| (local, shape)))
        .collect();
    result.sort_by_key(|(local, _)| local.index());
    result
}

struct Collector<'a> {
    db: &'a dyn Db,
    types: FxHashMap<Ty, Option<DataShape>>,
    locals: FxHashMap<LocalId, Option<DataShape>>,
}

impl Collector<'_> {
    fn walk(&mut self, body: &CExpr) {
        match &body.kind {
            K::Local(local) => self.note(*local, &body.ty),
            K::Let { local, value, .. } => self.note(*local, &value.ty),
            _ => {}
        }
        e_children(body, &mut |child| self.walk(child));
    }

    fn note(&mut self, local: LocalId, ty: &Ty) {
        if *ty == Ty::Error {
            return;
        }
        let shape = *self.types.entry(ty.clone()).or_insert_with(|| shape(self.db, ty));
        self.locals
            .entry(local)
            .and_modify(|previous| {
                *previous = match (*previous, shape) {
                    (Some(a), Some(b)) => Some(union(a, b)),
                    _ => None,
                };
            })
            .or_insert(shape);
    }
}

fn union(a: DataShape, b: DataShape) -> DataShape {
    DataShape {
        max_tag: a.max_tag.max(b.max_tag),
        max_fields: a.max_fields.max(b.max_fields),
        scalars: a.scalars | b.scalars,
    }
}

fn shape(db: &dyn Db, ty: &Ty) -> Option<DataShape> {
    match ty {
        Ty::App(head, _) => shape(db, head),
        Ty::Con(Con::List) => Some(DataShape { max_tag: 1, max_fields: 2, scalars: 0 }),
        Ty::Tuple(fields) => structural_shape(fields.iter()),
        Ty::Record(row) if row.tail == RowEnd::Closed => {
            structural_shape(row.fields.iter().map(|(_, ty)| ty))
        }
        Ty::Adt(adt) => {
            let file = db.source_file(adt.file)?;
            let decls = type_decls(db, file);
            let info = decls.type_named(adt.name)?;
            if info.is_alias {
                return None;
            }
            let mut result = DataShape { max_tag: 0, max_fields: 0, scalars: 0 };
            for name in &info.ctors {
                let ctor = decls.ctor(*name)?;
                if ctor.arity == 0 {
                    continue;
                }
                let scheme = fai_types::constructor_scheme(db, file, *name)?;
                let repr = fai_core::representation::runtime_type(db, &scheme.ty);
                let mut ty = &repr;
                let mut scalars = 0;
                for i in 0..ctor.arity {
                    let Ty::Arrow(from, to, _) = ty else {
                        return None;
                    };
                    if i < 64 && **from == Ty::Con(Con::Float) {
                        scalars |= 1 << i;
                    }
                    ty = to;
                }
                result = union(
                    result,
                    DataShape {
                        max_tag: ctor.tag,
                        max_fields: u32::try_from(ctor.arity).ok()?,
                        scalars,
                    },
                );
            }
            (result.max_fields > 0).then_some(result)
        }
        _ => None,
    }
}

fn structural_shape<'a>(fields: impl Iterator<Item = &'a Ty>) -> Option<DataShape> {
    let mut count = 0u32;
    let mut scalars = 0;
    for ty in fields {
        // A structural generic field can be supplied by a concrete Float cell;
        // nominal constructor parameters instead always use uniform slots.
        if count < 64 && matches!(ty, Ty::Con(Con::Float) | Ty::Var(_) | Ty::Error) {
            scalars |= 1 << count;
        }
        count = count.checked_add(1)?;
    }
    (count > 0).then_some(DataShape { max_tag: 0, max_fields: count, scalars })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_resolve::AdtRef;
    use fai_syntax::Symbol;

    fn nominal(source: &str) -> Option<DataShape> {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let file = db.add_source("M.fai".into(), source.into());
        shape(&db, &Ty::Adt(AdtRef::new(file, Symbol::intern("T"))))
    }

    #[test]
    fn constructor_bounds_include_every_boxed_variant() {
        assert_eq!(
            nominal("module M\ntype T = | Empty | One Int | Pair Int Float\n"),
            Some(DataShape { max_tag: 2, max_fields: 2, scalars: 2 })
        );
    }

    #[test]
    fn generic_constructor_fields_remain_uniform() {
        assert_eq!(
            nominal("module M\ntype T 'a = | C 'a 'a 'a 'a 'a 'a 'a 'a 'a\n"),
            Some(DataShape { max_tag: 0, max_fields: 9, scalars: 0 })
        );
    }

    #[test]
    fn a_wide_float_constructor_cannot_claim_a_compact_bitmap() {
        assert_eq!(
            nominal(
                "module M\ntype T = | A Int | B Float Float Float Float Float Float Float Float Float\n"
            ),
            Some(DataShape { max_tag: 1, max_fields: 9, scalars: 511 })
        );
    }

    #[test]
    fn generic_structural_fields_can_arrive_as_raw_floats() {
        let fields = vec![Ty::Var(fai_types::TyVarId(0)); 9];
        assert_eq!(
            structural_shape(fields.iter()),
            Some(DataShape { max_tag: 0, max_fields: 9, scalars: 511 })
        );
    }
}
