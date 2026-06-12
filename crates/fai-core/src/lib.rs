// salsa's `tracked` macro emits `unsafe impl`s for the query types; as elsewhere
// we write no unsafe by hand.
#![allow(unsafe_code)]

//! The typed, desugared Core IR and lowering from the surface AST.
//!
//! [`core`] lowers one definition to a [`LoweredDef`] (see [`lower`]); the IR is
//! defined in [`ir`]. Core is the canonical lowered form consumed by reference
//! counting (`fai-rc`) and code generation (`fai-codegen`). Diagnostics for
//! constructs outside the native subset use the backend `FAI7xxx` range.

pub mod fingerprint;
#[allow(unsafe_code)]
pub mod inline;
pub mod ir;
mod lit;
#[allow(unsafe_code)]
mod lower;
pub mod pretty;
pub mod wire;

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;

pub use fingerprint::fingerprint_def;
pub use inline::{PrimWrapper, core_inlined, prim_wrapper};
pub use ir::{CExpr, CoreFn, ExprKind, FnAbi, FnId, Lit, LoweredDef, Prim, Repr};
pub use lit::{decode_char, decode_float, decode_int, decode_string};
pub use lower::{LoweredBody, core, lower_params_body};
pub use pretty::pretty_def;
pub use wire::{
    Rebuilt, RebuiltTest, TestContract, TestWireBundle, WireBundle, WireContract, WireDef,
    WireDefId, from_wire, from_wire_test,
};

use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};

/// A construct is not supported by the native backend yet.
pub const UNSUPPORTED_NATIVE: DiagnosticCode = DiagnosticCode::new("FAI7001");
/// Row-polymorphic record access is not yet compiled (offset evidence is future
/// work); monomorphic record access uses constant offsets.
pub const ROW_POLY_UNSUPPORTED: DiagnosticCode = DiagnosticCode::new("FAI7002");

/// Diagnostic codes owned by the backend lowering layer (the `FAI7xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: UNSUPPORTED_NATIVE,
        title: "construct not supported by the native backend yet",
        default_severity: Severity::Error,
        explanation: "A definition reachable from `main` uses a construct the native backend \
                      does not lower yet. Reported only for reachable code, so unused unsupported \
                      constructs still type-check.",
    },
    CodeInfo {
        code: ROW_POLY_UNSUPPORTED,
        title: "row-polymorphic record access not yet supported by the native backend",
        default_severity: Severity::Error,
        explanation: "Reserved for a row-polymorphic record access or update the backend could \
                      not compile. Such access now lowers via offset-evidence passing, so this is \
                      kept reserved and not normally emitted.",
    },
];
