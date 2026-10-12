//! Pure package metadata, source identities, and span-preserving project manifests.
//!
//! Parsing and source planning take explicit inputs and perform no filesystem,
//! network, or compiler-database operations. Semantic models exclude source spans;
//! their document retains locations for diagnostics and comment-preserving edits.

mod diagnostic;
mod identity;
mod layout;
mod manifest;
mod model;
mod platform;
mod version;

pub use diagnostic::{
    CODES, COMPILER_MISMATCH, INVALID_MANIFEST, INVALID_PLATFORM, INVALID_SOURCE, INVALID_TARGET,
    PACKAGE_CYCLE, RESOLUTION_CONFLICT, SOURCE_UNAVAILABLE,
};
pub use identity::{
    DependencyAlias, GitBranch, GitPackage, GitRepository, GitRevision, PackageName, PackagePath,
};
pub use layout::{ExecutableSources, SourcePlan};
pub use manifest::{FieldLocation, MANIFEST_NAME, ManifestDocument, ManifestKind};
pub use model::{
    ActiveConfiguration, BinaryTarget, Dependency, GitSelector, Manifest, NativeRequirements,
    Override, Package, PlatformSection, Workspace,
};
pub use platform::{PlatformPredicate, TargetPlatform};
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
