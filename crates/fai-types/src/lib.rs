//! Hindley–Milner type inference for the Fai functional core.
//!
//! This crate owns the type representation (a flat per-scheme arena),
//! unification, let-generalization, and the per-def/SCC `infer` query. It builds
//! on `fai-resolve` (name resolution, the module graph, and SCCs) and emits
//! diagnostics in the `FAI3xxx` range; every code is catalogued in [`CODES`].
//!
//! Skeleton: the representation and queries land incrementally across M2.

mod contracts;
mod exhaustive;
mod infer;
mod lower;
#[allow(unsafe_code)]
mod query;
pub mod std_lib;
mod ty;

#[cfg(test)]
mod edge_tests;
#[cfg(test)]
mod prop_tests;
#[cfg(test)]
mod tests;

pub use contracts::check_contracts;
pub use infer::{
    Constraint, Env, InferCtx, SccEnv, SccInference, SolveTy, UnifyResult, Walker, contract_env,
    declared_scheme, error_scheme, generalize, infer_scc,
};
pub use lower::{LowerVars, lower_signature, lower_type};
pub use query::{
    BodyTypes, SccTypes, body_types, check_file, constructor_scheme, def_local_types, def_type,
    infer_scc_query,
};
pub use ty::{
    Con, RecordRow, RowEnd, RowVarId, Scheme, Ty, TyVarId, VarNames, render, render_canonical,
    render_scheme,
};

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
/// Record field access is used, but records are not supported yet (records land
/// with structural records).
pub const UNSUPPORTED_FIELD_ACCESS: DiagnosticCode = DiagnosticCode::new("FAI3009");
/// A record literal or type repeats a field label (the lacks constraint).
pub const DUPLICATE_FIELD: DiagnosticCode = DiagnosticCode::new("FAI3010");
/// A constructor pattern or application has the wrong number of arguments.
pub const CONSTRUCTOR_ARITY: DiagnosticCode = DiagnosticCode::new("FAI3011");
/// A type constructor is applied to the wrong number of arguments (a kind error).
pub const TYPE_ARITY: DiagnosticCode = DiagnosticCode::new("FAI3012");
/// A transparent type alias refers to itself (directly or transitively).
pub const RECURSIVE_ALIAS: DiagnosticCode = DiagnosticCode::new("FAI3013");
/// A `match` does not cover every possible value.
pub const NON_EXHAUSTIVE_MATCH: DiagnosticCode = DiagnosticCode::new("FAI4001");
/// A `match` arm can never be reached (an earlier arm already covers it).
pub const UNREACHABLE_ARM: DiagnosticCode = DiagnosticCode::new("FAI4002");

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
    CodeInfo {
        code: DUPLICATE_FIELD,
        title: "duplicate record field label",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: CONSTRUCTOR_ARITY,
        title: "wrong number of constructor arguments",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: TYPE_ARITY,
        title: "wrong number of type arguments",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: RECURSIVE_ALIAS,
        title: "recursive type alias",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: NON_EXHAUSTIVE_MATCH,
        title: "non-exhaustive match",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNREACHABLE_ARM,
        title: "unreachable match arm",
        default_severity: Severity::Error,
    },
];
