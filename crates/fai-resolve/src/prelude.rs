//! The built-in prelude.
//!
//! The prelude is a real module (`Prelude`) whose public values, types, and
//! constructors are visible unqualified everywhere — the one exception to the
//! qualified-only cross-module rule. Resolution discovers those through the
//! prelude file's interface; this module only names the reserved module and the
//! handful of **intrinsics** implemented in Rust (not in Fai), whose types come
//! from `fai-types` and whose code is a primitive or a runtime call.

use fai_syntax::Symbol;

/// The reserved prelude module name. The module that declares this name is the
/// prelude itself and is exempt from shadow-prelude warnings.
pub const PRELUDE_MODULE: &str = "Prelude";

/// Built-in intrinsics implemented in Rust rather than in Fai.
///
/// These resolve to [`Res::Builtin`](crate::Res::Builtin); their types come from
/// the `fai-types` builtin table and their code is a primitive or runtime call.
/// Everything else the prelude offers is an ordinary definition in the prelude
/// file (resolved as a `Def`/`Ctor`).
pub const INTRINSICS: &[&str] =
    &["intToString", "floatToString", "intToFloat", "floatToInt", "sqrt", "not"];

/// Returns whether `name` is a built-in intrinsic.
#[must_use]
pub fn is_intrinsic(name: Symbol) -> bool {
    INTRINSICS.contains(&name.as_str())
}
