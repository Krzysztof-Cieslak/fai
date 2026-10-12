//! Exact locked source snapshots for cache-independent offline builds.

use std::collections::BTreeSet;

use camino::Utf8Path;
use fai_package::{
    Digest, LockContext, LockFile, LockLocator, LockSource, LockedPackage, PackagePath,
    PackageVersion, ProviderFailure, TargetPlatform,
};
use serde::{Deserialize, Serialize};

use crate::{GitStore, MaterializedPackage, TreeEntry, io_error, tree_checksum};

/// One verified mapping from a complete locked descriptor to a vendored tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorPackage {
    /// Exact locked package record ID.
    pub id: String,
    /// Original logical identity, retained through replacements.
    pub logical: LockLocator,
    /// Exact actual Git source.
    pub source: LockSource,
    /// Declared package version.
    pub version: PackageVersion,
    /// Canonical source checksum.
    pub checksum: Digest,
    /// Contained content-addressed source directory.
    pub directory: PackagePath,
    /// Portable entry descriptors used to revalidate source bytes.
    pub entries: Vec<TreeEntry>,
}

/// Receipt describing which graph snapshot was prepared.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VendorCoverage {
    /// Shared or personal configuration.
    pub context: LockContext,
    /// Root package path.
    pub root: String,
    /// Target metadata selection.
    pub target: TargetPlatform,
    /// Semantic graph hash at materialization time.
    pub graph_hash: Digest,
    /// Locked Git package record IDs copied for this graph.
    pub packages: Vec<String>,
}

/// Versioned vendor index. Coverage is advisory; source descriptors are authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VendorIndex {
    /// Index format version.
    pub vendor_version: u32,
    /// Exact immutable package mappings.
    pub packages: Vec<VendorPackage>,
    /// Graph materialization receipts.
    pub coverage: Vec<VendorCoverage>,
}

impl Default for VendorIndex {
    fn default() -> Self {
        Self { vendor_version: 1, packages: Vec::new(), coverage: Vec::new() }
    }
}

impl VendorIndex {
    /// Reads a configured index without falling through to remote sources.
    pub fn read(root: &Utf8Path) -> Result<Self, ProviderFailure> {
        let text = std::fs::read_to_string(root.join("fai-vendor.toml"))
            .map_err(|error| unavailable(format!("vendor index is unavailable: {error}")))?;
        let index: Self = toml::from_str(&text).map_err(|error| unavailable(error.to_string()))?;
        if index.vendor_version != 1 {
            return Err(unavailable("unsupported vendor index version"));
        }
        let mut ids = BTreeSet::new();
        if index.packages.iter().any(|package| !ids.insert(&package.id)) {
            return Err(unavailable("duplicate vendor package entry"));
        }
        Ok(index)
    }

    /// Serializes a deterministic index for journaled publication by the caller.
    pub fn to_toml(&self) -> Result<String, ProviderFailure> {
        let mut index = self.clone();
        index.packages.sort_by(|a, b| a.id.cmp(&b.id));
        index.coverage.sort_by_key(|coverage| {
            (format!("{:?}", coverage.context), coverage.root.clone(), coverage.target.to_string())
        });
        toml::to_string_pretty(&index).map_err(io_error)
    }

    /// Resolves and verifies a locked Git package using only its vendor mapping.
    pub fn materialize(
        &self,
        root: &Utf8Path,
        package: &LockedPackage,
    ) -> Result<MaterializedPackage, ProviderFailure> {
        let entry = self
            .packages
            .iter()
            .find(|entry| {
                entry.id == package.id
                    && entry.logical == package.logical
                    && entry.source == package.source
                    && entry.version == package.version
                    && package.checksum.as_ref() == Some(&entry.checksum)
            })
            .ok_or_else(|| {
                unavailable(format!("vendor index has no exact entry for {}", package.id))
            })?;
        let directory = root.join(entry.directory.as_str());
        let canonical_root = root.canonicalize_utf8().map_err(io_error)?;
        let canonical = directory
            .canonicalize_utf8()
            .map_err(|_| unavailable(format!("vendored package {} is missing", package.id)))?;
        if !canonical.starts_with(&canonical_root)
            || std::fs::symlink_metadata(&directory).map_err(io_error)?.file_type().is_symlink()
        {
            return Err(unavailable("vendor package directory escapes its configured root"));
        }
        let checksum = tree_checksum(&directory, &entry.entries)
            .map_err(|error| unavailable(error.message))?;
        if checksum != entry.checksum {
            return Err(unavailable(format!("vendored package {} has been modified", package.id)));
        }
        Ok(MaterializedPackage { directory: canonical, checksum, entries: entry.entries.clone() })
    }

    /// Stages and publishes content-addressed trees, returning the replacement
    /// index. Manifest/index publication remains the caller's metadata transaction.
    pub fn prepare(
        root: &Utf8Path,
        lock: &LockFile,
        store: &GitStore,
    ) -> Result<Self, ProviderFailure> {
        std::fs::create_dir_all(root).map_err(io_error)?;
        let mut index = if root.join("fai-vendor.toml").is_file() {
            Self::read(root)?
        } else {
            Self::default()
        };
        let sources = root.join("sources");
        if std::fs::symlink_metadata(&sources)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(unavailable("vendor source storage cannot be a symbolic link"));
        }
        std::fs::create_dir_all(&sources).map_err(io_error)?;
        for package in &lock.packages {
            let LockSource::Git { git, subdir, rev } = &package.source else {
                continue;
            };
            let original = store.materialize(
                &fai_package::GitPackage { repository: git.clone(), subdir: subdir.clone() },
                rev,
                package.checksum.as_ref(),
            )?;
            let relative = PackagePath::parse(&format!(
                "sources/{}",
                original.checksum.as_str().trim_start_matches("blake3:")
            ))
            .map_err(io_error)?;
            let destination = root.join(relative.as_str());
            let current = tree_checksum(&destination, &original.entries).ok();
            if current.as_ref() != Some(&original.checksum) {
                let staging = tempfile::tempdir_in(&sources).map_err(io_error)?;
                let staged = Utf8Path::from_path(staging.path())
                    .ok_or_else(|| io_error("vendor directory is not UTF-8"))?
                    .join("tree");
                copy_tree(&original, &staged)?;
                if tree_checksum(&staged, &original.entries)? != original.checksum {
                    return Err(unavailable("vendor staging checksum mismatch"));
                }
                let backup = Utf8Path::from_path(staging.path())
                    .ok_or_else(|| io_error("vendor directory is not UTF-8"))?
                    .join("previous");
                if destination.exists() {
                    std::fs::rename(&destination, &backup).map_err(io_error)?;
                }
                if let Err(error) = std::fs::rename(&staged, &destination) {
                    if backup.exists() {
                        let _ = std::fs::rename(&backup, &destination);
                    }
                    return Err(io_error(error));
                }
            }
            let entry = VendorPackage {
                id: package.id.clone(),
                logical: package.logical.clone(),
                source: package.source.clone(),
                version: package.version.clone(),
                checksum: original.checksum,
                directory: relative,
                entries: original.entries,
            };
            index.packages.retain(|previous| previous.id != entry.id);
            index.packages.push(entry);
        }
        for graph in &lock.graphs {
            let packages = graph
                .packages
                .iter()
                .filter(|id| {
                    lock.package(id)
                        .is_some_and(|package| matches!(package.source, LockSource::Git { .. }))
                })
                .cloned()
                .collect();
            index.coverage.retain(|previous| {
                previous.context != lock.context
                    || previous.root != graph.root
                    || previous.target != graph.target
            });
            index.coverage.push(VendorCoverage {
                context: lock.context,
                root: graph.root.clone(),
                target: graph.target.clone(),
                graph_hash: graph.semantic_hash().map_err(|error| unavailable(error.message))?,
                packages,
            });
        }
        Ok(index)
    }
}

fn copy_tree(source: &MaterializedPackage, destination: &Utf8Path) -> Result<(), ProviderFailure> {
    std::fs::create_dir_all(destination).map_err(io_error)?;
    for entry in &source.entries {
        let from = source.directory.join(entry.path.as_str());
        let to = destination.join(entry.path.as_str());
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        if entry.symlink {
            let target = std::fs::read_link(&from).map_err(io_error)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &to).map_err(io_error)?;
            #[cfg(windows)]
            {
                if from.is_dir() {
                    std::os::windows::fs::symlink_dir(target, &to).map_err(io_error)?;
                } else {
                    std::os::windows::fs::symlink_file(target, &to).map_err(io_error)?;
                }
            }
        } else {
            std::fs::copy(&from, &to).map_err(io_error)?;
        }
    }
    Ok(())
}

fn unavailable(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure { code: fai_package::VENDOR_UNAVAILABLE, message: message.into() }
}
