//! Deterministic target source sets from explicit manifests and relative file inventories.

use std::collections::{BTreeMap, BTreeSet};

use fai_diagnostics::Diagnostic;
use serde::Serialize;

use crate::{
    ManifestDocument, PackageName, PackagePath, TargetPlatform,
    diagnostic::{INVALID_PLATFORM, INVALID_TARGET},
};

/// The input files for an executable, including shared library files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutableSources {
    /// Package-local executable name.
    pub name: PackageName,
    /// Its selected entry file.
    pub entry: PackagePath,
    /// Shared library files and this executable's entry, in stable path order.
    pub sources: Vec<PackagePath>,
}

/// Target-local source membership without filesystem or compiler state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourcePlan {
    /// Shared production modules; every executable and test sees these modules.
    pub library: Vec<PackagePath>,
    /// Additional test modules; these are not production module inputs.
    pub tests: Vec<PackagePath>,
    /// Executables with mutually isolated entry files.
    pub executables: Vec<ExecutableSources>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RootKind {
    Library,
    Tests,
}

struct Root<'a> {
    path: &'a PackagePath,
    kind: RootKind,
    active: bool,
}

impl SourcePlan {
    /// Classifies a complete package-relative file inventory.
    ///
    /// Include nested `fai.toml` paths in the inventory so nested package boundaries
    /// can be excluded. Filesystem enumeration and symlink containment checks belong
    /// to the caller; this function operates only on normalized lexical paths.
    pub fn build(
        document: &ManifestDocument,
        target: &TargetPlatform,
        files: &[PackagePath],
    ) -> Result<Self, Vec<Diagnostic>> {
        Self::build_scopes(document, target, files, true)
    }

    /// Plans only a consumed dependency's shared library sources.
    pub fn build_library(
        document: &ManifestDocument,
        target: &TargetPlatform,
        files: &[PackagePath],
    ) -> Result<Self, Vec<Diagnostic>> {
        Self::build_scopes(document, target, files, false)
    }

    fn build_scopes(
        document: &ManifestDocument,
        target: &TargetPlatform,
        files: &[PackagePath],
        targets: bool,
    ) -> Result<Self, Vec<Diagnostic>> {
        let manifest = document.manifest();
        let Some(package) = &manifest.package else {
            return Err(vec![document.diagnostic(
                INVALID_TARGET,
                "source planning requires a package manifest",
                "package",
            )]);
        };
        manifest.configuration(target).map_err(|error| {
            vec![document.diagnostic(
                INVALID_PLATFORM,
                error.to_string(),
                "package.supported-platforms",
            )]
        })?;
        let inventory: BTreeSet<_> = files.iter().cloned().collect();
        let nested: Vec<_> = files
            .iter()
            .filter(|file| file.file_name() == Some(crate::MANIFEST_NAME))
            .filter_map(PackagePath::parent)
            .filter(|path| !path.is_root())
            .collect();
        let excluded = |path: &PackagePath| {
            path.as_str() == ".fai"
                || path.as_str().starts_with(".fai/")
                || nested.iter().any(|root| path.is_within(root))
                || manifest.vendor_dir.as_ref().is_some_and(|root| path.is_within(root))
        };

        let main_file =
            package.source_dir.join(&PackagePath::parse("Main.fai").map_err(|error| {
                vec![document.diagnostic(INVALID_TARGET, error.to_string(), "package.source-dir")]
            })?);
        let mut entries: BTreeMap<PackageName, PackagePath> = manifest
            .binaries
            .iter()
            .map(|binary| (binary.name.clone(), binary.path.clone()))
            .collect();
        if package.auto_main
            && inventory.contains(&main_file)
            && !entries.contains_key(&package.name)
            && !entries.values().any(|path| path == &main_file)
        {
            entries.insert(package.name.clone(), main_file.clone());
        }
        let mut errors = Vec::new();
        for (name, path) in &entries {
            if !targets {
                continue;
            }
            let field =
                manifest.binaries.iter().position(|binary| &binary.name == name).map_or_else(
                    || "package.source-dir".to_owned(),
                    |index| format!("bin[{index}].path"),
                );
            if !inventory.contains(path) {
                errors.push(document.diagnostic(
                    INVALID_TARGET,
                    format!("executable `{name}` entry `{path}` is missing"),
                    &field,
                ));
            } else if excluded(path) {
                errors.push(document.diagnostic(INVALID_TARGET, format!("executable `{name}` entry `{path}` is outside this package's source ownership"), &field));
            }
        }
        if targets
            && let Some(default) = &package.default_run
            && !entries.contains_key(default)
        {
            errors.push(document.diagnostic(
                INVALID_TARGET,
                format!("default-run names unknown executable `{default}`"),
                "package.default-run",
            ));
        }
        if !errors.is_empty() {
            return Err(errors);
        }

        let mut roots = vec![
            Root { path: &package.source_dir, kind: RootKind::Library, active: true },
            Root { path: &package.test_dir, kind: RootKind::Tests, active: true },
        ];
        for platform in &manifest.platforms {
            let active = platform.when.matches(target);
            roots.extend(platform.source_dirs.iter().map(|path| Root {
                path,
                kind: RootKind::Library,
                active,
            }));
            roots.extend(platform.test_source_dirs.iter().map(|path| Root {
                path,
                kind: RootKind::Tests,
                active,
            }));
        }
        let entry_paths: BTreeSet<_> = entries.values().collect();
        let mut library = Vec::new();
        let mut tests = Vec::new();
        for file in &inventory {
            if !file.as_str().ends_with(".fai")
                || excluded(file)
                || entry_paths.contains(file)
                || (package.auto_main && file == &main_file)
            {
                continue;
            }
            let depth = roots
                .iter()
                .filter(|root| file.is_within(root.path))
                .map(|root| root.path.depth())
                .max();
            let Some(depth) = depth else {
                continue;
            };
            if let Some(root) = roots
                .iter()
                .find(|root| root.active && root.path.depth() == depth && file.is_within(root.path))
            {
                match root.kind {
                    RootKind::Library => library.push(file.clone()),
                    RootKind::Tests if targets => tests.push(file.clone()),
                    RootKind::Tests => {}
                }
            }
        }
        let executables = entries
            .into_iter()
            .filter(|_| targets)
            .map(|(name, entry)| {
                let mut sources = library.clone();
                sources.push(entry.clone());
                sources.sort();
                ExecutableSources { name, entry, sources }
            })
            .collect();
        Ok(Self { library, tests, executables })
    }
}
