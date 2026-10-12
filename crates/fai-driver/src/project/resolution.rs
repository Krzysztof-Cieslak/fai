//! Locked graph validation, conservative repair, and compiler instantiation.

use fai_package::{
    CandidateSet, Digest, LockContext, LockFile, LockedGraph, PackageCandidate, PackageKey,
    PackagePin, PackageRequirement, PackageSelector, PackageSource, ProviderFailure,
    RequirementOrigin, ResolutionError, ResolutionPolicy, ResolutionRequest, ResolvedEdge,
    ResolvedGraph, ResolverProvider,
};
use fai_package_store::{FileEdit, GitStore, MetadataTransaction};

use super::*;

pub(super) struct State {
    options: ProjectOptions,
    store: GitStore,
    lock: LockFile,
    original: Option<String>,
    shared: Option<LockFile>,
    shared_text: Option<String>,
    changed: bool,
    on_disk: LockFile,
    mode: PrepareMode,
    vendor_root: Option<Utf8PathBuf>,
}

impl State {
    pub(super) fn open(
        loader: &mut Loader<'_>,
        options: &ProjectOptions,
    ) -> Result<Self, DriverError> {
        let root = &loader.configuration_root;
        if !loader.mode.release_validation && root.join(".fai/transaction.json").exists() {
            MetadataTransaction::recover(root)?;
            return Err(error(
                fai_package::INPUTS_CHANGED,
                "recovered an interrupted metadata update; retry against the recovered manifests",
            ));
        }
        let context = if loader.project.local { LockContext::Local } else { LockContext::Shared };
        let original = read_optional(&root.join(lock_name(context)))?;
        if loader.mode.expected_lock.as_ref().is_some_and(|expected| {
            *expected != original.as_ref().map(|text| FileEdit::fingerprint(text.as_bytes()))
        }) {
            return Err(error(
                fai_package::INPUTS_CHANGED,
                "the active lock changed after the dependency edit preview",
            ));
        }
        let on_disk = match &original {
            _ if loader.mode.release_validation => LockFile::new(context),
            Some(text) => LockFile::parse(text)?,
            None => LockFile::new(context),
        };
        if on_disk.context != context {
            return Err(error(
                fai_package::INVALID_LOCK,
                "lockfile context does not match its selected filename",
            ));
        }
        let shared_text = if context == LockContext::Local {
            read_optional(&root.join("fai.lock"))?
        } else {
            None
        };
        let shared = shared_text.as_deref().map(LockFile::parse).transpose()?;
        if options.locked
            && (matches!(loader.mode.policy, PreparePolicy::Update(_))
                || !loader.mode.manifest_edits.is_empty())
        {
            return Err(error(
                fai_package::STALE_LOCK,
                "this metadata operation requires lock mutation and cannot use --locked",
            ));
        }
        let lock = loader.mode.seed_lock.clone().unwrap_or_else(|| on_disk.clone());
        let cache = loader
            .mode
            .cache_dir
            .clone()
            .map(Utf8PathBuf::into_std_path_buf)
            .or_else(crate::cache::cache_dir)
            .unwrap_or_else(|| std::env::temp_dir().join("fai-cache"));
        let cache = Utf8PathBuf::from_path_buf(cache)
            .map_err(|path| DriverError::NonUtf8Path(path.to_string_lossy().into_owned()))?;
        let mut store = GitStore::new(cache.join("packages"), options.offline);
        for (logical, actual) in &loader.mode.transports {
            store = store.with_transport(logical.clone(), actual.clone());
        }
        Ok(Self {
            options: options.clone(),
            store,
            lock,
            original,
            shared,
            shared_text,
            changed: false,
            on_disk,
            mode: loader.mode.clone(),
            vendor_root: if options.no_vendor { None } else { loader.vendor_dir.clone() },
        })
    }

    pub(super) fn resolve_root(
        &mut self,
        loader: &mut Loader<'_>,
        root: &Utf8Path,
        member: &PackagePath,
    ) -> Result<String, DriverError> {
        let root_name =
            portable_path(root.strip_prefix(&loader.configuration_root).unwrap_or(root));
        let previous = self.lock.graph(&root_name, &loader.target).cloned();
        let baseline =
            self.shared.as_ref().and_then(|lock| lock.graph(&root_name, &loader.target)).cloned();
        let mut provider = Provider::new(loader, &mut self.store, root, self.vendor_root.clone());
        let root_source = PackageSource::Path(provider.path_key(root));
        let root_key = PackageKey::Path(provider.path_key(root));
        let root_candidate = provider.candidate(&root_source, false)?;
        let overrides = provider.overrides(member)?;
        let mut request = ResolutionRequest {
            root: root_key,
            candidate: root_candidate,
            compiler: provider.loader.compiler.clone(),
            target: provider.loader.target.clone(),
            pins: pins(&self.lock, previous.as_ref()),
            overrides,
            policy: ResolutionPolicy::Conservative,
        };
        if let PreparePolicy::Update(selectors) = &self.mode.policy {
            request.policy = update_policy(selectors, &request, &self.lock, previous.as_ref())?;
        }
        if self.lock.context == LockContext::Local {
            reconcile(
                &mut request.pins,
                previous.as_ref(),
                self.shared.as_ref(),
                baseline.as_ref(),
            );
        }
        let current_fingerprint = inputs(&request)?;
        let locked = if self.mode.policy == PreparePolicy::Conservative || self.options.locked {
            if let Some(previous) = &previous {
                let baseline_hash =
                    baseline.as_ref().map(LockedGraph::semantic_hash).transpose()?;
                if previous.input_fingerprint == current_fingerprint
                    && previous.base_graph_hash == baseline_hash
                {
                    provider.locked_graph(&request, &self.lock, previous)?
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let mut resolved = if let Some(locked) = locked {
            locked
        } else {
            if self.options.locked {
                return Err(error(
                    fai_package::STALE_LOCK,
                    format!(
                        "selected graph {root_name} on {} is missing or stale; run fai resolve",
                        request.target
                    ),
                ));
            }
            let result = fai_package::resolve_graph(&mut provider, &request);
            let mut resolved = match result {
                Ok(resolved) => resolved,
                Err(failure) => return Err(provider.resolution_error(failure)),
            };
            for candidate in resolved.packages.values() {
                if candidate.released
                    && let PackageSource::Git { package, revision } = &candidate.source
                {
                    provider.store.accept_release(package, &candidate.version, revision)?;
                }
            }
            provider.materialize(&mut resolved)?;
            self.lock.record(
                &root_name,
                &request.target,
                &request.compiler,
                current_fingerprint,
                &resolved,
                self.shared.as_ref().zip(baseline.as_ref()),
            )?;
            self.changed = true;
            resolved
        };
        // Exact restoration also verifies cached bytes before publishing inputs.
        provider.materialize(&mut resolved)?;
        if self.mode.metadata_only { Ok(String::new()) } else { provider.install(&resolved) }
    }

    pub(super) fn inspect_root(
        &mut self,
        loader: &mut Loader<'_>,
        root: &Utf8Path,
        member: &PackagePath,
    ) -> Result<(), DriverError> {
        let name = portable_path(root.strip_prefix(&loader.configuration_root).unwrap_or(root));
        let mut provider = Provider::new(loader, &mut self.store, root, self.vendor_root.clone());
        let key = provider.path_key(root);
        let candidate = match provider.candidate(&PackageSource::Path(key.clone()), false) {
            Ok(candidate) => candidate,
            Err(failure) => {
                provider
                    .loader
                    .project
                    .stale_graphs
                    .push(format!("{name}@{}", provider.loader.target));
                provider.loader.project.notices.push(failure.to_string());
                return Ok(());
            }
        };
        let request = ResolutionRequest {
            root: PackageKey::Path(key.clone()),
            candidate,
            compiler: provider.loader.compiler.clone(),
            target: provider.loader.target.clone(),
            pins: BTreeMap::new(),
            overrides: provider.overrides(member)?,
            policy: ResolutionPolicy::Conservative,
        };
        let mut valid = self.lock.graph(&name, &request.target).is_some_and(|graph| {
            inputs(&request).is_ok_and(|fingerprint| fingerprint == graph.input_fingerprint)
        });
        if let Some(graph) = self.lock.graph(&name, &request.target) {
            for package in graph.packages.iter().filter_map(|id| self.lock.package(id)) {
                if matches!(package.source, fai_package::LockSource::Path { .. }) {
                    valid &= provider.candidate(&package.source.source(), false).is_ok_and(
                        |candidate| {
                            candidate.manifest_fingerprint == package.manifest_fingerprint.as_str()
                        },
                    );
                }
            }
            let shared_hash = self
                .shared
                .as_ref()
                .and_then(|shared| shared.graph(&name, &request.target))
                .map(LockedGraph::semantic_hash)
                .transpose()?;
            valid &= graph.base_graph_hash == shared_hash;
        }
        if !valid {
            provider.loader.project.stale_graphs.push(format!("{name}@{}", request.target));
        }
        Ok(())
    }

    pub(super) fn commit(self, loader: &mut Loader<'_>) -> Result<(), DriverError> {
        if self.changed && self.options.offline {
            loader.project.notices.push("resolved selected dependency graphs offline".into());
        }
        loader.project.notices.sort();
        loader.project.notices.dedup();
        let mut observations = Vec::new();
        for (path, fingerprint) in &loader.observed {
            observations.push((path.to_string(), Some(fingerprint.clone())));
        }
        if loader.project.local {
            observations.push((
                loader.configuration_root.join("fai.lock").to_string(),
                self.shared_text.as_deref().map(|text| FileEdit::fingerprint(text.as_bytes())),
            ));
        }
        if self.mode.write && (self.changed || !self.mode.manifest_edits.is_empty()) {
            let after = self.lock.to_toml()?;
            let mut edits = Vec::new();
            for (path, after) in &self.mode.manifest_edits {
                let relative = path.strip_prefix(&loader.configuration_root).map_err(|_| {
                    error(
                        fai_package::INVALID_SOURCE,
                        "edited manifest is outside the active project",
                    )
                })?;
                edits.push(FileEdit {
                    path: portable_path(relative),
                    before: loader.observed.get(path).cloned(),
                    after: after.clone(),
                });
            }
            edits.push(FileEdit::new(
                lock_name(self.lock.context).into(),
                self.original.as_deref(),
                after,
            ));
            MetadataTransaction::new(edits, observations.clone())
                .commit(&loader.configuration_root)?;
        }
        for (path, fingerprint) in observations {
            if let Some(fingerprint) = fingerprint {
                if self.mode.write && self.mode.manifest_edits.contains_key(Utf8Path::new(&path)) {
                    continue;
                }
                loader.project.observed.push((path.into(), fingerprint));
            }
        }
        let lock_path = loader.configuration_root.join(lock_name(self.lock.context));
        if !self.mode.write && read_optional(&lock_path)? != self.original {
            return Err(error(
                fai_package::INPUTS_CHANGED,
                "the active lock changed during read-only preparation",
            ));
        }
        if let Some(text) = read_optional(&lock_path)? {
            loader.project.observed.push((lock_path, FileEdit::fingerprint(text.as_bytes())));
        }
        loader.project.lock = Some(self.lock);
        loader.project.previous_lock = Some(self.on_disk);
        loader.project.original_lock = self.original;
        Ok(())
    }
}

struct Provider<'a, 'files> {
    loader: &'a mut Loader<'files>,
    store: &'a mut GitStore,
    root: Utf8PathBuf,
    metadata: BTreeMap<PackageSource, Arc<ManifestDocument>>,
    locations: BTreeMap<PackageSource, Utf8PathBuf>,
    verified: BTreeMap<PackageSource, Digest>,
    last_error: Option<DriverError>,
    vendor_root: Option<Utf8PathBuf>,
    vendor_index: Option<Arc<fai_package_store::VendorIndex>>,
}

impl<'a, 'files> Provider<'a, 'files> {
    fn new(
        loader: &'a mut Loader<'files>,
        store: &'a mut GitStore,
        root: &Utf8Path,
        vendor_root: Option<Utf8PathBuf>,
    ) -> Self {
        Self {
            loader,
            store,
            root: root.to_owned(),
            metadata: BTreeMap::new(),
            locations: BTreeMap::new(),
            verified: BTreeMap::new(),
            last_error: None,
            vendor_root,
            vendor_index: None,
        }
    }

    fn path_key(&self, path: &Utf8Path) -> String {
        path.strip_prefix(&self.loader.configuration_root)
            .map_or_else(|_| path.to_string(), portable_path)
    }

    fn local_path(&self, path: &str) -> Utf8PathBuf {
        self.loader.configuration_root.join(path)
    }

    fn dependency(
        &mut self,
        dependency: &Dependency,
        directory: &Utf8Path,
        origin: RequirementOrigin,
        git_source: bool,
    ) -> Result<PackageRequirement, DriverError> {
        let (package, selector) = match dependency {
            Dependency::Git { package, selector } => (
                PackageKey::Git(package.clone()),
                match selector {
                    GitSelector::Version(version) => PackageSelector::Version(version.clone()),
                    GitSelector::Revision(revision) => PackageSelector::Revision(revision.clone()),
                    GitSelector::Branch(branch) => PackageSelector::Branch(branch.clone()),
                },
            ),
            Dependency::Path { path, version } => {
                if git_source {
                    return Err(error(
                        fai_package::INVALID_MANIFEST,
                        "Git package dependencies must use portable Git declarations",
                    ));
                }
                let path = canonical(&directory.join(path))?;
                (PackageKey::Path(self.path_key(&path)), PackageSelector::Path(version.clone()))
            }
        };
        Ok(PackageRequirement { package, selector, origin })
    }

    fn candidate(
        &mut self,
        source: &PackageSource,
        released: bool,
    ) -> Result<PackageCandidate, DriverError> {
        let (document, directory, label) = if let Some(document) = self.metadata.get(source) {
            let directory = self.locations.get(source).cloned().unwrap_or_default();
            let label = match source {
                PackageSource::Path(_) => directory.join("fai.toml").to_string(),
                PackageSource::Git { package, revision } => {
                    format!("<git:{}@{revision}/{}>/fai.toml", package.repository, package.subdir)
                }
            };
            (document.clone(), directory, label)
        } else {
            match source {
                PackageSource::Path(path) => {
                    let directory = self.local_path(path);
                    let document =
                        self.loader.document(&directory.join(fai_package::MANIFEST_NAME), false)?;
                    self.locations.insert(source.clone(), directory.clone());
                    (
                        document,
                        directory.clone(),
                        directory.join(fai_package::MANIFEST_NAME).to_string(),
                    )
                }
                PackageSource::Git { package, revision } => {
                    let text = self.store.manifest(package, revision)?.ok_or_else(|| {
                        error(
                            fai_package::INVALID_MANIFEST,
                            format!(
                                "{} at {revision} has no package manifest in {}",
                                package.repository, package.subdir
                            ),
                        )
                    })?;
                    let label = format!(
                        "<git:{}@{revision}/{}>/fai.toml",
                        package.repository, package.subdir
                    );
                    let id = self.loader.sources.add(label.clone().into(), text.clone());
                    let document = Arc::new(
                        ManifestDocument::parse(id, &text)
                            .map_err(|diagnostics| self.loader.failure(diagnostics))?,
                    );
                    (document, Utf8PathBuf::new(), label)
                }
            }
        };
        self.metadata.insert(source.clone(), document.clone());
        let manifest = document.manifest();
        let package = manifest.package.as_ref().ok_or_else(|| {
            self.loader.error(
                &document,
                "package",
                fai_package::INVALID_MANIFEST,
                "selected source has no package metadata",
            )
        })?;
        let active = manifest.configuration(&self.loader.target).map_err(|failure| {
            self.loader.error(
                &document,
                "package.supported-platforms",
                fai_package::INVALID_PLATFORM,
                failure.to_string(),
            )
        })?;
        let mut dependencies = BTreeMap::new();
        for (alias, dependency) in &active.dependencies {
            let origin = RequirementOrigin {
                manifest: label.clone(),
                field: format!("dependencies.{alias}"),
            };
            dependencies.insert(
                alias.clone(),
                self.dependency(
                    dependency,
                    &directory,
                    origin,
                    matches!(source, PackageSource::Git { .. }),
                )?,
            );
        }
        let mut development = BTreeMap::new();
        if directory == self.root {
            for (alias, dependency) in &active.dev_dependencies {
                let origin = RequirementOrigin {
                    manifest: label.clone(),
                    field: format!("dev-dependencies.{alias}"),
                };
                development
                    .insert(alias.clone(), self.dependency(dependency, &directory, origin, false)?);
            }
        }
        // Development constraints are fingerprinted by the root graph, not by
        // each consumed production package's metadata record.
        let fingerprint = Digest::of(
            "fai.package-metadata.v1",
            &serde_json::to_vec(&(
                &package.version,
                &package.fai_version,
                &package.supported_platforms,
                &active.source_dirs,
                &active.test_source_dirs,
                package.auto_main,
                &manifest.binaries,
                &active.dependencies,
                &active.native,
            ))
            .map_err(|failure| project_message(failure.to_string()))?,
        );
        Ok(PackageCandidate {
            source: source.clone(),
            version: package.version.clone(),
            released,
            compiler: package.fai_version.clone(),
            platforms: package.supported_platforms.clone(),
            dependencies,
            development,
            manifest_fingerprint: fingerprint.as_str().into(),
            checksum: None,
        })
    }

    fn overrides(
        &mut self,
        member: &PackagePath,
    ) -> Result<BTreeMap<PackageKey, PackageRequirement>, DriverError> {
        let identities: BTreeSet<_> = self
            .loader
            .shared
            .iter()
            .chain(&self.loader.local)
            .map(|rule| rule.rule.replace.clone())
            .collect();
        let mut overrides = BTreeMap::new();
        for identity in identities {
            if let Some(rule) = self.loader.override_for(&identity, member).cloned() {
                let origin = RequirementOrigin {
                    manifest: rule
                        .directory
                        .join(if rule.document.kind() == ManifestKind::Local {
                            "fai.local.toml"
                        } else {
                            "fai.toml"
                        })
                        .to_string(),
                    field: format!("overrides[{}].with", rule.index),
                };
                overrides.insert(
                    PackageKey::Git(identity),
                    self.dependency(&rule.rule.with, &rule.directory, origin, false)?,
                );
            }
        }
        if let Some(identity) = self.loader.mode.root_alias.clone() {
            overrides.insert(
                PackageKey::Git(identity),
                PackageRequirement {
                    package: PackageKey::Path(self.path_key(&self.root)),
                    selector: PackageSelector::Path(None),
                    origin: RequirementOrigin {
                        manifest: self.root.join("fai.toml").to_string(),
                        field: "package.version".into(),
                    },
                },
            );
        }
        Ok(overrides)
    }

    fn locked_graph(
        &mut self,
        request: &ResolutionRequest,
        lock: &LockFile,
        graph: &LockedGraph,
    ) -> Result<Option<ResolvedGraph>, DriverError> {
        let mut packages = BTreeMap::new();
        let mut ids = BTreeMap::new();
        for id in &graph.packages {
            let record = lock.package(id).ok_or_else(|| {
                error(fai_package::INVALID_LOCK, "missing selected package record")
            })?;
            let source = record.source.source();
            let mut candidate = match &source {
                PackageSource::Path(_) => self.candidate(&source, false)?,
                PackageSource::Git { package, revision } => {
                    let materialized = if let Some((root, index)) = self.vendor()? {
                        index.materialize(&root, record)?
                    } else {
                        self.store.materialize(package, revision, record.checksum.as_ref())?
                    };
                    self.verified.insert(source.clone(), materialized.checksum.clone());
                    let document =
                        self.loader.document(&materialized.directory.join("fai.toml"), false)?;
                    self.metadata.insert(source.clone(), document);
                    self.locations.insert(source.clone(), materialized.directory);
                    self.candidate(&source, record.released)?
                }
            };
            if candidate.manifest_fingerprint != record.manifest_fingerprint.as_str()
                || candidate.version != record.version
            {
                if matches!(source, PackageSource::Path(_)) {
                    return Ok(None);
                }
                return Err(error(
                    fai_package::SOURCE_INTEGRITY,
                    "locked Git metadata does not match its package record",
                ));
            }
            candidate.checksum = record.checksum.as_ref().map(|checksum| checksum.as_str().into());
            ids.insert(id.clone(), record.logical.key());
            packages.insert(record.logical.key(), candidate);
        }
        let root = ids
            .get(&graph.root_package)
            .ok_or_else(|| error(fai_package::INVALID_LOCK, "missing selected root package"))?;
        if root != &request.root
            || packages
                .get(root)
                .is_none_or(|candidate| candidate.source != request.candidate.source)
        {
            return Err(error(
                fai_package::INVALID_LOCK,
                "locked root identity does not match the selected package",
            ));
        }
        let mut branches: BTreeMap<
            PackageKey,
            BTreeMap<fai_package::GitBranch, fai_package::GitRevision>,
        > = BTreeMap::new();
        for binding in &graph.branches {
            branches
                .entry(ids[&binding.logical_package].clone())
                .or_default()
                .insert(binding.branch.clone(), binding.rev.clone());
        }
        let aliases = root_aliases(request);
        if aliases.len() > 1 {
            return Err(error(
                fai_package::RESOLUTION_CONFLICT,
                "multiple logical identities replace the selected root",
            ));
        }
        let normalize = |key: &PackageKey| {
            if aliases.contains(key) { request.root.clone() } else { key.clone() }
        };
        let mut expected = Vec::new();
        for (key, candidate) in &packages {
            for (alias, requirement) in &candidate.dependencies {
                check_requirement(request, &packages, &branches, requirement, &normalize)?;
                expected.push(ResolvedEdge {
                    from: Some(key.clone()),
                    alias: alias.clone(),
                    to: normalize(&requirement.package),
                });
            }
        }
        for (alias, requirement) in &request.candidate.development {
            check_requirement(request, &packages, &branches, requirement, &normalize)?;
            expected.push(ResolvedEdge {
                from: None,
                alias: alias.clone(),
                to: normalize(&requirement.package),
            });
        }
        let edge_key =
            |edge: &ResolvedEdge| (edge.from.clone(), edge.alias.clone(), edge.to.clone());
        let mut recorded: Vec<_> = graph
            .edges
            .iter()
            .map(|edge| ResolvedEdge {
                from: ids.get(&edge.from).cloned(),
                alias: edge.alias.clone(),
                to: ids[&edge.to].clone(),
            })
            .collect();
        expected.sort_by_key(&edge_key);
        recorded.sort_by_key(&edge_key);
        if expected != recorded {
            return Err(error(
                fai_package::INVALID_LOCK,
                "locked edges disagree with selected package manifests",
            ));
        }
        for candidate in packages.values() {
            if !candidate.compiler.matches(&request.compiler) {
                return Err(error(
                    fai_package::COMPILER_MISMATCH,
                    format!(
                        "valid locked package {:?} requires Fai {}; selected compiler is {}",
                        candidate.source, candidate.compiler, request.compiler
                    ),
                ));
            }
        }
        Ok(Some(ResolvedGraph { root: root.clone(), packages, edges: expected, branches }))
    }

    fn materialize(&mut self, graph: &mut ResolvedGraph) -> Result<(), DriverError> {
        for (key, candidate) in &mut graph.packages {
            if let PackageSource::Git { package, revision } = &candidate.source {
                if let Some(verified) = self.verified.get(&candidate.source) {
                    if candidate
                        .checksum
                        .as_ref()
                        .is_some_and(|expected| expected != verified.as_str())
                    {
                        return Err(error(
                            fai_package::SOURCE_INTEGRITY,
                            "inconsistent selected source checksums",
                        ));
                    }
                    candidate.checksum = Some(verified.as_str().into());
                    continue;
                }
                let expected = candidate.checksum.as_deref().map(Digest::parse).transpose()?;
                let materialized = if let Some((root, index)) = self.vendor()? {
                    let matching = index
                        .packages
                        .iter()
                        .find(|entry| {
                            entry.logical.key() == *key
                                && entry.source.source() == candidate.source
                                && entry.version == candidate.version
                        })
                        .ok_or_else(|| {
                            error(
                                fai_package::VENDOR_UNAVAILABLE,
                                format!("no vendored source for {key:?}"),
                            )
                        })?;
                    candidate.checksum = Some(matching.checksum.as_str().into());
                    let record = fai_package::LockedPackage::from_candidate(key, candidate)?;
                    index.materialize(&root, &record)?
                } else {
                    self.store.materialize(package, revision, expected.as_ref())?
                };
                candidate.checksum = Some(materialized.checksum.as_str().into());
                self.verified.insert(candidate.source.clone(), materialized.checksum.clone());
                self.locations.insert(candidate.source.clone(), materialized.directory);
            }
        }
        Ok(())
    }

    fn install(&mut self, graph: &ResolvedGraph) -> Result<String, DriverError> {
        let mut keys: BTreeMap<PackageKey, String> = BTreeMap::new();
        let mut remaining: BTreeSet<_> = graph.packages.keys().cloned().collect();
        while !remaining.is_empty() {
            let ready: Vec<_> = remaining
                .iter()
                .filter(|key| {
                    graph
                        .edges
                        .iter()
                        .filter(|edge| edge.from.as_ref() == Some(*key))
                        .all(|edge| keys.contains_key(&edge.to))
                })
                .cloned()
                .collect();
            if ready.is_empty() {
                return Err(error(
                    fai_package::PACKAGE_CYCLE,
                    "normal package cycle while preparing compiler inputs",
                ));
            }
            for key in ready {
                let candidate = &graph.packages[&key];
                let dependencies: BTreeMap<_, _> = graph
                    .edges
                    .iter()
                    .filter(|edge| edge.from.as_ref() == Some(&key))
                    .map(|edge| (edge.alias.to_string(), keys[&edge.to].clone()))
                    .collect();
                let fingerprint = Digest::of(
                    "fai.package-instance.v1",
                    &serde_json::to_vec(&(
                        &key,
                        &candidate.source,
                        &candidate.version,
                        &self.loader.target,
                        &dependencies,
                    ))
                    .map_err(|failure| project_message(failure.to_string()))?,
                );
                let instance = format!("package:{}", fingerprint.as_str());
                let directory =
                    self.locations.get(&candidate.source).cloned().ok_or_else(|| {
                        error(
                            fai_package::SOURCE_UNAVAILABLE,
                            "selected package was not materialized",
                        )
                    })?;
                let package = self.loader.load_package(&directory, false)?;
                let native = native_requirements(
                    &package.directory,
                    &package.active.native,
                    &self.loader.target,
                )
                .map_err(project_message)?;
                self.loader.project.packages.entry(instance.clone()).or_insert(PreparedPackage {
                    dependencies,
                    development: BTreeMap::new(),
                    native,
                });
                for path in &package.plan.library {
                    self.loader.project.sources.push(PreparedSource {
                        path: directory.join(path.as_str()),
                        package: instance.clone(),
                        role: SourceRole::Library,
                        selected: false,
                        read_only: matches!(candidate.source, PackageSource::Git { .. }),
                    });
                }
                keys.insert(key.clone(), instance);
                remaining.remove(&key);
            }
        }
        let root = keys[&graph.root].clone();
        let development = graph
            .edges
            .iter()
            .filter(|edge| edge.from.is_none())
            .map(|edge| (edge.alias.to_string(), keys[&edge.to].clone()))
            .collect();
        if let Some(package) = self.loader.project.packages.get_mut(&root) {
            package.development = development;
        }
        Ok(root)
    }

    fn vendor(
        &mut self,
    ) -> Result<Option<(Utf8PathBuf, Arc<fai_package_store::VendorIndex>)>, DriverError> {
        let Some(root) = self.vendor_root.clone() else {
            return Ok(None);
        };
        if self.vendor_index.is_none() {
            self.vendor_index = Some(Arc::new(fai_package_store::VendorIndex::read(&root)?));
        }
        Ok(self.vendor_index.as_ref().map(|index| (root, index.clone())))
    }

    fn resolution_error(&mut self, failure: ResolutionError) -> DriverError {
        if let Some(error) = self.last_error.take() {
            return error;
        }
        match failure {
            ResolutionError::Provider(failure) => failure.into(),
            ResolutionError::Conflict(conflict) => {
                let code = if conflict.message.contains("cycle") {
                    fai_package::PACKAGE_CYCLE
                } else {
                    fai_package::RESOLUTION_CONFLICT
                };
                let mut diagnostics = Vec::new();
                for requirement in &conflict.requirements {
                    if let Some(document) =
                        self.loader.documents.get(Utf8Path::new(&requirement.origin.manifest))
                    {
                        let span = document
                            .location(&requirement.origin.field)
                            .map_or_else(crate::tooling_span, |location| location.value);
                        diagnostics.push(Diagnostic::error(
                            code,
                            format!("{}; required {:?}", conflict.message, requirement.selector),
                            span,
                        ));
                    }
                }
                if diagnostics.is_empty() {
                    error(code, conflict.message)
                } else {
                    self.loader.failure(diagnostics)
                }
            }
        }
    }
}

impl ResolverProvider for Provider<'_, '_> {
    fn candidates(
        &mut self,
        key: &PackageKey,
        selectors: &[PackageSelector],
        pin: Option<&PackagePin>,
        refresh: bool,
    ) -> Result<CandidateSet, ProviderFailure> {
        match self.candidate_set(key, selectors, pin, refresh) {
            Ok(set) => Ok(set),
            Err(failure) => {
                let message = failure.to_string();
                self.last_error = Some(failure);
                Err(ProviderFailure { code: fai_package::SOURCE_UNAVAILABLE, message })
            }
        }
    }
}

impl Provider<'_, '_> {
    fn candidate_set(
        &mut self,
        key: &PackageKey,
        selectors: &[PackageSelector],
        pin: Option<&PackagePin>,
        refresh: bool,
    ) -> Result<CandidateSet, DriverError> {
        let PackageKey::Git(package) = key else {
            let PackageKey::Path(path) = key else { unreachable!() };
            return Ok(CandidateSet {
                candidates: vec![self.candidate(&PackageSource::Path(path.clone()), false)?],
                branches: BTreeMap::new(),
            });
        };
        let needs_inventory = selectors.iter().any(|selector| match selector {
            PackageSelector::Version(_) => true,
            PackageSelector::Branch(branch) => {
                refresh || pin.is_none_or(|pin| !pin.branches.contains_key(branch))
            }
            _ => false,
        });
        let inventory = if needs_inventory {
            self.store.inventory(package)?
        } else {
            fai_package_store::GitInventory::default()
        };
        if let Some(pin) = pin
            && pin.released
            && let PackageSource::Git { package: old_package, revision } = &pin.source
            && old_package == package
            && inventory.releases.get(&pin.version).is_some_and(|current| current != revision)
        {
            return Err(error(
                fai_package::RELEASE_MOVED,
                format!(
                    "accepted release {} in {} now identifies another commit",
                    pin.version, package.repository
                ),
            ));
        }
        let mut revisions: BTreeMap<_, bool> =
            inventory.releases.values().map(|revision| (revision.clone(), false)).collect();
        let mut branches = BTreeMap::new();
        for selector in selectors {
            match selector {
                PackageSelector::Revision(revision) => {
                    revisions.insert(revision.clone(), true);
                }
                PackageSelector::Branch(branch) => {
                    let revision = (!refresh)
                        .then(|| pin.and_then(|pin| pin.branches.get(branch)))
                        .flatten()
                        .or_else(|| inventory.branches.get(branch))
                        .ok_or_else(|| {
                            error(
                                fai_package::SOURCE_UNAVAILABLE,
                                format!("branch {branch} is unavailable in {}", package.repository),
                            )
                        })?;
                    branches.insert(branch.clone(), revision.clone());
                    revisions.insert(revision.clone(), true);
                }
                _ => {}
            }
        }
        let mut candidates = Vec::new();
        for (revision, required) in revisions {
            let source =
                PackageSource::Git { package: package.clone(), revision: revision.clone() };
            let mut candidate = match self.candidate(&source, false) {
                Ok(candidate) => candidate,
                Err(DriverError::Project(failure))
                    if !required
                        && failure.diagnostics.iter().all(|diagnostic| {
                            matches!(
                                diagnostic.code.as_str(),
                                "FAI0800" | "FAI0801" | "FAI0802" | "FAI0803" | "FAI0810"
                            ) || (self.store.is_offline()
                                && matches!(diagnostic.code.as_str(), "FAI0811" | "FAI0813"))
                        }) =>
                {
                    self.loader.project.notices.push(format!(
                        "excluded unavailable or invalid candidate {source:?}: {}",
                        failure.human.trim()
                    ));
                    continue;
                }
                Err(failure) => return Err(failure),
            };
            candidate.released = inventory.releases.get(&candidate.version) == Some(&revision);
            if !required && !candidate.released {
                self.loader
                    .project
                    .notices
                    .push(format!("excluded version-mismatched release at {revision}"));
                continue;
            }
            if self.store.is_offline() && self.vendor_root.is_none() {
                match self.store.materialize(package, &revision, None) {
                    Ok(materialized) => {
                        candidate.checksum = Some(materialized.checksum.as_str().into());
                        self.verified.insert(source.clone(), materialized.checksum);
                        self.locations.insert(source.clone(), materialized.directory);
                    }
                    Err(failure)
                        if !required && matches!(failure.code.as_str(), "FAI0811" | "FAI0813") =>
                    {
                        self.loader
                            .project
                            .notices
                            .push(format!("excluded uncached candidate {source:?}"));
                        continue;
                    }
                    Err(failure) => return Err(failure.into()),
                }
            }
            candidates.push(candidate);
        }
        Ok(CandidateSet { candidates, branches })
    }
}

fn pins(lock: &LockFile, graph: Option<&LockedGraph>) -> BTreeMap<PackageKey, PackagePin> {
    let Some(graph) = graph else {
        return BTreeMap::new();
    };
    graph
        .packages
        .iter()
        .filter_map(|id| {
            let record = lock.package(id)?;
            let branches = graph
                .branches
                .iter()
                .filter(|binding| &binding.logical_package == id)
                .map(|binding| (binding.branch.clone(), binding.rev.clone()))
                .collect();
            Some((
                record.logical.key(),
                PackagePin {
                    source: record.source.source(),
                    version: record.version.clone(),
                    released: record.released,
                    branches,
                },
            ))
        })
        .collect()
}

fn reconcile(
    pins: &mut BTreeMap<PackageKey, PackagePin>,
    previous: Option<&LockedGraph>,
    shared: Option<&LockFile>,
    baseline: Option<&LockedGraph>,
) {
    let (Some(shared), Some(baseline)) = (shared, baseline) else {
        return;
    };
    let old: BTreeMap<_, _> = previous
        .into_iter()
        .flat_map(|graph| &graph.base_packages)
        .map(|package| (package.logical.key(), (&package.version, package.source.source())))
        .collect();
    for (key, pin) in self::pins(shared, Some(baseline)) {
        if old
            .get(&key)
            .is_none_or(|(version, source)| *version != &pin.version || source != &pin.source)
        {
            pins.insert(key, pin);
        }
    }
}

fn inputs(request: &ResolutionRequest) -> Result<Digest, DriverError> {
    let dependencies: Vec<_> = request
        .candidate
        .development
        .iter()
        .map(|(alias, requirement)| (alias, &requirement.package, &requirement.selector))
        .collect();
    let overrides: Vec<_> = request
        .overrides
        .iter()
        .map(|(key, requirement)| (key, &requirement.package, &requirement.selector))
        .collect();
    let bytes = serde_json::to_vec(&(
        &request.root,
        &request.target,
        &request.candidate.manifest_fingerprint,
        dependencies,
        overrides,
    ))
    .map_err(|failure| project_message(failure.to_string()))?;
    Ok(Digest::of("fai.graph-inputs.v1", &bytes))
}

fn root_aliases(request: &ResolutionRequest) -> BTreeSet<PackageKey> {
    request
        .overrides
        .iter()
        .filter(|(_, replacement)| replacement.package == request.root)
        .map(|(key, _)| key.clone())
        .collect()
}

fn check_requirement(
    request: &ResolutionRequest,
    packages: &BTreeMap<PackageKey, PackageCandidate>,
    branches: &BTreeMap<PackageKey, BTreeMap<fai_package::GitBranch, fai_package::GitRevision>>,
    requirement: &PackageRequirement,
    normalize: &impl Fn(&PackageKey) -> PackageKey,
) -> Result<(), DriverError> {
    let key = normalize(&requirement.package);
    let candidate = packages.get(&key).ok_or_else(|| {
        error(fai_package::INVALID_LOCK, "required logical package is absent from the lock")
    })?;
    let empty = BTreeMap::new();
    let bindings = branches.get(&key).unwrap_or(&empty);
    let actual = match &candidate.source {
        PackageSource::Path(path) => PackageKey::Path(path.clone()),
        PackageSource::Git { package, .. } => PackageKey::Git(package.clone()),
    };
    let valid = if let Some(replacement) = request.overrides.get(&requirement.package) {
        let version = match &requirement.selector {
            PackageSelector::Version(version) | PackageSelector::Path(Some(version)) => {
                version.matches(&candidate.version)
            }
            _ => true,
        };
        actual == replacement.package
            && version
            && fai_package::candidate_satisfies(candidate, &replacement.selector, bindings)
    } else {
        actual == requirement.package
            && fai_package::candidate_satisfies(candidate, &requirement.selector, bindings)
    };
    if !valid {
        return Err(error(
            fai_package::INVALID_LOCK,
            format!(
                "locked selection violates {:?} for {:?}",
                requirement.selector, requirement.package
            ),
        ));
    }
    Ok(())
}

fn read_optional(path: &Utf8Path) -> Result<Option<String>, DriverError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DriverError::Io { path: path.to_owned(), source }),
    }
}

fn lock_name(context: LockContext) -> &'static str {
    if context == LockContext::Shared { "fai.lock" } else { "fai.local.lock" }
}

fn update_policy(
    selectors: &[String],
    request: &ResolutionRequest,
    lock: &LockFile,
    previous: Option<&LockedGraph>,
) -> Result<ResolutionPolicy, DriverError> {
    if selectors.is_empty() {
        return Ok(ResolutionPolicy::UpdateAll);
    }
    let mut keys = BTreeSet::new();
    for selector in selectors {
        let mut matches = BTreeSet::new();
        for (alias, requirement) in
            request.candidate.dependencies.iter().chain(&request.candidate.development)
        {
            if alias.as_str() == selector {
                matches.insert(requirement.package.clone());
            }
        }
        if let Some(previous) = previous {
            for edge in &previous.edges {
                if edge.alias.as_str() == selector
                    && let Some(package) = lock.package(&edge.to)
                {
                    matches.insert(package.logical.key());
                }
            }
            for id in &previous.packages {
                if let Some(package) = lock.package(id) {
                    let key = package.logical.key();
                    let name = match &key {
                        PackageKey::Path(path) => path.clone(),
                        PackageKey::Git(package) => {
                            format!("{}#{}", package.repository, package.subdir)
                        }
                    };
                    if &name == selector {
                        matches.insert(key);
                    }
                }
            }
        }
        if matches.len() != 1 {
            return Err(error(
                fai_package::RESOLUTION_CONFLICT,
                format!("update selector `{selector}` must identify one dependency in this graph"),
            ));
        }
        keys.extend(matches);
    }
    Ok(ResolutionPolicy::Update(keys))
}

fn error(code: fai_diagnostics::DiagnosticCode, message: impl Into<String>) -> DriverError {
    ProviderFailure { code, message: message.into() }.into()
}
