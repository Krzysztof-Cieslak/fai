//! Local project loading exercises ownership, manifest changes, and source selection.

use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};
use fai_db::SourceRole;
use fai_driver::{CommandSpec, DriverError, OutputFormat, RenderOpts, Session, check, run_command};
use fai_syntax::Symbol;

struct Project {
    _directory: tempfile::TempDir,
    root: Utf8PathBuf,
}

impl Project {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        Self { _directory: directory, root }
    }

    fn write(&self, path: &str, text: &str) {
        let target = self.root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    fn package(&self, path: &str, name: &str, extra: &str) {
        self.write(
            &format!("{path}/fai.toml"),
            &format!(
                "manifest-version = 1\n[package]\nname = '{name}'\nversion = '1.0.0'\n{extra}"
            ),
        );
    }

    fn open(&self, path: &str) -> Session {
        Session::open(self.root.join(path)).unwrap()
    }
}

fn sample_root() -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../samples/package_imports")
            .canonicalize()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn sample_project_loads_path_dependencies_and_default_entry() {
    let session = Session::open(sample_root().join("app")).unwrap();
    assert!(session.is_package_project());
    assert_eq!(session.user_files().len(), 1);
    let entry = session.entry_file(Utf8Path::new("")).unwrap();
    assert!(
        matches!(entry.role(session.db()), SourceRole::Executable(name) if name == "import-demo")
    );
    let checked = check(session.db(), &session.user_files());
    assert!(checked.ok, "{:?}", checked.diagnostics);
    assert!(fai_driver::build_run_bundle(session.db(), entry).bundle.is_some());
}

#[test]
fn workspace_selects_members_and_coalesces_shared_local_libraries() {
    let session = Session::open(sample_root()).unwrap();
    assert_eq!(session.user_files().len(), 3);
    assert!(check(session.db(), &session.user_files()).ok);
    assert!(session.entry_file(Utf8Path::new("")).is_err());
    assert!(session.entry_file(Utf8Path::new("app/src/Main.fai")).is_ok());
}

#[test]
fn workspace_member_globs_do_not_parse_the_native_root_as_pattern_syntax() {
    let project = Project::new();
    project
        .write("root[1]/fai.toml", "manifest-version = 1\n[workspace]\nmembers = ['packages/*']\n");
    project.package("root[1]/packages/a", "a", "");
    project.package("root[1]/packages/b", "b", "");
    project.write("root[1]/packages/a/src/Library.fai", "module Library\n");
    project.write("root[1]/packages/b/src/Library.fai", "module Library\n");
    let session = project.open("root[1]");
    assert_eq!(session.user_files().len(), 2);
}

#[test]
fn package_scanning_ignores_unowned_sources() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\npublic value : Int\nlet value = 1\n");
    project.write("app/Unrelated.fai", "this is not Fai");
    let session = project.open("app");
    assert_eq!(session.user_files().len(), 1);
    assert!(check(session.db(), &session.user_files()).ok);
}

#[test]
fn tests_can_use_internal_library_members() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\ninternal value : Int\nlet value = 1\n");
    project.write("app/tests/Checks.fai", "module Checks\nexample: Library.value = 1\n");
    let session = project.open("app");
    assert!(check(session.db(), &session.user_files()).ok);
}

#[test]
fn a_development_dependency_does_not_become_a_production_import() {
    let project = Project::new();
    project.package("app", "app", "[dev-dependencies]\nhelper = {path = '../helper'}\n");
    project.package("helper", "helper", "");
    project.write("helper/src/Helper.fai", "module Helper\npublic value : Int\nlet value = 1\n");
    project.write(
        "app/src/Library.fai",
        "module Library\nimport helper.Helper\nlet value = Helper.value\n",
    );
    let session = project.open("app");
    let checked = check(session.db(), &session.user_files());
    assert!(checked.diagnostics.iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2021"));
}

#[test]
fn dependency_examples_are_not_implicitly_executed() {
    let project = Project::new();
    project.package("app", "app", "[dependencies]\nhelper = {path = '../helper'}\n");
    project.package("helper", "helper", "");
    project.write(
        "helper/src/Helper.fai",
        "module Helper\npublic value : Int\nlet value = 1\nexample: false\n",
    );
    project.write(
        "app/src/Library.fai",
        "module Library\nimport helper.Helper\nexample: Helper.value = 1\n",
    );
    let session = project.open("app");
    let rendered = run_command(
        &session,
        &CommandSpec::Check { path: None, examples: true },
        RenderOpts { format: OutputFormat::Json, color: false },
    );
    assert_eq!(rendered.exit, 0, "{} {}", rendered.stdout, rendered.stderr);
}

#[test]
fn checking_a_consumer_validates_dependency_code() {
    let project = Project::new();
    project.package("app", "app", "[dependencies]\nhelper = {path = '../helper'}\n");
    project.package("helper", "helper", "");
    project.write("helper/src/Helper.fai", "module Helper\npublic value : Int\nlet value = true\n");
    project.write("app/src/Library.fai", "module Library\nlet value = 1\n");
    let session = project.open("app");
    let checked = check(session.db(), &session.user_files());
    assert!(!checked.ok);
    assert!(checked.diagnostics.iter().any(|diagnostic| diagnostic.code.as_str() == "FAI3004"));
}

#[test]
fn missing_git_provider_is_an_explicit_project_error() {
    let project = Project::new();
    project.package(
        "app",
        "app",
        "[dependencies]\njson = {git = 'https://example.com/json', version = '^1.0.0'}\n",
    );
    let Err(DriverError::Project(error)) = Session::open(project.root.join("app")) else {
        panic!("expected located source error");
    };
    assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "FAI0811"));
}

#[test]
fn normal_package_cycles_are_rejected() {
    let project = Project::new();
    project.package("a", "a", "[dependencies]\nb = {path = '../b'}\n");
    project.package("b", "b", "[dependencies]\na = {path = '../a'}\n");
    let Err(DriverError::Project(error)) = Session::open(project.root.join("a")) else {
        panic!("expected cycle error");
    };
    assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "FAI0805"));
}

#[test]
fn development_helpers_can_refer_back_to_the_root_library() {
    let project = Project::new();
    project.package("app", "app", "[dev-dependencies]\nhelper = {path = '../helper'}\n");
    project.package("helper", "helper", "[dependencies]\napp = {path = '../app'}\n");
    project.write("app/src/Library.fai", "module Library\npublic value : Int\nlet value = 1\n");
    project.write(
        "helper/src/Helper.fai",
        "module Helper\nimport app.Library\npublic value : Int\nlet value = Library.value\n",
    );
    project.write(
        "app/tests/Checks.fai",
        "module Checks\nimport helper.Helper\nexample: Helper.value = 1\n",
    );
    let session = project.open("app");
    assert!(check(session.db(), &session.user_files()).ok);
}

#[test]
fn source_edits_reuse_package_and_file_identities() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\nlet value = 1\n");
    let mut session = project.open("app");
    let before = session.user_files()[0];
    project.write("app/src/Library.fai", "module Library\nlet value = true\n");
    session.refresh_disk_paths(&["src/Library.fai".into()]).unwrap();
    assert_eq!(session.user_files()[0], before);
    assert_eq!(
        fai_types::render_scheme(&fai_types::def_type(
            session.db(),
            before,
            Symbol::intern("value")
        )),
        "Bool"
    );
}

#[test]
fn unsaved_new_sources_get_package_ownership_and_survive_rescans() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\nlet value = New.value\n");
    let mut session = project.open("app");
    assert!(!check(session.db(), &session.user_files()).ok);
    session
        .set_overlay(
            Utf8Path::new("src/New.fai"),
            "module New\npublic value : Int\nlet value = 1\n".into(),
        )
        .unwrap();
    session.sync_from_disk().unwrap();
    assert!(check(session.db(), &session.user_files()).ok);
    session.clear_overlay(Utf8Path::new("src/New.fai")).unwrap();
    assert!(!check(session.db(), &session.user_files()).ok);
}

#[test]
fn manifest_errors_retain_locations_in_json_results() {
    let project = Project::new();
    project
        .write("fai.toml", "manifest-version = 1\n[package]\nname = 'app'\nversion = 'invalid'\n");
    let error = match Session::open(project.root.clone()) {
        Ok(_) => panic!("expected manifest error"),
        Err(error) => error,
    };
    let rendered = fai_driver::render_workspace_error(
        &error,
        RenderOpts { format: OutputFormat::Json, color: false },
    );
    let json: serde_json::Value = serde_json::from_str(&rendered.stdout).unwrap();
    assert_eq!(json["diagnostics"][0]["code"], "FAI0803");
    assert_eq!(json["diagnostics"][0]["primary"]["file"], "fai.toml");
}

#[test]
fn incompatible_path_dependencies_keep_the_declaring_location() {
    let project = Project::new();
    project.package("app", "app", "[dependencies]\nlib = {path = '../lib', version = '^2.0.0'}\n");
    project.package("lib", "lib", "");
    let Err(DriverError::Project(error)) = Session::open(project.root.join("app")) else {
        panic!("expected incompatibility");
    };
    assert_eq!(error.diagnostics[0].code, "FAI0804");
    assert!(error.human.contains("lib"));
    assert!(error.human.contains("fai.toml"));
}

#[test]
fn consumed_libraries_do_not_require_their_executable_sources() {
    let project = Project::new();
    project.package("app", "app", "[dependencies]\nlib = {path = '../lib'}\n");
    project.package("lib", "lib", "[[bin]]\nname = 'tool'\npath = 'missing.fai'\n");
    project.write("lib/src/Values.fai", "module Values\npublic value : Int\nlet value = 1\n");
    project.write(
        "app/src/Library.fai",
        "module Library\nimport lib.Values\nlet value = Values.value\n",
    );
    let session = project.open("app");
    assert!(check(session.db(), &session.user_files()).ok);
    assert!(Session::open(project.root.join("lib")).is_err());
}

#[test]
fn invalid_new_manifest_keeps_the_previous_compiler_snapshot() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\nlet value = 1\n");
    let mut session = project.open("app");
    let files = session.user_files();
    project.write("app/fai.toml", "not valid TOML");
    assert!(session.sync_from_disk().is_err());
    assert_eq!(session.user_files(), files);
    assert!(check(session.db(), &files).ok);
}

#[test]
fn unchanged_package_rescan_preserves_all_query_memoization() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\nlet value = 1\n");
    let mut session = project.open("app");
    assert!(check(session.db(), &session.user_files()).ok);
    session.enable_event_log();
    assert!(!session.sync_disk_changes().unwrap());
    assert!(check(session.db(), &session.user_files()).ok);
    assert!(session.take_events().is_empty());
}

#[test]
fn adding_a_dev_dependency_retains_the_production_file_identity() {
    let project = Project::new();
    project.package("app", "app", "");
    project.package("helper", "helper", "");
    project.write("app/src/Library.fai", "module Library\nlet value = 1\n");
    let mut session = project.open("app");
    let before = session.user_files()[0];
    project.package("app", "app", "[dev-dependencies]\nhelper = {path = '../helper'}\n");
    session.sync_from_disk().unwrap();
    assert_eq!(session.user_files()[0], before);
}

#[test]
fn package_tests_validate_sources_without_contracts() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\npublic value : Int\nlet value = true\n");
    let session = project.open("app");
    let plan = fai_driver::build_test_plan(
        session.db(),
        &session.user_files(),
        None,
        fai_driver::TestConfig::default(),
    );
    assert!(plan.blocked);
}

#[test]
fn two_main_entries_have_independent_executable_scopes() {
    let project = Project::new();
    project.package("app", "app", "default-run = 'first'\n[[bin]]\nname = 'first'\npath = 'commands/One.fai'\n[[bin]]\nname = 'second'\npath = 'commands/Two.fai'\n");
    project.write("app/commands/One.fai", "module Main\nlet value = 1\n");
    project.write("app/commands/Two.fai", "module Main\nlet value = true\n");
    let session = project.open("app");
    assert!(check(session.db(), &session.user_files()).ok);
    assert_eq!(
        session.executable_name(session.entry_file(Utf8Path::new("")).unwrap()),
        Some("first")
    );
}

#[cfg(unix)]
#[test]
fn active_source_symlinks_cannot_escape_the_package() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("outside/Hidden.fai", "module Hidden\n");
    std::os::unix::fs::symlink(project.root.join("outside"), project.root.join("app/src")).unwrap();
    assert!(Session::open(project.root.join("app")).is_err());
}

#[cfg(unix)]
#[test]
fn unowned_symlink_trees_do_not_join_source_discovery() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\n");
    std::os::unix::fs::symlink("missing-target", project.root.join("app/unowned")).unwrap();
    assert!(Session::open(project.root.join("app")).is_ok());
}

#[cfg(unix)]
#[test]
fn source_symlink_cycles_fail_without_unbounded_recursion() {
    let project = Project::new();
    project.package("app", "app", "");
    project.write("app/src/Library.fai", "module Library\n");
    std::os::unix::fs::symlink(".", project.root.join("app/src/again")).unwrap();
    assert!(Session::open(project.root.join("app")).is_err());
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn path_dependency_edits_match_clean_sessions(states in prop::collection::vec(any::<bool>(), 1..6)) {
            let project = Project::new();
            project.package("app", "app", "[dependencies]\nlib = {path = '../lib'}\n");
            project.package("lib", "lib", "");
            project.write("lib/src/Values.fai", "module Values\npublic value : Int\nlet value = 1\n");
            project.write("app/src/Library.fai", "module Library\nimport lib.Values\nlet value = Values.value\n");
            let mut session = project.open("app");
            for boolean in states {
                let text = if boolean { "module Values\npublic value : Bool\nlet value = true\n" } else { "module Values\npublic value : Int\nlet value = 1\n" };
                project.write("lib/src/Values.fai", text);
                session.sync_from_disk().unwrap();
                let clean = project.open("app");
                let ty = |session: &Session| fai_types::render_scheme(&fai_types::def_type(session.db(), session.user_files()[0], Symbol::intern("value")));
                prop_assert_eq!(ty(&session), ty(&clean));
                prop_assert!(check(session.db(), &session.user_files()).ok);
            }
        }
    }
}
