//! The built-in prelude names visible unqualified everywhere.
//!
//! M2 resolves prelude *names* here; their *types* (the primitive `name ->
//! Scheme` table) and the derived `.fai` prelude module live in `fai-types`
//! (Phase 2.5). Keeping the name set here lets resolution fall back to the
//! prelude without depending on the type machinery.

use fai_syntax::Symbol;

/// The built-in prelude value names (primitives + derived helpers).
///
/// Operators are handled directly by inference and are not listed here. This set
/// is the resolution fallback after local scope and the current module.
pub const PRELUDE_NAMES: &[&str] = &[
    // Type-only primitives (bodies/codegen land in M3).
    "intToString",
    "floatToString",
    "sqrt",
    "not",
    "length",
    "append",
    "reverse",
    "pi",
];

/// Returns whether `name` is a prelude name.
#[must_use]
pub fn is_prelude_name(name: Symbol) -> bool {
    PRELUDE_NAMES.contains(&name.as_str())
}
