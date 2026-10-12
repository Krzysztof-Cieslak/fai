//! Metadata commands preserve graph-local locks and shared/personal ownership.

use camino::{Utf8Path, Utf8PathBuf};
use fai_driver::{PackageCommand, ProjectOptions, Session, run_package_command};
use fai_package::{Dependency, DependencyAlias, LockFile};

struct Fixture {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
        Self { _temp: temp, root }
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn package(&self, path: &str, name: &str, version: &str, extra: &str) {
        self.write(
            &format!("{path}/fai.toml"),
            &format!(
                "manifest-version = 1\n[package]\nname = '{name}'\nversion = '{version}'\n{extra}"
            ),
        );
        self.write(
            &format!("{path}/src/Library.fai"),
            "module Library\npublic value : Int\nlet value = 1\n",
        );
    }
    fn resolve(&self, path: &str, no_local: bool) {
        run_package_command(
            &self.root.join(path),
            &PackageCommand::Resolve,
            &ProjectOptions { no_local, ..Default::default() },
            &[],
        )
        .unwrap();
    }
}

#[test]
fn metadata_resolution_prepares_the_declared_target_matrix() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "manifest-version = 1\nlock-targets = ['x86_64-unknown-linux-gnu', 'aarch64-apple-darwin']\n[package]\nname = 'app'\nversion = '1.0.0'\n");
    fixture.resolve("", false);
    let lock =
        LockFile::parse(&std::fs::read_to_string(fixture.root.join("fai.lock")).unwrap()).unwrap();
    assert_eq!(lock.graphs.len(), 2);
}

#[test]
fn a_body_edit_does_not_rewrite_the_lock() {
    let fixture = Fixture::new();
    fixture.package("app", "app", "1.0.0", "");
    fixture.resolve("app", false);
    let path = fixture.root.join("app/fai.lock");
    let before = std::fs::read(&path).unwrap();
    fixture.write("app/src/Library.fai", "module Library\npublic value : Int\nlet value = 2\n");
    Session::open_with_options(
        fixture.root.join("app"),
        ProjectOptions { locked: true, offline: true, ..Default::default() },
    )
    .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn unselected_stale_member_graphs_do_not_block_a_frozen_member() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "manifest-version = 1\n[workspace]\nmembers = ['a', 'b']\n");
    fixture.package("a", "a", "1.0.0", "");
    fixture.package("b", "b", "1.0.0", "");
    fixture.resolve("", false);
    fixture.package("b", "b", "1.0.0", "[dependencies]\nmissing = {path = '../missing'}\n");
    assert!(
        Session::open_with_options(
            fixture.root.join("a"),
            ProjectOptions { locked: true, offline: true, ..Default::default() }
        )
        .is_ok()
    );
}

#[test]
fn local_add_leaves_shared_graph_bytes_untouched_and_reports_staleness() {
    let fixture = Fixture::new();
    fixture.package("app", "app", "1.0.0", "");
    fixture.package("helper", "helper", "1.0.0", "");
    fixture.resolve("app", true);
    let before = std::fs::read(fixture.root.join("app/fai.lock")).unwrap();
    fixture.write("app/fai.local.toml", "manifest-version = 1\n");
    let result = run_package_command(
        &fixture.root.join("app"),
        &PackageCommand::Add {
            dependency: Dependency::Path { path: "../helper".into(), version: None },
            alias: Some(DependencyAlias::parse("helper").unwrap()),
            development: true,
            default_version: false,
        },
        &ProjectOptions::default(),
        &[],
    )
    .unwrap();
    assert!(result.stale_graphs.iter().any(|graph| graph.starts_with("shared:")));
    assert_eq!(std::fs::read(fixture.root.join("app/fai.lock")).unwrap(), before);
    assert!(fixture.root.join("app/fai.local.lock").is_file());
}

#[test]
fn changing_shared_baseline_requires_local_reconciliation() {
    let fixture = Fixture::new();
    fixture.package("first", "lib", "1.0.0", "");
    fixture.package("second", "lib", "1.2.0", "");
    fixture.package("personal", "lib", "1.1.0", "");
    let manifest = |path| {
        format!(
            "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[dependencies]\nlib = {{git = 'https://example.com/lib', version = '*'}}\n[[overrides]]\nreplace = {{git = 'https://example.com/lib'}}\nwith = {{path = '../{path}'}}\n"
        )
    };
    fixture.write("app/fai.toml", &manifest("first"));
    fixture.resolve("app", true);
    fixture.write("app/fai.local.toml", "manifest-version = 1\n[[overrides]]\nreplace = {git = 'https://example.com/lib'}\nwith = {path = '../personal'}\n");
    fixture.resolve("app", false);
    fixture.write("app/fai.toml", &manifest("second"));
    fixture.resolve("app", true);
    let frozen = Session::open_with_options(
        fixture.root.join("app"),
        ProjectOptions { locked: true, offline: true, ..Default::default() },
    );
    assert!(frozen.is_err());
    fixture.resolve("app", false);
    assert!(
        Session::open_with_options(
            fixture.root.join("app"),
            ProjectOptions { locked: true, offline: true, ..Default::default() }
        )
        .is_ok()
    );
    let local =
        LockFile::parse(&std::fs::read_to_string(fixture.root.join("app/fai.local.lock")).unwrap())
            .unwrap();
    assert!(local.packages.iter().any(|package| package.version.to_string() == "1.1.0"));
}

#[test]
fn read_only_dependency_inspection_reports_a_missing_graph() {
    let fixture = Fixture::new();
    fixture.package("app", "app", "1.0.0", "");
    let result = run_package_command(
        &fixture.root.join("app"),
        &PackageCommand::Deps { why: None },
        &ProjectOptions::default(),
        &[],
    )
    .unwrap();
    assert_eq!(result.stale_graphs.len(), 1);
    assert!(!fixture.root.join("app/fai.lock").exists());
}

#[test]
fn initialization_creates_a_formatted_application_without_a_lock() {
    let fixture = Fixture::new();
    let path = fixture.root.join("new-app");
    fai_driver::init_package(&path, None, false).unwrap();
    let text = std::fs::read_to_string(path.join("src/Main.fai")).unwrap();
    let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), &text);
    assert!(parsed.diagnostics.is_empty());
    assert_eq!(fai_fmt::format(&parsed.module, &parsed.comments, &text), text);
    assert!(!path.join("fai.lock").exists());
    assert!(Session::open(path).unwrap().entry_file(Utf8Path::new("")).is_ok());
}

#[test]
fn stale_prepared_graphs_cannot_replace_a_newer_compiler_snapshot() {
    let fixture = Fixture::new();
    fixture.package("app", "app", "1.0.0", "");
    let old =
        Session::prepare_update(fixture.root.join("app"), &[], ProjectOptions::default()).unwrap();
    fixture.package("helper", "helper", "1.0.0", "");
    fixture.package("app", "app", "1.0.0", "[dependencies]\nhelper = {path = '../helper'}\n");
    let new =
        Session::prepare_update(fixture.root.join("app"), &[], ProjectOptions::default()).unwrap();
    let mut session = Session::open_prepared(new).unwrap();
    let files = session.user_files();
    assert!(session.apply_prepared(old).is_err());
    assert_eq!(session.user_files(), files);
}

#[test]
fn vendor_snapshot_rejects_mutable_sources_outside_the_project() {
    let fixture = Fixture::new();
    fixture.package("app", "app", "1.0.0", "[dependencies]\nlib = {path = '../lib'}\n");
    fixture.package("lib", "lib", "1.0.0", "");
    let result = run_package_command(
        &fixture.root.join("app"),
        &PackageCommand::Vendor { directory: None },
        &ProjectOptions::default(),
        &[],
    );
    assert!(result.is_err());
    assert!(
        !std::fs::read_to_string(fixture.root.join("app/fai.toml")).unwrap().contains("[vendor]")
    );
    assert!(!fixture.root.join("app/fai.lock").exists());
}

#[test]
fn compiler_probe_reads_the_workspace_pin_from_a_member() {
    let fixture = Fixture::new();
    fixture.write(
        "fai.toml",
        "manifest-version = 1\n[workspace]\nmembers = ['app']\n[toolchain]\nversion = '9.9.9'\n",
    );
    fixture.package("app", "app", "1.0.0", "");
    assert_eq!(
        fai_driver::required_project_compiler(&fixture.root.join("app"))
            .unwrap()
            .unwrap()
            .to_string(),
        "9.9.9"
    );
}

#[test]
fn standalone_session_rescans_do_not_admit_new_neighbors() {
    let fixture = Fixture::new();
    fixture.write("Main.fai", "module Main\nlet value = 1\n");
    let mut session = Session::open_standalone(&fixture.root.join("Main.fai")).unwrap();
    fixture.write("Neighbor.fai", "module Neighbor\nlet value = true\n");
    session.sync_from_disk().unwrap();
    assert_eq!(session.user_files().len(), 1);
}
