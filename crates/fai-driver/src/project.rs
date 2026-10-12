//! Filesystem preparation of local package graphs before publishing compiler inputs.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use camino::{Utf8Path, Utf8PathBuf};
use fai_db::SourceRole;
use fai_diagnostics::{Diagnostic, DiagnosticCode};
use fai_package::{
    ActiveConfiguration, Dependency, GitPackage, GitSelector, ManifestDocument, ManifestKind,
    Override, PackagePath, PackageVersion, SourcePlan, TargetPlatform,
};
use fai_span::SourceMap;

use crate::{DriverError, NativeDeps};

/// Located project failures, renderable even before a compiler session exists.
#[derive(Debug, Clone)]
pub struct ProjectError {
    /// Human-readable diagnostics with source excerpts.
    pub human: String,
    /// Stable wire diagnostics with resolved file locations.
    pub diagnostics: Vec<fai_diagnostics::wire::DiagnosticWire>,
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.human.trim_end())
    }
}
impl std::error::Error for ProjectError {}

#[derive(Clone)]
pub(crate) struct PreparedSource {
    pub path: Utf8PathBuf,
    pub package: String,
    pub role: SourceRole,
    pub selected: bool,
}

#[derive(Clone)]
pub(crate) struct PreparedPackage {
    pub dependencies: BTreeMap<String, String>,
    pub development: BTreeMap<String, String>,
    pub native: NativeDeps,
}

#[derive(Clone)]
pub(crate) struct PreparedEntry {
    pub name: String,
    pub path: Utf8PathBuf,
    pub package: String,
}

#[derive(Clone)]
pub(crate) struct PreparedProject {
    pub base: Utf8PathBuf,
    pub packages: BTreeMap<String, PreparedPackage>,
    pub sources: Vec<PreparedSource>,
    pub entries: Vec<PreparedEntry>,
    pub default_entry: Option<(String, String)>,
    pub local: bool,
    pub roots: Vec<(String, Utf8PathBuf)>,
}

struct LoadedPackage {
    directory: Utf8PathBuf,
    document: Arc<ManifestDocument>,
    active: ActiveConfiguration,
    plan: SourcePlan,
}

#[derive(Clone)]
struct RootOverride {
    rule: Override,
    directory: Utf8PathBuf,
    document: Arc<ManifestDocument>,
    index: usize,
}

struct Loader<'a> {
    base: Utf8PathBuf,
    configuration_root: Utf8PathBuf,
    target: TargetPlatform,
    compiler: PackageVersion,
    overlays: &'a [Utf8PathBuf],
    sources: SourceMap,
    documents: BTreeMap<Utf8PathBuf, Arc<ManifestDocument>>,
    loaded: BTreeMap<(Utf8PathBuf, bool), Arc<LoadedPackage>>,
    shared: Vec<RootOverride>,
    local: Vec<RootOverride>,
    project: PreparedProject,
}

/// Returns `None` for a directory/legacy-native project.
pub(crate) fn prepare(
    root: &Utf8Path,
    overlays: &[Utf8PathBuf],
    no_local: bool,
) -> Result<Option<PreparedProject>, DriverError> {
    prepare_selected(root, overlays, no_local, None)
}

fn prepare_selected(
    root: &Utf8Path,
    overlays: &[Utf8PathBuf],
    no_local: bool,
    selector: Option<&str>,
) -> Result<Option<PreparedProject>, DriverError> {
    if !root.join(fai_package::MANIFEST_NAME).is_file() {
        return Ok(None);
    }
    let base = canonical(root)?;
    let mut loader = Loader {
        base: base.clone(),
        configuration_root: base.clone(),
        target: TargetPlatform::parse(&target_lexicon::HOST.to_string())
            .map_err(|error| project_message(error.to_string()))?,
        compiler: PackageVersion::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|error| project_message(error.to_string()))?,
        overlays,
        sources: SourceMap::new(),
        documents: BTreeMap::new(),
        loaded: BTreeMap::new(),
        shared: Vec::new(),
        local: Vec::new(),
        project: PreparedProject {
            base: base.clone(),
            packages: BTreeMap::new(),
            sources: Vec::new(),
            entries: Vec::new(),
            default_entry: None,
            local: false,
            roots: Vec::new(),
        },
    };
    let document = loader.document(&base.join(fai_package::MANIFEST_NAME), false)?;
    if document.kind() == ManifestKind::LegacyNative {
        return Ok(None);
    }
    let mut roots = if document.manifest().workspace.is_some() {
        loader.members(&base, &document)?
    } else {
        vec![base.clone()]
    };
    if document.manifest().workspace.is_some()
        && document.manifest().package.is_some()
        && !roots.contains(&base)
    {
        roots.push(base.clone());
    }
    let mut configuration = document.clone();
    if document.manifest().workspace.is_none() {
        for ancestor in base.ancestors().skip(1) {
            let path = ancestor.join(fai_package::MANIFEST_NAME);
            if !path.is_file() {
                continue;
            }
            let parent = loader.document(&path, false)?;
            if parent.manifest().workspace.is_some() {
                if loader.members(ancestor, &parent)?.contains(&base) {
                    loader.configuration_root = ancestor.to_owned();
                    configuration = parent;
                }
                break;
            }
        }
    }
    if let Some(pin) = &configuration.manifest().toolchain
        && pin != &loader.compiler
    {
        return Err(loader.error(
            &configuration,
            "toolchain.version",
            fai_package::COMPILER_MISMATCH,
            format!("project requires Fai {pin}; the selected compiler is {}", loader.compiler),
        ));
    }
    loader.shared = configuration
        .manifest()
        .overrides
        .iter()
        .enumerate()
        .map(|(index, rule)| RootOverride {
            rule: rule.clone(),
            directory: loader.configuration_root.clone(),
            document: configuration.clone(),
            index,
        })
        .collect();
    let local_path = loader.configuration_root.join("fai.local.toml");
    if !no_local && local_path.is_file() {
        let local = loader.document(&local_path, true)?;
        loader.local = local
            .manifest()
            .overrides
            .iter()
            .enumerate()
            .map(|(index, rule)| RootOverride {
                rule: rule.clone(),
                directory: loader.configuration_root.clone(),
                document: local.clone(),
                index,
            })
            .collect();
        loader.project.local = true;
    }
    roots.sort();
    roots.dedup();
    if let Some(selector) = selector {
        let requested = base.join(selector);
        let by_path = std::fs::canonicalize(&requested)
            .ok()
            .and_then(|path| Utf8PathBuf::from_path_buf(path).ok());
        let mut selected = Vec::new();
        for root in roots {
            if let Some(path) = &by_path {
                if path == &root {
                    selected.push(root);
                }
                continue;
            }
            let metadata = loader.document(&root.join(fai_package::MANIFEST_NAME), false)?;
            if metadata
                .manifest()
                .package
                .as_ref()
                .is_some_and(|package| package.name.as_str() == selector)
            {
                selected.push(root);
            }
        }
        if selected.len() != 1 {
            return Err(project_message(format!(
                "package selector `{selector}` must identify exactly one workspace member"
            )));
        }
        roots = selected;
    }
    let one_root = roots.len() == 1;
    for root in roots {
        let member = root.strip_prefix(&loader.configuration_root).unwrap_or(&root);
        let member = PackagePath::parse(&portable_path(member))
            .map_err(|error| project_message(error.to_string()))?;
        let mut graph =
            Graph { root: root.clone(), member, selected: BTreeMap::new(), visiting: Vec::new() };
        let logical = loader.local_key(&root);
        let key = loader.visit(&mut graph, logical, root.clone())?;
        let package = loader.load_package(&root, true)?;
        loader.project.roots.push((
            package
                .document
                .manifest()
                .package
                .as_ref()
                .expect("validated package")
                .name
                .to_string(),
            root.clone(),
        ));
        let development = loader.dependencies(
            &mut graph,
            &package,
            &package.active.dev_dependencies,
            "dev-dependencies",
        )?;
        if let Some(prepared) = loader.project.packages.get_mut(&key) {
            prepared.development = development;
        }
        for path in &package.plan.library {
            loader.project.sources.push(PreparedSource {
                path: root.join(path.as_str()),
                package: key.clone(),
                role: SourceRole::Library,
                selected: true,
            });
        }
        for path in &package.plan.tests {
            loader.project.sources.push(PreparedSource {
                path: root.join(path.as_str()),
                package: key.clone(),
                role: SourceRole::Test,
                selected: true,
            });
        }
        for executable in &package.plan.executables {
            let path = root.join(executable.entry.as_str());
            let name = executable.name.to_string();
            loader.project.sources.push(PreparedSource {
                path: path.clone(),
                package: key.clone(),
                role: SourceRole::Executable(name.clone()),
                selected: true,
            });
            loader.project.entries.push(PreparedEntry { name, path, package: key.clone() });
        }
        if one_root {
            let metadata = package.document.manifest().package.as_ref().expect("validated package");
            if let Some(default) = &metadata.default_run {
                loader.project.default_entry = Some((key.clone(), default.to_string()));
            } else if package.plan.executables.len() == 1 {
                loader.project.default_entry =
                    Some((key.clone(), package.plan.executables[0].name.to_string()));
            }
        }
    }
    Ok(Some(loader.project))
}

/// Resolves one workspace member selector before opening its compiler session.
pub fn project_package_root(
    root: &Utf8Path,
    selector: &str,
    no_local: bool,
) -> Result<Utf8PathBuf, DriverError> {
    let project = prepare_selected(root, &[], no_local, Some(selector))?.ok_or_else(|| {
        project_message("--package requires a versioned package or workspace manifest".into())
    })?;
    project
        .roots
        .first()
        .map(|(_, path)| path.clone())
        .ok_or_else(|| project_message("no selected package".into()))
}

/// Resolves a named or default executable to its physical entry file.
pub fn project_entry_path(
    root: &Utf8Path,
    binary: Option<&str>,
    no_local: bool,
) -> Result<Utf8PathBuf, DriverError> {
    let project = prepare(root, &[], no_local)?.ok_or_else(|| {
        project_message("executable selection requires a versioned package manifest".into())
    })?;
    let entries: Vec<_> = project
        .entries
        .iter()
        .filter(|entry| match binary {
            Some(name) => entry.name == name,
            None => project
                .default_entry
                .as_ref()
                .is_some_and(|(package, name)| package == &entry.package && name == &entry.name),
        })
        .collect();
    if entries.len() != 1 {
        return Err(project_message(
            "select exactly one executable with --package/--bin or configure default-run".into(),
        ));
    }
    Ok(entries[0].path.clone())
}

struct Graph {
    root: Utf8PathBuf,
    member: PackagePath,
    selected: BTreeMap<String, (Utf8PathBuf, String)>,
    visiting: Vec<String>,
}

impl Loader<'_> {
    fn document(
        &mut self,
        path: &Utf8Path,
        local: bool,
    ) -> Result<Arc<ManifestDocument>, DriverError> {
        if let Some(document) = self.documents.get(path) {
            return Ok(document.clone());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|source| DriverError::Io { path: path.to_owned(), source })?;
        let label = path.strip_prefix(&self.base).unwrap_or(path).to_owned();
        let id = self.sources.add(label, text.clone());
        let parsed = if local {
            ManifestDocument::parse_local(id, &text)
        } else {
            ManifestDocument::parse(id, &text)
        };
        let document = Arc::new(parsed.map_err(|diagnostics| self.failure(diagnostics))?);
        self.documents.insert(path.to_owned(), document.clone());
        Ok(document)
    }

    fn failure(&self, diagnostics: Vec<Diagnostic>) -> DriverError {
        DriverError::Project(Box::new(ProjectError {
            human: fai_diagnostics::render_human(&diagnostics, &self.sources, false),
            diagnostics: fai_diagnostics::wire::to_wire(&diagnostics, &self.sources),
        }))
    }

    fn error(
        &self,
        document: &ManifestDocument,
        field: &str,
        code: DiagnosticCode,
        message: impl Into<String>,
    ) -> DriverError {
        let span = document
            .location(field)
            .or_else(|| document.location("package"))
            .map_or_else(crate::tooling_span, |location| location.value);
        self.failure(vec![Diagnostic::error(code, message, span)])
    }

    fn members(
        &self,
        root: &Utf8Path,
        document: &ManifestDocument,
    ) -> Result<Vec<Utf8PathBuf>, DriverError> {
        let Some(workspace) = &document.manifest().workspace else {
            return Ok(vec![root.to_owned()]);
        };
        let mut members = BTreeSet::new();
        for (index, pattern) in workspace.members.iter().enumerate() {
            let pattern =
                format!("{}/{}", glob::Pattern::escape(&root.as_str().replace('\\', "/")), pattern);
            let matches = glob::glob(&pattern).map_err(|error| {
                self.error(
                    document,
                    &format!("workspace.members[{index}]"),
                    fai_package::INVALID_SOURCE,
                    error.to_string(),
                )
            })?;
            let mut found = false;
            for entry in matches {
                let path = entry.map_err(|error| project_message(error.to_string()))?;
                let path = Utf8PathBuf::from_path_buf(path).map_err(|path| {
                    DriverError::NonUtf8Path(path.to_string_lossy().into_owned())
                })?;
                let path = canonical(&path)?;
                if !path.starts_with(root) || !path.join(fai_package::MANIFEST_NAME).is_file() {
                    return Err(self.error(
                        document,
                        &format!("workspace.members[{index}]"),
                        fai_package::INVALID_SOURCE,
                        "workspace members must be package directories inside the workspace",
                    ));
                }
                members.insert(path);
                found = true;
            }
            if !found {
                return Err(self.error(
                    document,
                    &format!("workspace.members[{index}]"),
                    fai_package::INVALID_SOURCE,
                    "workspace member pattern matches no package directories",
                ));
            }
        }
        Ok(members.into_iter().collect())
    }

    fn load_package(
        &mut self,
        directory: &Utf8Path,
        targets: bool,
    ) -> Result<Arc<LoadedPackage>, DriverError> {
        if let Some(package) = self.loaded.get(&(directory.to_owned(), targets)) {
            return Ok(package.clone());
        }
        let document = self.document(&directory.join(fai_package::MANIFEST_NAME), false)?;
        let Some(metadata) = &document.manifest().package else {
            return Err(self.error(
                &document,
                "package",
                fai_package::INVALID_MANIFEST,
                "a dependency must declare a versioned [package]",
            ));
        };
        if !metadata.fai_version.matches(&self.compiler) {
            return Err(self.error(
                &document,
                "package.fai-version",
                fai_package::COMPILER_MISMATCH,
                format!(
                    "package {} requires Fai {}; selected compiler is {}",
                    metadata.name, metadata.fai_version, self.compiler
                ),
            ));
        }
        let active = document.manifest().configuration(&self.target).map_err(|error| {
            self.error(
                &document,
                "package.supported-platforms",
                fai_package::INVALID_PLATFORM,
                error.to_string(),
            )
        })?;
        let mut files = Vec::new();
        collect_inventory(
            directory,
            directory,
            &document,
            &self.target,
            targets,
            &mut BTreeSet::new(),
            &mut files,
        )?;
        for overlay in self.overlays {
            if let Ok(relative) = overlay.strip_prefix(directory)
                && let Ok(path) = PackagePath::parse(&portable_path(relative))
            {
                files.push(path);
            }
        }
        let plan = if targets {
            SourcePlan::build(&document, &self.target, &files)
        } else {
            SourcePlan::build_library(&document, &self.target, &files)
        }
        .map_err(|diagnostics| self.failure(diagnostics))?;
        let package =
            Arc::new(LoadedPackage { directory: directory.to_owned(), document, active, plan });
        self.loaded.insert((directory.to_owned(), targets), package.clone());
        Ok(package)
    }

    fn local_key(&self, path: &Utf8Path) -> String {
        format!(
            "path:{}",
            portable_path(path.strip_prefix(&self.configuration_root).unwrap_or(path))
        )
    }

    fn visit(
        &mut self,
        graph: &mut Graph,
        logical: String,
        directory: Utf8PathBuf,
    ) -> Result<String, DriverError> {
        let package = self.load_package(&directory, false)?;
        if graph.visiting.contains(&logical) || graph.visiting.len() >= 128 {
            return Err(self.error(
                &package.document,
                "dependencies",
                fai_package::PACKAGE_CYCLE,
                format!(
                    "normal package dependency cycle/depth limit: {} -> {logical}",
                    graph.visiting.join(" -> ")
                ),
            ));
        }
        if let Some((previous, key)) = graph.selected.get(&logical) {
            if previous != &directory {
                return Err(self.error(
                    &package.document,
                    "dependencies",
                    fai_package::RESOLUTION_CONFLICT,
                    format!("logical package {logical} has multiple local sources"),
                ));
            }
            return Ok(key.clone());
        }
        graph.visiting.push(logical.clone());
        let dependencies =
            self.dependencies(graph, &package, &package.active.dependencies, "dependencies")?;
        graph.visiting.pop();
        let version =
            &package.document.manifest().package.as_ref().expect("validated package").version;
        let description = serde_json::to_vec(&(
            logical.as_str(),
            self.local_key(&directory),
            version,
            self.target.to_string(),
            &dependencies,
        ))
        .map_err(|error| project_message(error.to_string()))?;
        let key = format!("local:{}", blake3::hash(&description).to_hex());
        if !self.project.packages.contains_key(&key) {
            let native = native_requirements(&directory, &package.active.native, &self.target)
                .map_err(|message| {
                    self.error(&package.document, "native", fai_package::INVALID_TARGET, message)
                })?;
            self.project.packages.insert(
                key.clone(),
                PreparedPackage { dependencies, development: BTreeMap::new(), native },
            );
            for path in &package.plan.library {
                self.project.sources.push(PreparedSource {
                    path: directory.join(path.as_str()),
                    package: key.clone(),
                    role: SourceRole::Library,
                    selected: false,
                });
            }
        }
        graph.selected.insert(logical, (directory, key.clone()));
        Ok(key)
    }

    fn dependencies(
        &mut self,
        graph: &mut Graph,
        package: &LoadedPackage,
        dependencies: &BTreeMap<fai_package::DependencyAlias, Dependency>,
        kind: &str,
    ) -> Result<BTreeMap<String, String>, DriverError> {
        let mut bindings = BTreeMap::new();
        for (alias, dependency) in dependencies {
            let field = format!("{kind}.{alias}");
            let (logical, directory, extra) =
                self.dependency_location(graph, package, dependency, &field)?;
            let selected = self.load_package(&directory, false)?;
            let metadata =
                selected.document.manifest().package.as_ref().expect("validated package");
            let requirement = match dependency {
                Dependency::Path { version, .. } => version.as_ref(),
                Dependency::Git { selector: GitSelector::Version(version), .. } => Some(version),
                _ => None,
            };
            if requirement.is_some_and(|requirement| !requirement.matches(&metadata.version))
                || extra.as_ref().is_some_and(|requirement| !requirement.matches(&metadata.version))
            {
                return Err(self.error(&package.document, &field, fai_package::RESOLUTION_CONFLICT,
                    format!("dependency `{alias}` selects version {}, which does not satisfy its declared requirements", metadata.version)));
            }
            let key = self.visit(graph, logical, directory)?;
            bindings.insert(alias.to_string(), key);
        }
        Ok(bindings)
    }

    fn dependency_location(
        &self,
        graph: &Graph,
        package: &LoadedPackage,
        dependency: &Dependency,
        field: &str,
    ) -> Result<(String, Utf8PathBuf, Option<fai_package::VersionRequirement>), DriverError> {
        match dependency {
            Dependency::Path { path, .. } => {
                let directory = canonical(&package.directory.join(path))?;
                Ok((self.local_key(&directory), directory, None))
            }
            Dependency::Git { package: identity, .. } => {
                let replacement = self.override_for(identity, &graph.member).ok_or_else(|| self.error(&package.document, field, fai_package::SOURCE_UNAVAILABLE,
                    format!("Git dependency `{field}` has no local source; configure an explicit path override until Git restoration is available")))?;
                let Dependency::Path { path, version } = &replacement.rule.with else {
                    return Err(self.error(
                        &replacement.document,
                        &format!("overrides[{}].with", replacement.index),
                        fai_package::SOURCE_UNAVAILABLE,
                        "this project requires a Git source that has not been restored",
                    ));
                };
                let directory = canonical(&replacement.directory.join(path))?;
                let logical = if directory == graph.root {
                    self.local_key(&directory)
                } else {
                    format!("git:{}#{}", identity.repository, identity.subdir)
                };
                Ok((logical, directory, version.clone()))
            }
        }
    }

    fn override_for(&self, identity: &GitPackage, member: &PackagePath) -> Option<&RootOverride> {
        let matches = |entry: &&RootOverride| {
            &entry.rule.replace == identity
                && entry.rule.members.as_ref().is_none_or(|members| members.contains(member))
        };
        self.local.iter().find(matches).or_else(|| self.shared.iter().find(matches))
    }
}

fn canonical(path: &Utf8Path) -> Result<Utf8PathBuf, DriverError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|source| DriverError::Io { path: path.to_owned(), source })?;
    Utf8PathBuf::from_path_buf(canonical)
        .map_err(|path| DriverError::NonUtf8Path(path.to_string_lossy().into_owned()))
}

fn collect_inventory(
    root: &Utf8Path,
    directory: &Utf8Path,
    document: &ManifestDocument,
    target: &TargetPlatform,
    targets: bool,
    ancestors: &mut BTreeSet<Utf8PathBuf>,
    out: &mut Vec<PackagePath>,
) -> Result<(), DriverError> {
    let real = canonical(directory)?;
    if !real.starts_with(root) || !ancestors.insert(real.clone()) {
        return Err(project_message(format!(
            "source directory {directory} escapes the package or forms a symlink cycle"
        )));
    }
    let entries = std::fs::read_dir(directory)
        .map_err(|source| DriverError::Io { path: directory.to_owned(), source })?;
    for entry in entries {
        let entry =
            entry.map_err(|source| DriverError::Io { path: directory.to_owned(), source })?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| DriverError::NonUtf8Path(path.to_string_lossy().into_owned()))?;
        let name = path.file_name().unwrap_or_default();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let relative =
            path.strip_prefix(root).map_err(|error| project_message(error.to_string()))?;
        let relative = PackagePath::parse(&portable_path(relative))
            .map_err(|error| project_message(error.to_string()))?;
        if !source_path_relevant(document, target, &relative, targets) {
            continue;
        }
        if path.is_dir() {
            let nested = path.join(fai_package::MANIFEST_NAME);
            if nested.is_file() {
                let relative = nested
                    .strip_prefix(root)
                    .map_err(|error| project_message(error.to_string()))?;
                out.push(
                    PackagePath::parse(&portable_path(relative))
                        .map_err(|error| project_message(error.to_string()))?,
                );
            } else {
                collect_inventory(root, &path, document, target, targets, ancestors, out)?;
            }
        } else if path.extension() == Some("fai") {
            if !canonical(&path)?.starts_with(root) {
                return Err(project_message(format!("source {path} escapes its package")));
            }
            let relative =
                path.strip_prefix(root).map_err(|error| project_message(error.to_string()))?;
            out.push(
                PackagePath::parse(&portable_path(relative))
                    .map_err(|error| project_message(error.to_string()))?,
            );
        }
    }
    ancestors.remove(&real);
    Ok(())
}

fn source_path_relevant(
    document: &ManifestDocument,
    target: &TargetPlatform,
    path: &PackagePath,
    targets: bool,
) -> bool {
    let manifest = document.manifest();
    if manifest.vendor_dir.as_ref().is_some_and(|vendor| path.is_within(vendor)) {
        return false;
    }
    let Some(package) = &manifest.package else {
        return false;
    };
    if manifest.binaries.iter().any(|binary| &binary.path == path) {
        return targets;
    }
    if targets && manifest.binaries.iter().any(|binary| binary.path.is_within(path)) {
        return true;
    }
    let main = if package.source_dir.is_root() {
        "Main.fai".to_owned()
    } else {
        format!("{}/Main.fai", package.source_dir)
    };
    if package.auto_main && path.as_str() == main {
        return targets;
    }
    let mut roots = vec![(&package.source_dir, true), (&package.test_dir, targets)];
    for platform in &manifest.platforms {
        let active = platform.when.matches(target);
        roots.extend(platform.source_dirs.iter().map(|root| (root, active)));
        roots.extend(platform.test_source_dirs.iter().map(|root| (root, active && targets)));
    }
    if roots.iter().any(|(root, active)| *active && root.is_within(path)) {
        return true;
    }
    let depth =
        roots.iter().filter(|(root, _)| path.is_within(root)).map(|(root, _)| root.depth()).max();
    roots
        .iter()
        .any(|(root, active)| *active && Some(root.depth()) == depth && path.is_within(root))
}

fn portable_path(path: &Utf8Path) -> String {
    let parts: Vec<_> = path.components().map(|part| part.as_str()).collect();
    if parts.is_empty() { ".".to_owned() } else { parts.join("/") }
}

fn native_requirements(
    root: &Utf8Path,
    native: &fai_package::NativeRequirements,
    target: &TargetPlatform,
) -> Result<NativeDeps, String> {
    if !native.frameworks.is_empty() && target.os() != "macos" {
        return Err("Apple frameworks require an Apple target".into());
    }
    Ok(NativeDeps {
        lib_dirs: native
            .library_dirs
            .iter()
            .map(|path| root.join(path).into_std_path_buf())
            .collect(),
        libs: native.libraries.clone(),
        objects: native.objects.iter().map(|path| root.join(path).into_std_path_buf()).collect(),
        frameworks: native.frameworks.clone(),
    })
}

pub(crate) fn message_error(message: String) -> DriverError {
    project_message(message)
}

fn project_message(message: String) -> DriverError {
    let diagnostic =
        Diagnostic::error(fai_package::INVALID_SOURCE, &message, crate::tooling_span());
    DriverError::Project(Box::new(ProjectError {
        human: message,
        diagnostics: fai_diagnostics::wire::to_wire(&[diagnostic], &SourceMap::new()),
    }))
}
