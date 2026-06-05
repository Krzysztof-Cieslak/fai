//! Name resolution, the module graph, and visibility for Fai.
//!
//! This crate pairs signatures with bindings, resolves every reference in a body
//! to a local, a definition, or a builtin, builds the module interface
//! (`module_exports`) used by the cross-module firewall, and computes the
//! per-module strongly-connected components that bound inference. It emits
//! diagnostics in the `FAI2xxx` range; every code is catalogued in [`CODES`].
//!
//! Skeleton: the queries land incrementally across M2.

mod ids;
// salsa's `tracked` macro emits `unsafe impl`s; these modules are the only place
// in the crate that carries them (we write no `unsafe` by hand), mirroring
// `fai-db`/`fai-syntax`.
#[allow(unsafe_code)]
mod bodies;
#[allow(unsafe_code)]
mod decls;
#[allow(unsafe_code)]
mod module;
pub mod prelude;
#[allow(unsafe_code)]
mod scc;
#[cfg(test)]
mod tests;

pub use bodies::{ResolvedBodies, resolve};
pub use decls::{CtorInfo, TypeDeclInfo, TypeDecls, type_decls};
pub use ids::{AdtRef, CtorRef, DefId, LocalId, Res, is_upper};
pub use module::{
    DefInfo, Export, ModuleDefs, ModuleInterface, ModuleName, duplicate_module_files,
    emit_duplicate_module_errors, module_defs, module_file, module_interface, module_name,
    prelude_file,
};
pub use scc::{ModuleSccs, Scc, def_deps, module_sccs};

use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};

/// A name could not be resolved to any binding, parameter, or builtin.
pub const UNBOUND_NAME: DiagnosticCode = DiagnosticCode::new("FAI2001");
/// A bare name resolves to more than one definition.
pub const AMBIGUOUS_NAME: DiagnosticCode = DiagnosticCode::new("FAI2002");
/// A qualified reference names a binding that is not `public`.
pub const PRIVATE_REFERENCE: DiagnosticCode = DiagnosticCode::new("FAI2003");
/// Two bindings in one module share a name.
pub const DUPLICATE_DEFINITION: DiagnosticCode = DiagnosticCode::new("FAI2004");
/// A signature has no matching binding.
pub const ORPHAN_SIGNATURE: DiagnosticCode = DiagnosticCode::new("FAI2005");
/// A name has more than one signature.
pub const MULTIPLE_SIGNATURES: DiagnosticCode = DiagnosticCode::new("FAI2006");
/// Two modules declare the same top-level name.
pub const DUPLICATE_MODULE: DiagnosticCode = DiagnosticCode::new("FAI2007");
/// A qualified reference names a module that does not exist.
pub const UNRESOLVED_MODULE: DiagnosticCode = DiagnosticCode::new("FAI2008");
/// A binding carries a visibility marker when a signature already exists.
pub const BINDING_VISIBILITY_MARKER: DiagnosticCode = DiagnosticCode::new("FAI2009");
/// A binding shadows a prelude name (a warning).
pub const SHADOWS_PRELUDE: DiagnosticCode = DiagnosticCode::new("FAI2010");
/// A `forall` repeats a binder name.
pub const DUPLICATE_BINDER: DiagnosticCode = DiagnosticCode::new("FAI2011");
/// An upper-case name is not a known data constructor.
pub const UNBOUND_CONSTRUCTOR: DiagnosticCode = DiagnosticCode::new("FAI2012");

/// Diagnostic codes owned by name resolution/visibility (the `FAI2xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo { code: UNBOUND_NAME, title: "unbound name", default_severity: Severity::Error },
    CodeInfo { code: AMBIGUOUS_NAME, title: "ambiguous name", default_severity: Severity::Error },
    CodeInfo {
        code: PRIVATE_REFERENCE,
        title: "reference to a private binding",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: DUPLICATE_DEFINITION,
        title: "duplicate definition",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: ORPHAN_SIGNATURE,
        title: "signature without a binding",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: MULTIPLE_SIGNATURES,
        title: "multiple signatures for one name",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: DUPLICATE_MODULE,
        title: "duplicate module name",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNRESOLVED_MODULE,
        title: "unresolved module",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: BINDING_VISIBILITY_MARKER,
        title: "visibility marker on a binding with a signature",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: SHADOWS_PRELUDE,
        title: "binding shadows a prelude name",
        default_severity: Severity::Warning,
    },
    CodeInfo {
        code: DUPLICATE_BINDER,
        title: "duplicate forall binder",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNBOUND_CONSTRUCTOR,
        title: "unbound constructor",
        default_severity: Severity::Error,
    },
];
