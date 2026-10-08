//! Physical types after front-end opacity checks. An abstract alias retains its
//! nominal identity during inference but uses its underlying native representation.

use std::sync::Arc;

use fai_db::Db;
use fai_resolve::{AdtRef, DefId};
use fai_types::{BodyTypes, RecordRow, Scheme, Ty};

/// Expands aliases recursively for native layout and calling conventions.
/// This must not be used for source-level type compatibility or visibility.
pub fn runtime_type(db: &dyn Db, ty: &Ty) -> Ty {
    expand(db, ty, &mut Vec::new())
}

fn expand(db: &dyn Db, ty: &Ty, active: &mut Vec<AdtRef>) -> Ty {
    let mut head = ty;
    let mut args = Vec::new();
    while let Ty::App(f, arg) = head {
        args.push((**arg).clone());
        head = f;
    }
    if let Ty::Adt(adt) = head {
        args.reverse();
        if active.contains(adt) {
            return Ty::Error;
        }
        if let Some(body) = fai_types::expand_alias_ty(db, *adt, &args) {
            active.push(*adt);
            let repr = expand(db, &body, active);
            active.pop();
            return repr;
        }
    }
    match ty {
        Ty::App(f, a) => Ty::App(Arc::new(expand(db, f, active)), Arc::new(expand(db, a, active))),
        Ty::Arrow(from, to, effect) => {
            Ty::arrow_eff(expand(db, from, active), expand(db, to, active), effect.clone())
        }
        Ty::Tuple(fields) => Ty::Tuple(fields.iter().map(|ty| expand(db, ty, active)).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(name, ty)| (*name, expand(db, ty, active))).collect(),
            tail: row.tail,
        }),
        _ => ty.clone(),
    }
}

/// A definition's scheme with all aliases expanded for native ownership,
/// evidence, layout, and calling-convention analysis.
pub fn definition_scheme(db: &dyn Db, def: DefId) -> Option<Scheme> {
    let mut scheme = fai_types::declared_or_inferred_scheme(db, def)?;
    scheme.ty = runtime_type(db, &scheme.ty);
    Some(scheme)
}

pub(crate) fn body_types(db: &dyn Db, types: &BodyTypes) -> BodyTypes {
    BodyTypes {
        types: types.types.iter().map(|(&id, ty)| (id, runtime_type(db, ty))).collect(),
        pat_types: types.pat_types.iter().map(|(&id, ty)| (id, runtime_type(db, ty))).collect(),
    }
}
