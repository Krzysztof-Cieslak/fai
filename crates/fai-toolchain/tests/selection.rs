//! Compiler selection and backwards-compatible state, exercised without network access.

use camino::Utf8PathBuf;
use fai_toolchain::{
    Invocation, Registry, Settings, atomic_write, discover_pin, pin_project, validate_version,
};

struct Fixture {
    _temporary: tempfile::TempDir,
    root: Utf8PathBuf,
    registry: Registry,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let registry = Registry { home: root.join("home"), directory: root.join("inventory") };
        Self { _temporary: temporary, root, registry }
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn invocation(&self, args: &[&str]) -> Invocation {
        Invocation::parse(args.iter().map(Into::into).collect(), &self.root).unwrap()
    }
    fn default(&self, version: &str) {
        atomic_write(
            &self.registry.home.join("settings.json"),
            &serde_json::to_vec(&Settings {
                schema_version: 1,
                default_version: Some(validate_version(version).unwrap()),
            })
            .unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn project_pin_precedes_the_global_default() {
    let fixture = Fixture::new();
    fixture.default("1.0.0");
    fixture.write("fai.toml", "manifest-version = 1\n[toolchain]\nversion = '2.0.0'\n");
    let (version, reason) =
        fixture.invocation(&["fai", "check"]).selection(&fixture.registry, None).unwrap();
    assert_eq!((version.to_string(), reason), ("2.0.0".into(), "project"));
}

#[test]
fn explicit_version_cannot_silently_override_a_project_pin() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "[toolchain]\nversion = '2.0.0'\n");
    let error = fixture
        .invocation(&["fai", "+1.0.0", "check"])
        .selection(&fixture.registry, None)
        .unwrap_err();
    assert!(error.to_string().contains("conflicts with project pin 2.0.0"));
}

#[test]
fn explicit_selection_precedes_the_shell_default() {
    let fixture = Fixture::new();
    let (version, reason) = fixture
        .invocation(&["fai", "+2.0.0", "check"])
        .selection(&fixture.registry, Some("1.0.0"))
        .unwrap();
    assert_eq!((version.to_string(), reason), ("2.0.0".into(), "argument"));
}

#[test]
fn explicit_selection_does_not_parse_an_ignored_environment_default() {
    let fixture = Fixture::new();
    let (version, reason) = fixture
        .invocation(&["fai", "+2.0.0", "check"])
        .selection(&fixture.registry, Some("invalid"))
        .unwrap();
    assert_eq!((version.to_string(), reason), ("2.0.0".into(), "argument"));
}

#[test]
fn program_arguments_do_not_change_compiler_selection() {
    let fixture = Fixture::new();
    let invocation = fixture.invocation(&[
        "fai",
        "run",
        "--",
        "+9.0.0",
        "-C",
        "missing",
        "--message-format=json",
    ]);
    assert!(invocation.explicit.is_none());
    assert!(!invocation.json);
    assert_eq!(invocation.directory, fixture.root);
    assert_eq!(invocation.arguments.len(), 7);
}

#[test]
fn future_compiler_options_survive_launcher_parsing() {
    let fixture = Fixture::new();
    let invocation = fixture.invocation(&["fai", "+2.0.0", "future", "--new-option=thing"]);
    assert_eq!(
        invocation.arguments,
        ["fai", "future", "--new-option=thing"].map(std::ffi::OsString::from)
    );
}

#[test]
fn selected_source_file_uses_its_own_project() {
    let fixture = Fixture::new();
    fixture.write("app/fai.toml", "[toolchain]\nversion = '2.0.0'\n");
    fixture.write("app/src/Main.fai", "module Main\n");
    let invocation = fixture.invocation(&["fai", "check", "app/src/Main.fai"]);
    assert_eq!(invocation.selection(&fixture.registry, None).unwrap().0.to_string(), "2.0.0");
}

#[test]
fn explicit_legacy_directory_roots_do_not_inherit_parent_pins() {
    let fixture = Fixture::new();
    fixture.default("1.0.0");
    fixture.write("fai.toml", "[toolchain]\nversion = '2.0.0'\n");
    fixture.write("legacy/Main.fai", "module Main\n");
    let invocation = fixture.invocation(&["fai", "check", "-C", "legacy"]);
    assert_eq!(invocation.selection(&fixture.registry, None).unwrap().0.to_string(), "1.0.0");
}

#[test]
fn workspace_members_use_the_workspace_pin() {
    let fixture = Fixture::new();
    fixture
        .write("fai.toml", "[workspace]\nmembers = ['apps/*']\n[toolchain]\nversion = '2.0.0'\n");
    fixture.write("apps/one/fai.toml", "[toolchain]\nversion = '1.0.0'\n");
    let pin = discover_pin(&fixture.root.join("apps/one")).unwrap().unwrap();
    assert_eq!(pin.manifest, fixture.root.join("fai.toml"));
    assert_eq!(pin.version.unwrap().to_string(), "2.0.0");
}

#[test]
fn workspace_member_patterns_allow_explicit_current_directory_prefixes() {
    let fixture = Fixture::new();
    fixture
        .write("fai.toml", "[workspace]\nmembers = ['./apps/*']\n[toolchain]\nversion = '2.0.0'\n");
    fixture.write("apps/one/fai.toml", "[toolchain]\nversion = '1.0.0'\n");
    assert_eq!(
        discover_pin(&fixture.root.join("apps/one")).unwrap().unwrap().version.unwrap().to_string(),
        "2.0.0"
    );
}

#[test]
fn unlisted_nested_project_keeps_its_own_pin() {
    let fixture = Fixture::new();
    fixture
        .write("fai.toml", "[workspace]\nmembers = ['apps/*']\n[toolchain]\nversion = '2.0.0'\n");
    fixture.write("other/fai.toml", "[toolchain]\nversion = '1.0.0'\n");
    let pin = discover_pin(&fixture.root.join("other")).unwrap().unwrap();
    assert_eq!(pin.version.unwrap().to_string(), "1.0.0");
}

#[test]
fn pinning_retains_manifest_comments_and_uses_the_workspace() {
    let fixture = Fixture::new();
    fixture
        .write("fai.toml", "# keep this\nmanifest-version = 1\n[workspace]\nmembers = ['app']\n");
    fixture.write("app/fai.toml", "manifest-version = 1\n");
    pin_project(&fixture.root.join("app"), &validate_version("2.0.0").unwrap()).unwrap();
    let text = std::fs::read_to_string(fixture.root.join("fai.toml")).unwrap();
    assert!(text.starts_with("# keep this\n"));
    assert!(text.contains("2.0.0"));
    assert!(
        !std::fs::read_to_string(fixture.root.join("app/fai.toml")).unwrap().contains("toolchain")
    );
}

#[test]
fn malformed_toolchain_tables_fail_without_changing_the_manifest() {
    let fixture = Fixture::new();
    let before = "manifest-version = 1\ntoolchain = 42\n";
    fixture.write("fai.toml", before);
    assert!(pin_project(&fixture.root, &validate_version("1.0.0").unwrap()).is_err());
    assert_eq!(std::fs::read_to_string(fixture.root.join("fai.toml")).unwrap(), before);
}

#[test]
fn legacy_inventory_shape_remains_readable() {
    let fixture = Fixture::new();
    let executable = fixture.root.join("compiler");
    fixture.write("inventory/installed.json", &serde_json::json!({"version":1,"compilers":[{
        "version":"1.0.0", "target":"x86_64-unknown-linux-gnu", "executable":executable,"buildId":"abc"
    }]}).to_string());
    let compilers = fixture.registry.list().unwrap();
    assert_eq!(compilers[0].version.to_string(), "1.0.0");
    assert_eq!(compilers[0].build_id, "abc");
}

#[test]
fn duplicate_inventory_versions_are_rejected() {
    let fixture = Fixture::new();
    let compiler = serde_json::json!({"version":"1.0.0","target":"host","executable":fixture.root.join("compiler"),"buildId":"abc"});
    fixture.write(
        "inventory/installed.json",
        &serde_json::json!({"version":1,"compilers":[compiler.clone(),compiler]}).to_string(),
    );
    assert!(fixture.registry.list().is_err());
}

#[test]
fn build_metadata_is_not_a_compiler_version() {
    assert!(validate_version("1.0.0+rebuild").is_err());
}

#[test]
fn missing_compiler_suggests_the_exact_install_command() {
    let fixture = Fixture::new();
    let error = fixture
        .registry
        .select(&validate_version("2.0.0").unwrap(), "x86_64-unknown-linux-gnu")
        .unwrap_err();
    assert!(error.to_string().contains("fai toolchain install 2.0.0"));
}
