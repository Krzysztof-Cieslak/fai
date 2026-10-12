//! Stable diagnostics for project metadata and source planning.

use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};

/// Invalid TOML, field shape, or required package metadata.
pub const INVALID_MANIFEST: DiagnosticCode = DiagnosticCode::new("FAI0800");
/// A manifest version that this parser cannot interpret.
pub const UNSUPPORTED_FORMAT: DiagnosticCode = DiagnosticCode::new("FAI0801");
/// Invalid source coordinates, source selectors, or package-contained paths.
pub const INVALID_SOURCE: DiagnosticCode = DiagnosticCode::new("FAI0802");
/// Invalid package versions or dependency requirements.
pub const INVALID_REQUIREMENT: DiagnosticCode = DiagnosticCode::new("FAI0803");
/// Local source selections do not satisfy a dependency requirement.
pub const RESOLUTION_CONFLICT: DiagnosticCode = DiagnosticCode::new("FAI0804");
/// Normal package dependencies form a cycle.
pub const PACKAGE_CYCLE: DiagnosticCode = DiagnosticCode::new("FAI0805");
/// The selected compiler does not satisfy project requirements.
pub const COMPILER_MISMATCH: DiagnosticCode = DiagnosticCode::new("FAI0809");
/// Invalid target descriptors or unsupported package platforms.
pub const INVALID_PLATFORM: DiagnosticCode = DiagnosticCode::new("FAI0810");
/// A declared source cannot be materialized by the current source provider.
pub const SOURCE_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new("FAI0811");
/// More than one replacement can apply to the same package and root.
pub const OVERLAPPING_OVERRIDES: DiagnosticCode = DiagnosticCode::new("FAI0816");
/// More than one dependency declaration can bind the same active alias.
pub const DUPLICATE_ALIAS: DiagnosticCode = DiagnosticCode::new("FAI0818");
/// Invalid executable entries or conflicting source ownership.
pub const INVALID_TARGET: DiagnosticCode = DiagnosticCode::new("FAI0819");

/// Package metadata diagnostics, shared by the parser and source planner.
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: RESOLUTION_CONFLICT,
        title: "dependency requirements conflict",
        default_severity: Severity::Error,
        explanation: "A selected package does not satisfy its dependency version requirements. Local paths and explicit replacements retain the declared compatibility constraints; select compatible sources or update the intended requirements.",
    },
    CodeInfo {
        code: PACKAGE_CYCLE,
        title: "package dependency cycle",
        default_severity: Severity::Error,
        explanation: "Normal package dependencies form a cycle. Move shared definitions into another package or remove an edge. A root's development dependencies are checked separately from its normal dependency node.",
    },
    CodeInfo {
        code: COMPILER_MISMATCH,
        title: "incompatible project compiler",
        default_severity: Severity::Error,
        explanation: "The selected compiler does not meet the package's compatible compiler requirement or the root's exact toolchain pin. Select the required installed compiler; compatibility requirements are not silently ignored.",
    },
    CodeInfo {
        code: SOURCE_UNAVAILABLE,
        title: "package source unavailable",
        default_severity: Severity::Error,
        explanation: "A declared package source is unavailable to the source provider. Local project loading supports path dependencies and explicit path replacements for Git sources. A Git source needs a materialized provider or a compatible local replacement.",
    },
    CodeInfo {
        code: INVALID_MANIFEST,
        title: "invalid package manifest",
        default_severity: Severity::Error,
        explanation: "A project manifest contains malformed TOML, an unknown field, a missing required field, or a value of the wrong type. Versioned manifests declare `manifest-version = 1` and a package, a workspace, or both. Personal manifests contain only their format version and root overrides.",
    },
    CodeInfo {
        code: UNSUPPORTED_FORMAT,
        title: "unsupported manifest version",
        default_severity: Severity::Error,
        explanation: "The declared manifest format is not supported by this tool. Use a supported format or a compiler that understands the declared version; an unfamiliar format is never silently interpreted as an older one.",
    },
    CodeInfo {
        code: INVALID_SOURCE,
        title: "invalid package source",
        default_severity: Severity::Error,
        explanation: "A package source has an invalid repository locator, revision, branch, package-relative path, or combination of selectors. Git dependencies choose exactly one release requirement, full commit, or branch. A package-relative source path cannot escape its owning package.",
    },
    CodeInfo {
        code: INVALID_REQUIREMENT,
        title: "invalid package version requirement",
        default_severity: Severity::Error,
        explanation: "A package version or requirement is malformed. Versions have three numeric components and an optional prerelease. A bare dependency version is exact; `^` and `~` request compatible ranges. Commas intersect requirements and `||` joins alternatives. Partial versions and build metadata are not accepted.",
    },
    CodeInfo {
        code: INVALID_PLATFORM,
        title: "invalid or unsupported package platform",
        default_severity: Severity::Error,
        explanation: "A target triple or platform predicate is invalid, or the package does not support the requested target. Platform predicates contain exact operating-system, architecture, and environment values and must contain at least one condition.",
    },
    CodeInfo {
        code: OVERLAPPING_OVERRIDES,
        title: "overlapping package overrides",
        default_severity: Severity::Error,
        explanation: "Two overrides in the same manifest can replace the same logical package for the same workspace member. Their order does not choose a winner. Use disjoint member scopes or a single replacement.",
    },
    CodeInfo {
        code: DUPLICATE_ALIAS,
        title: "duplicate active dependency alias",
        default_severity: Severity::Error,
        explanation: "A dependency alias has multiple declarations that can be active on one target, including normal/development and common/platform declarations. Use distinct aliases or disjoint platform conditions; platform declarations do not implicitly replace common dependencies.",
    },
    CodeInfo {
        code: INVALID_TARGET,
        title: "invalid package source target",
        default_severity: Severity::Error,
        explanation: "A package has duplicate executable names, invalid source-root ownership, a missing entry file, an invalid default executable, or an entry outside its package boundary. Executable and test sources are separate from the shared library surface.",
    },
];
