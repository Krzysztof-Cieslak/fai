//! Package metadata commands using the same preparation path as compiler sessions.

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::{
    Dependency, DependencyAlias, GitSelector, LockContext, LockFile, LockLocator, LockSource,
    ManifestDocument, PackageName, PackageVersion, TargetPlatform, VersionRequirement,
};
use serde::Serialize;

use crate::DriverError;
use crate::project::{
    PrepareMode, PreparePolicy, PreparedProject, ProjectError, ProjectOptions, prepare_request,
};

/// Explicit package operation; source edits are prepared before publication.
#[derive(Debug, Clone)]
pub enum PackageCommand {
    /// Conservatively repair selected graph records.
    Resolve,
    /// Repair and materialize exact selected sources.
    Fetch,
    /// Eagerly update all packages, or the named aliases/logical keys.
    Update(Vec<String>),
    /// Inspect recorded graphs without restoring or changing selections.
    Deps { why: Option<String> },
    /// Snapshot selected locked Git sources under a project-owned directory.
    Vendor { directory: Option<fai_package::PackagePath> },
    /// Add a dependency, deriving a caret requirement when requested.
    Add {
        dependency: Dependency,
        alias: Option<DependencyAlias>,
        development: bool,
        default_version: bool,
    },
    /// Remove only the selected normal/development declaration.
    Remove { alias: DependencyAlias, development: bool },
}

/// Exact source selection before or after a package operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageSelection {
    /// Declared package version.
    pub version: PackageVersion,
    /// Actual selected directory or Git commit.
    pub source: LockSource,
}

/// One graph-local source selection change.
#[derive(Debug, Clone, Serialize)]
pub struct PackageChange {
    /// Addition, removal, version change, branch movement, or replacement.
    pub kind: String,
    /// Original logical package identity.
    pub package: LockLocator,
    /// Previous selection, absent for additions.
    pub before: Option<PackageSelection>,
    /// New selection, absent for removals.
    pub after: Option<PackageSelection>,
    /// Selection policy responsible for the change.
    pub reason: String,
    /// Affected root/platform records.
    pub graphs: Vec<String>,
}

/// Versioned package-command result. Human notices are rendered separately.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageOutput {
    /// Stable output envelope version.
    pub schema_version: u32,
    /// Invoked metadata command.
    pub command: String,
    /// Whether preparation completed successfully.
    pub ok: bool,
    /// Selected root configuration layer.
    pub context: LockContext,
    /// Active lock filename.
    pub lockfile: String,
    /// Selected root paths relative to the active project.
    pub roots: Vec<String>,
    /// Selected metadata target triples.
    pub targets: Vec<String>,
    /// Changed logical source selections.
    pub changes: Vec<PackageChange>,
    /// Affected graph records still requiring preparation.
    pub stale_graphs: Vec<String>,
    /// Located machine-readable diagnostics.
    pub diagnostics: Vec<fai_diagnostics::wire::DiagnosticWire>,
    /// Read-only selected graph details for dependency inspection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<LockFile>,
    /// Recorded incoming edges explaining a dependency selector.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<serde_json::Value>,
    /// Advisory preparation details, printed separately from machine output.
    pub notices: Vec<String>,
}

/// Executes package preparation or a manifest edit without compiling source bodies.
pub fn run_package_command(
    root: &Utf8Path,
    command: &PackageCommand,
    options: &ProjectOptions,
    targets: &[TargetPlatform],
) -> Result<PackageOutput, DriverError> {
    if let PackageCommand::Vendor { directory } = command {
        return vendor_project(root, directory.as_ref(), options, targets);
    }
    let name = match command {
        PackageCommand::Resolve => "resolve",
        PackageCommand::Fetch => "fetch",
        PackageCommand::Update(_) => "update",
        PackageCommand::Deps { .. } => "deps",
        PackageCommand::Vendor { .. } => "vendor",
        PackageCommand::Add { .. } => "add",
        PackageCommand::Remove { .. } => "remove",
    };
    let mut mode =
        PrepareMode { metadata_only: true, targets: targets.to_vec(), ..PrepareMode::default() };
    let mut edited_root = None;
    let mut authored = None;
    match command {
        PackageCommand::Resolve | PackageCommand::Fetch => mode.policy = PreparePolicy::Resolve,
        PackageCommand::Update(selectors) => mode.policy = PreparePolicy::Update(selectors.clone()),
        PackageCommand::Deps { .. } => {
            mode.inspect = true;
            mode.write = false;
        }
        PackageCommand::Vendor { .. } => unreachable!("handled before graph mutation"),
        PackageCommand::Add { .. } | PackageCommand::Remove { .. } => {
            if mode.targets.is_empty() {
                mode.targets.push(host()?);
            }
            let selected = required_project(prepare_request(
                root,
                &[],
                options,
                None,
                PrepareMode {
                    selection_only: true,
                    metadata_only: true,
                    targets: vec![host()?],
                    write: false,
                    ..Default::default()
                },
            )?)?;
            if selected.roots.len() != 1 {
                return Err(failure("add/remove requires one selected package; use --package"));
            }
            let path = selected.roots[0].1.join("fai.toml");
            let original = read_manifest(&path)?;
            mode.expected_inputs.insert(
                path.clone(),
                fai_package_store::FileEdit::fingerprint(original.text().as_bytes()),
            );
            edited_root = Some(selected.roots[0].1.clone());
            match command {
                PackageCommand::Remove { alias, development } => {
                    let changed = original.without_dependency(alias, *development).map_err(
                        |diagnostics| manifest_failure(&path, original.text(), diagnostics),
                    )?;
                    mode.manifest_edits.insert(path.clone(), changed.text().to_owned());
                }
                PackageCommand::Add { dependency, alias, development, default_version } => {
                    let temporary = match alias {
                        Some(alias) => alias.clone(),
                        None => {
                            let mut index = 0;
                            loop {
                                let alias = DependencyAlias::parse(&format!("__fai_added_{index}"))
                                    .map_err(|error| failure(error.to_string()))?;
                                if !original.manifest().dependencies.contains_key(&alias)
                                    && !original.manifest().dev_dependencies.contains_key(&alias)
                                {
                                    break alias;
                                }
                                index += 1;
                            }
                        }
                    };
                    let changed =
                        original.with_dependency(&temporary, dependency, *development).map_err(
                            |diagnostics| manifest_failure(&path, original.text(), diagnostics),
                        )?;
                    mode.manifest_edits.insert(path.clone(), changed.text().into());
                    mode.write = false;
                    let preview =
                        required_project(prepare_request(root, &[], options, None, mode.clone())?)?;
                    let lock = preview
                        .lock
                        .as_ref()
                        .ok_or_else(|| failure("dependency preview produced no graph"))?;
                    let graph = lock
                        .graphs
                        .iter()
                        .find(|graph| graph.target == mode.targets[0])
                        .ok_or_else(|| failure("dependency preview has no host graph"))?;
                    let edge = graph
                        .edges
                        .iter()
                        .find(|edge| {
                            edge.alias == temporary
                                && if *development {
                                    edge.from == "$root"
                                } else {
                                    edge.from == graph.root_package
                                }
                        })
                        .ok_or_else(|| failure("dependency preview has no added edge"))?;
                    let package = lock
                        .package(&edge.to)
                        .ok_or_else(|| failure("dependency preview has no selected source"))?;
                    let selected_alias = if let Some(alias) = alias {
                        alias.clone()
                    } else {
                        let metadata =
                            selected_manifest(&preview.configuration_root, package, options)?;
                        let name = &metadata
                            .manifest()
                            .package
                            .as_ref()
                            .ok_or_else(|| failure("added source is not a package"))?
                            .name;
                        DependencyAlias::parse(&name.as_str().replace('-', "_")).map_err(|_| {
                            failure("package name is not an available import alias; supply --as")
                        })?
                    };
                    if alias.is_none()
                        && (original.manifest().dependencies.contains_key(&selected_alias)
                            || original.manifest().dev_dependencies.contains_key(&selected_alias))
                    {
                        return Err(failure(format!(
                            "dependency alias `{selected_alias}` already exists; choose --as"
                        )));
                    }
                    let mut selected_dependency = dependency.clone();
                    if *default_version
                        && let Dependency::Git { selector, .. } = &mut selected_dependency
                    {
                        *selector = GitSelector::Version(
                            VersionRequirement::parse(&format!("^{}", package.version))
                                .map_err(|error| failure(error.to_string()))?,
                        );
                    }
                    let changed = original
                        .with_dependency(&selected_alias, &selected_dependency, *development)
                        .map_err(|diagnostics| {
                            manifest_failure(&path, original.text(), diagnostics)
                        })?;
                    mode.manifest_edits.insert(path.clone(), changed.text().into());
                    authored = Some(format!("added {selected_alias} at {}", package.version));
                    mode.expected_lock = Some(
                        preview
                            .original_lock
                            .as_ref()
                            .map(|text| fai_package_store::FileEdit::fingerprint(text.as_bytes())),
                    );
                    mode.seed_lock = preview.lock;
                    mode.write = true;
                }
                _ => unreachable!(),
            }
        }
    }
    let prepared = required_project(prepare_request(root, &[], options, None, mode)?)?;
    let mut output = output(name, &prepared)?;
    if let Some(notice) = authored {
        output.notices.push(notice);
    }
    if let Some(edited) = edited_root {
        let edited = relative(&prepared.configuration_root, &edited);
        if let Some(previous) = &prepared.previous_lock {
            output.stale_graphs.extend(
                previous
                    .graphs
                    .iter()
                    .filter(|graph| {
                        graph.root == edited && !prepared.targets.contains(&graph.target)
                    })
                    .map(|graph| format!("{}@{}", graph.root, graph.target)),
            );
        }
        if prepared.local
            && let Ok(text) = std::fs::read_to_string(prepared.configuration_root.join("fai.lock"))
            && let Ok(shared) = LockFile::parse(&text)
        {
            for graph in &shared.graphs {
                let affected = graph.root == edited || graph.packages.iter().filter_map(|id| shared.package(id)).any(|package| matches!(&package.source, LockSource::Path { path } if path == &edited));
                if affected {
                    output.stale_graphs.push(format!("shared:{}@{}", graph.root, graph.target));
                }
            }
        }
    }
    output.stale_graphs.sort();
    output.stale_graphs.dedup();
    if let PackageCommand::Deps { why } = command {
        output.graph = prepared.lock.clone().map(|mut lock| {
            lock.graphs.retain(|graph| {
                output.roots.contains(&graph.root) && prepared.targets.contains(&graph.target)
            });
            let ids: BTreeSet<_> =
                lock.graphs.iter().flat_map(|graph| &graph.packages).cloned().collect();
            lock.packages.retain(|package| ids.contains(&package.id));
            lock
        });
        if let (Some(selector), Some(lock)) = (why, output.graph.as_ref()) {
            let mut reasons = Vec::new();
            for graph in &lock.graphs {
                let selected: BTreeSet<_> = graph
                    .edges
                    .iter()
                    .filter(|edge| edge.alias.as_str() == selector)
                    .map(|edge| edge.to.clone())
                    .collect();
                let mut needed = selected;
                loop {
                    let before = needed.len();
                    for edge in &graph.edges {
                        if needed.contains(&edge.to) {
                            needed.insert(edge.from.clone());
                        }
                    }
                    if before == needed.len() {
                        break;
                    }
                }
                for edge in &graph.edges {
                    if needed.contains(&edge.to) {
                        reasons.push(serde_json::json!({"root": graph.root, "target": graph.target, "edge": edge}));
                    }
                }
            }
            if reasons.is_empty() {
                return Err(failure(format!("no recorded dependency matches `{selector}`")));
            }
            output.why = Some(serde_json::Value::Array(reasons));
        }
    }
    Ok(output)
}

fn required_project(project: Option<PreparedProject>) -> Result<PreparedProject, DriverError> {
    project.ok_or_else(|| failure("package commands require a versioned fai.toml manifest"))
}

fn vendor_project(
    root: &Utf8Path,
    requested: Option<&fai_package::PackagePath>,
    options: &ProjectOptions,
    targets: &[TargetPlatform],
) -> Result<PackageOutput, DriverError> {
    let restore = ProjectOptions { no_vendor: true, ..options.clone() };
    let project = required_project(prepare_request(
        root,
        &[],
        &restore,
        None,
        PrepareMode {
            metadata_only: true,
            write: false,
            targets: targets.to_vec(),
            ..Default::default()
        },
    )?)?;
    let mut result = output("vendor", &project)?;
    let lock =
        project.lock.as_ref().ok_or_else(|| failure("vendor preparation has no selected graph"))?;
    let mut selected = lock.clone();
    selected.graphs.retain(|graph| {
        result.roots.contains(&graph.root) && project.targets.contains(&graph.target)
    });
    let used: BTreeSet<_> =
        selected.graphs.iter().flat_map(|graph| &graph.packages).cloned().collect();
    selected.packages.retain(|package| used.contains(&package.id));
    for package in &selected.packages {
        if let LockSource::Path { path } = &package.source {
            let actual =
                project.configuration_root.join(path).canonicalize_utf8().map_err(|source| {
                    DriverError::Io { path: project.configuration_root.join(path), source }
                })?;
            if !actual.starts_with(&project.configuration_root) {
                return Err(fai_package::ProviderFailure { code: fai_package::VENDOR_UNAVAILABLE, message: "vendor snapshots cannot include mutable paths outside the active project; use a shared in-project source or immutable Git dependency".into() }.into());
            }
        }
    }
    let manifest_path = project.configuration_root.join("fai.toml");
    let document = read_manifest(&manifest_path)?;
    if project.observed.iter().any(|(path, expected)| {
        path == &manifest_path
            && fai_package_store::FileEdit::fingerprint(document.text().as_bytes()) != *expected
    }) {
        return Err(fai_package::ProviderFailure {
            code: fai_package::INPUTS_CHANGED,
            message: "root manifest changed during vendor preparation".into(),
        }
        .into());
    }
    let directory = requested
        .cloned()
        .or_else(|| document.manifest().vendor_dir.clone())
        .unwrap_or_else(|| fai_package::PackagePath::parse("vendor").expect("literal vendor path"));
    if directory.is_root() {
        return Err(failure("vendor directory must be a project subdirectory"));
    }
    let destination = project.configuration_root.join(directory.as_str());
    let mut existing = destination.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| failure("invalid vendor destination"))?;
    }
    if !existing
        .canonicalize_utf8()
        .map_err(|source| DriverError::Io { path: existing.to_owned(), source })?
        .starts_with(&project.configuration_root)
    {
        return Err(failure("vendor directory escapes the active project through a symlink"));
    }
    let _guard =
        fai_package_store::lock_file(&project.configuration_root.join(".fai/vendor.lock"))?;
    let old_index = match std::fs::read_to_string(destination.join("fai-vendor.toml")) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(DriverError::Io { path: destination.join("fai-vendor.toml"), source });
        }
    };
    let cache = crate::cache::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("fai-cache"));
    let cache =
        Utf8PathBuf::from_path_buf(cache).map_err(|_| failure("cache path is not UTF-8"))?;
    let index = fai_package_store::VendorIndex::prepare(
        &destination,
        &selected,
        &fai_package_store::GitStore::new(cache.join("packages"), options.offline),
    )?;
    let changed = document
        .with_vendor_directory(&directory)
        .map_err(|diagnostics| manifest_failure(&manifest_path, document.text(), diagnostics))?;
    let mut edits = vec![fai_package_store::FileEdit::new(
        format!("{directory}/fai-vendor.toml"),
        old_index.as_deref(),
        index.to_toml()?,
    )];
    if changed.text() != document.text() {
        edits.push(fai_package_store::FileEdit::new(
            "fai.toml".into(),
            Some(document.text()),
            changed.text().into(),
        ));
    }
    if project.previous_lock.as_ref() != Some(lock) {
        edits.push(fai_package_store::FileEdit::new(
            result.lockfile.clone(),
            project.original_lock.as_deref(),
            lock.to_toml()?,
        ));
    }
    let observed = project
        .observed
        .iter()
        .map(|(path, digest)| (path.to_string(), Some(digest.clone())))
        .collect();
    fai_package_store::MetadataTransaction::new(edits, observed)
        .commit(&project.configuration_root)?;
    result.notices.push(format!(
        "vendored {} Git package sources into {directory}",
        selected
            .packages
            .iter()
            .filter(|package| matches!(package.source, LockSource::Git { .. }))
            .count()
    ));
    Ok(result)
}

fn output(command: &str, project: &PreparedProject) -> Result<PackageOutput, DriverError> {
    let lock = project
        .lock
        .as_ref()
        .ok_or_else(|| failure("package preparation produced no lock model"))?;
    let mut changes = Vec::new();
    for graph in &lock.graphs {
        let before = project
            .previous_lock
            .as_ref()
            .and_then(|lock| {
                lock.graph(&graph.root, &graph.target).map(|graph| selections(lock, graph))
            })
            .unwrap_or_default();
        let after = selections(lock, graph);
        let keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
        for key in keys {
            let left = before.get(&key);
            let right = after.get(&key);
            if left == right {
                continue;
            }
            let kind = match (left, right) {
                (None, _) => "add",
                (_, None) => "remove",
                (Some(left), Some(right)) if left.version < right.version => "upgrade",
                (Some(left), Some(right)) if left.version > right.version => "downgrade",
                (Some(left), Some(right)) if matches!((&left.source, &right.source), (LockSource::Git { git: a, subdir: x, .. }, LockSource::Git { git: b, subdir: y, .. }) if a == b && x == y) => {
                    "branch-move"
                }
                _ => "source-replacement",
            };
            changes.push(PackageChange {
                kind: kind.into(),
                package: key,
                before: left.cloned(),
                after: right.cloned(),
                reason: if command == "update" {
                    "requested-update"
                } else {
                    "conservative-resolution"
                }
                .into(),
                graphs: vec![format!("{}@{}", graph.root, graph.target)],
            });
        }
    }
    Ok(PackageOutput {
        schema_version: 1,
        command: command.into(),
        ok: true,
        context: lock.context,
        lockfile: if project.local { "fai.local.lock" } else { "fai.lock" }.into(),
        roots: project
            .roots
            .iter()
            .map(|(_, path)| relative(&project.configuration_root, path))
            .collect(),
        targets: project.targets.iter().map(ToString::to_string).collect(),
        changes,
        stale_graphs: project.stale_graphs.clone(),
        diagnostics: Vec::new(),
        graph: None,
        why: None,
        notices: project.notices.clone(),
    })
}

fn selections(
    lock: &LockFile,
    graph: &fai_package::LockedGraph,
) -> BTreeMap<LockLocator, PackageSelection> {
    graph
        .packages
        .iter()
        .filter_map(|id| lock.package(id))
        .map(|package| {
            (
                package.logical.clone(),
                PackageSelection {
                    version: package.version.clone(),
                    source: package.source.clone(),
                },
            )
        })
        .collect()
}

fn selected_manifest(
    root: &Utf8Path,
    package: &fai_package::LockedPackage,
    options: &ProjectOptions,
) -> Result<ManifestDocument, DriverError> {
    let directory = match &package.source {
        LockSource::Path { path } => root.join(path),
        LockSource::Git { git, subdir, rev } => {
            let cache =
                crate::cache::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("fai-cache"));
            let cache = Utf8PathBuf::from_path_buf(cache)
                .map_err(|_| failure("cache path is not UTF-8"))?;
            fai_package_store::GitStore::new(cache.join("packages"), options.offline)
                .materialize(
                    &fai_package::GitPackage { repository: git.clone(), subdir: subdir.clone() },
                    rev,
                    package.checksum.as_ref(),
                )?
                .directory
        }
    };
    read_manifest(&directory.join("fai.toml"))
}

fn read_manifest(path: &Utf8Path) -> Result<ManifestDocument, DriverError> {
    let text = std::fs::read_to_string(path)
        .map_err(|source| DriverError::Io { path: path.to_owned(), source })?;
    ManifestDocument::parse(fai_span::SourceId::new(0), &text)
        .map_err(|diagnostics| manifest_failure(path, &text, diagnostics))
}

fn manifest_failure(
    path: &Utf8Path,
    text: &str,
    diagnostics: Vec<fai_diagnostics::Diagnostic>,
) -> DriverError {
    let mut sources = fai_span::SourceMap::new();
    sources.add(path.to_owned(), text.into());
    DriverError::Project(Box::new(ProjectError {
        human: fai_diagnostics::render_human(&diagnostics, &sources, false),
        diagnostics: fai_diagnostics::wire::to_wire(&diagnostics, &sources),
    }))
}

fn relative(root: &Utf8Path, path: &Utf8Path) -> String {
    let path = path.strip_prefix(root).unwrap_or(path);
    if path.as_str().is_empty() {
        ".".into()
    } else {
        path.components().map(|part| part.as_str()).collect::<Vec<_>>().join("/")
    }
}

fn host() -> Result<TargetPlatform, DriverError> {
    TargetPlatform::parse(&target_lexicon::HOST.to_string())
        .map_err(|error| failure(error.to_string()))
}
fn failure(message: impl Into<String>) -> DriverError {
    fai_package::ProviderFailure { code: fai_package::INVALID_MANIFEST, message: message.into() }
        .into()
}

/// Creates a package from explicit name intent without fetching dependencies.
pub fn init_package(path: &Utf8Path, name: Option<&str>, library: bool) -> Result<(), DriverError> {
    let name = PackageName::parse(name.unwrap_or_else(|| path.file_name().unwrap_or("app")))
        .map_err(|error| failure(error.to_string()))?;
    let manifest = format!(
        "manifest-version = 1\n\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nfai-version = \"^{}\"\n",
        env!("CARGO_PKG_VERSION")
    );
    let (entry, source) = if library {
        (
            "src/Library.fai",
            "module Library\n\npublic identity : 'a -> 'a\nlet identity value = value\n",
        )
    } else {
        (
            "src/Main.fai",
            "module Main\n\npublic main : Runtime -> Unit / { Console }\nlet main runtime = runtime.console.writeLine \"Hello, Fai!\"\n",
        )
    };
    let files = [(path.join("fai.toml"), manifest.as_str()), (path.join(entry), source)];
    if files.iter().any(|(path, _)| path.exists()) {
        return Err(failure(
            "package initialization would overwrite an existing manifest or source",
        ));
    }
    let mut created: Vec<Utf8PathBuf> = Vec::new();
    for (path, contents) in files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| DriverError::Io { path: parent.to_owned(), source })?;
        }
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, contents.as_bytes()));
        if let Err(source) = result {
            for path in created {
                let _ = std::fs::remove_file(path);
            }
            return Err(DriverError::Io { path, source });
        }
        created.push(path);
    }
    let ignore = path.join(".gitignore");
    let mut text = match std::fs::read_to_string(&ignore) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(DriverError::Io { path: ignore, source }),
    };
    for entry in [".fai/", "fai.local.toml", "fai.local.lock"] {
        if !text.lines().any(|line| line == entry) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(entry);
            text.push('\n');
        }
    }
    fai_package_store::atomic_write(&ignore, text.as_bytes())?;
    Ok(())
}
