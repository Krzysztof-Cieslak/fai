//! The workspace session: builds and seeds a one-shot database.
//!
//! A [`Session`] owns a [`FaiDatabase`] populated from the `.fai` files under a
//! workspace root. The CLI (and, later, the daemon) drive command entry points
//! against the session's database; the daemon variant will keep the database
//! warm across requests instead of rebuilding it.

use camino::{Utf8Path, Utf8PathBuf};
use fai_db::{Db, DbSpanResolver, FaiDatabase, SourceFile};

use crate::DriverError;

/// A workspace root plus its populated query database.
pub struct Session {
    db: FaiDatabase,
    root: Utf8PathBuf,
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
        let mut sources = Vec::new();
        collect_fai_files(&root, &mut sources)?;
        sources.sort();
        for absolute in sources {
            let relative = absolute.strip_prefix(&root).unwrap_or(&absolute).to_owned();
            let text = std::fs::read_to_string(&absolute)
                .map_err(|source| DriverError::Io { path: absolute.clone(), source })?;
            db.add_source(relative, text);
        }
        Ok(Self { db, root })
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

    /// Selects the loaded source files under `path` (a file or directory,
    /// workspace-relative or absolute), or every file when `path` is `None`.
    #[must_use]
    pub fn select_files(&self, path: Option<&Utf8Path>) -> Vec<SourceFile> {
        let db = self.db();
        let files = db.all_source_files();
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
