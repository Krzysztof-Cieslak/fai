//! Native forwarding, selection, and manager availability across compiler versions.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    compiler: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("compiler.rs");
        std::fs::write(
            &source,
            r#"
fn main() {
    let directory = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "build-info") {
        println!("{}", std::fs::read_to_string(directory.join("identity.json")).unwrap());
    } else {
        println!("{}", std::fs::read_to_string(directory.join("version.txt")).unwrap());
        println!("{}", args.join("|"));
    }
}
"#,
        )
        .unwrap();
        let compiler = root.join(if cfg!(windows) { "compiler.exe" } else { "compiler" });
        assert!(
            Command::new("rustc")
                .arg(&source)
                .arg("--edition=2024")
                .arg("-o")
                .arg(&compiler)
                .status()
                .unwrap()
                .success()
        );
        Self { _temp: temp, root, compiler }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai-installer"));
        command
            .current_dir(&self.root)
            .env("FAI_HOME", self.root.join("home"))
            .env("FAI_TOOLCHAIN_DIR", self.root.join("inventory"))
            .env_remove("FAI_TOOLCHAIN")
            .env_remove("FAI_TOOLCHAIN_PROBE");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let result = self.run(args);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        String::from_utf8(result.stdout).unwrap()
    }
    fn register(&self, version: &str) -> PathBuf {
        let directory = self.root.join(format!("external {version}"));
        std::fs::create_dir(&directory).unwrap();
        let compiler = directory.join(self.compiler.file_name().unwrap());
        std::fs::copy(&self.compiler, &compiler).unwrap();
        write_identity(&directory, version, "test-build");
        std::fs::write(directory.join("version.txt"), version).unwrap();
        self.ok(&["toolchain", "register", compiler.to_str().unwrap()]);
        compiler
    }
}

fn write_identity(directory: &Path, version: &str, build: &str) {
    std::fs::write(
        directory.join("identity.json"),
        serde_json::json!({
            "version": version, "target": fai_toolchain::host(), "toolBuildId": build
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn two_versions_dispatch_before_parsing_version_specific_commands() {
    let fixture = Fixture::new();
    fixture.register("1.0.0");
    fixture.register("2.0.0");
    fixture.ok(&["toolchain", "default", "1.0.0"]);
    assert!(fixture.ok(&["update", "library"]).starts_with("1.0.0\nupdate|library"));
    assert!(
        fixture
            .ok(&["+2.0.0", "future-command", "--future-option"])
            .starts_with("2.0.0\nfuture-command|--future-option")
    );
    std::fs::write(
        fixture.root.join("fai.toml"),
        "manifest-version = 1\n[toolchain]\nversion = '2.0.0'\n",
    )
    .unwrap();
    assert!(fixture.ok(&["future-command", "--future-option"]).starts_with("2.0.0\n"));
    assert!(!fixture.run(&["+1.0.0", "check"]).status.success());
    let selected: serde_json::Value =
        serde_json::from_str(&fixture.ok(&["toolchain", "which", "--message-format=json"]))
            .unwrap();
    assert_eq!(selected["result"]["reason"], "project");
}

#[test]
fn manager_is_available_without_a_compiler() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fai-installer"))
        .args(["installer", "--version", "--message-format=json"])
        .env("FAI_HOME", temp.path())
        .env_remove("FAI_TOOLCHAIN_PROBE")
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["kind"], "fai-installer");
}

#[test]
fn changed_registered_compilers_fail_before_forwarding() {
    let fixture = Fixture::new();
    let compiler = fixture.register("1.0.0");
    fixture.ok(&["toolchain", "default", "1.0.0"]);
    write_identity(compiler.parent().unwrap(), "1.0.0", "different-build");
    let output = fixture.run(&["check"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("registered compiler changed"));
}

#[test]
fn unregistering_external_compilers_preserves_their_files() {
    let fixture = Fixture::new();
    let compiler = fixture.register("1.0.0");
    fixture.ok(&["toolchain", "uninstall", "1.0.0"]);
    assert!(compiler.is_file());
    assert!(!fixture.ok(&["toolchain", "list"]).contains("1.0.0"));
}

#[test]
fn registering_the_launcher_cannot_recurse() {
    let temp = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_fai-installer"))
        .args(["toolchain", "register", env!("CARGO_BIN_EXE_fai-installer")])
        .env("FAI_HOME", temp.path())
        .env("FAI_TOOLCHAIN_DIR", temp.path().join("toolchains"))
        .env_remove("FAI_TOOLCHAIN_PROBE")
        .output()
        .unwrap();
    assert!(!result.status.success());
}
