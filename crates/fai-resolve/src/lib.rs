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
pub mod intrinsics;
#[allow(unsafe_code)]
mod module;
#[allow(unsafe_code)]
mod scc;
#[cfg(test)]
mod tests;

pub use bodies::{ResolvedBodies, resolve};
pub use decls::{
    CtorInfo, InterfaceDecls, InterfaceInfo, TypeDeclInfo, TypeDecls, interface_decls, type_decls,
};
pub use ids::{AdtRef, CtorRef, DefId, InterfaceRef, LocalId, Res, is_upper, qualify};
pub use module::{
    DefInfo, DuplicateExport, Export, ExportKind, ModuleDefs, ModuleInterface, ModuleName,
    PRELUDE_MODULE, PreludeExports, duplicate_module_files, emit_duplicate_module_errors,
    emit_duplicate_prelude_export_errors, merge_auto_imports, module_defs, module_file,
    module_interface, module_name, prelude_exports, prelude_module_file, std_files,
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
/// More than one auto-imported module exports the same name (a warning).
pub const DUPLICATE_PRELUDE_EXPORT: DiagnosticCode = DiagnosticCode::new("FAI2013");
/// The `Prim` intrinsics module is referenced outside a standard-library module.
pub const INTRINSIC_OUTSIDE_STD: DiagnosticCode = DiagnosticCode::new("FAI2014");
/// A public surface (signature, alias body, or constructor field) exposes a
/// same-module private type.
pub const PRIVATE_TYPE_IN_PUBLIC_SIGNATURE: DiagnosticCode = DiagnosticCode::new("FAI2015");
/// A nested module's name collides with another module, type, interface, or
/// constructor declared in the same scope.
pub const MODULE_NAME_CONFLICT: DiagnosticCode = DiagnosticCode::new("FAI2016");
/// A qualified path names a module where a value or type is expected (it has no
/// trailing member).
pub const MODULE_AS_VALUE: DiagnosticCode = DiagnosticCode::new("FAI2017");
/// A constructor of an opaque type is referenced from another file.
pub const OPAQUE_CONSTRUCTOR: DiagnosticCode = DiagnosticCode::new("FAI2018");

/// Diagnostic codes owned by name resolution/visibility (the `FAI2xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: UNBOUND_NAME,
        title: "unbound name",
        default_severity: Severity::Error,
        explanation: "A name could not be resolved to any local, this module's top level, or the \
                      auto-imported prelude. Check for a typo, a missing binding, or a needed \
                      qualified `Module.name`.",
    },
    CodeInfo {
        code: AMBIGUOUS_NAME,
        title: "ambiguous name",
        default_severity: Severity::Error,
        explanation: "A bare name resolves to more than one definition. Disambiguate it with a \
                      qualified `Module.name`.",
    },
    CodeInfo {
        code: PRIVATE_REFERENCE,
        title: "reference to a private binding",
        default_severity: Severity::Error,
        explanation: "A qualified reference names a member that is not `public` in the target \
                      module, so it is not visible across files. Mark the member `public` (and \
                      give it a signature), or move the caller into the same file.",
    },
    CodeInfo {
        code: DUPLICATE_DEFINITION,
        title: "duplicate definition",
        default_severity: Severity::Error,
        explanation: "Two bindings in the same module scope share a name. Rename or remove one.",
    },
    CodeInfo {
        code: ORPHAN_SIGNATURE,
        title: "signature without a binding",
        default_severity: Severity::Error,
        explanation: "A type signature has no matching `let` binding of the same name. Add the \
                      binding or remove the signature.",
    },
    CodeInfo {
        code: MULTIPLE_SIGNATURES,
        title: "multiple signatures for one name",
        default_severity: Severity::Error,
        explanation: "A name has more than one type signature in the same scope. Keep one.",
    },
    CodeInfo {
        code: DUPLICATE_MODULE,
        title: "duplicate module name",
        default_severity: Severity::Error,
        explanation: "Two files declare the same top-level module name; module names must be \
                      unique across the workspace. The duplicated name is excluded from \
                      cross-module lookup until resolved.",
    },
    CodeInfo {
        code: UNRESOLVED_MODULE,
        title: "unresolved module",
        default_severity: Severity::Error,
        explanation: "A qualified path's leading segment names no module — neither a nested \
                      module in scope nor a workspace file module. Check the module name.",
    },
    CodeInfo {
        code: BINDING_VISIBILITY_MARKER,
        title: "visibility marker on a binding with a signature",
        default_severity: Severity::Error,
        explanation: "Visibility lives on the signature, so a `let` binding may not carry \
                      `public` when a signature already exists. Move `public` to the signature.",
    },
    CodeInfo {
        code: SHADOWS_PRELUDE,
        title: "binding shadows a prelude name",
        default_severity: Severity::Warning,
        explanation: "A binding reuses a name auto-imported from the prelude, hiding it in this \
                      scope. Rename the binding if the prelude name was intended.",
    },
    CodeInfo {
        code: DUPLICATE_BINDER,
        title: "duplicate forall binder",
        default_severity: Severity::Error,
        explanation: "A `forall` contract lists the same binder name twice. Give each binder a \
                      distinct name.",
    },
    CodeInfo {
        code: UNBOUND_CONSTRUCTOR,
        title: "unbound constructor",
        default_severity: Severity::Error,
        explanation: "An upper-case name in expression or pattern position is not a known data \
                      constructor. Check for a typo or a missing `type` declaration.",
    },
    CodeInfo {
        code: DUPLICATE_PRELUDE_EXPORT,
        title: "duplicate auto-imported export",
        default_severity: Severity::Warning,
        explanation: "More than one auto-imported module exports the same name; auto-imported \
                      modules must export disjoint names. (Contributor-facing: it concerns the \
                      standard library's own modules.)",
    },
    CodeInfo {
        code: INTRINSIC_OUTSIDE_STD,
        title: "intrinsics used outside the standard library",
        default_severity: Severity::Error,
        explanation: "The prelude-private `Prim.*` intrinsics are reachable only from \
                      standard-library modules. Use the public wrapper (e.g. `Int.toString`) \
                      instead.",
    },
    CodeInfo {
        code: PRIVATE_TYPE_IN_PUBLIC_SIGNATURE,
        title: "private type exposed by a public signature",
        default_severity: Severity::Error,
        explanation: "A public surface (a signature, alias body, or constructor field) names a \
                      same-file type that is not itself cross-file-accessible. Make the type \
                      public, or make the surface private.",
    },
    CodeInfo {
        code: MODULE_NAME_CONFLICT,
        title: "name already declared in this module",
        default_severity: Severity::Error,
        explanation: "A nested module's name collides with another module, type, interface, or \
                      constructor in the same scope (they share the upper-case namespace). \
                      Rename one.",
    },
    CodeInfo {
        code: MODULE_AS_VALUE,
        title: "module name used as a value or type",
        default_severity: Severity::Error,
        explanation: "A qualified path resolved to a module rather than a member. Name a member \
                      of the module (e.g. `Module.value`).",
    },
    CodeInfo {
        code: OPAQUE_CONSTRUCTOR,
        title: "constructor of an opaque type",
        default_severity: Severity::Error,
        explanation: "A constructor of an `opaque` type is referenced from another file. An \
                      opaque type exports its name but not its constructors, so it can only be \
                      built and matched through the functions its module provides. Use those \
                      operations instead of the constructor.",
    },
];
