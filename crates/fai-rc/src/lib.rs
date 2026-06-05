// salsa's `tracked` macro emits `unsafe impl`s; we write no unsafe by hand.
#![allow(unsafe_code)]

//! Reference-count insertion over the Core IR.
//!
//! Under the uniform **consume** convention (every operation consumes its
//! operands), plain reference counting reduces to two rules, applied here with
//! no reuse analysis (that is M6):
//!
//! * **Duplicate at every use.** Each variable occurrence becomes
//!   `Dup{x; x}` — increment then consume — so the binding's own reference is
//!   preserved across the use. This is path-insensitive: a use inside one branch
//!   of an `if` is self-balancing, so no branch balancing is needed.
//! * **Drop once at scope end.** Each *owned* binding (a parameter or a `let`)
//!   is released exactly once at the end of its scope. Captured variables are
//!   borrowed (the closure owns them and releases them when it dies), so they
//!   are duplicated on use but never dropped here.
//!
//! Duplicating immediates and dropping them are runtime no-ops (tag-checked), so
//! this is correct for every value kind; closures, partial applications, and
//! strings are released precisely.

use std::sync::Arc;

use fai_core::core;
use fai_core::ir::{CExpr, ExprKind as K, LoweredDef};
use fai_db::{Db, SourceFile};
use fai_resolve::LocalId;
use fai_syntax::Symbol;

/// Inserts reference-count operations into `name`'s lowered definition.
#[salsa::tracked]
pub fn rc(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let lowered = core(db, file, name);
    let mut fns = lowered.fns.clone();
    for f in &mut fns {
        let body = insert_dups(f.body.clone());
        f.body = drop_params(body, &f.params);
    }
    Arc::new(LoweredDef { def: lowered.def, fns })
}

/// Rewrites every variable use to `Dup{x; x}` and inserts a scope-end drop after
/// each `let` body.
fn insert_dups(expr: CExpr) -> CExpr {
    let CExpr { kind, ty } = expr;
    match kind {
        K::Local(x) => {
            let used = CExpr::new(K::Local(x), ty.clone());
            CExpr::new(K::Dup { local: x, body: Box::new(used) }, ty)
        }
        K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => CExpr::new(kind, ty),
        K::Prim { op, args } => {
            CExpr::new(K::Prim { op, args: args.into_iter().map(insert_dups).collect() }, ty)
        }
        K::MakeData { tag, args } => {
            CExpr::new(K::MakeData { tag, args: args.into_iter().map(insert_dups).collect() }, ty)
        }
        K::DataTag(base) => CExpr::new(K::DataTag(Box::new(insert_dups(*base))), ty),
        K::DataField { base, index } => {
            CExpr::new(K::DataField { base: Box::new(insert_dups(*base)), index }, ty)
        }
        K::App { func, args } => CExpr::new(
            K::App {
                func: Box::new(insert_dups(*func)),
                args: args.into_iter().map(insert_dups).collect(),
            },
            ty,
        ),
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(insert_dups(*cond)),
                then: Box::new(insert_dups(*then)),
                els: Box::new(insert_dups(*els)),
            },
            ty,
        ),
        K::Let { local, value, body } => {
            let value = Box::new(insert_dups(*value));
            let body = insert_dups(*body);
            let dropped = drop_after(body, local);
            CExpr::new(K::Let { local, value, body: Box::new(dropped) }, ty)
        }
        // No dup/drop exists before this pass; pass through defensively.
        K::Dup { local, body } => {
            CExpr::new(K::Dup { local, body: Box::new(insert_dups(*body)) }, ty)
        }
        K::Drop { local, body } => {
            CExpr::new(K::Drop { local, body: Box::new(insert_dups(*body)) }, ty)
        }
    }
}

/// Wraps `body` so that, after it evaluates, `local` is dropped (drop-after).
fn drop_after(body: CExpr, local: LocalId) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Drop { local, body: Box::new(body) }, ty)
}

/// Drops each parameter at the end of the function body (in order, outermost
/// first).
fn drop_params(body: CExpr, params: &[LocalId]) -> CExpr {
    let mut e = body;
    for &p in params.iter().rev() {
        e = drop_after(e, p);
    }
    e
}

#[cfg(test)]
mod tests;
