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

use camino::{Utf8Path, Utf8PathBuf};
use fai_db::{Db, DbSpanResolver, FaiDatabase, SourceFile};
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
    /// The source files currently present on disk (excludes deleted files whose
    /// salsa input lingers, and the synthetic standard library).
    live: FxHashSet<SourceId>,
}

impl Session {
    /// Opens a session rooted at `root`, loading every `.fai` source beneath it.
    ///
    /// Hidden directories and `target` are skipped. Files are loaded with paths
    /// relative to `root`, so diagnostics report workspace-relative locations.
    pub fn open(root: Utf8PathBuf) -> Result<Self, DriverError> {
        if !root.is_dir() {
            return Err(DriverError::NotADirectory(root));
        }
        let mut db = FaiDatabase::new();
        // The embedded standard library is loaded first as high-durability
        // synthetic files, so their module names are reserved and rarely-changing.
        fai_types::std_lib::load_std(&mut db);
        let mut session =
            Self { db, root, stats: FxHashMap::default(), live: FxHashSet::default() };
        session.sync_from_disk()?;
        Ok(session)
    }

    /// Re-scans the workspace and updates the database for files that actually
    /// changed (stat-gated, hash-confirmed). New files are added, deleted files
    /// are dropped from the live set, and unchanged files are left untouched.
    pub fn sync_from_disk(&mut self) -> Result<(), DriverError> {
        let mut present = Vec::new();
        collect_fai_files(&self.root, &mut present)?;
        present.sort();

        let mut live = FxHashSet::default();
        let mut seen_paths = FxHashSet::default();
        for absolute in present {
            let relative = absolute.strip_prefix(&self.root).unwrap_or(&absolute).to_owned();
            seen_paths.insert(relative.clone());

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
                self.db.add_source(relative.clone(), text)
            } else {
                self.db.id_for_path(&relative).expect("known path has an id")
            };
            self.stats.insert(relative, FileStat { mtime, len, hash });
            live.insert(id);
        }

        // Forget stats for files that disappeared (their salsa input lingers but
        // is excluded from the live set, so commands ignore it).
        self.stats.retain(|path, _| seen_paths.contains(path));
        self.live = live;
        Ok(())
    }

    /// Applies a client-supplied dirty-set as a fast path: each entry's content
    /// (inline, or re-read from disk) is hashed and the input updated if changed.
    pub fn apply_dirty(&mut self, dirty: &[DirtyFile]) -> Result<(), DriverError> {
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
        }
        Ok(())
    }

    /// The user-facing source files currently present (excludes the synthetic
    /// standard library and deleted files).
    #[must_use]
    pub fn user_files(&self) -> Vec<SourceFile> {
        self.db
            .all_source_files()
            .into_iter()
            .filter(|f| self.live.contains(&f.source(&self.db)))
            .filter(|f| !fai_db::is_std_path(f.path(&self.db)))
            .collect()
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
        let target = path.strip_prefix(&self.root).unwrap_or(path).as_str().trim_end_matches('/');
        files
            .into_iter()
            .filter(|file| {
                let rel = file.path(db).as_str();
                rel == target || rel.strip_prefix(target).is_some_and(|rest| rest.starts_with('/'))
            })
            .collect()
    }
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
