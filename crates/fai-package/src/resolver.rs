//! Provider-independent, deterministic resolution of one root/platform graph.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    DependencyAlias, GitBranch, GitPackage, GitRevision, PackageVersion, PlatformPredicate,
    TargetPlatform, VersionRequirement,
};

/// A logical package, independent of dependency aliases and selected revisions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum PackageKey {
    /// A canonical repository and package subdirectory.
    Git(GitPackage),
    /// A caller-normalized local directory identity.
    Path(String),
}

/// The immutable Git revision or mutable local directory supplying a package.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum PackageSource {
    /// Exact source bytes from a repository snapshot.
    Git { package: GitPackage, revision: GitRevision },
    /// Caller-normalized package directory.
    Path(String),
}

/// A source selector on one dependency edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum PackageSelector {
    /// A requirement over verified release tags.
    Version(VersionRequirement),
    /// An immutable full commit identifier.
    Revision(GitRevision),
    /// A literal branch with a provider-supplied, snapshot-consistent binding.
    Branch(GitBranch),
    /// A mutable path, optionally constrained by its declared version.
    Path(Option<VersionRequirement>),
}

/// A location-independent link back to an authored manifest field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequirementOrigin {
    /// Manifest identifier chosen by the caller.
    pub manifest: String,
    /// Field path understood by the manifest document's location table.
    pub field: String,
}

/// One declared dependency, with an explicit logical identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRequirement {
    /// Identity to unify within this graph.
    pub package: PackageKey,
    /// Authored source/version constraint.
    pub selector: PackageSelector,
    /// Source provenance for conflict explanations.
    pub origin: RequirementOrigin,
}

/// Validated metadata for a candidate package snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageCandidate {
    /// Actual selected source, distinct from a replaced logical key.
    pub source: PackageSource,
    /// Version declared by this snapshot.
    pub version: PackageVersion,
    /// Whether the declared version has a verified canonical release tag here.
    pub released: bool,
    /// Compatible compiler releases.
    pub compiler: VersionRequirement,
    /// Empty for unrestricted, otherwise an OR-list of target predicates.
    pub platforms: Vec<PlatformPredicate>,
    /// Active normal dependencies; inactive target declarations are excluded.
    pub dependencies: BTreeMap<DependencyAlias, PackageRequirement>,
    /// Active development declarations, used only for the selected root.
    pub development: BTreeMap<DependencyAlias, PackageRequirement>,
    /// Digest of semantic manifest inputs, independent of comments and locations.
    pub manifest_fingerprint: String,
    /// Verified immutable source digest, absent for mutable local directories.
    pub checksum: Option<String>,
}

/// A previously selected source/version, with mutable-ref bindings kept explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackagePin {
    /// Exact previous source.
    pub source: PackageSource,
    /// Previous declared version.
    pub version: PackageVersion,
    /// Whether a canonical release mapping was accepted for this version/source.
    pub released: bool,
    /// Branch selections to retain during conservative repair.
    pub branches: BTreeMap<GitBranch, GitRevision>,
}

impl PackagePin {
    /// Captures a candidate without inventing any branch provenance.
    #[must_use]
    pub fn candidate(candidate: &PackageCandidate) -> Self {
        Self {
            source: candidate.source.clone(),
            version: candidate.version.clone(),
            released: candidate.released,
            branches: BTreeMap::new(),
        }
    }

    fn matches(&self, candidate: &PackageCandidate) -> bool {
        self.source == candidate.source && self.version == candidate.version
    }
}

/// Snapshot-consistent candidate metadata returned by a source provider.
#[derive(Debug, Clone, Default)]
pub struct CandidateSet {
    /// All available candidates, not just the current per-edge maximum.
    pub candidates: Vec<PackageCandidate>,
    /// Bindings for requested branches. The provider retains unchanged pins
    /// unless `refresh` is requested or that branch has no prior binding.
    pub branches: BTreeMap<GitBranch, GitRevision>,
}

/// A retrieval/integrity failure that must never be treated as an older-version fallback.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProviderFailure {
    /// Stable package diagnostic category.
    pub code: fai_diagnostics::DiagnosticCode,
    /// Sanitized provider explanation.
    pub message: String,
}

/// Supplies immutable metadata without exposing I/O to compiler queries.
pub trait ResolverProvider {
    /// Returns release candidates plus every requested commit/branch candidate.
    /// Version selectors must not prematurely discard candidates needed by
    /// targeted updates. Results are sorted and filtered by the resolver.
    fn candidates(
        &mut self,
        package: &PackageKey,
        selectors: &[PackageSelector],
        pin: Option<&PackagePin>,
        refresh: bool,
    ) -> Result<CandidateSet, ProviderFailure>;
}

/// Observable version-selection preference, after all constraints are enforced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionPolicy {
    /// Retain existing pins where possible, then select newest eligible sources.
    Conservative,
    /// Release every relevant pin and refresh mutable refs.
    UpdateAll,
    /// Maximize these keys first, retaining other pins where possible.
    Update(BTreeSet<PackageKey>),
}

/// Inputs for exactly one root, target, and shared/personal context.
pub struct ResolutionRequest {
    /// The selected root's logical identity.
    pub root: PackageKey,
    /// Current root metadata; development edges originate at a synthetic root.
    pub candidate: PackageCandidate,
    /// Compiler eligibility filter.
    pub compiler: PackageVersion,
    /// Target eligibility filter.
    pub target: TargetPlatform,
    /// Existing source selections for this graph only.
    pub pins: BTreeMap<PackageKey, PackagePin>,
    /// Fully precedence-resolved root replacements, keyed by original identity.
    pub overrides: BTreeMap<PackageKey, PackageRequirement>,
    /// Source-selection preferences.
    pub policy: ResolutionPolicy,
}

/// One exact graph edge; `None` denotes the synthetic development root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedEdge {
    /// Normal source package, or the synthetic root for development dependencies.
    pub from: Option<PackageKey>,
    /// Authored import alias.
    pub alias: DependencyAlias,
    /// Selected logical package.
    pub to: PackageKey,
}

/// A complete, validated source graph with one selection per logical package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedGraph {
    /// Selected root.
    pub root: PackageKey,
    /// Exact selected sources and metadata.
    pub packages: BTreeMap<PackageKey, PackageCandidate>,
    /// Exact alias-labelled normal and development edges.
    pub edges: Vec<ResolvedEdge>,
    /// Only branch bindings actually used in this solution.
    pub branches: BTreeMap<PackageKey, BTreeMap<GitBranch, GitRevision>>,
}

/// A conflict explains the converging authored requirements, rather than only a range string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ResolutionConflict {
    /// Causal requirements, with manifest-field provenance.
    pub requirements: Vec<PackageRequirement>,
    /// Explanation of the unsatisfied constraint or cycle.
    pub message: String,
}

/// Distinguishes unsatisfiability from fatal provider failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionError {
    /// No selection can satisfy the authored requirements.
    #[error(transparent)]
    Conflict(#[from] ResolutionConflict),
    /// Source retrieval or integrity failed; preferences cannot bypass it.
    #[error(transparent)]
    Provider(#[from] ProviderFailure),
}

struct Resolver<'a, P> {
    provider: &'a mut P,
    request: &'a ResolutionRequest,
    cache: BTreeMap<PackageKey, (Vec<PackageSelector>, CandidateSet)>,
    conflict: ResolutionConflict,
    root_alias: Option<PackageKey>,
    required_targets: BTreeSet<PackageKey>,
}

/// Resolves one graph. Provider response order and manifest table order do not
/// influence the result, and development requirements never propagate from consumers.
pub fn resolve_graph(
    provider: &mut impl ResolverProvider,
    request: &ResolutionRequest,
) -> Result<ResolvedGraph, ResolutionError> {
    if let Some(alias) = request
        .candidate
        .dependencies
        .keys()
        .find(|alias| request.candidate.development.contains_key(*alias))
    {
        return Err(ResolutionConflict {
            requirements: vec![
                request.candidate.dependencies[alias].clone(),
                request.candidate.development[alias].clone(),
            ],
            message: format!("duplicate active dependency alias `{alias}`"),
        }
        .into());
    }
    let aliases: Vec<_> = request.overrides.iter().filter(|(_, replacement)| {
        matches!((&replacement.package, &request.candidate.source), (PackageKey::Path(path), PackageSource::Path(root)) if path == root)
    }).map(|(key, _)| key.clone()).collect();
    if aliases.len() > 1 {
        return Err(ResolutionConflict {
            requirements: Vec::new(),
            message: "multiple logical identities replace the selected root package".into(),
        }
        .into());
    }
    let mut resolver = Resolver {
        provider,
        request,
        cache: BTreeMap::new(),
        root_alias: aliases.first().cloned(),
        required_targets: match &request.policy {
            ResolutionPolicy::Update(keys) => keys.clone(),
            _ => BTreeSet::new(),
        },
        conflict: ResolutionConflict {
            requirements: Vec::new(),
            message: "no compatible dependency graph".into(),
        },
    };
    if !resolver.eligible(&request.candidate) {
        return Err(ResolutionConflict {
            requirements: Vec::new(),
            message: "the root does not support the selected compiler/platform".into(),
        }
        .into());
    }
    let mut fixed = BTreeMap::new();
    if let ResolutionPolicy::Update(targets) = &request.policy {
        let initial = resolver
            .search(&fixed, BTreeMap::from([(request.root.clone(), request.candidate.clone())]), 0)?
            .ok_or_else(|| resolver.conflict.clone())?;
        for key in targets {
            if !initial.packages.contains_key(key) {
                return Err(ResolutionConflict {
                    requirements: Vec::new(),
                    message: format!(
                        "requested update package {key:?} is not in the selected graph"
                    ),
                }
                .into());
            }
            if key == &request.root {
                continue;
            }
            let candidates =
                resolver.cache.get(key).map(|(_, set)| set.candidates.clone()).unwrap_or_default();
            let mut accepted = false;
            for candidate in candidates {
                let mut trial = fixed.clone();
                trial.insert(key.clone(), PackagePin::candidate(&candidate));
                if resolver
                    .prefer_pins(&trial)?
                    .is_some_and(|graph| graph.packages.contains_key(key))
                {
                    fixed = trial;
                    accepted = true;
                    break;
                }
            }
            if !accepted {
                return Err(resolver.conflict.clone().into());
            }
        }
    }
    resolver.prefer_pins(&fixed)?.ok_or_else(|| resolver.conflict.into())
}

impl<P: ResolverProvider> Resolver<'_, P> {
    fn key(&self, key: &PackageKey) -> PackageKey {
        if self.root_alias.as_ref() == Some(key) { self.request.root.clone() } else { key.clone() }
    }

    fn eligible(&self, candidate: &PackageCandidate) -> bool {
        candidate.compiler.matches(&self.request.compiler)
            && (candidate.platforms.is_empty()
                || candidate
                    .platforms
                    .iter()
                    .any(|predicate| predicate.matches(&self.request.target)))
    }

    fn prefer_pins(
        &mut self,
        fixed: &BTreeMap<PackageKey, PackagePin>,
    ) -> Result<Option<ResolvedGraph>, ResolutionError> {
        let root = || BTreeMap::from([(self.request.root.clone(), self.request.candidate.clone())]);
        let initial = root();
        if self.request.policy == ResolutionPolicy::UpdateAll {
            return self.search(fixed, initial, 0);
        }
        let mut all = self.request.pins.clone();
        all.remove(&self.request.root);
        all.extend(fixed.clone());
        if let Some(graph) = self.search(&all, initial.clone(), 0)? {
            return Ok(Some(graph));
        }
        let Some(mut best) = self.search(fixed, initial.clone(), 0)? else {
            return Ok(None);
        };
        let mut retained = fixed.clone();
        for (key, pin) in &self.request.pins.clone() {
            if key == &self.request.root || fixed.contains_key(key) {
                continue;
            }
            let mut trial = retained.clone();
            trial.insert(key.clone(), pin.clone());
            if let Some(graph) = self.search(&trial, initial.clone(), 0)? {
                retained = trial;
                best = graph;
            }
        }
        Ok(Some(best))
    }

    fn requirements(
        &self,
        selected: &BTreeMap<PackageKey, PackageCandidate>,
    ) -> BTreeMap<PackageKey, Vec<PackageRequirement>> {
        let mut required: BTreeMap<PackageKey, Vec<PackageRequirement>> = BTreeMap::new();
        for candidate in selected.values() {
            for requirement in candidate.dependencies.values() {
                required
                    .entry(self.key(&requirement.package))
                    .or_default()
                    .push(requirement.clone());
            }
        }
        for requirement in self.request.candidate.development.values() {
            required.entry(self.key(&requirement.package)).or_default().push(requirement.clone());
        }
        required
    }

    fn candidate_set(
        &mut self,
        key: &PackageKey,
        requirements: &[PackageRequirement],
    ) -> Result<CandidateSet, ResolutionError> {
        let replacement = self.request.overrides.get(key);
        let actual = replacement.map_or(key, |replacement| &replacement.package);
        let mut selectors = if let Some(replacement) = replacement {
            vec![replacement.selector.clone()]
        } else {
            requirements.iter().map(|requirement| requirement.selector.clone()).collect()
        };
        selectors.sort_by_key(|selector| format!("{selector:?}"));
        selectors.dedup();
        if let Some((previous, set)) = self.cache.get(key)
            && previous == &selectors
        {
            return Ok(set.clone());
        }
        let refresh = match &self.request.policy {
            ResolutionPolicy::Conservative => false,
            ResolutionPolicy::UpdateAll => true,
            ResolutionPolicy::Update(targets) => targets.contains(key),
        };
        let pin = self.request.pins.get(key).filter(|pin| match (&pin.source, actual) {
            (PackageSource::Git { package, .. }, PackageKey::Git(expected)) => package == expected,
            (PackageSource::Path(path), PackageKey::Path(expected)) => path == expected,
            _ => false,
        });
        let mut set = self.provider.candidates(actual, &selectors, pin, refresh)?;
        let mut sources = BTreeMap::new();
        for candidate in &set.candidates {
            let same_source = match (actual, &candidate.source) {
                (PackageKey::Git(expected), PackageSource::Git { package, .. }) => {
                    expected == package
                }
                (PackageKey::Path(expected), PackageSource::Path(path)) => expected == path,
                _ => false,
            };
            if !same_source
                || sources
                    .insert(candidate.source.clone(), candidate)
                    .is_some_and(|previous| previous != candidate)
            {
                return Err(ProviderFailure {
                    code: crate::SOURCE_UNAVAILABLE,
                    message: "source provider returned inconsistent package identity or metadata"
                        .into(),
                }
                .into());
            }
        }
        set.candidates.retain(|candidate| self.eligible(candidate));
        set.candidates.sort_by(|left, right| {
            right.version.cmp(&left.version).then_with(|| left.source.cmp(&right.source))
        });
        set.candidates.dedup();
        self.cache.insert(key.clone(), (selectors, set.clone()));
        Ok(set)
    }

    fn satisfies(
        &self,
        candidate: &PackageCandidate,
        requirement: &PackageRequirement,
        branches: &BTreeMap<GitBranch, GitRevision>,
    ) -> bool {
        if self.request.overrides.contains_key(&requirement.package) {
            return match &requirement.selector {
                PackageSelector::Version(version) => version.matches(&candidate.version),
                PackageSelector::Path(Some(version)) => version.matches(&candidate.version),
                _ => true,
            };
        }
        matches_selector(candidate, &requirement.selector, branches)
    }

    fn search(
        &mut self,
        fixed: &BTreeMap<PackageKey, PackagePin>,
        selected: BTreeMap<PackageKey, PackageCandidate>,
        depth: usize,
    ) -> Result<Option<ResolvedGraph>, ResolutionError> {
        if depth > 256 {
            return Err(ResolutionConflict {
                requirements: Vec::new(),
                message: "dependency search exceeds 256 package decisions".into(),
            }
            .into());
        }
        let required = self.requirements(&selected);
        for (key, candidate) in &selected {
            let requirements = required.get(key).cloned().unwrap_or_default();
            let branches = if key == &self.request.root {
                BTreeMap::new()
            } else {
                self.candidate_set(key, &requirements)?.branches
            };
            if fixed.get(key).is_some_and(|pin| !pin.matches(candidate))
                || requirements
                    .iter()
                    .any(|requirement| !self.satisfies(candidate, requirement, &branches))
                || (key == &self.request.root
                    && self
                        .root_alias
                        .as_ref()
                        .and_then(|key| self.request.overrides.get(key))
                        .is_some_and(|replacement| {
                            !matches_selector(candidate, &replacement.selector, &branches)
                        }))
            {
                self.conflict = ResolutionConflict {
                    requirements,
                    message: format!(
                        "requirements for {key:?} cannot use version {} from {:?}",
                        candidate.version, candidate.source
                    ),
                };
                return Ok(None);
            }
        }
        if has_cycle(&selected, |key| self.key(key)) {
            self.conflict = ResolutionConflict {
                requirements: required.values().flatten().cloned().collect(),
                message: "normal package dependencies form a cycle".into(),
            };
            return Ok(None);
        }
        let rank = |key: &PackageKey| {
            if self
                .request
                .candidate
                .dependencies
                .values()
                .any(|requirement| self.key(&requirement.package) == *key)
            {
                0
            } else if self
                .request
                .candidate
                .development
                .values()
                .any(|requirement| self.key(&requirement.package) == *key)
            {
                1
            } else {
                2
            }
        };
        let next = required
            .keys()
            .filter(|key| !selected.contains_key(*key))
            .min_by_key(|key| (rank(key), *key))
            .cloned();
        let Some(key) = next else {
            if self.required_targets.iter().any(|key| !selected.contains_key(key)) {
                return Ok(None);
            }
            return Ok(Some(self.graph(selected)));
        };
        let requirements = &required[&key];
        let set = self.candidate_set(&key, requirements)?;
        let mut attempted = false;
        for candidate in set.candidates {
            if fixed.get(&key).is_some_and(|pin| !pin.matches(&candidate))
                || requirements
                    .iter()
                    .any(|requirement| !self.satisfies(&candidate, requirement, &set.branches))
                || self.request.overrides.get(&key).is_some_and(|replacement| {
                    !matches_selector(&candidate, &replacement.selector, &set.branches)
                })
            {
                continue;
            }
            attempted = true;
            let mut chosen = selected.clone();
            chosen.insert(key.clone(), candidate);
            if let Some(graph) = self.search(fixed, chosen, depth + 1)? {
                return Ok(Some(graph));
            }
        }
        if !attempted {
            self.conflict = ResolutionConflict {
                requirements: required.values().flatten().cloned().collect(),
                message: format!(
                    "no source for {key:?} satisfies all required selectors on {} with compiler {}",
                    self.request.target, self.request.compiler
                ),
            };
        }
        Ok(None)
    }

    fn graph(&self, packages: BTreeMap<PackageKey, PackageCandidate>) -> ResolvedGraph {
        let mut edges = Vec::new();
        for (key, candidate) in &packages {
            for (alias, requirement) in &candidate.dependencies {
                edges.push(ResolvedEdge {
                    from: Some(key.clone()),
                    alias: alias.clone(),
                    to: self.key(&requirement.package),
                });
            }
        }
        for (alias, requirement) in &self.request.candidate.development {
            edges.push(ResolvedEdge {
                from: None,
                alias: alias.clone(),
                to: self.key(&requirement.package),
            });
        }
        let mut branches = BTreeMap::new();
        for (key, requirements) in self.requirements(&packages) {
            let requested: Vec<_> = if let Some(replacement) = self.request.overrides.get(&key) {
                vec![&replacement.selector]
            } else {
                requirements.iter().map(|requirement| &requirement.selector).collect()
            };
            let Some((_, set)) = self.cache.get(&key) else {
                continue;
            };
            let used: BTreeMap<_, _> = requested
                .into_iter()
                .filter_map(|selector| {
                    let PackageSelector::Branch(branch) = selector else {
                        return None;
                    };
                    Some((branch.clone(), set.branches.get(branch)?.clone()))
                })
                .collect();
            if !used.is_empty() {
                branches.insert(key, used);
            }
        }
        ResolvedGraph { root: self.request.root.clone(), packages, edges, branches }
    }
}

/// Checks one source selector against validated candidate and branch metadata.
pub fn matches_selector(
    candidate: &PackageCandidate,
    selector: &PackageSelector,
    branches: &BTreeMap<GitBranch, GitRevision>,
) -> bool {
    match selector {
        PackageSelector::Version(version) => {
            candidate.released && version.matches(&candidate.version)
        }
        PackageSelector::Path(version) => {
            matches!(candidate.source, PackageSource::Path(_))
                && version.as_ref().is_none_or(|version| version.matches(&candidate.version))
        }
        PackageSelector::Revision(revision) => {
            matches!(&candidate.source, PackageSource::Git { revision: selected, .. } if revision == selected)
        }
        PackageSelector::Branch(branch) => {
            matches!(&candidate.source, PackageSource::Git { revision, .. } if branches.get(branch) == Some(revision))
        }
    }
}

fn has_cycle(
    packages: &BTreeMap<PackageKey, PackageCandidate>,
    key: impl Fn(&PackageKey) -> PackageKey,
) -> bool {
    let mut incoming: BTreeMap<_, usize> = packages.keys().cloned().map(|key| (key, 0)).collect();
    for candidate in packages.values() {
        for requirement in candidate.dependencies.values() {
            if let Some(count) = incoming.get_mut(&key(&requirement.package)) {
                *count += 1;
            }
        }
    }
    let mut ready: Vec<_> =
        incoming.iter().filter(|(_, count)| **count == 0).map(|(key, _)| key.clone()).collect();
    let mut count = 0;
    while let Some(current) = ready.pop() {
        count += 1;
        for requirement in packages[&current].dependencies.values() {
            let next = key(&requirement.package);
            if let Some(count) = incoming.get_mut(&next) {
                *count -= 1;
                if *count == 0 {
                    ready.push(next);
                }
            }
        }
    }
    count != packages.len()
}
