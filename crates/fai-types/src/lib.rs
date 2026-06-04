//! Hindley–Milner type inference for the Fai functional core.
//!
//! This crate owns the type representation (a flat per-scheme arena),
//! unification, let-generalization, and the per-def/SCC `infer` query. It builds
//! on `fai-resolve` (name resolution, the module graph, and SCCs) and emits
//! diagnostics in the `FAI3xxx` range; every code is catalogued in [`CODES`].
//!
//! Skeleton: the representation and queries land incrementally across M2.

mod contracts;
mod infer;
mod lower;
mod prelude;
#[allow(unsafe_code)]
mod query;
#[cfg(test)]
mod tests;
mod ty;

pub use contracts::check_contracts;
pub use infer::{
    Constraint, Env, InferCtx, SccEnv, SccInference, SolveTy, UnifyResult, Walker, contract_env,
    declared_scheme, error_scheme, generalize, infer_scc,
};
pub use lower::{LowerVars, lower_signature, lower_type};
pub use query::{SccTypes, check_file, def_type, infer_scc_query};
pub use ty::{Con, Scheme, Ty, TyVarId, VarNames, render, render_scheme};

use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};

/// Two types could not be unified.
pub const TYPE_MISMATCH: DiagnosticCode = DiagnosticCode::new("FAI3001");
/// A type variable would have to contain itself (the occurs check failed).
pub const OCCURS_CHECK: DiagnosticCode = DiagnosticCode::new("FAI3002");
/// A `public` binding has no explicit signature.
pub const MISSING_PUBLIC_SIGNATURE: DiagnosticCode = DiagnosticCode::new("FAI3003");
/// A binding's declared signature disagrees with its inferred type.
pub const SIGNATURE_MISMATCH: DiagnosticCode = DiagnosticCode::new("FAI3004");
/// A type is ambiguous (e.g. an unresolved numeric/Ord variable would generalize).
pub const AMBIGUOUS_TYPE: DiagnosticCode = DiagnosticCode::new("FAI3005");
/// Equality (`=`/`<>`) was used on a function-typed value.
pub const EQUALITY_ON_FUNCTION: DiagnosticCode = DiagnosticCode::new("FAI3006");
/// A contract (`example`/`forall`) body does not have type `Bool`.
pub const CONTRACT_NOT_BOOL: DiagnosticCode = DiagnosticCode::new("FAI3007");
/// A signature names an unknown type constructor.
pub const UNKNOWN_TYPE_CONSTRUCTOR: DiagnosticCode = DiagnosticCode::new("FAI3008");
/// Record field access is used, but records are not supported yet (M4).
pub const UNSUPPORTED_FIELD_ACCESS: DiagnosticCode = DiagnosticCode::new("FAI3009");

/// Diagnostic codes owned by the type system (the `FAI3xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo { code: TYPE_MISMATCH, title: "type mismatch", default_severity: Severity::Error },
    CodeInfo {
        code: OCCURS_CHECK,
        title: "infinite type (occurs check)",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: MISSING_PUBLIC_SIGNATURE,
        title: "missing public signature",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: SIGNATURE_MISMATCH,
        title: "signature disagrees with inferred type",
        default_severity: Severity::Error,
    },
    CodeInfo { code: AMBIGUOUS_TYPE, title: "ambiguous type", default_severity: Severity::Error },
    CodeInfo {
        code: EQUALITY_ON_FUNCTION,
        title: "equality on a function type",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: CONTRACT_NOT_BOOL,
        title: "contract is not Bool",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNKNOWN_TYPE_CONSTRUCTOR,
        title: "unknown type constructor",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNSUPPORTED_FIELD_ACCESS,
        title: "record field access not supported yet",
        default_severity: Severity::Error,
    },
];
