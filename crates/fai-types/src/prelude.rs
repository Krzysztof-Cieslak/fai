//! The built-in prelude's *types*.
//!
//! M2 ships type-only primitives: each has a declared [`Scheme`] but no body
//! (codegen lands in M3). Resolution (in `fai-resolve`) decides *which* names are
//! prelude names; this table gives their types so inference can use them.
//!
//! (The derived `.fai` prelude module is added in Phase 2.5; until then these
//! primitives cover the names the sample corpus needs.)

use fai_syntax::Symbol;

use crate::ty::{Con, Scheme, Ty, TyVarId};

/// The scheme of a prelude name, if known.
#[must_use]
pub fn builtin_scheme(name: Symbol) -> Option<Scheme> {
    let a = || Ty::Var(TyVarId(0));
    Some(match name.as_str() {
        "true" | "false" => Scheme::mono(Ty::bool()),
        "intToString" => Scheme::mono(Ty::arrow(Ty::int(), Ty::Con(Con::String))),
        "floatToString" => Scheme::mono(Ty::arrow(Ty::Con(Con::Float), Ty::Con(Con::String))),
        "sqrt" => Scheme::mono(Ty::arrow(Ty::Con(Con::Float), Ty::Con(Con::Float))),
        "not" => Scheme::mono(Ty::arrow(Ty::bool(), Ty::bool())),
        "pi" => Scheme::mono(Ty::Con(Con::Float)),
        // length : List 'a -> Int
        "length" => Scheme { vars: vec![TyVarId(0)], ty: Ty::arrow(Ty::list(a()), Ty::int()) },
        // append : List 'a -> List 'a -> List 'a
        "append" => Scheme {
            vars: vec![TyVarId(0)],
            ty: Ty::arrows([Ty::list(a()), Ty::list(a())], Ty::list(a())),
        },
        // reverse : List 'a -> List 'a
        "reverse" => Scheme { vars: vec![TyVarId(0)], ty: Ty::arrow(Ty::list(a()), Ty::list(a())) },
        _ => return None,
    })
}
