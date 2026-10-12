//! Pure package metadata, source identities, and span-preserving project manifests.
//!
//! Parsing and source planning take explicit inputs and perform no filesystem,
//! network, or compiler-database operations. Semantic models exclude source spans;
//! their document retains locations for diagnostics and comment-preserving edits.

mod diagnostic;
mod identity;
mod layout;
mod lock;
mod manifest;
mod model;
mod platform;
mod resolver;
mod version;

pub use diagnostic::{AUTHENTICATION_REQUIRED, UNSUPPORTED_SOURCE_LAYOUT, VENDOR_UNAVAILABLE};

pub use diagnostic::{
    CODES, COMPILER_MISMATCH, INPUTS_CHANGED, INVALID_LOCK, INVALID_MANIFEST, INVALID_PLATFORM,
    INVALID_SOURCE, INVALID_TARGET, OFFLINE_UNAVAILABLE, PACKAGE_CYCLE, RELEASE_MOVED,
    RESOLUTION_CONFLICT, SOURCE_INTEGRITY, SOURCE_UNAVAILABLE, STALE_LOCK, UNSUPPORTED_LOCK,
};
pub use identity::{
    DependencyAlias, GitBranch, GitPackage, GitRepository, GitRevision, PackageName, PackagePath,
};
pub use layout::{ExecutableSources, SourcePlan};
pub use lock::{
    BaselinePackage, Digest, LockContext, LockError, LockFile, LockLocator, LockSource,
    LockedBranch, LockedEdge, LockedEdgeKind, LockedGraph, LockedPackage,
};
pub use manifest::{FieldLocation, MANIFEST_NAME, ManifestDocument, ManifestKind};
pub use model::{
    ActiveConfiguration, BinaryTarget, Dependency, GitSelector, Manifest, NativeRequirements,
    Override, Package, PlatformSection, Workspace,
};
pub use platform::{PlatformPredicate, TargetPlatform};
pub use resolver::{
    CandidateSet, PackageCandidate, PackageKey, PackagePin, PackageRequirement, PackageSelector,
    PackageSource, ProviderFailure, RequirementOrigin, ResolutionConflict, ResolutionError,
    ResolutionPolicy, ResolutionRequest, ResolvedEdge, ResolvedGraph, ResolverProvider,
    matches_selector as candidate_satisfies, resolve_graph,
};
pub use version::{PackageVersion, VersionRequirement};

/// A malformed package identifier, source locator, path, or requirement.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MetadataError(pub(crate) String);

impl MetadataError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
