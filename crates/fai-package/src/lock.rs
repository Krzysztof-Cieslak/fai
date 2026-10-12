//! Versioned lock records, structural validation, and graph-local replacement.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::{
    DependencyAlias, GitBranch, GitPackage, GitRepository, GitRevision, PackageCandidate,
    PackageKey, PackagePath, PackageSource, PackageVersion, ResolvedGraph, TargetPlatform,
};

/// A canonical BLAKE3 digest including its algorithm prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// Hashes bytes in a length-delimited, versioned domain.
    #[must_use]
    pub fn of(domain: &str, bytes: &[u8]) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(&(domain.len() as u64).to_le_bytes());
        hash.update(domain.as_bytes());
        hash.update(bytes);
        Self(format!("blake3:{}", hash.finalize().to_hex()))
    }

    /// Validates a canonical prefixed digest.
    pub fn parse(text: &str) -> Result<Self, LockError> {
        if text.strip_prefix("blake3:").is_some_and(|hex| {
            hex.len() == 64
                && hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            Ok(Self(text.to_owned()))
        } else {
            Err(LockError::invalid("expected a canonical blake3 digest"))
        }
    }

    /// Canonical prefixed spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Shared project configuration or personal root overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockContext {
    /// Root configuration checked into the project.
    Shared,
    /// Root configuration plus personal overrides.
    Local,
}

/// A portable logical locator in generated metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum LockLocator {
    /// Git repository plus independently versioned package directory.
    Git { git: GitRepository, subdir: PackagePath },
    /// Root-relative path when possible, otherwise an explicit outside path.
    Path { path: String },
}

impl LockLocator {
    /// Converts the resolver's logical identity to its portable record.
    #[must_use]
    pub fn from_key(key: &PackageKey) -> Self {
        match key {
            PackageKey::Git(package) => {
                Self::Git { git: package.repository.clone(), subdir: package.subdir.clone() }
            }
            PackageKey::Path(path) => Self::Path { path: path.clone() },
        }
    }

    /// Restores the exact logical key; path interpretation belongs to the caller.
    #[must_use]
    pub fn key(&self) -> PackageKey {
        match self {
            Self::Git { git, subdir } => {
                PackageKey::Git(GitPackage { repository: git.clone(), subdir: subdir.clone() })
            }
            Self::Path { path } => PackageKey::Path(path.clone()),
        }
    }

    fn validate(&self) -> Result<(), LockError> {
        if let Self::Path { path } = self {
            validate_path(path)?;
        }
        Ok(())
    }
}

/// Actual source recorded separately from an override's original logical key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum LockSource {
    /// Exact Git commit, with the Git object-format prefix retained in the wire form.
    Git {
        git: GitRepository,
        subdir: PackagePath,
        #[serde(with = "revision_wire")]
        rev: GitRevision,
    },
    /// A mutable directory, never presented as immutable content.
    Path { path: String },
}

impl LockSource {
    /// Converts an actual source to its generated record.
    #[must_use]
    pub fn from_source(source: &PackageSource) -> Self {
        match source {
            PackageSource::Git { package, revision } => Self::Git {
                git: package.repository.clone(),
                subdir: package.subdir.clone(),
                rev: revision.clone(),
            },
            PackageSource::Path(path) => Self::Path { path: path.clone() },
        }
    }

    /// Restores the actual source identity.
    #[must_use]
    pub fn source(&self) -> PackageSource {
        match self {
            Self::Git { git, subdir, rev } => PackageSource::Git {
                package: GitPackage { repository: git.clone(), subdir: subdir.clone() },
                revision: rev.clone(),
            },
            Self::Path { path } => PackageSource::Path(path.clone()),
        }
    }
}

/// One source snapshot. Dependency edges belong to graphs, never this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LockedPackage {
    /// Content-derived descriptor identity, separate from compiler input IDs.
    pub id: String,
    /// Original logical package, retained across overrides.
    pub logical: LockLocator,
    /// Version declared by the selected source.
    pub version: PackageVersion,
    /// Actual directory or exact Git snapshot.
    pub source: LockSource,
    /// Position-free metadata fingerprint.
    pub manifest_fingerprint: Digest,
    /// Verified immutable tree checksum; absent for local directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<Digest>,
    /// Retained evidence that the selected version was a canonical release.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
}

impl LockedPackage {
    /// Records verified selected metadata; immutable Git sources need a checksum.
    pub fn from_candidate(
        key: &PackageKey,
        candidate: &PackageCandidate,
    ) -> Result<Self, LockError> {
        let mut record = Self {
            id: String::new(),
            logical: LockLocator::from_key(key),
            version: candidate.version.clone(),
            source: LockSource::from_source(&candidate.source),
            manifest_fingerprint: Digest::parse(&candidate.manifest_fingerprint)?,
            checksum: candidate.checksum.as_deref().map(Digest::parse).transpose()?,
            released: candidate.released,
        };
        record.id = record.expected_id()?;
        record.validate()?;
        Ok(record)
    }

    fn expected_id(&self) -> Result<String, LockError> {
        Ok(format!(
            "p-{}",
            json_hash(
                "fai.package-record.v1",
                &(
                    &self.logical,
                    &self.version,
                    &self.source,
                    &self.manifest_fingerprint,
                    &self.checksum,
                    self.released
                )
            )?
            .as_str()
            .trim_start_matches("blake3:")
        ))
    }

    fn validate(&self) -> Result<(), LockError> {
        self.logical.validate()?;
        match &self.source {
            LockSource::Git { .. } if self.checksum.is_none() => {
                return Err(LockError::invalid(
                    "Git package record is missing its immutable checksum",
                ));
            }
            LockSource::Path { path } => {
                validate_path(path)?;
                if self.checksum.is_some() || self.released {
                    return Err(LockError::invalid(
                        "a path source cannot claim immutable release bytes",
                    ));
                }
            }
            _ => {}
        }
        if self.id != self.expected_id()? {
            return Err(LockError::invalid(
                "package record identity does not match its descriptor",
            ));
        }
        Ok(())
    }
}

/// Normal dependency or synthetic-root-only development dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LockedEdgeKind {
    /// An edge exported by the source package.
    Normal,
    /// An edge private to the root's development environment.
    Development,
}

/// An exact import-alias binding within one graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedEdge {
    /// Source package ID, or `$root` for development dependencies.
    pub from: String,
    /// File-importable dependency alias.
    pub alias: DependencyAlias,
    /// Normal or synthetic-root development edge.
    pub kind: LockedEdgeKind,
    /// Exact destination package ID.
    pub to: String,
}

/// Mutable-ref provenance, pinned until the appropriate explicit update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LockedBranch {
    /// Selected package ID carrying this branch binding.
    pub logical_package: String,
    /// Authored literal branch.
    pub branch: GitBranch,
    /// Commit selected when the branch was resolved.
    #[serde(with = "revision_wire")]
    pub rev: GitRevision,
}

/// A shared selection retained for per-key three-way local reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselinePackage {
    /// Original shared identity.
    pub logical: LockLocator,
    /// Previously observed shared version.
    pub version: PackageVersion,
    /// Previously observed shared source.
    pub source: LockSource,
}

/// One independently valid root/platform solution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LockedGraph {
    /// Stable root/platform key.
    pub id: String,
    /// Root package directory, relative to the active project when possible.
    pub root: String,
    /// Selected target descriptor.
    pub target: TargetPlatform,
    /// Package record supplying the root's production library.
    pub root_package: String,
    /// Current graph-local declaration and override inputs.
    pub input_fingerprint: Digest,
    /// Reachable package descriptor IDs.
    pub packages: Vec<String>,
    /// Exact normal and development alias bindings.
    #[serde(default)]
    pub edges: Vec<LockedEdge>,
    /// Mutable-ref provenance needed to retain pins during repair.
    #[serde(default)]
    pub branches: Vec<LockedBranch>,
    /// Informational compiler release, excluded from baseline identity.
    pub resolved_with: PackageVersion,
    /// Corresponding shared graph's semantic hash for personal reconciliation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_graph_hash: Option<Digest>,
    /// Per-key shared selections observed with that baseline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_packages: Vec<BaselinePackage>,
}

impl LockedGraph {
    /// Stable graph identity excludes selections and timestamps.
    pub fn identity(root: &str, target: &TargetPlatform) -> Result<String, LockError> {
        Ok(format!(
            "g-{}",
            json_hash("fai.graph-key.v1", &(root, target))?.as_str().trim_start_matches("blake3:")
        ))
    }

    /// Hashes semantic shared selections, excluding informational compiler data.
    pub fn semantic_hash(&self) -> Result<Digest, LockError> {
        let mut packages = self.packages.clone();
        packages.sort();
        let mut edges = self.edges.clone();
        edges.sort();
        let mut branches = self.branches.clone();
        branches
            .sort_by(|a, b| (&a.logical_package, &a.branch).cmp(&(&b.logical_package, &b.branch)));
        json_hash(
            "fai.graph-baseline.v1",
            &(
                &self.root,
                &self.target,
                &self.root_package,
                &self.input_fingerprint,
                packages,
                edges,
                branches,
            ),
        )
    }
}

/// Generated lock document holding independently replaceable graph records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LockFile {
    /// Version of this generated file format.
    pub lock_version: u32,
    /// Version of observable solver preference policy.
    pub resolver_version: u32,
    /// Configuration layer represented by this file.
    pub context: LockContext,
    /// Source descriptors shared by the graph records.
    #[serde(default)]
    pub packages: Vec<LockedPackage>,
    /// Independently replaceable root/platform solutions.
    #[serde(default)]
    pub graphs: Vec<LockedGraph>,
}

impl LockFile {
    /// Empty generated state for one configuration context.
    #[must_use]
    pub fn new(context: LockContext) -> Self {
        Self {
            lock_version: 1,
            resolver_version: 1,
            context,
            packages: Vec::new(),
            graphs: Vec::new(),
        }
    }

    /// Parses supported formats and validates every structural reference.
    pub fn parse(text: &str) -> Result<Self, LockError> {
        let value: toml::Value =
            toml::from_str(text).map_err(|error| LockError::invalid(error.to_string()))?;
        if value.get("lock-version").and_then(toml::Value::as_integer) != Some(1)
            || value.get("resolver-version").and_then(toml::Value::as_integer) != Some(1)
        {
            return Err(LockError {
                code: crate::UNSUPPORTED_LOCK,
                message: "unsupported lock or resolver version".into(),
            });
        }
        let lock: Self = value
            .try_into()
            .map_err(|error: toml::de::Error| LockError::invalid(error.to_string()))?;
        lock.validate()?;
        Ok(lock)
    }

    /// Produces deterministic generated TOML after structural validation.
    pub fn to_toml(&self) -> Result<String, LockError> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical.packages.sort_by(|a, b| a.id.cmp(&b.id));
        canonical
            .graphs
            .sort_by(|a, b| (&a.root, a.target.to_string()).cmp(&(&b.root, b.target.to_string())));
        for graph in &mut canonical.graphs {
            graph.packages.sort();
            graph.edges.sort();
            graph.branches.sort_by(|a, b| {
                (&a.logical_package, &a.branch).cmp(&(&b.logical_package, &b.branch))
            });
            graph.base_packages.sort_by(|a, b| a.logical.cmp(&b.logical));
        }
        toml::to_string_pretty(&canonical)
            .map(|text| format!("# Generated by Fai.\n{text}"))
            .map_err(|error| LockError::invalid(error.to_string()))
    }

    /// Locates only the selected root/platform record.
    #[must_use]
    pub fn graph(&self, root: &str, target: &TargetPlatform) -> Option<&LockedGraph> {
        self.graphs.iter().find(|graph| graph.root == root && &graph.target == target)
    }

    /// Locates a package descriptor by its generated identity.
    #[must_use]
    pub fn package(&self, id: &str) -> Option<&LockedPackage> {
        self.packages.iter().find(|package| package.id == id)
    }

    /// Replaces one graph, retaining all unrelated graph selections and records.
    pub fn record(
        &mut self,
        root: &str,
        target: &TargetPlatform,
        compiler: &PackageVersion,
        fingerprint: Digest,
        resolved: &ResolvedGraph,
        baseline: Option<(&LockFile, &LockedGraph)>,
    ) -> Result<(), LockError> {
        let mut next = self.clone();
        next.record_in_place(root, target, compiler, fingerprint, resolved, baseline)?;
        *self = next;
        Ok(())
    }

    fn record_in_place(
        &mut self,
        root: &str,
        target: &TargetPlatform,
        compiler: &PackageVersion,
        fingerprint: Digest,
        resolved: &ResolvedGraph,
        baseline: Option<(&LockFile, &LockedGraph)>,
    ) -> Result<(), LockError> {
        let mut ids = BTreeMap::new();
        for (key, candidate) in &resolved.packages {
            let record = LockedPackage::from_candidate(key, candidate)?;
            ids.insert(key.clone(), record.id.clone());
            if !self.packages.iter().any(|existing| existing.id == record.id) {
                self.packages.push(record);
            }
        }
        let id = |key: &PackageKey| {
            ids.get(key)
                .cloned()
                .ok_or_else(|| LockError::invalid("resolved edge names an unselected package"))
        };
        let mut edges = Vec::new();
        for edge in &resolved.edges {
            edges.push(LockedEdge {
                from: edge.from.as_ref().map(&id).transpose()?.unwrap_or_else(|| "$root".into()),
                alias: edge.alias.clone(),
                kind: if edge.from.is_some() {
                    LockedEdgeKind::Normal
                } else {
                    LockedEdgeKind::Development
                },
                to: id(&edge.to)?,
            });
        }
        edges.sort();
        let mut branches = Vec::new();
        for (key, bindings) in &resolved.branches {
            for (branch, rev) in bindings {
                branches.push(LockedBranch {
                    logical_package: id(key)?,
                    branch: branch.clone(),
                    rev: rev.clone(),
                });
            }
        }
        let mut graph = LockedGraph {
            id: LockedGraph::identity(root, target)?,
            root: root.into(),
            target: target.clone(),
            root_package: id(&resolved.root)?,
            input_fingerprint: fingerprint,
            packages: ids.into_values().collect(),
            edges,
            branches,
            resolved_with: compiler.clone(),
            base_graph_hash: None,
            base_packages: Vec::new(),
        };
        graph.packages.sort();
        if let Some((shared, base)) = baseline {
            graph.base_graph_hash = Some(base.semantic_hash()?);
            for id in &base.packages {
                let package = shared
                    .package(id)
                    .ok_or_else(|| LockError::invalid("shared baseline has a missing package"))?;
                graph.base_packages.push(BaselinePackage {
                    logical: package.logical.clone(),
                    version: package.version.clone(),
                    source: package.source.clone(),
                });
            }
            graph.base_packages.sort_by(|a, b| a.logical.cmp(&b.logical));
        }
        self.graphs.retain(|previous| previous.root != root || &previous.target != target);
        self.graphs.push(graph);
        let live: BTreeSet<_> =
            self.graphs.iter().flat_map(|graph| graph.packages.iter()).collect();
        self.packages.retain(|package| live.contains(&package.id));
        self.validate()
    }

    /// Validates record identities, aliases, graph reachability, and normal cycles.
    pub fn validate(&self) -> Result<(), LockError> {
        if self.lock_version != 1 || self.resolver_version != 1 {
            return Err(LockError {
                code: crate::UNSUPPORTED_LOCK,
                message: "unsupported lock or resolver version".into(),
            });
        }
        let mut packages = BTreeMap::new();
        for package in &self.packages {
            package.validate()?;
            if packages.insert(&package.id, package).is_some() {
                return Err(LockError::invalid("duplicate package record"));
            }
        }
        let mut graphs = BTreeSet::new();
        for graph in &self.graphs {
            validate_path(&graph.root)?;
            if graph.id != LockedGraph::identity(&graph.root, &graph.target)?
                || !graphs.insert(&graph.id)
            {
                return Err(LockError::invalid("duplicate or inconsistent graph identity"));
            }
            if self.context == LockContext::Shared
                && (graph.base_graph_hash.is_some() || !graph.base_packages.is_empty())
            {
                return Err(LockError::invalid("shared graphs cannot carry a personal baseline"));
            }
            let mut logical = BTreeSet::new();
            let members: BTreeSet<_> = graph.packages.iter().collect();
            if members.len() != graph.packages.len() || !members.contains(&graph.root_package) {
                return Err(LockError::invalid("graph has duplicate members or a missing root"));
            }
            for member in &members {
                let package = packages
                    .get(member)
                    .ok_or_else(|| LockError::invalid("graph names an absent package"))?;
                if !logical.insert(&package.logical) {
                    return Err(LockError::invalid(
                        "one graph selects multiple instances of a logical package",
                    ));
                }
            }
            let mut aliases = BTreeSet::new();
            let mut successors: BTreeMap<&String, Vec<&String>> = BTreeMap::new();
            let mut pending = VecDeque::from([&graph.root_package]);
            let mut indegree: BTreeMap<&String, usize> =
                members.iter().map(|member| (*member, 0)).collect();
            for edge in &graph.edges {
                if !members.contains(&edge.to) {
                    return Err(LockError::invalid("dependency points outside its graph"));
                }
                let source = if edge.kind == LockedEdgeKind::Development {
                    if edge.from != "$root" {
                        return Err(LockError::invalid(
                            "development edges must originate at the synthetic root",
                        ));
                    }
                    pending.push_back(&edge.to);
                    &graph.root_package
                } else {
                    if !members.contains(&edge.from) {
                        return Err(LockError::invalid("dependency source is not a graph member"));
                    }
                    successors.entry(&edge.from).or_default().push(&edge.to);
                    *indegree
                        .get_mut(&edge.to)
                        .ok_or_else(|| LockError::invalid("dependency target is absent"))? += 1;
                    &edge.from
                };
                if !aliases.insert((source, &edge.alias)) {
                    return Err(LockError::invalid("duplicate active dependency alias"));
                }
            }
            let mut reached = BTreeSet::new();
            while let Some(package) = pending.pop_front() {
                if reached.insert(package) {
                    pending.extend(successors.get(package).into_iter().flatten().copied());
                }
            }
            if reached != members {
                return Err(LockError::invalid("graph contains unreachable package records"));
            }
            let mut ready: Vec<_> =
                indegree.iter().filter(|(_, count)| **count == 0).map(|(key, _)| *key).collect();
            let mut processed = 0;
            while let Some(package) = ready.pop() {
                processed += 1;
                for target in successors.get(package).into_iter().flatten() {
                    let count = indegree
                        .get_mut(target)
                        .ok_or_else(|| LockError::invalid("dependency target is absent"))?;
                    *count -= 1;
                    if *count == 0 {
                        ready.push(target);
                    }
                }
            }
            if processed != members.len() {
                return Err(LockError::invalid("normal package dependency cycle"));
            }
            let mut branch_keys = BTreeSet::new();
            for binding in &graph.branches {
                let package = packages
                    .get(&binding.logical_package)
                    .ok_or_else(|| LockError::invalid("branch binding names an absent package"))?;
                if !members.contains(&binding.logical_package)
                    || !branch_keys.insert((&binding.logical_package, &binding.branch))
                    || !matches!(&package.source, LockSource::Git { rev, .. } if rev == &binding.rev)
                {
                    return Err(LockError::invalid(
                        "branch binding conflicts with its selected source",
                    ));
                }
            }
            let mut baselines = BTreeSet::new();
            for baseline in &graph.base_packages {
                baseline.logical.validate()?;
                if !baselines.insert(&baseline.logical) {
                    return Err(LockError::invalid("duplicate shared baseline selection"));
                }
            }
        }
        Ok(())
    }
}

/// Typed generated-state error for callers to locate in their lock document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LockError {
    /// Stable generated-state diagnostic category.
    pub code: fai_diagnostics::DiagnosticCode,
    /// Actionable validation explanation.
    pub message: String,
}

impl LockError {
    fn invalid(message: impl Into<String>) -> Self {
        Self { code: crate::INVALID_LOCK, message: message.into() }
    }
}

fn validate_path(path: &str) -> Result<(), LockError> {
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err(LockError::invalid("invalid local package path"));
    }
    Ok(())
}

fn json_hash(domain: &str, value: &impl Serialize) -> Result<Digest, LockError> {
    serde_json::to_vec(value)
        .map(|bytes| Digest::of(domain, &bytes))
        .map_err(|error| LockError::invalid(error.to_string()))
}

mod revision_wire {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        revision: &GitRevision,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!(
            "{}:{}",
            if revision.as_str().len() == 40 { "sha1" } else { "sha256" },
            revision
        ))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<GitRevision, D::Error> {
        let text = String::deserialize(deserializer)?;
        let hex = text
            .strip_prefix("sha1:")
            .filter(|value| value.len() == 40)
            .or_else(|| text.strip_prefix("sha256:").filter(|value| value.len() == 64))
            .ok_or_else(|| serde::de::Error::custom("expected a full sha1: or sha256: revision"))?;
        GitRevision::parse(hex).map_err(serde::de::Error::custom)
    }
}
