// salsa's `tracked` macro emits `unsafe impl`s for the query types; as elsewhere
// we write no unsafe by hand.
#![allow(unsafe_code)]

//! The typed, desugared Core IR and lowering from the surface AST.
//!
//! [`core`] lowers one definition to a [`LoweredDef`] (see [`lower`]); the IR is
//! defined in [`ir`]. Core is the canonical lowered form consumed by reference
//! counting (`fai-rc`) and code generation (`fai-codegen`). Diagnostics for
//! constructs outside the native subset use the backend `FAI7xxx` range.

pub mod ir;
mod lit;
#[allow(unsafe_code)]
mod lower;
pub mod pretty;

#[cfg(test)]
mod tests;

pub use ir::{CExpr, CoreFn, ExprKind, FnId, Lit, LoweredDef, Prim};
pub use lit::{decode_int, decode_string};
pub use lower::core;
pub use pretty::pretty_def;

use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};

/// A construct is not supported by the native backend yet (the M3 subset).
pub const UNSUPPORTED_NATIVE: DiagnosticCode = DiagnosticCode::new("FAI7001");

/// Diagnostic codes owned by the backend lowering layer (the `FAI7xxx` range).
pub const CODES: &[CodeInfo] = &[CodeInfo {
    code: UNSUPPORTED_NATIVE,
    title: "construct not supported by the native backend yet",
    default_severity: Severity::Error,
}];
