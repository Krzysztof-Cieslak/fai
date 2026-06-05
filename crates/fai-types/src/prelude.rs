//! The built-in prelude's *types*.
//!
//! M2 ships type-only primitives: each has a declared [`Scheme`] but no body
//! (codegen lands in M3). Resolution (in `fai-resolve`) decides *which* names are
//! prelude names; this table gives their types so inference can use them.
//!
//! (The derived `.fai` prelude module is added in Phase 2.5; until then these
//! primitives cover the names the sample corpus needs.)

use fai_db::{Durability, FaiDatabase};
use fai_span::SourceId;
use fai_syntax::Symbol;

use crate::ty::{Con, Scheme, Ty};

/// The synthetic path of the embedded prelude (outside any user workspace).
pub const PRELUDE_PATH: &str = "<prelude>/Prelude.fai";

/// The reserved prelude module name (a user redeclaring it is a duplicate).
pub const PRELUDE_MODULE: &str = "Prelude";

/// The embedded prelude source.
pub const PRELUDE_SOURCE: &str = include_str!("Prelude.fai");

/// Loads the embedded prelude into `db` as a high-durability synthetic file,
/// returning its [`SourceId`]. Idempotent (re-registering reuses the id).
pub fn load_prelude(db: &mut FaiDatabase) -> SourceId {
    db.add_source_with_durability(PRELUDE_PATH.into(), PRELUDE_SOURCE.to_owned(), Durability::HIGH)
}

/// Whether `path` is the synthetic prelude (excluded from default query/check).
#[must_use]
pub fn is_prelude_path(path: &str) -> bool {
    path == PRELUDE_PATH
}

/// The scheme of a built-in intrinsic, if known.
///
/// Only Rust-implemented intrinsics live here; everything else the prelude offers
/// is an ordinary definition in `Prelude.fai`, resolved as a `Def`/`Ctor` and
/// typed through normal inference.
#[must_use]
pub fn builtin_scheme(name: Symbol) -> Option<Scheme> {
    Some(match name.as_str() {
        "true" | "false" => Scheme::mono(Ty::bool()),
        // The Console capability's sole member, reached as `Console.writeLine`
        // (a qualified builtin). A placeholder until interfaces/records land:
        // `Runtime -> String -> Unit`.
        "writeLine" => {
            Scheme::mono(Ty::arrows([Ty::Con(Con::Runtime), Ty::Con(Con::String)], Ty::Unit))
        }
        "intToString" => Scheme::mono(Ty::arrow(Ty::int(), Ty::Con(Con::String))),
        "floatToString" => Scheme::mono(Ty::arrow(Ty::Con(Con::Float), Ty::Con(Con::String))),
        "intToFloat" => Scheme::mono(Ty::arrow(Ty::int(), Ty::Con(Con::Float))),
        "floatToInt" => Scheme::mono(Ty::arrow(Ty::Con(Con::Float), Ty::int())),
        "sqrt" => Scheme::mono(Ty::arrow(Ty::Con(Con::Float), Ty::Con(Con::Float))),
        "not" => Scheme::mono(Ty::arrow(Ty::bool(), Ty::bool())),
        _ => return None,
    })
}
