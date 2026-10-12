//! Journaled compare-and-replace updates for manifests and generated lockfiles.

use std::{fs::File, io::Write};

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::{Digest, ProviderFailure};
use serde::{Deserialize, Serialize};

use crate::io_error;

/// An exclusive advisory lock, released when the returned file is dropped.
pub fn lock_file(path: &Utf8Path) -> Result<File, ProviderFailure> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io_error)?;
    file.lock().map_err(io_error)?;
    Ok(file)
}

/// Publishes complete bytes through a synchronized temporary sibling and rename.
pub fn atomic_write(path: &Utf8Path, bytes: &[u8]) -> Result<(), ProviderFailure> {
    let parent = path.parent().ok_or_else(|| io_error("output has no parent directory"))?;
    std::fs::create_dir_all(parent).map_err(io_error)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
    temporary.write_all(bytes).map_err(io_error)?;
    temporary.as_file().sync_all().map_err(io_error)?;
    temporary.persist(path).map_err(io_error)?;
    #[cfg(unix)]
    File::open(parent).and_then(|directory| directory.sync_all()).map_err(io_error)?;
    Ok(())
}

/// One intended metadata replacement, with an exact precondition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEdit {
    /// Canonical root-relative destination.
    pub path: String,
    /// Exact precondition, or absence for a new file.
    pub before: Option<Digest>,
    /// Complete proposed replacement bytes as UTF-8 text.
    pub after: String,
}

impl FileEdit {
    /// Fingerprints observed bytes for a compare-and-replace precondition.
    #[must_use]
    pub fn fingerprint(bytes: &[u8]) -> Digest {
        digest(bytes)
    }
    /// Creates an edit relative to a transaction's root.
    #[must_use]
    pub fn new(path: String, before: Option<&str>, after: String) -> Self {
        Self { path, before: before.map(|text| digest(text.as_bytes())), after }
    }
}

/// Prepared writes and additional read-only preconditions checked before mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataTransaction {
    /// Journal format version.
    pub version: u32,
    /// Ordered compare-and-replace operations.
    pub edits: Vec<FileEdit>,
    /// Read-only input preconditions, relative to the root or explicitly absolute.
    pub observed: Vec<(String, Option<Digest>)>,
}

impl MetadataTransaction {
    /// Captures a complete multi-file metadata update.
    #[must_use]
    pub fn new(edits: Vec<FileEdit>, observed: Vec<(String, Option<Digest>)>) -> Self {
        Self { version: 1, edits, observed }
    }

    /// Checks all preconditions before the first mutation and persists a journal.
    pub fn commit(&self, root: &Utf8Path) -> Result<(), ProviderFailure> {
        validate_state_directory(root)?;
        let _lock = lock_file(&root.join(".fai/metadata.lock"))?;
        Self::recover_locked(root)?;
        self.validate(root)?;
        for (path, expected) in &self.observed {
            if fingerprint(&root.join(path))? != *expected {
                return Err(changed(path));
            }
        }
        for edit in &self.edits {
            if fingerprint(&metadata_path(root, &edit.path)?)? != edit.before {
                return Err(changed(&edit.path));
            }
        }
        if self.edits.is_empty() {
            return Ok(());
        }
        atomic_write(
            &root.join(".fai/transaction.json"),
            &serde_json::to_vec(self).map_err(io_error)?,
        )?;
        self.finish(root)
    }

    /// Resumes only journal entries still equal to their recorded before/after state.
    pub fn recover(root: &Utf8Path) -> Result<(), ProviderFailure> {
        validate_state_directory(root)?;
        let journal = root.join(".fai/transaction.json");
        if !journal.exists() {
            return Ok(());
        }
        let _lock = lock_file(&root.join(".fai/metadata.lock"))?;
        Self::recover_locked(root)
    }

    fn recover_locked(root: &Utf8Path) -> Result<(), ProviderFailure> {
        let journal = root.join(".fai/transaction.json");
        let bytes = match std::fs::read(&journal) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        };
        let transaction: Self = serde_json::from_slice(&bytes).map_err(io_error)?;
        transaction.validate(root)?;
        transaction.finish(root)
    }

    fn validate(&self, root: &Utf8Path) -> Result<(), ProviderFailure> {
        if self.version != 1 {
            return Err(io_error("unsupported package metadata transaction version"));
        }
        let mut paths = std::collections::BTreeSet::new();
        for edit in &self.edits {
            metadata_path(root, &edit.path)?;
            if !paths.insert(&edit.path) {
                return Err(io_error("duplicate metadata transaction destination"));
            }
        }
        Ok(())
    }

    fn finish(&self, root: &Utf8Path) -> Result<(), ProviderFailure> {
        for (path, expected) in &self.observed {
            if self.edits.iter().any(|edit| root.join(&edit.path) == root.join(path)) {
                continue;
            }
            if fingerprint(&root.join(path))? != *expected {
                return Err(changed(path));
            }
        }
        // Validate every destination before resuming any unfinished entry.
        for edit in &self.edits {
            let current = fingerprint(&metadata_path(root, &edit.path)?)?;
            if current != edit.before && current != Some(digest(edit.after.as_bytes())) {
                return Err(changed(&edit.path));
            }
        }
        for edit in &self.edits {
            let path = metadata_path(root, &edit.path)?;
            if fingerprint(&path)? != Some(digest(edit.after.as_bytes())) {
                atomic_write(&path, edit.after.as_bytes())?;
            }
        }
        std::fs::remove_file(root.join(".fai/transaction.json")).map_err(io_error)?;
        Ok(())
    }
}

fn metadata_path(root: &Utf8Path, path: &str) -> Result<Utf8PathBuf, ProviderFailure> {
    let relative = fai_package::PackagePath::parse(path).map_err(io_error)?;
    if relative.as_str() != path
        || !matches!(
            relative.file_name(),
            Some("fai.toml" | "fai.lock" | "fai.local.lock" | "fai-vendor.toml")
        )
    {
        return Err(io_error(
            "metadata transaction path must name a contained manifest, lock, or vendor index",
        ));
    }
    let destination = root.join(path);
    let canonical_root = root.canonicalize_utf8().map_err(io_error)?;
    let mut ancestor = destination.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| io_error("invalid metadata destination"))?;
    }
    if !ancestor.canonicalize_utf8().map_err(io_error)?.starts_with(&canonical_root) {
        return Err(io_error("metadata destination escapes its project through a symlink"));
    }
    if std::fs::symlink_metadata(&destination)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(io_error("metadata destinations cannot be symbolic links"));
    }
    Ok(destination)
}

fn validate_state_directory(root: &Utf8Path) -> Result<(), ProviderFailure> {
    if std::fs::symlink_metadata(root.join(".fai"))
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(io_error("the package metadata state directory cannot be a symbolic link"));
    }
    Ok(())
}

fn fingerprint(path: &Utf8Path) -> Result<Option<Digest>, ProviderFailure> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(digest(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

fn digest(bytes: &[u8]) -> Digest {
    Digest::of("fai.metadata-bytes.v1", bytes)
}

fn changed(path: &str) -> ProviderFailure {
    ProviderFailure {
        code: fai_package::INPUTS_CHANGED,
        message: format!(
            "metadata {path} changed during preparation; retain the new contents and retry"
        ),
    }
}
