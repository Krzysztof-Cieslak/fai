//! The workspace session: a query database plus the disk-sync bookkeeping that
//! keeps it current.
//!
//! A [`Session`] owns a [`FaiDatabase`] populated from the `.fai` files under a
//! workspace root. The one-shot CLI builds a session per invocation; the daemon
//! keeps one alive and calls [`Session::sync_from_disk`] (or
//! [`Session::apply_dirty`]) before each request so the warm database tracks the
//! filesystem. Change detection is stat-gated and hash-confirmed, so an unchanged
//! file (or a `touch` that doesn't change content) never bumps the salsa
//! revision — preserving early cutoff.

use std::time::SystemTime;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use camino::{Utf8Path, Utf8PathBuf};
use fai_db::{Db, DbSpanResolver, FaiDatabase, PackageId, SourceFile, SourceOrigin, SourceRole};
use fai_span::SourceId;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::DriverError;
use crate::command::DirtyFile;

/// A cached file stat: enough to skip re-reading an unchanged file.
#[derive(Clone, Copy)]
struct FileStat {
    mtime: Option<SystemTime>,
    len: u64,
    hash: [u8; 32],
}

/// A workspace root, its populated query database, and disk-sync bookkeeping.
pub struct Session {
    db: FaiDatabase,
    root: Utf8PathBuf,
    /// Per-file stats keyed by workspace-relative path (user files only).
    stats: FxHashMap<Utf8PathBuf, FileStat>,
    /// The active user files from the latest disk sync or dirty overlay.
    live: FxHashSet<SourceId>,
    /// Editor-owned inputs that disk scans must not replace or deactivate.
    overlays: FxHashSet<Utf8PathBuf>,
    overlay_contents: FxHashMap<Utf8PathBuf, String>,
    /// All package inputs, including dependencies excluded from user selection.
    loaded: FxHashSet<SourceId>,
    project: Option<Arc<LiveProject>>,
    options: crate::ProjectOptions,
}

struct LiveProject {
    local: bool,
    native: BTreeMap<PackageId, crate::NativeDeps>,
    entries: Vec<(String, SourceId)>,
    default_entry: Option<SourceId>,
    read_only: FxHashSet<SourceId>,
    notices: Vec<String>,
}

/// Fully prepared project metadata, produced without holding a compiler database lock.
pub struct PreparedProjectUpdate {
    root: Utf8PathBuf,
    options: crate::ProjectOptions,
    project: Option<crate::project::PreparedProject>,
    dirty: Vec<DirtyFile>,
}

impl Session {
    /// Opens a session rooted at `root`, loading every `.fai` source beneath it.
    ///
    /// Hidden directories and `target` are skipped. Files are loaded with paths
    /// relative to `root`, so diagnostics report workspace-relative locations.
    pub fn open(root: Utf8PathBuf) -> Result<Self, DriverError> {
        Self::open_with_local(root, true)
    }

    /// Opens a project with explicit control over personal root overrides.
    pub fn open_with_local(root: Utf8PathBuf, use_local: bool) -> Result<Self, DriverError> {
        Self::open_with_options(
            root,
            crate::ProjectOptions { no_local: !use_local, ..crate::ProjectOptions::default() },
        )
    }

    /// Opens a session under the same lock and network policy used by package commands.
    pub fn open_with_options(
        root: Utf8PathBuf,
        options: crate::ProjectOptions,
    ) -> Result<Self, DriverError> {
        Self::open_prepared(Self::prepare_update(root, &[], options)?)
    }

    /// Performs dependency restoration before taking a semantic database write lock.
    pub fn prepare_update(
        root: Utf8PathBuf,
        dirty: &[DirtyFile],
        options: crate::ProjectOptions,
    ) -> Result<PreparedProjectUpdate, DriverError> {
        if !root.is_dir() {
            return Err(DriverError::NotADirectory(root));
        }
        let root =
            root.canonicalize_utf8().map_err(|source| DriverError::Io { path: root, source })?;
        let overlays: Vec<_> = dirty
            .iter()
            .filter(|entry| entry.content.is_some())
            .map(|entry| root.join(&entry.path))
            .collect();
        let project = crate::project::prepare_with_options(&root, &overlays, &options)?;
        Ok(PreparedProjectUpdate { root, options, project, dirty: dirty.to_vec() })
    }

    /// Creates a database from an already restored graph snapshot.
    pub fn open_prepared(update: PreparedProjectUpdate) -> Result<Self, DriverError> {
        let mut db = FaiDatabase::new();
        // The embedded standard library is loaded first as high-durability
        // synthetic files, so their module names are reserved and rarely-changing.
        fai_types::std_lib::load_std(&mut db);
        let mut session = Self {
            db,
            root: update.root.clone(),
            stats: FxHashMap::default(),
            live: FxHashSet::default(),
            overlays: FxHashSet::default(),
            overlay_contents: FxHashMap::default(),
            loaded: FxHashSet::default(),
            project: None,
            options: update.options.clone(),
        };
        session.apply_prepared(update)?;
        Ok(session)
    }

    pub(crate) fn from_project(
        root: Utf8PathBuf,
        project: crate::project::PreparedProject,
    ) -> Result<Self, DriverError> {
        Self::open_prepared(PreparedProjectUpdate {
            root,
            options: crate::ProjectOptions { no_local: true, ..Default::default() },
            project: Some(project),
            dirty: Vec::new(),
        })
    }

    /// Publishes restored metadata after checking that its inputs are still current.
    pub fn apply_prepared(&mut self, update: PreparedProjectUpdate) -> Result<(), DriverError> {
        if self.root != update.root {
            return Err(DriverError::InvalidSelection(update.root));
        }
        self.options = update.options;
        if let Some(project) = update.project {
            self.apply_project(project, &update.dirty)?;
        } else {
            self.sync_directory()?;
            self.apply_directory_dirty(&update.dirty)?;
        }
        Ok(())
    }

    /// Re-scans the workspace and updates the database for files that actually
    /// changed (stat-gated, hash-confirmed). New files are added, deleted files
    /// are deactivated in semantic lookup and selection, and unchanged files are
    /// left untouched.
    pub fn sync_from_disk(&mut self) -> Result<(), DriverError> {
        self.sync_disk_changes().map(|_| ())
    }

    /// Refreshes explicitly changed workspace-relative paths, bypassing their
    /// stat gate because a watcher is authoritative even if timestamps match.
    pub fn refresh_disk_paths(&mut self, paths: &[Utf8PathBuf]) -> Result<bool, DriverError> {
        for path in paths {
            if !self.overlays.contains(path) {
                self.stats.remove(path);
            }
        }
        self.sync_disk_changes()
    }

    /// Synchronizes disk-backed inputs, preserving open editor overlays. Returns
    /// whether source content or active membership changed (not just file stats).
    pub fn sync_disk_changes(&mut self) -> Result<bool, DriverError> {
        if let Some(project) = self.prepare_project(&[])? {
            return self.apply_project(project, &[]);
        }
        self.sync_directory()
    }

    fn sync_directory(&mut self) -> Result<bool, DriverError> {
        if self.project.take().is_some() {
            self.db.remove_sources(self.loaded.iter().copied());
            self.loaded.clear();
            self.live.clear();
            self.stats.clear();
        }
        let mut present = Vec::new();
        collect_fai_files(&self.root, &mut present)?;
        present.sort();

        let mut live: FxHashSet<_> =
            self.overlays.iter().filter_map(|path| self.db.id_for_path(path)).collect();
        let mut seen_paths = self.overlays.clone();
        let mut content_changed = false;
        for absolute in present {
            let relative = absolute.strip_prefix(&self.root).unwrap_or(&absolute).to_owned();
            seen_paths.insert(relative.clone());
            if self.overlays.contains(&relative) {
                continue;
            }

            let metadata = std::fs::metadata(&absolute)
                .map_err(|source| DriverError::Io { path: absolute.clone(), source })?;
            let mtime = metadata.modified().ok();
            let len = metadata.len();

            // Stat gate: identical (mtime, len) ⇒ assume unchanged, reuse the id.
            if let Some(prev) = self.stats.get(&relative)
                && prev.mtime == mtime
                && prev.len == len
                && let Some(id) = self.db.id_for_path(&relative)
            {
                live.insert(id);
                continue;
            }

            let text = std::fs::read_to_string(&absolute)
                .map_err(|source| DriverError::Io { path: absolute.clone(), source })?;
            let hash = *blake3::hash(text.as_bytes()).as_bytes();

            // Hash confirm: only touch the input when the content really changed,
            // so a no-op mtime bump doesn't cascade recompute.
            let changed = self.stats.get(&relative).is_none_or(|prev| prev.hash != hash);
            let id = if changed {
                if let Some(id) = self.db.id_for_path(&relative)
                    && self.db.source_file(id).is_some_and(|file| file.text(&self.db) == &text)
                {
                    id
                } else {
                    content_changed = true;
                    self.db.add_source(relative.clone(), text)
                }
            } else {
                self.db.id_for_path(&relative).expect("known path has an id")
            };
            self.stats.insert(relative, FileStat { mtime, len, hash });
            live.insert(id);
        }

        // Remove disappeared files from semantic lookup as well as selection.
        // Their historical inputs remain available for stable-id reactivation.
        content_changed |= self.live != live;
        self.db.remove_sources(self.live.difference(&live).copied());
        self.stats.retain(|path, _| seen_paths.contains(path));
        self.live = live;
        self.loaded = self.live.clone();
        Ok(content_changed)
    }

    fn prepare_project(
        &self,
        dirty: &[DirtyFile],
    ) -> Result<Option<crate::project::PreparedProject>, DriverError> {
        let base = if self.root.is_absolute() {
            self.root.clone()
        } else {
            let current = std::env::current_dir()
                .map_err(|source| DriverError::Io { path: self.root.clone(), source })?;
            Utf8PathBuf::from_path_buf(current)
                .map_err(|path| DriverError::NonUtf8Path(path.to_string_lossy().into_owned()))?
                .join(&self.root)
        };
        let mut overlays: Vec<_> = self.overlays.iter().map(|path| base.join(path)).collect();
        overlays.extend(
            dirty
                .iter()
                .filter(|entry| entry.content.is_some())
                .map(|entry| base.join(&entry.path)),
        );
        crate::project::prepare_with_options(&self.root, &overlays, &self.options)
    }

    fn apply_project(
        &mut self,
        project: crate::project::PreparedProject,
        dirty: &[DirtyFile],
    ) -> Result<bool, DriverError> {
        for (path, expected) in &project.observed {
            let bytes = std::fs::read(path)
                .map_err(|source| DriverError::Io { path: path.clone(), source })?;
            if fai_package_store::FileEdit::fingerprint(&bytes) != *expected {
                return Err(fai_package::ProviderFailure {
                    code: fai_package::INPUTS_CHANGED,
                    message: "project inputs changed before compiler snapshot publication".into(),
                }
                .into());
            }
        }
        let previous: BTreeMap<_, _> = self
            .loaded
            .iter()
            .filter_map(|id| self.db.source_file(*id))
            .map(|file| (Utf8PathBuf::from(file.path(&self.db)), file))
            .collect();
        let mut contents: BTreeMap<Utf8PathBuf, (String, FileStat)> = BTreeMap::new();
        for source in &project.sources {
            let relative =
                source.path.strip_prefix(&project.base).unwrap_or(&source.path).to_owned();
            if source.read_only
                && (self.overlay_contents.contains_key(&relative)
                    || dirty.iter().any(|entry| {
                        project.base.join(&entry.path) == source.path && entry.content.is_some()
                    }))
            {
                return Err(DriverError::InvalidSelection(source.path.clone()));
            }
            if contents.contains_key(&relative) {
                continue;
            }
            let metadata = std::fs::metadata(&source.path).ok();
            let mtime = metadata.as_ref().and_then(|metadata| metadata.modified().ok());
            let len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
            let supplied = self
                .overlay_contents
                .get(&relative)
                .cloned()
                .or_else(|| {
                    self.overlays
                        .contains(&relative)
                        .then(|| previous.get(&relative).map(|file| file.text(&self.db).clone()))
                        .flatten()
                })
                .or_else(|| {
                    dirty
                        .iter()
                        .find(|entry| project.base.join(&entry.path) == source.path)
                        .and_then(|entry| entry.content.clone())
                });
            let cached = supplied.or_else(|| {
                if dirty.iter().any(|entry| project.base.join(&entry.path) == source.path) {
                    return None;
                }
                self.stats
                    .get(&relative)
                    .filter(|previous| previous.mtime == mtime && previous.len == len)?;
                previous.get(&relative).map(|file| file.text(&self.db).clone())
            });
            let text = match cached {
                Some(text) => text,
                None => std::fs::read_to_string(&source.path).map_err(|source_error| {
                    DriverError::Io { path: source.path.clone(), source: source_error }
                })?,
            };
            let hash = *blake3::hash(text.as_bytes()).as_bytes();
            contents.insert(relative, (text, FileStat { mtime, len, hash }));
        }
        let ids: BTreeMap<_, _> = project
            .packages
            .keys()
            .map(|key| (key.clone(), self.db.register_package(key.clone())))
            .collect();
        let mut changed =
            self.project.as_ref().is_none_or(|previous| previous.local != project.local);
        let mut native = BTreeMap::new();
        for (key, package) in &project.packages {
            let normal =
                package.dependencies.iter().map(|(alias, key)| (alias.clone(), ids[key])).collect();
            let development =
                package.development.iter().map(|(alias, key)| (alias.clone(), ids[key])).collect();
            changed |= self.db.set_package_dependencies(ids[key], normal, development);
            native.insert(ids[key], package.native.clone());
        }
        let mut loaded = FxHashSet::default();
        let mut live = FxHashSet::default();
        let mut read_only = FxHashSet::default();
        for source in &project.sources {
            let relative =
                source.path.strip_prefix(&project.base).unwrap_or(&source.path).to_owned();
            let origin = SourceOrigin::Package(ids[&source.package]);
            let text = &contents[&relative].0;
            let existing = self.db.id_for_scoped_path(&relative, origin, &source.role);
            let id = if let Some(id) = existing.filter(|id| {
                self.db.source_file(*id).is_some_and(|file| file.text(&self.db) == text)
            }) {
                id
            } else {
                changed = true;
                self.db.add_source_scoped(relative, text.clone(), origin, source.role.clone())
            };
            loaded.insert(id);
            if source.read_only {
                read_only.insert(id);
            }
            if source.selected {
                live.insert(id);
            }
        }
        changed |= self.loaded != loaded || self.live != live;
        self.db.remove_sources(self.loaded.difference(&loaded).copied());
        self.loaded = loaded;
        self.live = live;
        self.stats = contents.into_iter().map(|(path, (_, stat))| (path, stat)).collect();
        let mut entries = Vec::new();
        let mut default_entry = None;
        for entry in &project.entries {
            let path = entry.path.strip_prefix(&project.base).unwrap_or(&entry.path);
            if let Some(id) = self.db.id_for_scoped_path(
                path,
                SourceOrigin::Package(ids[&entry.package]),
                &SourceRole::Executable(entry.name.clone()),
            ) {
                if project
                    .default_entry
                    .as_ref()
                    .is_some_and(|(package, name)| package == &entry.package && name == &entry.name)
                {
                    default_entry = Some(id);
                }
                entries.push((entry.name.clone(), id));
            }
        }
        self.project = Some(Arc::new(LiveProject {
            local: project.local,
            native,
            entries,
            default_entry,
            read_only,
            notices: project.notices,
        }));
        Ok(changed)
    }

    /// Gives the editor ownership of one source until [`Self::clear_overlay`].
    /// Its text and active membership survive external edits and deletion.
    pub fn set_overlay(&mut self, path: &Utf8Path, text: String) -> Result<(), DriverError> {
        if self.analysis_files(Some(path)).iter().any(|&file| self.is_read_only(file)) {
            return Err(DriverError::InvalidSelection(path.to_owned()));
        }
        if self.project.is_some() {
            let previous = self.overlay_contents.insert(path.to_owned(), text.clone());
            let was_open = !self.overlays.insert(path.to_owned());
            if let Err(error) = self.apply_dirty(&[DirtyFile {
                path: path.to_string(),
                hash: None,
                content: Some(text),
            }]) {
                if !was_open {
                    self.overlays.remove(path);
                }
                if let Some(previous) = previous {
                    self.overlay_contents.insert(path.to_owned(), previous);
                } else {
                    self.overlay_contents.remove(path);
                }
                return Err(error);
            }
            return Ok(());
        }
        self.apply_dirty(&[DirtyFile { path: path.to_string(), hash: None, content: Some(text) }])?;
        self.overlays.insert(path.to_owned());
        if let Some(id) = self.db.id_for_path(path)
            && let Some(file) = self.db.source_file(id)
        {
            self.overlay_contents.insert(path.to_owned(), file.text(&self.db).clone());
        }
        Ok(())
    }

    /// Returns an editor-owned input to the filesystem, including deactivation
    /// when its on-disk file has been deleted.
    pub fn clear_overlay(&mut self, path: &Utf8Path) -> Result<bool, DriverError> {
        self.overlays.remove(path);
        self.overlay_contents.remove(path);
        self.stats.remove(path);
        self.sync_disk_changes()
    }

    /// Applies a client-supplied dirty-set as a fast path: each entry's content
    /// (inline, or re-read from disk) is hashed and the input updated if changed.
    pub fn apply_dirty(&mut self, dirty: &[DirtyFile]) -> Result<(), DriverError> {
        if let Some(project) = self.prepare_project(dirty)? {
            self.apply_project(project, dirty)?;
            return Ok(());
        }
        self.apply_directory_dirty(dirty)
    }

    fn apply_directory_dirty(&mut self, dirty: &[DirtyFile]) -> Result<(), DriverError> {
        for entry in dirty {
            let relative = Utf8PathBuf::from(&entry.path);
            let absolute = self.root.join(&relative);
            let text = match &entry.content {
                Some(content) => content.clone(),
                None => std::fs::read_to_string(&absolute)
                    .map_err(|source| DriverError::Io { path: absolute.clone(), source })?,
            };
            let hash = *blake3::hash(text.as_bytes()).as_bytes();
            let changed = self.stats.get(&relative).is_none_or(|prev| prev.hash != hash);
            let id = if changed {
                self.db.add_source(relative.clone(), text)
            } else {
                self.db.id_for_path(&relative).expect("known path has an id")
            };
            let metadata = std::fs::metadata(&absolute).ok();
            let mtime = metadata.as_ref().and_then(|m| m.modified().ok());
            let len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
            self.stats.insert(relative, FileStat { mtime, len, hash });
            self.live.insert(id);
            self.loaded.insert(id);
        }
        Ok(())
    }

    /// The user-facing source files currently present (excludes the synthetic
    /// standard library and deleted files).
    #[must_use]
    pub fn user_files(&self) -> Vec<SourceFile> {
        let mut files: Vec<_> = self
            .db
            .all_source_files()
            .into_iter()
            .filter(|f| self.live.contains(&f.source(&self.db)))
            .filter(|f| !f.is_std(&self.db))
            .collect();
        files.sort_by(|a, b| a.path(&self.db).cmp(b.path(&self.db)));
        files
    }

    /// Sources available to IDE analysis, including materialized dependencies.
    /// An explicit path can select only files already owned by the loaded graph.
    #[must_use]
    pub fn analysis_files(&self, path: Option<&Utf8Path>) -> Vec<SourceFile> {
        let target = path.map(|path| {
            let path = self
                .selection_path(path)
                .map(|relative| self.root.join(relative))
                .unwrap_or_else(|| {
                    self.root
                        .join(path)
                        .canonicalize_utf8()
                        .unwrap_or_else(|_| self.root.join(path))
                });
            normalized_path(&path)
        });
        let mut files: Vec<_> = self
            .db
            .all_source_files()
            .into_iter()
            .filter(|file| self.loaded.contains(&file.source(&self.db)))
            .filter(|file| {
                target.as_ref().is_none_or(|target| {
                    normalized_path(&self.root.join(file.path(&self.db))) == *target
                })
            })
            .collect();
        files.sort_by_key(|file| {
            (
                !self.live.contains(&file.source(&self.db)),
                file.path(&self.db).clone(),
                file.source(&self.db),
            )
        });
        files
    }

    /// Bounds the in-memory native-object cache to `capacity` blobs (0 =
    /// unbounded). These `object_code` blobs are large and backed by the on-disk
    /// content-addressed cache, so the long-lived daemon caps them to keep its
    /// warm database's footprint bounded; the one-shot CLI leaves it unbounded.
    pub fn set_object_cache_capacity(&mut self, capacity: usize) {
        crate::backend::set_object_cache_capacity(&mut self.db, capacity);
    }

    /// A read-only snapshot of this session: an independent database handle that
    /// shares salsa's storage (and thus its memoization) with the original, plus
    /// a consistent copy of the live-file set.
    ///
    /// The daemon takes one of these per read request so distinct requests run
    /// concurrently on their own handles (salsa coordinates execution and cancels
    /// outstanding snapshots when an input is mutated). The snapshot is **read
    /// only**: it carries no file stats, so [`sync_from_disk`](Self::sync_from_disk)
    /// must never be called on it (it would treat every file as new). Mutate the
    /// authoritative session under exclusive access instead, then take a fresh
    /// snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Session {
        Session {
            db: self.db.clone(),
            root: self.root.clone(),
            // Stats drive disk-sync (a write); a read-only snapshot never syncs,
            // so it carries none.
            stats: FxHashMap::default(),
            live: self.live.clone(),
            overlays: self.overlays.clone(),
            overlay_contents: self.overlay_contents.clone(),
            loaded: self.loaded.clone(),
            project: self.project.clone(),
            options: self.options.clone(),
        }
    }

    /// The session's database as a trait object.
    #[must_use]
    pub fn db(&self) -> &dyn Db {
        &self.db
    }

    /// The workspace root.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Selects a declared default entry or an explicit source-file entry.
    pub fn entry_file(&self, path: &Utf8Path) -> Result<SourceFile, DriverError> {
        if path.as_str().is_empty() {
            let entry = self
                .project
                .as_ref()
                .and_then(|project| project.default_entry)
                .and_then(|id| self.db.source_file(id));
            return entry.ok_or_else(|| {
                DriverError::InvalidSelection(
                    "<default executable: select an entry file or configure default-run>".into(),
                )
            });
        }
        let files = self.select_files_checked(Some(path))?;
        let exact = self.selection_path(path);
        let matching: Vec<_> = files
            .into_iter()
            .filter(|file| exact.as_ref().is_some_and(|path| file.path(&self.db) == path.as_str()))
            .collect();
        match matching.as_slice() {
            [file] => Ok(*file),
            _ => Err(DriverError::InvalidSelection(path.to_owned())),
        }
    }

    /// The declared name for an executable source, if this is a package project.
    #[must_use]
    pub fn executable_name(&self, file: SourceFile) -> Option<&str> {
        self.project
            .as_ref()?
            .entries
            .iter()
            .find(|(_, id)| *id == file.source(&self.db))
            .map(|(name, _)| name.as_str())
    }

    /// Whether source ownership is supplied by versioned package manifests.
    #[must_use]
    pub fn is_package_project(&self) -> bool {
        self.project.is_some()
    }

    /// Whether edits require an explicit local replacement of the selected source.
    #[must_use]
    pub fn is_read_only(&self, file: SourceFile) -> bool {
        file.is_std(&self.db)
            || self
                .project
                .as_ref()
                .is_some_and(|project| project.read_only.contains(&file.source(&self.db)))
    }

    /// Whether personal root overrides are active in this package project.
    #[must_use]
    pub fn has_local_overrides(&self) -> bool {
        self.project.as_ref().is_some_and(|project| project.local)
    }

    /// Notices emitted while preparing this request's dependency graph.
    #[must_use]
    pub fn package_notices(&self) -> &[String] {
        self.project.as_ref().map_or(&[], |project| project.notices.as_slice())
    }

    /// Native requirements of a source's selected package dependency projection.
    pub fn native_dependencies(&self, file: SourceFile) -> Result<crate::NativeDeps, DriverError> {
        let Some(project) = &self.project else {
            return crate::read_native_manifest(&self.root).map_err(crate::project::message_error);
        };
        let SourceOrigin::Package(root) = file.origin(&self.db) else {
            return Ok(crate::NativeDeps::default());
        };
        let mut pending = vec![root];
        if matches!(file.role(&self.db), SourceRole::Test)
            && let Some(package) = fai_db::package_environment(&self.db, root)
        {
            pending.extend(package.development(&self.db).values().copied());
        }
        let mut edges = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if edges.contains_key(&id) {
                continue;
            }
            let Some(package) = fai_db::package_environment(&self.db, id) else {
                continue;
            };
            let children: BTreeSet<_> = package.dependencies(&self.db).values().copied().collect();
            pending.extend(children.iter().copied());
            edges.insert(id, children);
        }
        let mut indegrees: BTreeMap<_, usize> = edges.keys().copied().map(|id| (id, 0)).collect();
        for children in edges.values() {
            for child in children {
                *indegrees.entry(*child).or_default() += 1;
            }
        }
        let key = |id| {
            fai_db::package_environment(&self.db, id)
                .map(|package| package.key(&self.db).clone())
                .unwrap_or_default()
        };
        let mut ready: BTreeSet<_> = indegrees
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(&id, _)| (key(id), id))
            .collect();
        let mut result = crate::NativeDeps::default();
        while let Some((_, id)) = ready.pop_first() {
            if let Some(native) = project.native.get(&id) {
                result.append(native);
            }
            for child in &edges[&id] {
                if let Some(count) = indegrees.get_mut(child) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert((key(*child), *child));
                    }
                }
            }
        }
        Ok(result)
    }

    /// Production source validation plus selected root source scopes, without
    /// implicitly executing dependency contracts.
    #[must_use]
    pub fn validation_files(&self, selected: &[SourceFile]) -> Vec<SourceFile> {
        crate::dependency_validation_files(&self.db, selected)
    }

    /// A span resolver backed by this session's database.
    #[must_use]
    pub fn resolver(&self) -> DbSpanResolver<'_> {
        DbSpanResolver::new(&self.db)
    }

    /// Enables query-execution event recording on the underlying database (for
    /// diagnostics and the incremental guards).
    pub fn enable_event_log(&self) {
        self.db.enable_event_log();
    }

    /// Drains and returns the recorded query-execution events.
    #[must_use]
    pub fn take_events(&self) -> Vec<String> {
        self.db.take_events()
    }

    /// Selects the loaded source files under `path` (a file or directory,
    /// workspace-relative or absolute), or every file when `path` is `None`.
    #[must_use]
    pub fn select_files(&self, path: Option<&Utf8Path>) -> Vec<SourceFile> {
        let db = self.db();
        // Never select the synthetic standard library: a dependency, not user code.
        let files = self.user_files();
        let Some(path) = path else {
            return files;
        };
        let Some(target) = self.selection_path(path) else { return Vec::new() };
        files.into_iter().filter(|file| Utf8Path::new(file.path(db)).starts_with(&target)).collect()
    }

    /// Selects a command's explicit target, rejecting missing or outside-workspace
    /// paths. An existing empty directory is valid; an absent selection still
    /// means the whole workspace. Loaded editor overlays remain selectable.
    pub fn select_files_checked(
        &self,
        path: Option<&Utf8Path>,
    ) -> Result<Vec<SourceFile>, DriverError> {
        let files = self.select_files(path);
        let Some(path) = path else { return Ok(files) };
        let directory =
            self.selection_path(path).is_some_and(|relative| self.root.join(relative).is_dir());
        if !files.is_empty() || directory {
            Ok(files)
        } else {
            Err(DriverError::InvalidSelection(path.to_owned()))
        }
    }

    fn selection_path(&self, path: &Utf8Path) -> Option<Utf8PathBuf> {
        let root = if self.root.is_absolute() {
            self.root.clone()
        } else {
            Utf8PathBuf::from_path_buf(std::env::current_dir().ok()?).ok()?.join(&self.root)
        };
        let root = normalized_path(&root);
        let target = normalized_path(&root.join(path));
        target.strip_prefix(&root).ok().map(Utf8Path::to_owned).or_else(|| {
            target.canonicalize_utf8().ok()?.strip_prefix(&root).ok().map(Utf8Path::to_owned)
        })
    }
}

/// Lexical normalization preserves the workspace's symlink/overlay spelling
/// while making dot components and either platform's separators equivalent.
fn normalized_path(path: &Utf8Path) -> Utf8PathBuf {
    let mut normalized = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            camino::Utf8Component::CurDir => {}
            camino::Utf8Component::ParentDir => {
                if normalized.file_name().is_some_and(|name| name != "..") {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_str()),
        }
    }
    normalized
}

/// Recursively collects `.fai` files under `dir`, skipping hidden entries and
/// `target`.
fn collect_fai_files(dir: &Utf8Path, out: &mut Vec<Utf8PathBuf>) -> Result<(), DriverError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|source| DriverError::Io { path: dir.to_owned(), source })?;
    for entry in entries {
        let entry = entry.map_err(|source| DriverError::Io { path: dir.to_owned(), source })?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| DriverError::NonUtf8Path(p.to_string_lossy().into_owned()))?;
        let name = path.file_name().unwrap_or_default();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        let file_type =
            entry.file_type().map_err(|source| DriverError::Io { path: path.clone(), source })?;
        if file_type.is_dir() {
            collect_fai_files(&path, out)?;
        } else if path.extension() == Some("fai") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Session) {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        std::fs::write(root.join("Main.fai"), "module Main\nlet value = 1\n").unwrap();
        let session = Session::open(root).unwrap();
        (directory, session)
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let (_directory, session) = workspace();
        assert!(matches!(
            session.select_files_checked(Some(Utf8Path::new("missing.fai"))),
            Err(DriverError::InvalidSelection(_))
        ));
    }

    #[test]
    fn explicit_missing_directory_is_an_error() {
        let (_directory, session) = workspace();
        assert!(session.select_files_checked(Some(Utf8Path::new("missing/"))).is_err());
    }

    #[test]
    fn existing_empty_directory_is_a_valid_selection() {
        let (_directory, session) = workspace();
        std::fs::create_dir(session.root().join("empty")).unwrap();
        assert!(session.select_files_checked(Some(Utf8Path::new("./empty/"))).unwrap().is_empty());
    }

    #[test]
    fn dot_components_select_the_same_loaded_file() {
        let (_directory, session) = workspace();
        let direct = session.select_files_checked(Some(Utf8Path::new("Main.fai"))).unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(
            session.select_files_checked(Some(Utf8Path::new("./nested/../Main.fai"))).unwrap(),
            direct
        );
    }

    #[test]
    fn absolute_root_selects_all_loaded_files() {
        let (_directory, session) = workspace();
        assert_eq!(
            session.select_files_checked(Some(session.root())).unwrap(),
            session.user_files()
        );
        assert_eq!(session.select_files_checked(Some(Utf8Path::new("."))).unwrap().len(), 1);
    }

    #[test]
    fn absolute_file_selection_is_normalized() {
        let (_directory, session) = workspace();
        let path = session.root().join("nested/../Main.fai");
        assert_eq!(session.select_files_checked(Some(&path)).unwrap().len(), 1);
    }

    #[test]
    fn absolute_selection_uses_the_callers_native_root_spelling() {
        let (directory, session) = workspace();
        let path = Utf8Path::from_path(directory.path()).unwrap().join("Main.fai");
        assert_eq!(session.entry_file(&path).unwrap(), session.user_files()[0]);
    }

    #[cfg(unix)]
    #[test]
    fn absolute_selection_resolves_an_aliased_workspace_root() {
        let directory = tempfile::tempdir().unwrap();
        let base = Utf8Path::from_path(directory.path()).unwrap();
        std::fs::create_dir(base.join("real")).unwrap();
        std::fs::write(base.join("real/Main.fai"), "module Main\nlet value = 1\n").unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("alias")).unwrap();
        let session = Session::open(base.join("alias")).unwrap();
        assert_eq!(
            session.entry_file(&base.join("alias/Main.fai")).unwrap(),
            session.user_files()[0]
        );
        assert_eq!(
            session.analysis_files(Some(&base.join("alias/Main.fai"))),
            session.user_files()
        );
    }

    #[test]
    fn selections_cannot_escape_the_workspace() {
        let (_directory, session) = workspace();
        assert!(session.select_files_checked(Some(Utf8Path::new(".."))).is_err());
    }

    #[test]
    fn non_source_files_are_not_successful_empty_selections() {
        let (_directory, session) = workspace();
        std::fs::write(session.root().join("notes.txt"), "notes").unwrap();
        assert!(session.select_files_checked(Some(Utf8Path::new("notes.txt"))).is_err());
    }

    #[test]
    fn disk_sync_preserves_overlay_text_and_membership() {
        let (_directory, mut session) = workspace();
        let path = Utf8Path::new("Main.fai");
        let overlay = "module Main\nlet value = true\n";
        session.set_overlay(path, overlay.into()).unwrap();
        let file = session.user_files()[0];
        std::fs::remove_file(session.root().join(path)).unwrap();
        assert!(!session.sync_disk_changes().unwrap());
        assert_eq!(session.user_files(), vec![file]);
        assert_eq!(file.text(session.db()), overlay);
        assert!(session.clear_overlay(path).unwrap());
        assert!(session.user_files().is_empty());
    }

    #[test]
    fn closing_an_overlay_restores_disk_despite_unchanged_disk_stats() {
        let (_directory, mut session) = workspace();
        let path = Utf8Path::new("Main.fai");
        session.set_overlay(path, "module Main\nlet value = true\n".into()).unwrap();
        assert!(session.clear_overlay(path).unwrap());
        assert_eq!(session.user_files()[0].text(session.db()), "module Main\nlet value = 1\n");
    }

    #[test]
    fn no_op_disk_sync_does_not_invalidate_overlay_queries() {
        let (_directory, mut session) = workspace();
        session
            .set_overlay(Utf8Path::new("Main.fai"), "module Main\nlet value = true\n".into())
            .unwrap();
        let file = session.user_files()[0];
        let _ = fai_types::def_type(session.db(), file, fai_syntax::Symbol::intern("value"));
        assert!(!session.sync_disk_changes().unwrap());
        session.enable_event_log();
        assert!(!session.sync_disk_changes().unwrap());
        let _ = fai_types::def_type(session.db(), file, fai_syntax::Symbol::intern("value"));
        let events = session.take_events();
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn explicit_file_events_refresh_content_with_unchanged_metadata() {
        let (_directory, mut session) = workspace();
        let relative = Utf8PathBuf::from("Main.fai");
        let path = session.root().join(&relative);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let changed = "module Main\nlet value = 2\n";
        std::fs::write(&path, changed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(session.refresh_disk_paths(&[relative]).unwrap());
        assert_eq!(session.user_files()[0].text(session.db()), changed);
    }

    #[test]
    fn repeated_unchanged_events_keep_query_memoization() {
        let (_directory, mut session) = workspace();
        let paths = [Utf8PathBuf::from("Main.fai")];
        let file = session.user_files()[0];
        assert!(!session.refresh_disk_paths(&paths).unwrap());
        let _ = fai_types::def_type(session.db(), file, fai_syntax::Symbol::intern("value"));
        session.enable_event_log();
        assert!(!session.refresh_disk_paths(&paths).unwrap());
        let _ = fai_types::def_type(session.db(), file, fai_syntax::Symbol::intern("value"));
        let events = session.take_events();
        assert!(events.is_empty(), "{events:?}");
    }
}
