//! Git fixtures are local repositories; no test depends on a hosted forge.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::{GitPackage, GitRepository, GitRevision, PackagePath};
use fai_package_store::GitStore;

struct Repository {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
    cache: Utf8PathBuf,
    package: GitPackage,
}

impl Repository {
    fn new() -> Self {
        Self::with_format("sha1")
    }

    fn with_format(format: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
        let root = base.join("repo with spaces");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", &format!("--object-format={format}")]);
        git(&root, &["config", "user.name", "Package Test"]);
        git(&root, &["config", "user.email", "packages@example.invalid"]);
        let package = GitPackage {
            repository: GitRepository::parse(
                url::Url::from_directory_path(&root).unwrap().as_str(),
            )
            .unwrap(),
            subdir: PackagePath::root(),
        };
        let repository = Self { _temp: temp, root, cache: base.join("cache"), package };
        repository.write(
            "fai.toml",
            "manifest-version = 1\n[package]\nname = 'sample'\nversion = '1.0.0'\n",
        );
        repository.write("src/Library.fai", "module Library\npublic value : Int\nlet value = 1\n");
        repository.commit();
        repository
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn commit(&self) -> GitRevision {
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "-q", "-m", "package snapshot"]);
        GitRevision::parse(git(&self.root, &["rev-parse", "HEAD"]).trim()).unwrap()
    }
    fn head(&self) -> GitRevision {
        GitRevision::parse(git(&self.root, &["rev-parse", "HEAD"]).trim()).unwrap()
    }
    fn store(&self, offline: bool) -> GitStore {
        GitStore::new(self.cache.clone(), offline)
    }
}

fn git(root: &Utf8Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn lightweight_and_annotated_tags_peel_to_commits() {
    let repository = Repository::new();
    git(&repository.root, &["tag", "v1.0.0"]);
    git(&repository.root, &["tag", "-a", "v1.1.0", "-m", "annotated"]);
    let inventory = repository.store(false).inventory(&repository.package).unwrap();
    assert_eq!(inventory.releases.len(), 2);
    assert!(inventory.releases.values().all(|revision| revision == &repository.head()));
}

#[test]
fn nested_package_versions_ignore_other_tag_namespaces() {
    let repository = Repository::new();
    git(&repository.root, &["tag", "v1.0.0"]);
    git(&repository.root, &["tag", "packages/nested/v2.0.0"]);
    let package = GitPackage {
        subdir: PackagePath::parse("packages/nested").unwrap(),
        ..repository.package.clone()
    };
    let inventory = repository.store(false).inventory(&package).unwrap();
    assert_eq!(inventory.releases.keys().map(ToString::to_string).collect::<Vec<_>>(), ["2.0.0"]);
}

#[test]
fn source_bytes_are_read_from_the_selected_commit() {
    let repository = Repository::new();
    let original = repository.head();
    repository.write("src/Library.fai", "module Library\nlet value = 2\n");
    repository.commit();
    let materialized =
        repository.store(false).materialize(&repository.package, &original, None).unwrap();
    assert!(
        std::fs::read_to_string(materialized.directory.join("src/Library.fai"))
            .unwrap()
            .contains("value = 1")
    );
}

#[test]
fn materialized_directories_use_the_canonical_native_path() {
    let repository = Repository::new();
    let source =
        repository.store(false).materialize(&repository.package, &repository.head(), None).unwrap();
    assert_eq!(source.directory, source.directory.canonicalize_utf8().unwrap());
}

#[cfg(unix)]
#[test]
fn cache_directory_aliases_do_not_change_returned_source_locations() {
    let repository = Repository::new();
    std::fs::create_dir(&repository.cache).unwrap();
    let alias = repository.cache.with_extension("alias");
    std::os::unix::fs::symlink(&repository.cache, &alias).unwrap();
    let source = GitStore::new(alias, false)
        .materialize(&repository.package, &repository.head(), None)
        .unwrap();
    assert!(source.directory.starts_with(repository.cache.canonicalize_utf8().unwrap()));
}

#[test]
fn frozen_materialization_works_after_the_remote_disappears() {
    let repository = Repository::new();
    let revision = repository.head();
    let prepared =
        repository.store(false).materialize(&repository.package, &revision, None).unwrap();
    std::fs::rename(&repository.root, repository.root.with_extension("moved")).unwrap();
    let restored = repository
        .store(true)
        .materialize(&repository.package, &revision, Some(&prepared.checksum))
        .unwrap();
    assert_eq!(restored.checksum, prepared.checksum);
}

#[test]
fn altered_cached_bytes_fail_offline_integrity_checks() {
    let repository = Repository::new();
    let revision = repository.head();
    let prepared =
        repository.store(false).materialize(&repository.package, &revision, None).unwrap();
    std::fs::write(prepared.directory.join("src/Library.fai"), "changed").unwrap();
    let error = repository
        .store(true)
        .materialize(&repository.package, &revision, Some(&prepared.checksum))
        .unwrap_err();
    assert_eq!(error.code, fai_package::SOURCE_INTEGRITY);
}

#[test]
fn an_observed_release_cannot_move_to_another_commit() {
    let repository = Repository::new();
    git(&repository.root, &["tag", "v1.0.0"]);
    let mut store = repository.store(false);
    let first = store.inventory(&repository.package).unwrap();
    let (version, original) = first.releases.first_key_value().unwrap();
    store.accept_release(&repository.package, version, original).unwrap();
    repository.write("src/Library.fai", "module Library\nlet value = 2\n");
    let changed = repository.commit();
    git(&repository.root, &["tag", "-f", "v1.0.0"]);
    let inventory = repository.store(false).inventory(&repository.package).unwrap();
    assert_eq!(inventory.releases[version], changed);
    assert_eq!(
        store.accept_release(&repository.package, version, &changed).unwrap_err().code,
        fai_package::RELEASE_MOVED
    );
}

#[test]
fn operation_ref_snapshots_do_not_mix_ref_movements() {
    let repository = Repository::new();
    git(&repository.root, &["tag", "v1.0.0"]);
    let mut store = repository.store(false);
    let original = store.inventory(&repository.package).unwrap();
    repository.write("src/Library.fai", "module Library\nlet value = 2\n");
    repository.commit();
    git(&repository.root, &["tag", "-f", "v1.0.0"]);
    assert_eq!(store.inventory(&repository.package).unwrap().releases, original.releases);
}

#[test]
fn an_uncached_offline_revision_never_falls_back_to_another_release() {
    let repository = Repository::new();
    let error = repository
        .store(true)
        .materialize(&repository.package, &repository.head(), None)
        .unwrap_err();
    assert_eq!(error.code, fai_package::OFFLINE_UNAVAILABLE);
}

#[test]
fn tag_object_ids_are_not_accepted_as_full_commit_selectors() {
    let repository = Repository::new();
    git(&repository.root, &["tag", "-a", "v1.0.0", "-m", "annotated"]);
    let tag = GitRevision::parse(git(&repository.root, &["rev-parse", "v1.0.0"]).trim()).unwrap();
    assert!(repository.store(false).manifest(&repository.package, &tag).is_err());
}

#[cfg(unix)]
#[test]
fn escaping_git_symlinks_are_rejected_before_materialization() {
    let repository = Repository::new();
    std::os::unix::fs::symlink("../../outside", repository.root.join("src/escape")).unwrap();
    let revision = repository.commit();
    assert!(repository.store(false).materialize(&repository.package, &revision, None).is_err());
}

#[test]
fn lfs_pointer_content_is_not_silently_treated_as_package_source() {
    let repository = Repository::new();
    repository.write(
        "large.bin",
        "version https://git-lfs.github.com/spec/v1\noid sha256:abcd\nsize 42\n",
    );
    let revision = repository.commit();
    assert!(repository.store(false).materialize(&repository.package, &revision, None).is_err());
}

#[test]
fn sha256_repositories_preserve_the_full_object_identity() {
    let repository = Repository::with_format("sha256");
    let revision = repository.head();
    assert_eq!(revision.as_str().len(), 64);
    assert!(repository.store(false).materialize(&repository.package, &revision, None).is_ok());
}

#[test]
fn gitlinks_are_not_silently_replaced_by_empty_directories() {
    let repository = Repository::new();
    let revision = repository.head();
    git(
        &repository.root,
        &["update-index", "--add", "--cacheinfo", &format!("160000,{revision},nested")],
    );
    git(&repository.root, &["commit", "-q", "-m", "gitlink"]);
    assert!(
        repository.store(false).materialize(&repository.package, &repository.head(), None).is_err()
    );
}

#[test]
fn required_online_refresh_failure_does_not_use_stale_refs() {
    let repository = Repository::new();
    git(&repository.root, &["tag", "v1.0.0"]);
    repository.store(false).inventory(&repository.package).unwrap();
    std::fs::rename(&repository.root, repository.root.with_extension("moved")).unwrap();
    assert!(repository.store(false).inventory(&repository.package).is_err());
    assert_eq!(repository.store(true).inventory(&repository.package).unwrap().releases.len(), 1);
}

#[test]
fn checksums_are_independent_of_cache_location_and_checkout_filters() {
    let repository = Repository::new();
    let revision = repository.head();
    let first = repository.store(false).materialize(&repository.package, &revision, None).unwrap();
    git(&repository.root, &["config", "core.autocrlf", "true"]);
    let second = GitStore::new(repository.cache.with_extension("other"), false)
        .materialize(&repository.package, &revision, None)
        .unwrap();
    assert_ne!(first.directory, second.directory);
    assert_eq!(first.checksum, second.checksum);
}

#[test]
fn concurrent_materializers_observe_only_complete_trees() {
    let repository = Repository::new();
    let revision = repository.head();
    let (first, second) = std::thread::scope(|scope| {
        let first = scope
            .spawn(|| repository.store(false).materialize(&repository.package, &revision, None));
        let second = scope
            .spawn(|| repository.store(false).materialize(&repository.package, &revision, None));
        (first.join().unwrap().unwrap(), second.join().unwrap().unwrap())
    });
    assert_eq!(first.checksum, second.checksum);
    assert_eq!(first.directory, second.directory);
}

#[test]
fn offline_metadata_rejects_partial_clone_fetch_configuration() {
    let repository = Repository::new();
    repository.store(false).inventory(&repository.package).unwrap();
    let mirror = std::fs::read_dir(repository.cache.join("git"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let mirror = Utf8Path::from_path(&mirror).unwrap();
    git(mirror, &["config", "remote.origin.promisor", "true"]);
    let error = repository.store(true).inventory(&repository.package).unwrap_err();
    assert_eq!(error.code, fai_package::OFFLINE_UNAVAILABLE);
}

#[cfg(unix)]
#[test]
fn contained_symbolic_links_retain_their_own_tree_identity() {
    let repository = Repository::new();
    std::os::unix::fs::symlink("Library.fai", repository.root.join("src/Linked.fai")).unwrap();
    let revision = repository.commit();
    let source = repository.store(false).materialize(&repository.package, &revision, None).unwrap();
    assert!(
        std::fs::symlink_metadata(source.directory.join("src/Linked.fai"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        repository
            .store(true)
            .materialize(&repository.package, &revision, Some(&source.checksum))
            .unwrap()
            .checksum,
        source.checksum
    );
}
