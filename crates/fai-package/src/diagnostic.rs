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
/// A selected graph is missing or stale under locked operation.
pub const STALE_LOCK: DiagnosticCode = DiagnosticCode::new("FAI0806");
/// The lock format or resolution policy is not supported.
pub const UNSUPPORTED_LOCK: DiagnosticCode = DiagnosticCode::new("FAI0807");
/// Lock references or graph structure are inconsistent.
pub const INVALID_LOCK: DiagnosticCode = DiagnosticCode::new("FAI0808");
/// The selected compiler does not satisfy project requirements.
pub const COMPILER_MISMATCH: DiagnosticCode = DiagnosticCode::new("FAI0809");
/// Invalid target descriptors or unsupported package platforms.
pub const INVALID_PLATFORM: DiagnosticCode = DiagnosticCode::new("FAI0810");
/// A declared source cannot be materialized by the current source provider.
pub const SOURCE_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new("FAI0811");
/// Git credentials are unavailable or rejected.
pub const AUTHENTICATION_REQUIRED: DiagnosticCode = DiagnosticCode::new("FAI0812");
/// Required metadata or content is not available offline.
pub const OFFLINE_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new("FAI0813");
/// Immutable source bytes disagree with their expected identity.
pub const SOURCE_INTEGRITY: DiagnosticCode = DiagnosticCode::new("FAI0814");
/// A previously accepted release now points at another commit.
pub const RELEASE_MOVED: DiagnosticCode = DiagnosticCode::new("FAI0815");
/// A metadata transaction observed concurrent changes.
pub const INPUTS_CHANGED: DiagnosticCode = DiagnosticCode::new("FAI0825");
/// The source tree requires unsupported external content or link semantics.
pub const UNSUPPORTED_SOURCE_LAYOUT: DiagnosticCode = DiagnosticCode::new("FAI0827");
/// A required vendored source has no matching verified entry.
pub const VENDOR_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new("FAI0820");
/// Publication inputs, version intent, or portability are invalid.
pub const INVALID_PUBLICATION: DiagnosticCode = DiagnosticCode::new("FAI0821");
/// Candidate consumer or development validation failed.
pub const PUBLISH_VALIDATION: DiagnosticCode = DiagnosticCode::new("FAI0822");
/// Publication could not atomically push its prepared refs.
pub const PUBLISH_CONFLICT: DiagnosticCode = DiagnosticCode::new("FAI0823");
/// A prepared release must be resumed or explicitly replanned.
pub const PUBLISH_PENDING: DiagnosticCode = DiagnosticCode::new("FAI0824");
/// More than one replacement can apply to the same package and root.
pub const OVERLAPPING_OVERRIDES: DiagnosticCode = DiagnosticCode::new("FAI0816");
/// More than one dependency declaration can bind the same active alias.
pub const DUPLICATE_ALIAS: DiagnosticCode = DiagnosticCode::new("FAI0818");
/// Invalid executable entries or conflicting source ownership.
pub const INVALID_TARGET: DiagnosticCode = DiagnosticCode::new("FAI0819");

/// Package metadata diagnostics, shared by the parser and source planner.
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: INVALID_PUBLICATION,
        title: "invalid publication inputs",
        default_severity: Severity::Error,
        explanation: "Publishing requires clean committed source, explicit version intent, portable dependency declarations, and an unambiguous Git destination. Correct the selected package metadata or publication arguments before retrying.",
    },
    CodeInfo {
        code: PUBLISH_VALIDATION,
        title: "release candidate validation failed",
        default_severity: Severity::Error,
        explanation: "The candidate release did not pass its independent normal-consumer and development-root validation. Fix the underlying source, type, contract, or dependency errors before publishing.",
    },
    CodeInfo {
        code: PUBLISH_CONFLICT,
        title: "atomic publication failed",
        default_severity: Severity::Error,
        explanation: "The prepared branch and tags could not be atomically published, or a release tag already identifies another commit. Never force-move release tags. Resolve destination rules or access and resume the prepared release.",
    },
    CodeInfo {
        code: PUBLISH_PENDING,
        title: "prepared publication needs recovery",
        default_severity: Severity::Error,
        explanation: "A publication receipt records a prepared commit and tag set. Use publish --resume to complete that exact release. Changed source or refs require explicit replanning rather than another implicit version bump.",
    },
    CodeInfo {
        code: VENDOR_UNAVAILABLE,
        title: "vendored source unavailable",
        default_severity: Severity::Error,
        explanation: "A required source has no matching verified vendor entry or its bytes changed. Refresh the vendor snapshot, or explicitly use --no-vendor to restore through the normal source provider. Configured vendoring never silently falls through to the network.",
    },
    CodeInfo {
        code: AUTHENTICATION_REQUIRED,
        title: "Git authentication required",
        default_severity: Severity::Error,
        explanation: "Git credentials are unavailable or repository access was rejected. Configure the installed Git credential helper or SSH access, then retry. Credentials are not stored in package manifests, lockfiles, or diagnostic URLs.",
    },
    CodeInfo {
        code: UNSUPPORTED_SOURCE_LAYOUT,
        title: "unsupported package source layout",
        default_severity: Severity::Error,
        explanation: "The package tree contains external-content dependencies or nonportable entries such as Git submodules, LFS pointers, or escaping/cyclic links. Package source must be fully available inside its selected tree.",
    },
    CodeInfo {
        code: STALE_LOCK,
        title: "selected locked graph is stale",
        default_severity: Severity::Error,
        explanation: "The selected root/platform graph is missing or its manifest, replacement, or shared-baseline inputs changed. Prepare that graph without --locked before using it in locked or frozen mode. Unselected stale graphs do not require updates.",
    },
    CodeInfo {
        code: UNSUPPORTED_LOCK,
        title: "unsupported lock format",
        default_severity: Severity::Error,
        explanation: "The lockfile format or resolver policy version is not supported by this compiler. Use a compatible compiler or explicitly regenerate the lock; do not silently reinterpret another version.",
    },
    CodeInfo {
        code: INVALID_LOCK,
        title: "invalid locked graph",
        default_severity: Severity::Error,
        explanation: "A lockfile is malformed or contains inconsistent identities, references, dependency aliases, reachability, or cycles. Restore a valid lock or explicitly regenerate it. A plausible fingerprint does not replace structural validation.",
    },
    CodeInfo {
        code: OFFLINE_UNAVAILABLE,
        title: "source unavailable offline",
        default_severity: Severity::Error,
        explanation: "Required source bytes or version metadata are absent from the local cache or vendor tree. Prepare them online before using --offline or --frozen. A valid locked selection is never downgraded to make an offline build succeed.",
    },
    CodeInfo {
        code: SOURCE_INTEGRITY,
        title: "package source integrity mismatch",
        default_severity: Severity::Error,
        explanation: "Package bytes do not match the expected immutable checksum or source identity. Restore the exact source; modified cache or vendor content cannot replace a locked package.",
    },
    CodeInfo {
        code: RELEASE_MOVED,
        title: "package release tag moved",
        default_severity: Severity::Error,
        explanation: "A previously accepted package version now points at a different commit. Release mappings are immutable once observed. Publish a new version rather than replacing a release tag.",
    },
    CodeInfo {
        code: INPUTS_CHANGED,
        title: "package inputs changed during preparation",
        default_severity: Severity::Error,
        explanation: "A manifest or lock changed while a metadata operation was preparing its result. The newer inputs are retained. Retry from the current state; a pending journal resumes only when its recorded before/after contents still match.",
    },
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
