//! TOML decoding with strict fields, accumulated diagnostics, and retained locations.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use fai_diagnostics::{Diagnostic, DiagnosticCode, Label};
use fai_span::{ByteOffset, SourceId, Span, TextRange};
use toml_edit::{ImDocument, Item, TableLike, Value};

use crate::{
    BinaryTarget, Dependency, DependencyAlias, GitBranch, GitPackage, GitRepository, GitRevision,
    GitSelector, Manifest, MetadataError, NativeRequirements, Override, Package, PackageName,
    PackagePath, PackageVersion, PlatformPredicate, PlatformSection, TargetPlatform,
    VersionRequirement, Workspace, diagnostic::*,
};

/// The project and package manifest filename.
pub const MANIFEST_NAME: &str = "fai.toml";

/// The kind of configuration represented by a parsed document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    /// A versioned package or workspace document.
    Project,
    /// A root-only personal override document.
    Local,
    /// An unversioned document containing only native requirements.
    LegacyNative,
}

/// Locations of a field's key and value, kept outside semantic models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldLocation {
    /// Location of the field name.
    pub key: Span,
    /// Location of its value or table.
    pub value: Span,
}

/// Checked declarations and the original source needed for precise diagnostics/edits.
#[derive(Debug, Clone)]
pub struct ManifestDocument {
    source: SourceId,
    kind: ManifestKind,
    manifest: Manifest,
    document: ImDocument<String>,
    locations: BTreeMap<String, FieldLocation>,
}

impl ManifestDocument {
    /// Parses a versioned project or an unversioned native-only manifest.
    pub fn parse(source: SourceId, text: &str) -> Result<Self, Vec<Diagnostic>> {
        Self::parse_kind(source, text, false)
    }

    /// Parses a personal manifest, which may contain only version and overrides.
    pub fn parse_local(source: SourceId, text: &str) -> Result<Self, Vec<Diagnostic>> {
        Self::parse_kind(source, text, true)
    }

    fn parse_kind(source: SourceId, text: &str, local: bool) -> Result<Self, Vec<Diagnostic>> {
        let mut parser = Parser {
            source,
            text,
            locations: BTreeMap::new(),
            diagnostics: Vec::new(),
            platform_paths: Vec::new(),
            binary_paths: Vec::new(),
            override_paths: Vec::new(),
            path_origins: BTreeMap::new(),
        };
        if text.len() > u32::MAX as usize {
            parser.error(
                INVALID_MANIFEST,
                "manifest exceeds the source offset limit",
                Span::new(source, TextRange::default()),
            );
            return Err(parser.diagnostics);
        }
        let document = ImDocument::parse(text.to_owned()).map_err(|error| {
            vec![Diagnostic::error(
                INVALID_MANIFEST,
                error.message(),
                parser.span(error.span().unwrap_or(0..0)),
            )]
        })?;
        let table = document.as_table();
        let kind = if local {
            ManifestKind::Local
        } else if !table.contains_key("manifest-version")
            && table.iter().all(|(key, _)| key == "native")
        {
            ManifestKind::LegacyNative
        } else {
            ManifestKind::Project
        };
        let manifest = parser.read_manifest(table, kind);
        if !parser.diagnostics.is_empty() {
            parser
                .diagnostics
                .sort_by_key(|diagnostic| (diagnostic.primary.start(), diagnostic.code.as_str()));
            return Err(parser.diagnostics);
        }
        Ok(Self { source, kind, manifest, document, locations: parser.locations })
    }

    /// Returns location-free declarations suitable for semantic comparison.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Returns the kind of this document.
    #[must_use]
    pub fn kind(&self) -> ManifestKind {
        self.kind
    }

    /// Returns the exact original text, including comments and whitespace.
    #[must_use]
    pub fn text(&self) -> &str {
        self.document.raw()
    }

    /// Finds a field by its structural path, such as `platform[0].dependencies.json`.
    #[must_use]
    pub fn location(&self, path: &str) -> Option<FieldLocation> {
        self.locations.get(path).copied()
    }

    /// Produces a revalidated candidate version edit while retaining surrounding comments.
    pub fn with_package_version(&self, version: &PackageVersion) -> Result<Self, Vec<Diagnostic>> {
        let mut document = self.document.clone().into_mut();
        let Some(value) = document
            .get_mut("package")
            .and_then(Item::as_table_like_mut)
            .and_then(|package| package.get_mut("version"))
            .and_then(Item::as_value_mut)
        else {
            return Err(vec![self.diagnostic(
                INVALID_MANIFEST,
                "this document has no package version",
                "package",
            )]);
        };
        let mut replacement = Value::from(version.to_string());
        *replacement.decor_mut() = value.decor().clone();
        *value = replacement;
        Self::parse(self.source, &document.to_string())
    }

    pub(crate) fn diagnostic(
        &self,
        code: DiagnosticCode,
        message: impl Into<String>,
        field: &str,
    ) -> Diagnostic {
        let span = self
            .location(field)
            .map_or(Span::new(self.source, TextRange::default()), |location| location.value);
        Diagnostic::error(code, message, span)
    }
}

struct Parser<'a> {
    source: SourceId,
    text: &'a str,
    locations: BTreeMap<String, FieldLocation>,
    diagnostics: Vec<Diagnostic>,
    platform_paths: Vec<String>,
    binary_paths: Vec<String>,
    override_paths: Vec<String>,
    path_origins: BTreeMap<String, Vec<String>>,
}

struct ConditionalLocation<'a> {
    condition: Option<&'a PlatformPredicate>,
    path: &'a str,
}

impl Parser<'_> {
    fn span(&self, range: Range<usize>) -> Span {
        let mut start = range.start.min(self.text.len());
        let mut end = range.end.max(start).min(self.text.len());
        while !self.text.is_char_boundary(start) {
            start -= 1;
        }
        while !self.text.is_char_boundary(end) {
            end += 1;
        }
        Span::new(
            self.source,
            TextRange::new(ByteOffset::from_usize(start), ByteOffset::from_usize(end)),
        )
    }

    fn at(&self, path: &str) -> Span {
        self.locations.get(path).map_or_else(|| self.span(0..0), |location| location.value)
    }

    fn error(&mut self, code: DiagnosticCode, message: impl Into<String>, span: Span) {
        self.diagnostics.push(Diagnostic::error(code, message, span));
    }

    fn bad(&mut self, code: DiagnosticCode, path: &str, message: impl Into<String>) {
        self.error(code, message, self.at(path));
    }

    fn fields(&mut self, table: &dyn TableLike, parent: &str, allowed: &[&str]) {
        for (key, item) in table.iter() {
            let path = field(parent, key);
            let key_span = table.get_key_value(key).and_then(|(key, _)| key.span()).unwrap_or(0..0);
            let value_span = item.span().unwrap_or_else(|| key_span.clone());
            let location = FieldLocation { key: self.span(key_span), value: self.span(value_span) };
            self.locations.insert(path, location);
            if !allowed.contains(&key) {
                self.error(INVALID_MANIFEST, format!("unknown field `{key}`"), location.key);
            }
        }
    }

    fn string(
        &mut self,
        table: &dyn TableLike,
        key: &str,
        parent: &str,
        required: bool,
    ) -> Option<String> {
        let path = field(parent, key);
        match table.get(key) {
            Some(item) => match item.as_str() {
                Some(value) => Some(value.to_owned()),
                None => {
                    self.bad(INVALID_MANIFEST, &path, format!("`{key}` must be a string"));
                    None
                }
            },
            None => {
                if required {
                    self.bad(INVALID_MANIFEST, parent, format!("missing required field `{key}`"));
                }
                None
            }
        }
    }

    fn boolean(&mut self, table: &dyn TableLike, key: &str, parent: &str, default: bool) -> bool {
        match table.get(key) {
            Some(item) => item.as_bool().unwrap_or_else(|| {
                self.bad(
                    INVALID_MANIFEST,
                    &field(parent, key),
                    format!("`{key}` must be a boolean"),
                );
                default
            }),
            None => default,
        }
    }

    fn convert<T>(
        &mut self,
        value: Option<String>,
        path: &str,
        code: DiagnosticCode,
        parse: impl FnOnce(&str) -> Result<T, MetadataError>,
    ) -> Option<T> {
        value.and_then(|value| match parse(&value) {
            Ok(value) => Some(value),
            Err(error) => {
                self.bad(code, path, error.to_string());
                None
            }
        })
    }

    fn table<'a>(
        &mut self,
        table: &'a dyn TableLike,
        key: &str,
        parent: &str,
        required: bool,
    ) -> Option<&'a dyn TableLike> {
        match table.get(key) {
            Some(item) => match item.as_table_like() {
                Some(table) => Some(table),
                None => {
                    self.bad(
                        INVALID_MANIFEST,
                        &field(parent, key),
                        format!("`{key}` must be a table"),
                    );
                    None
                }
            },
            None => {
                if required {
                    self.bad(INVALID_MANIFEST, parent, format!("missing required table `{key}`"));
                }
                None
            }
        }
    }

    fn strings(&mut self, table: &dyn TableLike, key: &str, parent: &str) -> Vec<(String, String)> {
        let path = field(parent, key);
        let Some(item) = table.get(key) else {
            return Vec::new();
        };
        let Some(array) = item.as_array() else {
            self.bad(INVALID_MANIFEST, &path, format!("`{key}` must be an array of strings"));
            return Vec::new();
        };
        let mut result = Vec::new();
        for (index, value) in array.iter().enumerate() {
            let path = format!("{path}[{index}]");
            let span = self.span(value.span().unwrap_or(0..0));
            self.locations.insert(path.clone(), FieldLocation { key: span, value: span });
            if let Some(value) = value.as_str() {
                result.push((value.to_owned(), path));
            } else {
                self.error(INVALID_MANIFEST, "expected a string array element", span);
            }
        }
        result
    }

    fn tables<'a>(
        &mut self,
        table: &'a dyn TableLike,
        key: &str,
        parent: &str,
    ) -> Vec<(String, &'a dyn TableLike)> {
        let path = field(parent, key);
        let Some(item) = table.get(key) else {
            return Vec::new();
        };
        if let Some(array) = item.as_array_of_tables() {
            return array
                .iter()
                .enumerate()
                .map(|(index, table)| {
                    let path = format!("{path}[{index}]");
                    let span = self.span(table.span().unwrap_or(0..0));
                    self.locations.insert(path.clone(), FieldLocation { key: span, value: span });
                    (path, table as &dyn TableLike)
                })
                .collect();
        }
        if let Some(array) = item.as_array() {
            let mut result = Vec::new();
            for (index, value) in array.iter().enumerate() {
                let path = format!("{path}[{index}]");
                let span = self.span(value.span().unwrap_or(0..0));
                self.locations.insert(path.clone(), FieldLocation { key: span, value: span });
                if let Some(table) = value.as_inline_table() {
                    result.push((path, table as &dyn TableLike));
                } else {
                    self.error(INVALID_MANIFEST, "expected a table array element", span);
                }
            }
            return result;
        }
        self.bad(INVALID_MANIFEST, &path, format!("`{key}` must be an array of tables"));
        Vec::new()
    }

    fn paths(&mut self, table: &dyn TableLike, key: &str, parent: &str) -> Vec<PackagePath> {
        let mut values = Vec::new();
        let mut origins = Vec::new();
        for (value, path) in self.strings(table, key, parent) {
            if let Some(value) =
                self.convert(Some(value), &path, INVALID_SOURCE, PackagePath::parse)
            {
                values.push(value);
                origins.push(path);
            }
        }
        self.path_origins.insert(field(parent, key), origins);
        values
    }

    fn read_manifest(&mut self, table: &dyn TableLike, kind: ManifestKind) -> Manifest {
        if kind != ManifestKind::LegacyNative
            && let Some(item) = table.get("manifest-version")
            && item.as_integer().is_some_and(|version| version != 1)
        {
            self.error(
                UNSUPPORTED_FORMAT,
                "supported manifest version is 1",
                self.span(item.span().unwrap_or(0..0)),
            );
            return Manifest::default();
        }
        let allowed: &[&str] = match kind {
            ManifestKind::Project => &[
                "manifest-version",
                "package",
                "workspace",
                "dependencies",
                "dev-dependencies",
                "bin",
                "native",
                "platform",
                "toolchain",
                "lock-targets",
                "overrides",
                "vendor",
                "metadata",
            ],
            ManifestKind::Local => &["manifest-version", "overrides"],
            ManifestKind::LegacyNative => &["native"],
        };
        self.fields(table, "", allowed);
        if kind != ManifestKind::LegacyNative {
            match table.get("manifest-version").and_then(Item::as_integer) {
                Some(1) => {}
                Some(_) => {
                    self.bad(
                        UNSUPPORTED_FORMAT,
                        "manifest-version",
                        "supported manifest version is 1",
                    );
                    return Manifest::default();
                }
                None => self.bad(
                    INVALID_MANIFEST,
                    "manifest-version",
                    "manifest-version = 1 is required",
                ),
            }
        }
        if kind == ManifestKind::LegacyNative {
            return Manifest { native: self.native(table, "native", ""), ..Manifest::default() };
        }
        if kind == ManifestKind::Local {
            let overrides = self.overrides(table);
            self.validate_overrides(&overrides);
            return Manifest { overrides, ..Manifest::default() };
        }
        let package = self.table(table, "package", "", false).and_then(|table| self.package(table));
        let workspace =
            self.table(table, "workspace", "", false).map(|table| self.workspace(table));
        if !table.contains_key("package") && !table.contains_key("workspace") {
            self.bad(
                INVALID_MANIFEST,
                "",
                "a project must declare [package], [workspace], or both",
            );
        }
        if !table.contains_key("package") {
            for key in ["dependencies", "dev-dependencies", "bin", "native", "platform"] {
                if table.contains_key(key) {
                    self.bad(INVALID_MANIFEST, key, format!("`{key}` requires a [package] table"));
                }
            }
        }
        let dependencies = self.dependencies(table, "dependencies", "");
        let dev_dependencies = self.dependencies(table, "dev-dependencies", "");
        let binaries = self
            .tables(table, "bin", "")
            .into_iter()
            .filter_map(|(path, table)| {
                self.binary(table, &path).inspect(|_| self.binary_paths.push(path))
            })
            .collect();
        let native = self.native(table, "native", "");
        let platforms = self
            .tables(table, "platform", "")
            .into_iter()
            .filter_map(|(path, table)| {
                self.platform(table, &path).inspect(|_| self.platform_paths.push(path))
            })
            .collect();
        let toolchain = self.table(table, "toolchain", "", false).and_then(|table| {
            self.fields(table, "toolchain", &["version"]);
            let version = self.string(table, "version", "toolchain", true);
            self.convert(version, "toolchain.version", INVALID_REQUIREMENT, PackageVersion::parse)
        });
        let lock_targets = self
            .strings(table, "lock-targets", "")
            .into_iter()
            .filter_map(|(value, path)| {
                self.convert(Some(value), &path, INVALID_PLATFORM, TargetPlatform::parse)
            })
            .collect();
        let overrides = self.overrides(table);
        self.validate_overrides(&overrides);
        let vendor_dir = self.table(table, "vendor", "", false).and_then(|table| {
            self.fields(table, "vendor", &["directory"]);
            let directory = self.string(table, "directory", "vendor", true);
            self.convert(directory, "vendor.directory", INVALID_SOURCE, PackagePath::parse)
        });
        self.table(table, "metadata", "", false);
        let manifest = Manifest {
            package,
            workspace,
            dependencies,
            dev_dependencies,
            binaries,
            native,
            platforms,
            toolchain,
            lock_targets,
            overrides,
            vendor_dir,
        };
        self.validate_aliases(&manifest);
        self.validate_targets(&manifest);
        manifest
    }

    fn package(&mut self, table: &dyn TableLike) -> Option<Package> {
        self.fields(
            table,
            "package",
            &[
                "name",
                "version",
                "fai-version",
                "source-dir",
                "test-dir",
                "auto-main",
                "default-run",
                "supported-platforms",
                "description",
                "license",
                "repository",
            ],
        );
        let name = self.string(table, "name", "package", true);
        let name = self.convert(name, "package.name", INVALID_MANIFEST, PackageName::parse);
        let version = self.string(table, "version", "package", true);
        let version =
            self.convert(version, "package.version", INVALID_REQUIREMENT, PackageVersion::parse);
        let fai_version = self.string(table, "fai-version", "package", false);
        let fai_version = self
            .convert(
                fai_version,
                "package.fai-version",
                INVALID_REQUIREMENT,
                VersionRequirement::parse,
            )
            .unwrap_or_default();
        let source_dir =
            self.string(table, "source-dir", "package", false).unwrap_or_else(|| "src".to_owned());
        let source_dir = self.convert(
            Some(source_dir),
            "package.source-dir",
            INVALID_SOURCE,
            PackagePath::parse,
        );
        let test_dir =
            self.string(table, "test-dir", "package", false).unwrap_or_else(|| "tests".to_owned());
        let test_dir =
            self.convert(Some(test_dir), "package.test-dir", INVALID_SOURCE, PackagePath::parse);
        let auto_main = self.boolean(table, "auto-main", "package", true);
        let default_run = self.string(table, "default-run", "package", false);
        let default_run =
            self.convert(default_run, "package.default-run", INVALID_TARGET, PackageName::parse);
        let supported_platforms: Vec<_> = self
            .tables(table, "supported-platforms", "package")
            .into_iter()
            .filter_map(|(path, table)| self.predicate(table, &path))
            .collect();
        if table.contains_key("supported-platforms") && supported_platforms.is_empty() {
            self.bad(
                INVALID_PLATFORM,
                "package.supported-platforms",
                "supported-platforms must contain at least one predicate",
            );
        }
        let description = self.string(table, "description", "package", false);
        let license = self.string(table, "license", "package", false);
        let repository = self.string(table, "repository", "package", false);
        Some(Package {
            name: name?,
            version: version?,
            fai_version,
            explicit_fai_version: table.contains_key("fai-version"),
            source_dir: source_dir?,
            test_dir: test_dir?,
            auto_main,
            default_run,
            supported_platforms,
            description,
            license,
            repository,
        })
    }

    fn workspace(&mut self, table: &dyn TableLike) -> Workspace {
        self.fields(table, "workspace", &["members"]);
        let members = self
            .strings(table, "members", "workspace")
            .into_iter()
            .filter_map(|(pattern, path)| {
                if pattern.is_empty()
                    || pattern.starts_with('/')
                    || pattern.contains(['\\', ':'])
                    || pattern.chars().any(char::is_control)
                    || pattern.split('/').any(|part| part == "..")
                {
                    self.bad(
                        INVALID_SOURCE,
                        &path,
                        "workspace members must be non-escaping relative paths or globs",
                    );
                    None
                } else {
                    Some(pattern)
                }
            })
            .collect();
        Workspace { members }
    }

    fn binary(&mut self, table: &dyn TableLike, path: &str) -> Option<BinaryTarget> {
        self.fields(table, path, &["name", "path"]);
        let name = self.string(table, "name", path, true);
        let name = self.convert(name, &field(path, "name"), INVALID_TARGET, PackageName::parse);
        let entry = self.string(table, "path", path, true);
        let entry = self.convert(entry, &field(path, "path"), INVALID_SOURCE, PackagePath::parse);
        if let Some(entry) = &entry
            && !entry.as_str().ends_with(".fai")
        {
            self.bad(
                INVALID_TARGET,
                &field(path, "path"),
                "an executable entry must be a .fai file",
            );
        }
        Some(BinaryTarget { name: name?, path: entry? })
    }

    fn native(&mut self, table: &dyn TableLike, key: &str, parent: &str) -> NativeRequirements {
        let Some(table) = self.table(table, key, parent, false) else {
            return NativeRequirements::default();
        };
        let path = field(parent, key);
        self.fields(table, &path, &["library-dirs", "libraries", "objects", "frameworks"]);
        let mut values = |key| {
            self.strings(table, key, &path).into_iter().filter_map(|(value, path)| {
            if value.is_empty() || value.chars().any(char::is_control) {
                self.bad(INVALID_MANIFEST, &path, "native requirements must be nonempty strings without control characters");
                None
            } else { Some(value) }
        }).collect()
        };
        NativeRequirements {
            library_dirs: values("library-dirs"),
            libraries: values("libraries"),
            objects: values("objects"),
            frameworks: values("frameworks"),
        }
    }

    fn predicate(&mut self, table: &dyn TableLike, path: &str) -> Option<PlatformPredicate> {
        self.fields(table, path, &["os", "arch", "env"]);
        let os = self.string(table, "os", path, false);
        let arch = self.string(table, "arch", path, false);
        let env = self.string(table, "env", path, false);
        match PlatformPredicate::new(os.as_deref(), arch.as_deref(), env.as_deref()) {
            Ok(predicate) => Some(predicate),
            Err(error) => {
                self.bad(INVALID_PLATFORM, path, error.to_string());
                None
            }
        }
    }

    fn platform(&mut self, table: &dyn TableLike, path: &str) -> Option<PlatformSection> {
        self.fields(
            table,
            path,
            &[
                "when",
                "source-dirs",
                "test-source-dirs",
                "dependencies",
                "dev-dependencies",
                "native",
            ],
        );
        let when = self
            .table(table, "when", path, true)
            .and_then(|table| self.predicate(table, &field(path, "when")));
        let source_dirs = self.paths(table, "source-dirs", path);
        let test_source_dirs = self.paths(table, "test-source-dirs", path);
        let dependencies = self.dependencies(table, "dependencies", path);
        let dev_dependencies = self.dependencies(table, "dev-dependencies", path);
        let native = self.native(table, "native", path);
        Some(PlatformSection {
            when: when?,
            source_dirs,
            test_source_dirs,
            dependencies,
            dev_dependencies,
            native,
        })
    }

    fn dependencies(
        &mut self,
        table: &dyn TableLike,
        key: &str,
        parent: &str,
    ) -> BTreeMap<DependencyAlias, Dependency> {
        let Some(table) = self.table(table, key, parent, false) else {
            return BTreeMap::new();
        };
        let path = field(parent, key);
        let keys: Vec<_> = table.iter().map(|(key, _)| key).collect();
        self.fields(table, &path, &keys);
        let mut dependencies = BTreeMap::new();
        for (key, _) in table.iter() {
            let location = field(&path, key);
            let alias = self.convert(
                Some(key.to_owned()),
                &location,
                INVALID_MANIFEST,
                DependencyAlias::parse,
            );
            let dependency = self
                .table(table, key, &path, true)
                .and_then(|table| self.dependency(table, &location));
            if let (Some(alias), Some(dependency)) = (alias, dependency) {
                dependencies.insert(alias, dependency);
            }
        }
        dependencies
    }

    fn git_package(&mut self, table: &dyn TableLike, path: &str) -> Option<GitPackage> {
        let repository = self.string(table, "git", path, true);
        let repository =
            self.convert(repository, &field(path, "git"), INVALID_SOURCE, GitRepository::parse);
        let subdir = self.string(table, "subdir", path, false).unwrap_or_else(|| ".".to_owned());
        let subdir =
            self.convert(Some(subdir), &field(path, "subdir"), INVALID_SOURCE, PackagePath::parse);
        Some(GitPackage { repository: repository?, subdir: subdir? })
    }

    fn dependency(&mut self, table: &dyn TableLike, path: &str) -> Option<Dependency> {
        self.fields(table, path, &["git", "subdir", "version", "branch", "rev", "path"]);
        let requirement = self.string(table, "version", path, false);
        let requirement = self.convert(
            requirement,
            &field(path, "version"),
            INVALID_REQUIREMENT,
            VersionRequirement::parse,
        );
        if table.contains_key("path") {
            for key in ["git", "subdir", "branch", "rev"] {
                if table.contains_key(key) {
                    self.bad(
                        INVALID_SOURCE,
                        &field(path, key),
                        format!("`{key}` cannot be combined with a path dependency"),
                    );
                }
            }
            let local = self.string(table, "path", path, true)?;
            if local.is_empty() || local.chars().any(char::is_control) {
                self.bad(
                    INVALID_SOURCE,
                    &field(path, "path"),
                    "a local package path must be nonempty and contain no control characters",
                );
            }
            return Some(Dependency::Path { path: local, version: requirement });
        }
        let package = self.git_package(table, path);
        let count =
            ["version", "branch", "rev"].iter().filter(|key| table.contains_key(key)).count();
        if count != 1 {
            self.bad(
                INVALID_SOURCE,
                path,
                "a Git dependency must choose exactly one of version, branch, or rev",
            );
        }
        let branch = self.string(table, "branch", path, false);
        let branch = self.convert(branch, &field(path, "branch"), INVALID_SOURCE, GitBranch::parse);
        let revision = self.string(table, "rev", path, false);
        let revision =
            self.convert(revision, &field(path, "rev"), INVALID_SOURCE, GitRevision::parse);
        let selector = requirement
            .map(GitSelector::Version)
            .or_else(|| branch.map(GitSelector::Branch))
            .or_else(|| revision.map(GitSelector::Revision));
        Some(Dependency::Git { package: package?, selector: selector? })
    }

    fn overrides(&mut self, table: &dyn TableLike) -> Vec<Override> {
        self.tables(table, "overrides", "")
            .into_iter()
            .filter_map(|(path, table)| {
                self.fields(table, &path, &["replace", "with", "members"]);
                let replace = self.table(table, "replace", &path, true).and_then(|table| {
                    let path = field(&path, "replace");
                    self.fields(table, &path, &["git", "subdir"]);
                    self.git_package(table, &path)
                });
                let with = self
                    .table(table, "with", &path, true)
                    .and_then(|table| self.dependency(table, &field(&path, "with")));
                let members = table.contains_key("members").then(|| {
                    self.paths(table, "members", &path).into_iter().collect::<BTreeSet<_>>()
                });
                if members.as_ref().is_some_and(BTreeSet::is_empty) {
                    self.bad(
                        INVALID_MANIFEST,
                        &field(&path, "members"),
                        "an explicit override member scope must not be empty",
                    );
                }
                let value = Override { replace: replace?, with: with?, members };
                self.override_paths.push(path);
                Some(value)
            })
            .collect()
    }

    fn validate_overrides(&mut self, overrides: &[Override]) {
        #[derive(Default)]
        struct Occupied<'a> {
            first: Option<usize>,
            all: Option<usize>,
            members: BTreeMap<&'a PackagePath, usize>,
        }
        let mut occupied: BTreeMap<&GitPackage, Occupied<'_>> = BTreeMap::new();
        for (index, current) in overrides.iter().enumerate() {
            let scope = occupied.entry(&current.replace).or_default();
            let earlier = match &current.members {
                None => scope.first,
                Some(members) => scope
                    .all
                    .into_iter()
                    .chain(members.iter().filter_map(|member| scope.members.get(member).copied()))
                    .min(),
            };
            scope.first.get_or_insert(index);
            match &current.members {
                None => {
                    scope.all.get_or_insert(index);
                }
                Some(members) => {
                    for member in members {
                        scope.members.entry(member).or_insert(index);
                    }
                }
            }
            if let Some(earlier) = earlier {
                self.diagnostics.push(
                    Diagnostic::error(
                        OVERLAPPING_OVERRIDES,
                        "overrides for this package have overlapping member scopes",
                        self.at(&field(&self.override_paths[index], "replace")),
                    )
                    .with_label(Label::new(
                        self.at(&field(&self.override_paths[earlier], "replace")),
                        "earlier replacement",
                    )),
                );
            }
        }
    }

    fn validate_aliases(&mut self, manifest: &Manifest) {
        let mut bindings = Vec::new();
        for (kind, dependencies) in [
            ("dependencies", &manifest.dependencies),
            ("dev-dependencies", &manifest.dev_dependencies),
        ] {
            for alias in dependencies.keys() {
                bindings.push((alias, None, field(kind, alias.as_str())));
            }
        }
        for (index, platform) in manifest.platforms.iter().enumerate() {
            for (kind, dependencies) in [
                ("dependencies", &platform.dependencies),
                ("dev-dependencies", &platform.dev_dependencies),
            ] {
                for alias in dependencies.keys() {
                    bindings.push((
                        alias,
                        Some(&platform.when),
                        format!("{}.{kind}.{alias}", self.platform_paths[index]),
                    ));
                }
            }
        }
        let mut previous: BTreeMap<_, Vec<ConditionalLocation<'_>>> = BTreeMap::new();
        for (alias, condition, path) in &bindings {
            let earlier_bindings = previous.entry(alias).or_default();
            if let Some(earlier) = earlier_bindings
                .iter()
                .find(|earlier| conditions_overlap(*condition, earlier.condition))
            {
                self.diagnostics.push(
                    Diagnostic::error(
                        DUPLICATE_ALIAS,
                        format!("dependency alias `{alias}` can be active more than once"),
                        self.at(path),
                    )
                    .with_label(Label::new(self.at(earlier.path), "other active declaration")),
                );
            }
            earlier_bindings.push(ConditionalLocation { condition: *condition, path });
        }
    }

    fn validate_targets(&mut self, manifest: &Manifest) {
        let mut names = BTreeMap::new();
        for (index, binary) in manifest.binaries.iter().enumerate() {
            if let Some(earlier) = names.insert(&binary.name, index) {
                self.diagnostics.push(
                    Diagnostic::error(
                        INVALID_TARGET,
                        format!("duplicate executable name `{}`", binary.name),
                        self.at(&field(&self.binary_paths[index], "name")),
                    )
                    .with_label(Label::new(
                        self.at(&field(&self.binary_paths[earlier], "name")),
                        "earlier executable",
                    )),
                );
            }
        }
        let mut roots = Vec::new();
        if let Some(package) = &manifest.package {
            roots.push((&package.source_dir, None, "package.source-dir".to_owned()));
            roots.push((&package.test_dir, None, "package.test-dir".to_owned()));
        }
        for (index, platform) in manifest.platforms.iter().enumerate() {
            for (field, paths) in [
                ("source-dirs", &platform.source_dirs),
                ("test-source-dirs", &platform.test_source_dirs),
            ] {
                for (root_index, root) in paths.iter().enumerate() {
                    let key = format!("{}.{field}", self.platform_paths[index]);
                    let origin = self
                        .path_origins
                        .get(&key)
                        .and_then(|paths| paths.get(root_index))
                        .cloned()
                        .unwrap_or(key);
                    roots.push((root, Some(&platform.when), origin));
                }
            }
        }
        let mut previous: BTreeMap<_, Vec<ConditionalLocation<'_>>> = BTreeMap::new();
        for (root, condition, path) in &roots {
            let earlier_roots = previous.entry(root).or_default();
            if let Some(earlier) = earlier_roots
                .iter()
                .find(|earlier| conditions_overlap(*condition, earlier.condition))
            {
                self.diagnostics.push(
                    Diagnostic::error(
                        INVALID_TARGET,
                        format!("source directory `{root}` has multiple active owners"),
                        self.at(path),
                    )
                    .with_label(Label::new(self.at(earlier.path), "other source root")),
                );
            }
            earlier_roots.push(ConditionalLocation { condition: *condition, path });
        }
    }
}

fn field(parent: &str, key: &str) -> String {
    if parent.is_empty() { key.to_owned() } else { format!("{parent}.{key}") }
}

fn conditions_overlap(a: Option<&PlatformPredicate>, b: Option<&PlatformPredicate>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.overlaps(b),
        _ => true,
    }
}
