//! Release plans, candidate validation, atomic pushes, and exact retry identities.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let remote = temp.path().join("remote.git");
        let cache = temp.path().join("cache");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Release Test"]);
        git(&root, &["config", "user.email", "release@example.invalid"]);
        git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
        let fixture = Self { _temp: temp, root, remote, cache };
        fixture.write(".gitignore", ".fai/\nfai.local.toml\nfai.local.lock\n");
        fixture.write("fai.toml", "manifest-version = 1\n[package]\nname = 'library'\nversion = '1.0.0'\nfai-version = '^0.1.0'\n");
        fixture.write("src/Library.fai", "module Library\npublic identity : Int -> Int\nlet identity value = value\nexample: identity 1 = 1\n");
        fixture.commit();
        git(&fixture.root, &["tag", "v1.0.0"]);
        git(&fixture.root, &["push", "-q", "-u", "origin", "main", "--tags"]);
        fixture
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn commit(&self) {
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "-q", "-m", "committed package code"]);
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fai"))
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("FAI_CACHE_DIR", &self.cache)
            .output()
            .unwrap()
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn plan_validates_candidate_without_changing_worktree_or_refs() {
    let fixture = Fixture::new();
    let head = git(&fixture.root, &["rev-parse", "HEAD"]);
    let manifest = std::fs::read(fixture.root.join("fai.toml")).unwrap();
    let tags = git(&fixture.root, &["tag"]);
    let result = fixture.run(&["publish", "minor", "--plan", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(output["status"], "planned");
    assert_eq!(output["packages"][0]["version"], "1.1.0");
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(fixture.root.join("fai.toml")).unwrap(), manifest);
    assert_eq!(git(&fixture.root, &["tag"]), tags);
    assert!(git(&fixture.root, &["status", "--porcelain"]).is_empty());
}

#[cfg(windows)]
#[test]
fn canonical_windows_paths_can_prepare_release_candidates() {
    let mut fixture = Fixture::new();
    fixture.root = fixture.root.canonicalize().unwrap();
    let result = fixture.run(&["publish", "patch", "--plan", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(output["status"], "planned");
    assert_eq!(output["packages"][0]["version"], "1.0.1");
}

#[test]
fn publish_prepares_versions_and_atomically_pushes_branch_and_tags() {
    let fixture = Fixture::new();
    let source = std::fs::read(fixture.root.join("src/Library.fai")).unwrap();
    let result = fixture.run(&["publish", "minor", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let commit = output["commit"].as_str().unwrap();
    assert_eq!(output["status"], "published");
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), commit);
    assert_eq!(git(&fixture.remote, &["rev-parse", "refs/heads/main"]), commit);
    assert_eq!(git(&fixture.remote, &["rev-parse", "refs/tags/v1.1.0"]), commit);
    assert_eq!(std::fs::read(fixture.root.join("src/Library.fai")).unwrap(), source);
    assert!(!fixture.root.join(".fai/publish.json").exists());
    assert!(git(&fixture.root, &["status", "--porcelain"]).is_empty());
}

#[test]
fn uncommitted_source_is_rejected_before_release_preparation() {
    let fixture = Fixture::new();
    fixture.write("src/Library.fai", "module Library\nlet changed = 2\n");
    let before = git(&fixture.root, &["rev-parse", "HEAD"]);
    let result = fixture.run(&["publish", "patch", "--message-format=json"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("FAI0821"));
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), before);
    assert!(!fixture.root.join(".fai/publish.json").exists());
}

#[test]
fn failed_atomic_push_can_resume_without_another_version_bump() {
    let fixture = Fixture::new();
    let base = git(&fixture.root, &["rev-parse", "HEAD"]);
    let tree = git(&fixture.root, &["rev-parse", "HEAD^{tree}"]);
    let other =
        git(&fixture.root, &["commit-tree", &tree, "-p", &base, "-m", "concurrent remote change"]);
    git(&fixture.root, &["push", "-q", "origin", &format!("{other}:refs/heads/race")]);
    git(&fixture.remote, &["update-ref", "refs/heads/main", &other]);
    let rejected = fixture.run(&["publish", "minor", "--message-format=json"]);
    assert!(!rejected.status.success(), "{rejected:?}");
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.root.join(".fai/publish.json")).unwrap())
            .unwrap();
    let prepared = receipt["output"]["commit"].as_str().unwrap();
    assert_eq!(git(&fixture.remote, &["tag"]), "v1.0.0");
    git(&fixture.remote, &["update-ref", "refs/heads/main", &base]);
    git(&fixture.root, &["tag", "-d", "v1.1.0"]);
    let resumed = fixture.run(&["publish", "--resume", "--message-format=json"]);
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(git(&fixture.remote, &["rev-parse", "refs/tags/v1.1.0"]), prepared);
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), prepared);
}

#[test]
fn a_batch_uses_planned_dependency_releases_and_preserves_authored_ranges() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "manifest-version = 1\n[workspace]\nmembers = ['json', 'web']\n");
    fixture.write("json/fai.toml", "manifest-version = 1\n[package]\nname = 'json'\nversion = '1.0.0'\nfai-version = '^0.1.0'\n");
    fixture.write("json/src/Json.fai", "module Json\npublic value : Int\nlet value = 42\n");
    let url = lsp_types::Url::from_file_path(&fixture.remote).unwrap();
    fixture.write("web/fai.toml", &format!("manifest-version = 1\n[package]\nname = 'web'\nversion = '1.0.0'\nfai-version = '^0.1.0'\n[dependencies]\njson = {{git = '{url}', subdir = 'json', version = '^1.1.0'}}\n"));
    fixture.write("web/src/Web.fai", "module Web\nimport json.Json\npublic value : Int\nlet value = Json.value\nexample: value = 42\n");
    fixture.commit();
    git(&fixture.root, &["push", "-q", "origin", "main"]);
    let result = fixture.run(&["publish", "json=minor", "web=patch", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        git(&fixture.remote, &["rev-parse", "refs/tags/json/v1.1.0"]),
        git(&fixture.remote, &["rev-parse", "refs/tags/web/v1.0.1"])
    );
    assert!(
        std::fs::read_to_string(fixture.root.join("web/fai.toml"))
            .unwrap()
            .contains("version = '^1.1.0'")
    );
}

#[test]
fn a_development_pin_cannot_mask_a_normal_consumer_failure() {
    let fixture = Fixture::new();
    let url = lsp_types::Url::from_file_path(&fixture.remote).unwrap();
    fixture.write(
        "dep/fai.toml",
        "manifest-version = 1\n[package]\nname = 'dep'\nversion = '1.0.0'\n",
    );
    fixture.write("dep/src/Dep.fai", "module Dep\npublic oldValue : Int\nlet oldValue = 1\n");
    fixture.commit();
    git(&fixture.root, &["tag", "dep/v1.0.0"]);
    fixture.write(
        "dep/fai.toml",
        "manifest-version = 1\n[package]\nname = 'dep'\nversion = '2.0.0'\n",
    );
    fixture.write("dep/src/Dep.fai", "module Dep\npublic newValue : Int\nlet newValue = 2\n");
    fixture.write("helper/fai.toml", &format!("manifest-version = 1\n[package]\nname = 'helper'\nversion = '1.0.0'\n[dependencies]\ndep = {{git = '{url}', subdir = 'dep', version = '^1.0.0'}}\n"));
    fixture.write("helper/src/Helper.fai", "module Helper\n");
    fixture.commit();
    git(&fixture.root, &["tag", "dep/v2.0.0"]);
    git(&fixture.root, &["tag", "helper/v1.0.0"]);
    fixture.write("fai.toml", &format!("manifest-version = 1\n[package]\nname = 'library'\nversion = '1.0.0'\nfai-version = '^0.1.0'\n[dependencies]\ndep = {{git = '{url}', subdir = 'dep', version = '*'}}\n[dev-dependencies]\nhelper = {{git = '{url}', subdir = 'helper', version = '*'}}\n"));
    fixture.write(
        "src/Library.fai",
        "module Library\nimport dep.Dep\npublic value : Int\nlet value = Dep.oldValue\n",
    );
    fixture.commit();
    git(&fixture.root, &["push", "-q", "origin", "main", "--tags"]);
    let development = fixture.run(&["check", "--no-examples", "--no-daemon"]);
    assert!(development.status.success(), "{development:?}");
    let base = git(&fixture.root, &["rev-parse", "HEAD"]);
    let publication = fixture.run(&["publish", "patch", "--message-format=json"]);
    assert!(!publication.status.success(), "{publication:?}");
    assert!(String::from_utf8_lossy(&publication.stdout).contains("oldValue"));
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), base);
}

#[test]
fn api_removals_are_reported_without_rejecting_explicit_patch_intent() {
    let fixture = Fixture::new();
    fixture.write(
        "src/Library.fai",
        "module Library\npublic replacement : Int -> Int\nlet replacement value = value\n",
    );
    fixture.commit();
    let result = fixture.run(&["publish", "patch", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(
        output["apiChanges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["name"] == "library:Library.identity"
                && change["kind"] == "removed")
    );
}

#[test]
fn publication_does_not_embed_a_future_commit_in_development_locks() {
    let fixture = Fixture::new();
    fixture
        .write("fai.lock", "an existing development record intentionally ignored by publication\n");
    fixture.commit();
    let before = std::fs::read(fixture.root.join("fai.lock")).unwrap();
    let result = fixture.run(&["publish", "patch", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(std::fs::read(fixture.root.join("fai.lock")).unwrap(), before);
    assert_eq!(
        git(&fixture.root, &["show", "HEAD:fai.lock"]).as_bytes(),
        before.strip_suffix(b"\n").unwrap()
    );
}

#[test]
fn publication_uses_committed_manifest_bytes_with_crlf_checkout_conversion() {
    let fixture = Fixture::new();
    git(&fixture.root, &["config", "core.autocrlf", "true"]);
    std::fs::remove_file(fixture.root.join("fai.toml")).unwrap();
    git(&fixture.root, &["checkout-index", "--force", "--", "fai.toml"]);
    git(&fixture.root, &["add", "--renormalize", "fai.toml"]);
    let manifest = std::fs::read_to_string(fixture.root.join("fai.toml")).unwrap();
    assert!(manifest.contains("\r\n"));
    let status = git(&fixture.root, &["status", "--porcelain"]);
    assert!(status.is_empty(), "{status}");
    let result = fixture.run(&["publish", "patch", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let committed = git(&fixture.root, &["show", "HEAD:fai.toml"]);
    assert!(!committed.contains('\r'));
    assert!(committed.contains("1.0.1"));
    assert!(git(&fixture.root, &["status", "--porcelain"]).is_empty());
}

#[test]
fn conflicting_release_tags_are_never_force_moved() {
    let fixture = Fixture::new();
    let original = git(&fixture.root, &["rev-parse", "HEAD"]);
    git(&fixture.root, &["tag", "v1.1.0"]);
    let result = fixture.run(&["publish", "minor", "--plan", "--message-format=json"]);
    assert!(!result.status.success());
    assert_eq!(git(&fixture.root, &["rev-parse", "refs/tags/v1.1.0"]), original);
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), original);
}
