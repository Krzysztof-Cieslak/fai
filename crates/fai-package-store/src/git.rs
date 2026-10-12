//! Immutable package trees obtained from the installed Git command.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    process::{Command, Stdio},
};

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::{
    Digest, GitBranch, GitPackage, GitRepository, GitRevision, PackagePath, PackageVersion,
    ProviderFailure,
};
use serde::{Deserialize, Serialize};

use crate::{atomic_write, io_error, lock_file};

/// A complete operation-local snapshot of repository refs.
#[derive(Debug, Clone, Default)]
pub struct GitInventory {
    /// Canonical path-scoped release versions and peeled commits.
    pub releases: BTreeMap<PackageVersion, GitRevision>,
    /// Literal branch names and the operation's snapshot commits.
    pub branches: BTreeMap<GitBranch, GitRevision>,
}

/// Portable tree metadata; executable bits are retained even on non-Unix filesystems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeEntry {
    /// Canonical path within the package tree.
    pub path: PackagePath,
    /// Git executable mode, retained across host filesystems.
    pub executable: bool,
    /// Whether bytes represent a relative symbolic-link destination.
    pub symlink: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    repository: GitRepository,
    subdir: PackagePath,
    revision: GitRevision,
    checksum: Digest,
    entries: Vec<TreeEntry>,
}

/// An integrity-checked immutable package tree.
#[derive(Debug, Clone)]
pub struct MaterializedPackage {
    /// Directory containing verified source bytes.
    pub directory: Utf8PathBuf,
    /// Canonical checksum of every entry and its bytes.
    pub checksum: Digest,
    /// Entry descriptors used for subsequent verification.
    pub entries: Vec<TreeEntry>,
}

/// Per-operation provider with per-repository advisory locks and pinned ref snapshots.
pub struct GitStore {
    cache: Utf8PathBuf,
    offline: bool,
    snapshots: BTreeMap<GitRepository, BTreeMap<String, GitRevision>>,
    transports: BTreeMap<GitRepository, GitRepository>,
}

impl GitStore {
    /// Whether this operation forbids all source-network access.
    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.offline
    }
    /// Constructs a store rooted in the caller's configured cache directory.
    #[must_use]
    pub fn new(cache: Utf8PathBuf, offline: bool) -> Self {
        Self { cache, offline, snapshots: BTreeMap::new(), transports: BTreeMap::new() }
    }

    /// Redirects transport in an isolated validation cache, retaining logical identities.
    #[must_use]
    pub fn with_transport(mut self, logical: GitRepository, actual: GitRepository) -> Self {
        self.transports.insert(logical, actual);
        self
    }

    fn repository_dir(&self, repository: &GitRepository) -> Utf8PathBuf {
        self.cache.join("git").join(blake3::hash(repository.as_str().as_bytes()).to_hex().as_str())
    }

    fn repository_lock(&self, repository: &GitRepository) -> Utf8PathBuf {
        self.repository_dir(repository).with_extension("lock")
    }

    fn ensure_repository(
        &self,
        repository: &GitRepository,
    ) -> Result<Utf8PathBuf, ProviderFailure> {
        let path = self.repository_dir(repository);
        if path.join("HEAD").is_file() {
            if self.offline {
                let output = command(Some(&path))
                    .args([
                        "config",
                        "--get-regexp",
                        "^(extensions[.]partialclone|remote[.].*[.]promisor)$",
                    ])
                    .output()
                    .map_err(io_error)?;
                if output.status.success() && !output.stdout.is_empty() {
                    return Err(offline(
                        "partial-clone metadata cannot trigger lazy fetching in offline mode",
                    ));
                }
            }
            return Ok(path);
        }
        if self.offline {
            return Err(offline("repository metadata is not cached"));
        }
        let parent = path.parent().ok_or_else(|| io_error("repository cache has no parent"))?;
        std::fs::create_dir_all(parent).map_err(io_error)?;
        let temporary = tempfile::tempdir_in(parent).map_err(io_error)?;
        let temp = Utf8Path::from_path(temporary.path())
            .ok_or_else(|| io_error("cache path is not UTF-8"))?;
        git(
            None,
            &[
                "clone",
                "--mirror",
                "--no-hardlinks",
                "--",
                self.transports.get(repository).unwrap_or(repository).as_str(),
                temp.as_str(),
            ],
        )?;
        std::fs::rename(temp, &path).map_err(io_error)?;
        Ok(path)
    }

    /// Refreshes refs once online, or reads the complete cached snapshot offline.
    pub fn inventory(&mut self, package: &GitPackage) -> Result<GitInventory, ProviderFailure> {
        if !self.snapshots.contains_key(&package.repository) {
            let _lock = lock_file(&self.repository_lock(&package.repository))?;
            let directory = self.ensure_repository(&package.repository)?;
            if !self.offline {
                git(
                    Some(&directory),
                    &[
                        "fetch",
                        "--force",
                        "--prune",
                        "--prune-tags",
                        "--no-recurse-submodules",
                        "origin",
                        "+refs/heads/*:refs/heads/*",
                        "+refs/tags/*:refs/tags/*",
                    ],
                )?;
            }
            let output = command(Some(&directory))
                .args(["show-ref", "--dereference"])
                .output()
                .map_err(io_error)?;
            if !output.status.success() && output.status.code() != Some(1) {
                return Err(git_failure("read refs", &output.stderr));
            }
            let mut refs = BTreeMap::new();
            let text = String::from_utf8(output.stdout).map_err(io_error)?;
            for line in text.lines() {
                let Some((revision, name)) = line.split_once(' ') else {
                    return Err(io_error("invalid Git ref inventory"));
                };
                let revision = GitRevision::parse(revision).map_err(io_error)?;
                refs.insert(name.to_owned(), revision);
            }
            let peeled: Vec<_> = refs
                .iter()
                .filter_map(|(name, revision)| {
                    name.strip_suffix("^{}").map(|name| (name.to_owned(), revision.clone()))
                })
                .collect();
            for (name, revision) in peeled {
                refs.insert(name, revision);
            }
            self.snapshots.insert(package.repository.clone(), refs);
        }
        let mut inventory = GitInventory::default();
        for (name, revision) in &self.snapshots[&package.repository] {
            if let Some(tag) = name.strip_prefix("refs/tags/")
                && let Some(version) = package.version_from_tag(tag)
            {
                inventory.releases.insert(version, revision.clone());
            } else if let Some(branch) = name.strip_prefix("refs/heads/")
                && let Ok(branch) = GitBranch::parse(branch)
            {
                inventory.branches.insert(branch, revision.clone());
            }
        }
        Ok(inventory)
    }

    /// Checks a known release mapping before accepting its bytes again.
    pub fn accept_release(
        &self,
        package: &GitPackage,
        version: &PackageVersion,
        revision: &GitRevision,
    ) -> Result<(), ProviderFailure> {
        let _lock = lock_file(&self.repository_lock(&package.repository))?;
        let directory = self.ensure_repository(&package.repository)?;
        let path = directory.join("fai-releases.json");
        let mut accepted: BTreeMap<String, GitRevision> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io_error)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(io_error(error)),
        };
        let tag = package.release_tag(version).map_err(io_error)?;
        if let Some(previous) = accepted.get(&tag) {
            if previous != revision {
                return Err(ProviderFailure {
                    code: fai_package::RELEASE_MOVED,
                    message: format!(
                        "release {tag} in {} moved from {previous} to {revision}",
                        package.repository
                    ),
                });
            }
        } else {
            accepted.insert(tag, revision.clone());
            atomic_write(&path, &serde_json::to_vec(&accepted).map_err(io_error)?)?;
        }
        Ok(())
    }

    fn ensure_revision(
        &self,
        package: &GitPackage,
        revision: &GitRevision,
    ) -> Result<Utf8PathBuf, ProviderFailure> {
        let directory = self.ensure_repository(&package.repository)?;
        let exists = || {
            command(Some(&directory))
                .args(["cat-file", "-t", revision.as_str()])
                .stderr(Stdio::null())
                .output()
        };
        let current = exists().map_err(io_error)?;
        if current.status.success() {
            if current.stdout != b"commit\n" {
                return Err(layout("a full revision must identify a commit, not a tag or tree"));
            }
            return Ok(directory);
        }
        if self.offline {
            return Err(offline(&format!("commit {revision} is not cached")));
        }
        git(
            Some(&directory),
            &["fetch", "--no-tags", "--no-recurse-submodules", "origin", revision.as_str()],
        )?;
        let current = exists().map_err(io_error)?;
        if !current.status.success() || current.stdout != b"commit\n" {
            return Err(io_error(format!("Git did not supply requested commit {revision}")));
        }
        Ok(directory)
    }

    /// Reads the raw manifest from the selected commit without checkout filters.
    pub fn manifest(
        &self,
        package: &GitPackage,
        revision: &GitRevision,
    ) -> Result<Option<String>, ProviderFailure> {
        let _lock = lock_file(&self.repository_lock(&package.repository))?;
        let directory = self.ensure_revision(package, revision)?;
        let path = if package.subdir.is_root() {
            "fai.toml".to_owned()
        } else {
            format!("{}/fai.toml", package.subdir)
        };
        let object = format!("{revision}:{path}");
        let exists = command(Some(&directory))
            .args(["cat-file", "-e", &object])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(io_error)?;
        if !exists.success() {
            return Ok(None);
        }
        let bytes = git(Some(&directory), &["show", &object])?;
        String::from_utf8(bytes).map(Some).map_err(|_| ProviderFailure {
            code: fai_package::INVALID_MANIFEST,
            message: "Git package manifest is not valid UTF-8".into(),
        })
    }

    /// Materializes and verifies exactly one package subdirectory at a commit.
    pub fn materialize(
        &self,
        package: &GitPackage,
        revision: &GitRevision,
        expected: Option<&Digest>,
    ) -> Result<MaterializedPackage, ProviderFailure> {
        let descriptor = serde_json::to_vec(&(package, revision)).map_err(io_error)?;
        let key = blake3::hash(&descriptor).to_hex().to_string();
        let parent = self.cache.join("sources");
        let destination = parent.join(&key);
        let _source_lock = lock_file(&parent.join(format!("{key}.lock")))?;
        if destination.exists() {
            match read_materialized(&destination, package, revision, expected) {
                Ok(materialized) => return Ok(materialized),
                Err(error) if self.offline => return Err(error),
                Err(_) => std::fs::remove_dir_all(&destination).map_err(io_error)?,
            }
        }
        let _repository_lock = lock_file(&self.repository_lock(&package.repository))?;
        let repository = self.ensure_revision(package, revision)?;
        let tree = if package.subdir.is_root() {
            revision.to_string()
        } else {
            format!("{revision}:{}", package.subdir)
        };
        let entries = git(Some(&repository), &["ls-tree", "-rz", "--full-tree", &tree])?;
        let mut descriptors = Vec::new();
        let mut objects = Vec::new();
        for entry in entries.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
            let split = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or_else(|| io_error("invalid Git tree record"))?;
            let header = std::str::from_utf8(&entry[..split]).map_err(io_error)?;
            let mut parts = header.split(' ');
            let mode = parts.next().unwrap_or_default();
            let kind = parts.next().unwrap_or_default();
            let object = parts.next().unwrap_or_default();
            if kind != "blob" || !matches!(mode, "100644" | "100755" | "120000") {
                return Err(layout("submodules and non-blob Git package entries are unsupported"));
            }
            let raw = std::str::from_utf8(&entry[split + 1..]).map_err(io_error)?;
            let path = PackagePath::parse(raw).map_err(io_error)?;
            if path.is_root() || path.as_str() != raw {
                return Err(layout("a Git tree entry must name a canonical relative file"));
            }
            descriptors.push(TreeEntry {
                path,
                executable: mode == "100755",
                symlink: mode == "120000",
            });
            objects.push(object.to_owned());
        }
        let contents = blobs(&repository, &objects)?;
        let mut files: Vec<_> = descriptors.into_iter().zip(contents).collect();
        files.sort_by(|a, b| a.0.path.cmp(&b.0.path));
        validate_links(&files)?;
        for (_, bytes) in &files {
            if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
                return Err(layout(
                    "Git LFS content must be explicitly materialized before packaging",
                ));
            }
        }
        let checksum = hash_files(&files);
        if expected.is_some_and(|expected| expected != &checksum) {
            return Err(integrity("Git source does not match the locked package checksum"));
        }
        std::fs::create_dir_all(&parent).map_err(io_error)?;
        let staging = tempfile::tempdir_in(&parent).map_err(io_error)?;
        let staging_path = Utf8Path::from_path(staging.path())
            .ok_or_else(|| io_error("cache path is not UTF-8"))?;
        let tree = staging_path.join("tree");
        std::fs::create_dir(&tree).map_err(io_error)?;
        for (entry, bytes) in files.iter().filter(|(entry, _)| !entry.symlink) {
            let path = tree.join(entry.path.as_str());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io_error)?;
            }
            let mut file = std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(io_error)?;
            file.write_all(bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(if entry.executable {
                    0o755
                } else {
                    0o644
                }))
                .map_err(io_error)?;
            }
        }
        for (entry, bytes) in files.iter().filter(|(entry, _)| entry.symlink) {
            let path = tree.join(entry.path.as_str());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io_error)?;
            }
            let target = std::str::from_utf8(bytes).map_err(io_error)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &path).map_err(io_error)?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(target, &path).map_err(io_error)?;
        }
        let receipt = Receipt {
            version: 1,
            repository: package.repository.clone(),
            subdir: package.subdir.clone(),
            revision: revision.clone(),
            checksum: checksum.clone(),
            entries: files.into_iter().map(|(entry, _)| entry).collect(),
        };
        atomic_write(
            &staging_path.join("receipt.json"),
            &serde_json::to_vec(&receipt).map_err(io_error)?,
        )?;
        std::fs::rename(staging_path, &destination).map_err(io_error)?;
        read_materialized(&destination, package, revision, expected)
    }
}

fn command(directory: Option<&Utf8Path>) -> Command {
    let mut command = Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    if let Some(directory) = directory {
        command.arg("--git-dir").arg(directory);
    }
    command
}

fn git(directory: Option<&Utf8Path>, args: &[&str]) -> Result<Vec<u8>, ProviderFailure> {
    let output = command(directory).args(args).output().map_err(io_error)?;
    if !output.status.success() {
        return Err(git_failure(args.first().copied().unwrap_or("command"), &output.stderr));
    }
    Ok(output.stdout)
}

fn git_failure(operation: &str, stderr: &[u8]) -> ProviderFailure {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    // Transport output may contain credential-helper or rewritten URLs. Report
    // actionable categories without copying those secret-bearing strings.
    let authentication = stderr.contains("authentication")
        || stderr.contains("permission denied")
        || stderr.contains("could not read username");
    let mut failure = io_error(format!(
        "Git {operation} failed; {}",
        if authentication {
            "check Git credentials and repository access"
        } else {
            "check the repository, requested revision, and connectivity"
        }
    ));
    if authentication {
        failure.code = fai_package::AUTHENTICATION_REQUIRED;
    }
    failure
}

fn blobs(repository: &Utf8Path, objects: &[String]) -> Result<Vec<Vec<u8>>, ProviderFailure> {
    let mut child = command(Some(repository))
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_error)?;
    let mut input = child.stdin.take().ok_or_else(|| io_error("Git batch input is unavailable"))?;
    let output = std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> std::io::Result<()> {
            for object in objects {
                writeln!(input, "{object}")?;
            }
            Ok(())
        });
        let output = child.wait_with_output().map_err(io_error)?;
        writer.join().map_err(|_| io_error("Git batch writer failed"))?.map_err(io_error)?;
        Ok::<_, ProviderFailure>(output)
    })?;
    if !output.status.success() {
        return Err(git_failure("read package blobs", &output.stderr));
    }
    let mut remaining = output.stdout.as_slice();
    let mut files = Vec::with_capacity(objects.len());
    for object in objects {
        let newline = remaining
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| io_error("incomplete Git blob header"))?;
        let header = std::str::from_utf8(&remaining[..newline]).map_err(io_error)?;
        let prefix = format!("{object} blob ");
        let len = header
            .strip_prefix(&prefix)
            .ok_or_else(|| io_error("unexpected Git object identity"))?
            .parse::<usize>()
            .map_err(io_error)?;
        remaining = &remaining[newline + 1..];
        if len >= remaining.len() || remaining[len] != b'\n' {
            return Err(io_error("incomplete Git blob content"));
        }
        files.push(remaining[..len].to_vec());
        remaining = &remaining[len + 1..];
    }
    if !remaining.is_empty() {
        return Err(io_error("unexpected extra Git blob output"));
    }
    Ok(files)
}

fn validate_links(files: &[(TreeEntry, Vec<u8>)]) -> Result<(), ProviderFailure> {
    let links: BTreeMap<_, _> = files
        .iter()
        .filter(|(entry, _)| entry.symlink)
        .map(|(entry, bytes)| (entry.path.as_str(), bytes.as_slice()))
        .collect();
    for (path, bytes) in &links {
        let target = std::str::from_utf8(bytes).map_err(io_error)?;
        if target.starts_with('/') || target.contains(['\\', ':']) {
            return Err(layout("absolute or nonportable package symlink"));
        }
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let mut next = PackagePath::parse(format!("{parent}/{target}").trim_start_matches('/'))
            .map_err(|_| layout("package symlink escapes its source root"))?;
        let mut visited = BTreeSet::from([(*path).to_owned()]);
        loop {
            let prefix = next
                .as_str()
                .split('/')
                .scan(String::new(), |prefix, part| {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(part);
                    Some(prefix.clone())
                })
                .find(|prefix| links.contains_key(prefix.as_str()));
            let Some(prefix) = prefix else {
                break;
            };
            if !visited.insert(prefix.clone()) {
                return Err(layout("package symlinks form a cycle"));
            }
            let target = std::str::from_utf8(links[prefix.as_str()]).map_err(io_error)?;
            let parent = prefix.rsplit_once('/').map_or("", |(parent, _)| parent);
            let suffix = next.as_str().strip_prefix(&prefix).unwrap_or_default();
            next = PackagePath::parse(format!("{parent}/{target}{suffix}").trim_start_matches('/'))
                .map_err(|_| layout("package symlink escapes its source root"))?;
        }
    }
    Ok(())
}

fn hash_files(files: &[(TreeEntry, Vec<u8>)]) -> Digest {
    let mut hash = blake3::Hasher::new();
    hash.update(b"fai.package-tree.v1\0");
    for (entry, bytes) in files {
        hash.update(&(entry.path.as_str().len() as u64).to_le_bytes());
        hash.update(entry.path.as_str().as_bytes());
        hash.update(&[u8::from(entry.symlink), u8::from(entry.executable)]);
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    // Digest::of provides the common algorithm/domain framing used by all records.
    Digest::of("fai.package-tree-root.v1", hash.finalize().as_bytes())
}

/// Checks source bytes and entry membership without following symlinks.
pub fn tree_checksum(
    directory: &Utf8Path,
    entries: &[TreeEntry],
) -> Result<Digest, ProviderFailure> {
    if !std::fs::symlink_metadata(directory).map_err(io_error)?.file_type().is_dir() {
        return Err(integrity("immutable source root is not a directory"));
    }
    let expected: BTreeMap<_, _> =
        entries.iter().map(|entry| (entry.path.clone(), entry)).collect();
    let mut found = Vec::new();
    let mut pending = vec![directory.to_owned()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| io_error("package filename is not UTF-8"))?;
            let metadata = std::fs::symlink_metadata(&path).map_err(io_error)?;
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() && !metadata.file_type().is_symlink() {
                return Err(integrity("immutable source contains a non-file entry"));
            }
            let relative = path
                .strip_prefix(directory)
                .map_err(io_error)?
                .components()
                .map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join("/");
            let relative = PackagePath::parse(&relative).map_err(io_error)?;
            let descriptor = expected
                .get(&relative)
                .ok_or_else(|| integrity("immutable source contains an unexpected file"))?;
            if descriptor.symlink != metadata.file_type().is_symlink() {
                return Err(integrity("immutable source entry type changed"));
            }
            #[cfg(unix)]
            if !descriptor.symlink {
                use std::os::unix::fs::PermissionsExt;
                if descriptor.executable != (metadata.permissions().mode() & 0o111 != 0) {
                    return Err(integrity("immutable source executable mode changed"));
                }
            }
            let bytes = if descriptor.symlink {
                std::fs::read_link(&path)
                    .map_err(io_error)?
                    .to_str()
                    .ok_or_else(|| layout("package symlink is not UTF-8"))?
                    .as_bytes()
                    .to_vec()
            } else {
                let mut bytes = Vec::new();
                std::fs::File::open(&path)
                    .and_then(|mut file| file.read_to_end(&mut bytes))
                    .map_err(io_error)?;
                bytes
            };
            found.push(((*descriptor).clone(), bytes));
        }
    }
    if found.len() != entries.len() {
        return Err(integrity("immutable source is missing a file"));
    }
    found.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    validate_links(&found)?;
    Ok(hash_files(&found))
}

fn read_materialized(
    root: &Utf8Path,
    package: &GitPackage,
    revision: &GitRevision,
    expected: Option<&Digest>,
) -> Result<MaterializedPackage, ProviderFailure> {
    let receipt: Receipt =
        serde_json::from_slice(&std::fs::read(root.join("receipt.json")).map_err(io_error)?)
            .map_err(io_error)?;
    if receipt.version != 1
        || receipt.repository != package.repository
        || receipt.subdir != package.subdir
        || receipt.revision != *revision
    {
        return Err(integrity("immutable source receipt has the wrong identity"));
    }
    let directory = root.join("tree");
    let checksum = tree_checksum(&directory, &receipt.entries)?;
    if checksum != receipt.checksum || expected.is_some_and(|expected| expected != &checksum) {
        return Err(integrity("immutable source checksum changed"));
    }
    let directory = directory.canonicalize_utf8().map_err(io_error)?;
    Ok(MaterializedPackage { directory, checksum, entries: receipt.entries })
}

fn integrity(message: &str) -> ProviderFailure {
    ProviderFailure { code: fai_package::SOURCE_INTEGRITY, message: message.into() }
}
fn offline(message: &str) -> ProviderFailure {
    ProviderFailure { code: fai_package::OFFLINE_UNAVAILABLE, message: message.into() }
}
fn layout(message: &str) -> ProviderFailure {
    ProviderFailure { code: fai_package::UNSUPPORTED_SOURCE_LAYOUT, message: message.into() }
}
