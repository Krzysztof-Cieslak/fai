//! Hindley–Milner type inference for the Fai functional core.
//!
//! This crate owns the type representation (a flat per-scheme arena),
//! unification, let-generalization, and the per-def/SCC `infer` query. It builds
//! on `fai-resolve` (name resolution, the module graph, and SCCs) and emits
//! diagnostics in the `FAI3xxx` range; every code is catalogued in [`CODES`].
//!
//! Skeleton: the representation and queries land incrementally across M2.

mod contracts;
pub mod evidence;
mod exhaustive;
mod infer;
mod lower;
pub mod perf;
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
pub use evidence::{EvidenceReq, evidence_count, evidence_requirements};
pub use infer::{
    Constraint, Env, InferCtx, SccEnv, SccInference, SolveTy, UnifyResult, Walker, contract_env,
    declared_scheme, error_scheme, generalize, infer_scc,
};
pub use lower::{LowerVars, expand_alias_ty, lower_signature, lower_type};
pub use query::{
    BodyTypes, SccTypes, body_types, check_file, constructor_scheme, contract_body_types,
    declared_or_inferred_scheme, def_effect, def_local_types, def_type, infer_scc_query,
};
pub use ty::{
    Con, EffEnd, EffRowVarId, EffectRow, RecordRow, RowEnd, RowVarId, Scheme, Ty, TyVarId,
    VarNames, render, render_canonical, render_scheme,
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
/// A method is accessed/implemented that the interface does not declare.
pub const UNKNOWN_METHOD: DiagnosticCode = DiagnosticCode::new("FAI3014");
/// An interface instance does not implement exactly the declared methods.
pub const INSTANCE_METHOD_SET: DiagnosticCode = DiagnosticCode::new("FAI3015");
/// `{ Name with … }` names something that is not an interface.
pub const NOT_AN_INTERFACE: DiagnosticCode = DiagnosticCode::new("FAI3016");
/// `{ Name with … }` instantiates a sealed built-in interface (`Num`/`Eq`/`Ord`).
pub const SEALED_INTERFACE: DiagnosticCode = DiagnosticCode::new("FAI3017");
/// The representation of an opaque type is accessed from another file (a field
/// access, record construction, or `{ r with … }` update).
pub const OPAQUE_ACCESS: DiagnosticCode = DiagnosticCode::new("FAI3018");
/// An interface type parameter is used as both a type and an effect row across
/// its methods (an ill-kinded parameter).
pub const INTERFACE_PARAM_KIND: DiagnosticCode = DiagnosticCode::new("FAI3019");
/// An interface argument has the wrong kind — an effect row where a type is
/// expected (or vice versa), or an effect row written outside an interface
/// argument.
pub const EFFECT_ARG_KIND: DiagnosticCode = DiagnosticCode::new("FAI3020");
/// A `match` does not cover every possible value.
pub const NON_EXHAUSTIVE_MATCH: DiagnosticCode = DiagnosticCode::new("FAI4001");
/// A `match` arm can never be reached (an earlier arm already covers it).
pub const UNREACHABLE_ARM: DiagnosticCode = DiagnosticCode::new("FAI4002");
/// A binding's declared effect row disagrees with the effect inferred from its
/// body (the capabilities it actually uses).
pub const EFFECT_MISMATCH: DiagnosticCode = DiagnosticCode::new("FAI5001");

/// Diagnostic codes owned by the type system (the `FAI3xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: TYPE_MISMATCH,
        title: "type mismatch",
        default_severity: Severity::Error,
        explanation: "Two types that had to be equal could not be unified (e.g. an `Int` used \
                      where a `String` was expected). The message shows the expected and actual \
                      types. Note there is no implicit `Int`/`Float` coercion — use \
                      `Int.toFloat`/`Float.toInt`.",
    },
    CodeInfo {
        code: OCCURS_CHECK,
        title: "infinite type (occurs check)",
        default_severity: Severity::Error,
        explanation: "Unification would make a type contain itself (an infinite type), usually \
                      from a self-application or a mis-shaped recursive definition. Add a \
                      signature or fix the recursion.",
    },
    CodeInfo {
        code: MISSING_PUBLIC_SIGNATURE,
        title: "missing public signature",
        default_severity: Severity::Error,
        explanation: "Every `public` binding must carry an explicit type signature (so a \
                      module's API is readable from its signatures alone). Add the signature on \
                      the line above the binding.",
    },
    CodeInfo {
        code: SIGNATURE_MISMATCH,
        title: "signature disagrees with inferred type",
        default_severity: Severity::Error,
        explanation: "A binding's declared signature does not match the type inferred from its \
                      body (signatures are checked, not trusted). Fix the body or the signature.",
    },
    CodeInfo {
        code: AMBIGUOUS_TYPE,
        title: "ambiguous type",
        default_severity: Severity::Error,
        explanation: "Inference could not determine a type (e.g. an unresolved numeric or \
                      constrained variable that would escape without a signature). Add a type \
                      annotation or a conversion.",
    },
    CodeInfo {
        code: EQUALITY_ON_FUNCTION,
        title: "equality on a function type",
        default_severity: Severity::Error,
        explanation: "`=`/`<>` (and ordering) are structural and undefined on function-typed \
                      values. Compare the results of applying the functions instead.",
    },
    CodeInfo {
        code: CONTRACT_NOT_BOOL,
        title: "contract is not Bool",
        default_severity: Severity::Error,
        explanation: "An `example`/`forall` contract body must have type `Bool`. Make the body a \
                      boolean expression (often an equality).",
    },
    CodeInfo {
        code: UNKNOWN_TYPE_CONSTRUCTOR,
        title: "unknown type constructor",
        default_severity: Severity::Error,
        explanation: "A type name in a signature or declaration is not a known built-in, \
                      in-scope, prelude, or qualified type. Check the spelling or qualify it.",
    },
    CodeInfo {
        code: UNSUPPORTED_FIELD_ACCESS,
        title: "record field access not supported yet",
        default_severity: Severity::Error,
        explanation: "A record field access shape is not yet supported by the type checker. \
                      (Retired in current builds; kept reserved so the code is never reused.)",
    },
    CodeInfo {
        code: DUPLICATE_FIELD,
        title: "duplicate record field label",
        default_severity: Severity::Error,
        explanation: "A record type or literal lists the same field label twice. Records have no \
                      duplicate labels; remove the repeat.",
    },
    CodeInfo {
        code: CONSTRUCTOR_ARITY,
        title: "wrong number of constructor arguments",
        default_severity: Severity::Error,
        explanation: "A data constructor was applied to the wrong number of arguments. Supply \
                      exactly the fields the constructor declares.",
    },
    CodeInfo {
        code: TYPE_ARITY,
        title: "wrong number of type arguments",
        default_severity: Severity::Error,
        explanation: "A type constructor or interface was applied to the wrong number of type \
                      arguments. Match the declared parameter count.",
    },
    CodeInfo {
        code: RECURSIVE_ALIAS,
        title: "recursive type alias",
        default_severity: Severity::Error,
        explanation: "A transparent `type` alias refers to itself (directly or indirectly); \
                      aliases must be acyclic. Use a discriminated union for a recursive type.",
    },
    CodeInfo {
        code: UNKNOWN_METHOD,
        title: "unknown interface method",
        default_severity: Severity::Error,
        explanation: "An interface instance defines a method the interface does not declare. \
                      Match the interface's method set.",
    },
    CodeInfo {
        code: INSTANCE_METHOD_SET,
        title: "interface instance method set mismatch",
        default_severity: Severity::Error,
        explanation: "An interface instance does not implement exactly the interface's methods \
                      (some missing or extra). Provide each declared method once.",
    },
    CodeInfo {
        code: NOT_AN_INTERFACE,
        title: "not an interface",
        default_severity: Severity::Error,
        explanation: "An instance `{ Name with … }` names something that is not an interface. \
                      Use a declared interface name.",
    },
    CodeInfo {
        code: SEALED_INTERFACE,
        title: "sealed built-in interface cannot be instantiated",
        default_severity: Severity::Error,
        explanation: "The operator interfaces (`Num`/`Eq`/`Ord`) are sealed to their built-in \
                      instances and cannot be instantiated by user code.",
    },
    CodeInfo {
        code: OPAQUE_ACCESS,
        title: "access to an opaque type's representation",
        default_severity: Severity::Error,
        explanation: "An opaque type's representation (its record fields or alias body) is \
                      accessed from another file — a field access, record construction, or \
                      `{ r with … }` update. An opaque type exports its name but not its \
                      structure, so build and inspect its values through the functions its \
                      module provides.",
    },
    CodeInfo {
        code: INTERFACE_PARAM_KIND,
        title: "interface parameter used as both a type and an effect",
        default_severity: Severity::Error,
        explanation: "An interface type parameter (`'a`) is used in type position in one method \
                      and as an effect row (after `/`) in another. A parameter is one kind or the \
                      other — give the type use and the effect use separate parameters.",
    },
    CodeInfo {
        code: EFFECT_ARG_KIND,
        title: "wrong kind of interface argument",
        default_severity: Severity::Error,
        explanation: "An interface argument has the wrong kind: an effect row (`{ Console }`) was \
                      supplied for a type parameter, a type for an effect parameter, or an effect \
                      row was written somewhere other than an interface argument. Supply the kind \
                      the parameter expects.",
    },
    CodeInfo {
        code: NON_EXHAUSTIVE_MATCH,
        title: "non-exhaustive match",
        default_severity: Severity::Error,
        explanation: "A `match` does not cover every possible value of the scrutinee. Add the \
                      missing arms, or a `_` catch-all.",
    },
    CodeInfo {
        code: UNREACHABLE_ARM,
        title: "unreachable match arm",
        default_severity: Severity::Error,
        explanation: "A `match` arm can never be reached because earlier arms already cover its \
                      values. Remove or reorder it.",
    },
    CodeInfo {
        code: EFFECT_MISMATCH,
        title: "effect disagrees with inferred effect",
        default_severity: Severity::Error,
        explanation: "A binding's declared effect row (the capabilities after `/`) does not match \
                      the effect inferred from its body — it either performs a capability the \
                      signature omits, or declares one it never uses. Fix the body or the \
                      declared effect.",
    },
];
