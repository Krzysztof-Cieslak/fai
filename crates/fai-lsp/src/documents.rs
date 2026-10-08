//! Read-only, content-addressed source files for embedded-library navigation.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fai_db::{Db, SourceFile};
use lsp_types::Url;

pub(crate) struct StandardDocuments {
    by_path: HashMap<String, Url>,
    by_uri: HashMap<Url, SourceFile>,
}

impl StandardDocuments {
    pub(crate) fn new(db: &dyn Db) -> io::Result<Self> {
        let cache = std::env::var_os("FAI_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("fai"));
        Self::in_cache(db, &cache)
    }

    fn in_cache(db: &dyn Db, cache: &Path) -> io::Result<Self> {
        let mut files: Vec<_> =
            db.all_source_files().into_iter().filter(|file| file.is_std(db)).collect();
        files.sort_by(|a, b| a.path(db).cmp(b.path(db)));
        let mut hash = blake3::Hasher::new();
        for file in &files {
            hash.update(&(file.path(db).len() as u64).to_le_bytes());
            hash.update(file.path(db).as_bytes());
            hash.update(&(file.text(db).len() as u64).to_le_bytes());
            hash.update(file.text(db).as_bytes());
        }
        // Stay outside workspace source scans even if the configured cache is
        // under the workspace root: hidden directories are excluded by the loader.
        let directory = cache.join(".lsp-std-sources").join(hash.finalize().to_hex().as_str());
        let mut by_path = HashMap::new();
        let mut by_uri = HashMap::new();
        for file in files {
            let relative = file.path(db).strip_prefix(fai_db::STD_PATH_PREFIX).expect("std path");
            if Path::new(relative)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err(io::Error::other("invalid embedded source path"));
            }
            let path = directory.join(relative);
            materialize(&path, file.text(db).as_bytes())?;
            let uri = Url::from_file_path(std::fs::canonicalize(path)?)
                .map_err(|()| io::Error::other("cannot represent embedded source as a file URI"))?;
            by_path.insert(file.path(db).clone(), uri.clone());
            by_uri.insert(uri, file);
        }
        Ok(Self { by_path, by_uri })
    }

    pub(crate) fn uri(&self, path: &str) -> Option<&Url> {
        self.by_path.get(path)
    }

    pub(crate) fn file(&self, uri: &Url) -> Option<SourceFile> {
        self.by_uri.get(uri).copied()
    }
}

fn materialize(path: &Path, text: &[u8]) -> io::Result<()> {
    if std::fs::read(path).is_ok_and(|existing| existing == text) {
        return mark_read_only(path);
    }
    let parent =
        path.parent().ok_or_else(|| io::Error::other("source cache path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(text)?;
    #[cfg(windows)]
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions)?;
    }
    let file = match temporary.persist(path) {
        Ok(file) => file,
        // Another server may have published the same immutable file first.
        Err(error) if std::fs::read(path).is_ok_and(|existing| existing == text) => {
            drop(error);
            return mark_read_only(path);
        }
        Err(error) => return Err(error.error),
    };
    drop(file);
    mark_read_only(path)
}

fn mark_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_content_changes_use_a_distinct_readable_location() {
        let cache = tempfile::tempdir().unwrap();
        let mut db = fai_db::FaiDatabase::new();
        db.add_source_with_origin(
            "<std>/Lib.fai".into(),
            "module Lib\nlet value = 1\n".into(),
            fai_db::SourceOrigin::StandardLibrary,
        );
        let first = StandardDocuments::in_cache(&db, cache.path()).unwrap();
        let first_uri = first.uri("<std>/Lib.fai").unwrap();
        db.add_source_with_origin(
            "<std>/Lib.fai".into(),
            "module Lib\nlet value = 2\n".into(),
            fai_db::SourceOrigin::StandardLibrary,
        );
        let second = StandardDocuments::in_cache(&db, cache.path()).unwrap();
        let second_uri = second.uri("<std>/Lib.fai").unwrap();
        assert_ne!(first_uri, second_uri);
        assert_eq!(
            std::fs::read_to_string(first_uri.to_file_path().unwrap()).unwrap(),
            "module Lib\nlet value = 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(second_uri.to_file_path().unwrap()).unwrap(),
            "module Lib\nlet value = 2\n"
        );
        assert!(
            std::fs::metadata(second_uri.to_file_path().unwrap()).unwrap().permissions().readonly()
        );
    }

    #[test]
    fn concurrent_publication_keeps_complete_immutable_content() {
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("Module.fai");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    materialize(&path, b"module Module\nlet value = 1\n").unwrap()
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"module Module\nlet value = 1\n");
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    }
}
