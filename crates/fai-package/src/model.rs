//! Position-independent manifest values and active target configuration.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    DependencyAlias, GitBranch, GitPackage, GitRevision, MetadataError, PackageName, PackagePath,
    PackageVersion, PlatformPredicate, TargetPlatform, VersionRequirement,
};

/// A package's checked identity and source conventions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Package {
    /// Local display/selection name, independent of source identity.
    pub name: PackageName,
    /// Declared source version.
    pub version: PackageVersion,
    /// Whether this package may be published; private workspace projects opt out.
    pub publish: bool,
    /// Compatible compiler releases; absent manifests use the wildcard requirement.
    pub fai_version: VersionRequirement,
    /// Whether compiler compatibility was explicitly declared for publication.
    pub explicit_fai_version: bool,
    /// Shared library source root.
    pub source_dir: PackagePath,
    /// Additional tests' source root.
    pub test_dir: PackagePath,
    /// Whether a Main.fai entry is inferred under the shared source root.
    pub auto_main: bool,
    /// Preferred executable name.
    pub default_run: Option<PackageName>,
    /// Supported target predicates, or unrestricted when empty.
    pub supported_platforms: Vec<PlatformPredicate>,
    /// Optional descriptive metadata.
    pub description: Option<String>,
    /// Optional license metadata.
    pub license: Option<String>,
    /// Optional informational repository URL; does not select package identity.
    pub repository: Option<String>,
}

/// Explicit workspace member patterns, relative to the workspace manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Workspace {
    /// Member paths/globs; a colocated root package is also a member.
    pub members: Vec<String>,
}

/// An executable's entry file. Its source scope excludes other executable entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BinaryTarget {
    /// Package-local executable selection name.
    pub name: PackageName,
    /// Entry file relative to the package directory.
    pub path: PackagePath,
}

/// A selector on a single logical Git package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum GitSelector {
    /// A requirement over canonical release tags.
    Version(VersionRequirement),
    /// A complete immutable commit identifier.
    Revision(GitRevision),
    /// A branch whose selected commit is retained in a lock.
    Branch(GitBranch),
}

/// A checked dependency declaration; aliases are stored by the owning manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Dependency {
    /// A Git source and exactly one selector.
    Git {
        /// Logical repository/subdirectory identity.
        package: GitPackage,
        /// Version, commit, or branch choice.
        selector: GitSelector,
    },
    /// A local package. Its path may lie outside the declaring package.
    Path {
        /// Path relative to the declaring manifest, or an explicit absolute path.
        path: String,
        /// Optional compatibility check against that package's declared version.
        version: Option<VersionRequirement>,
    },
}

/// System linker/loader requirements, with authored ordering retained.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NativeRequirements {
    /// Native library search directories.
    pub library_dirs: Vec<String>,
    /// Native library names in link order.
    pub libraries: Vec<String>,
    /// Native object/archive paths.
    pub objects: Vec<String>,
    /// Apple framework names.
    pub frameworks: Vec<String>,
}

impl NativeRequirements {
    pub(crate) fn append(&mut self, other: &Self) {
        self.library_dirs.extend_from_slice(&other.library_dirs);
        self.libraries.extend_from_slice(&other.libraries);
        self.objects.extend_from_slice(&other.objects);
        self.frameworks.extend_from_slice(&other.frameworks);
    }
}

/// Target-selected additions to source and dependency environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformSection {
    /// Exact target-property conjunction.
    pub when: PlatformPredicate,
    /// Additional shared source roots.
    pub source_dirs: Vec<PackagePath>,
    /// Additional test source roots.
    pub test_source_dirs: Vec<PackagePath>,
    /// Normal dependencies added on matching targets.
    pub dependencies: BTreeMap<DependencyAlias, Dependency>,
    /// Development dependencies added on matching targets.
    pub dev_dependencies: BTreeMap<DependencyAlias, Dependency>,
    /// Ordered native additions.
    pub native: NativeRequirements,
}

/// A root's explicit replacement of a logical Git package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Override {
    /// Original logical identity, preserved by replacement.
    pub replace: GitPackage,
    /// Actual replacement source.
    pub with: Dependency,
    /// Exact member paths, or all roots when absent.
    pub members: Option<BTreeSet<PackagePath>>,
}

impl Override {
    /// Whether two entries could replace the same package for the same root.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.replace == other.replace
            && match (&self.members, &other.members) {
                (Some(left), Some(right)) => !left.is_disjoint(right),
                _ => true,
            }
    }
}

/// Validated, position-independent project declarations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Manifest {
    /// Package metadata, absent for a virtual workspace or personal overlay.
    pub package: Option<Package>,
    /// Workspace membership.
    pub workspace: Option<Workspace>,
    /// Shared source dependencies.
    pub dependencies: BTreeMap<DependencyAlias, Dependency>,
    /// Additional development dependencies.
    pub dev_dependencies: BTreeMap<DependencyAlias, Dependency>,
    /// Explicit executable targets.
    pub binaries: Vec<BinaryTarget>,
    /// Common native requirements.
    pub native: NativeRequirements,
    /// Authored target-selected additions.
    pub platforms: Vec<PlatformSection>,
    /// Exact active-root compiler pin.
    pub toolchain: Option<PackageVersion>,
    /// Active-root metadata preparation matrix.
    pub lock_targets: Vec<TargetPlatform>,
    /// Root-controlled substitutions.
    pub overrides: Vec<Override>,
    /// Root-local vendored source directory.
    pub vendor_dir: Option<PackagePath>,
}

/// Effective declarations for one platform, excluding all location metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveConfiguration {
    /// Shared source roots.
    pub source_dirs: Vec<PackagePath>,
    /// Additional test source roots.
    pub test_source_dirs: Vec<PackagePath>,
    /// Effective normal dependency environment.
    pub dependencies: BTreeMap<DependencyAlias, Dependency>,
    /// Effective development-only environment.
    pub dev_dependencies: BTreeMap<DependencyAlias, Dependency>,
    /// Native configuration in common-then-platform order.
    pub native: NativeRequirements,
}

impl Manifest {
    /// Selects declarations for a target without performing I/O or dependency solving.
    pub fn configuration(
        &self,
        target: &TargetPlatform,
    ) -> Result<ActiveConfiguration, MetadataError> {
        let package = self
            .package
            .as_ref()
            .ok_or_else(|| MetadataError::new("a package is required for target configuration"))?;
        if !package.supported_platforms.is_empty()
            && !package.supported_platforms.iter().any(|predicate| predicate.matches(target))
        {
            return Err(MetadataError::new(format!(
                "package {} does not support {target}",
                package.name
            )));
        }
        let mut active = ActiveConfiguration {
            source_dirs: vec![package.source_dir.clone()],
            test_source_dirs: vec![package.test_dir.clone()],
            dependencies: self.dependencies.clone(),
            dev_dependencies: self.dev_dependencies.clone(),
            native: self.native.clone(),
        };
        for platform in self.platforms.iter().filter(|section| section.when.matches(target)) {
            active.source_dirs.extend_from_slice(&platform.source_dirs);
            active.test_source_dirs.extend_from_slice(&platform.test_source_dirs);
            for (alias, dependency) in &platform.dependencies {
                if active.dependencies.insert(alias.clone(), dependency.clone()).is_some() {
                    return Err(MetadataError::new(format!(
                        "duplicate active dependency alias {alias}"
                    )));
                }
            }
            for (alias, dependency) in &platform.dev_dependencies {
                if active.dev_dependencies.insert(alias.clone(), dependency.clone()).is_some() {
                    return Err(MetadataError::new(format!(
                        "duplicate active dependency alias {alias}"
                    )));
                }
            }
            active.native.append(&platform.native);
        }
        if let Some(alias) =
            active.dependencies.keys().find(|alias| active.dev_dependencies.contains_key(*alias))
        {
            return Err(MetadataError::new(format!("duplicate active dependency alias {alias}")));
        }
        Ok(active)
    }
}
