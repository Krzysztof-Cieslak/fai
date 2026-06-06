//! The prelude-private intrinsics.
//!
//! A handful of operations cannot be written in Fai; they are implemented in
//! Rust and reached only as `Prim.<name>` from *inside* standard-library modules
//! (a reference elsewhere is [`INTRINSIC_OUTSIDE_STD`](crate::INTRINSIC_OUTSIDE_STD)).
//! The standard library re-exports the user-facing ones under clean qualified
//! names (`Int.toString`, `String.split`, …); their types come from the
//! `fai-types` builtin table and their code is a primitive or a runtime call.

use fai_syntax::Symbol;

/// Intrinsics implemented in Rust rather than in Fai, reached as `Prim.<name>`.
///
/// These resolve to [`Res::Builtin`](crate::Res::Builtin); everything else the
/// standard library offers is an ordinary definition (resolved as a `Def`/`Ctor`).
pub const INTRINSICS: &[&str] = &[
    "intToString",
    "floatToString",
    "intToFloat",
    "floatToInt",
    "sqrt",
    "not",
    "stringLength",
    "toUpper",
    "toLower",
    "trim",
    "stringContains",
    "stringConcat",
    "split",
    "join",
];

/// The synthetic module through which standard-library code reaches intrinsics.
pub const PRIM_MODULE: &str = "Prim";

/// Returns whether `name` is a built-in intrinsic.
#[must_use]
pub fn is_intrinsic(name: Symbol) -> bool {
    INTRINSICS.contains(&name.as_str())
}
