//! Project discovery, standalone-file scope, library builds, and installed compilers.

use std::{
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        Self { _temp: temp, root }
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .current_dir(&self.root)
            .env("FAI_CACHE_DIR", self.root.join("cache"))
            .env("FAI_RUNTIME_DIR", self.root.join("runtime"))
            .env("FAI_TOOLCHAIN_DIR", self.root.join("toolchains"));
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

#[test]
fn standalone_files_do_not_discover_neighboring_modules() {
    let fixture = Fixture::new();
    fixture.write("Main.fai", "module Main\nlet value = Neighbor.value\n");
    fixture.write("Neighbor.fai", "module Neighbor\npublic value : Int\nlet value = 42\n");
    let result = fixture.run(&["check", "Main.fai", "--no-examples", "--message-format=json"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("FAI2008"), "{result:?}");
}

#[test]
fn standalone_execution_uses_std_without_loading_broken_neighbors() {
    let fixture = Fixture::new();
    fixture.write("Main.fai", "module Main\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine \"standalone\"\n");
    fixture.write("Broken.fai", "not a Fai module");
    let result = fixture.run(&["run", "Main.fai"]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"standalone\n");
}

#[test]
fn member_subdirectories_discover_the_owning_manifest() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n");
    fixture.write("src/Main.fai", "module Main\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine \"project\"\n");
    let result = fixture
        .command()
        .current_dir(fixture.root.join("src"))
        .args(["run", "--no-daemon"])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"project\n");
}

#[test]
fn library_builds_do_not_require_an_executable_entry() {
    let fixture = Fixture::new();
    fixture.write(
        "fai.toml",
        "manifest-version = 1\n[package]\nname = 'library'\nversion = '1.0.0'\n",
    );
    fixture.write(
        "src/Library.fai",
        "module Library\npublic identity : Int -> Int\nlet identity value = value\n",
    );
    let result = fixture.run(&["build", "--no-daemon", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(json["artifacts"].as_array().unwrap().is_empty());
}

#[test]
fn package_builds_emit_each_executable_in_an_isolated_output_directory() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[[bin]]\nname = 'first'\npath = 'commands/One.fai'\n[[bin]]\nname = 'second'\npath = 'commands/Two.fai'\n");
    fixture.write("commands/One.fai", "module Main\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine \"one\"\n");
    fixture.write("commands/Two.fai", "module Main\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine \"two\"\n");
    let result = fixture.run(&["build", "--no-daemon", "--message-format=json"]);
    assert!(result.status.success(), "{result:?}");
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    let artifacts = json["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
    assert_ne!(artifacts[0], artifacts[1]);
    assert!(artifacts.iter().all(|path| std::path::Path::new(path.as_str().unwrap()).is_file()));
}

#[test]
fn registering_an_installed_compiler_records_its_exact_build_identity() {
    let fixture = Fixture::new();
    let registered =
        fixture.run(&["toolchain", "register", env!("CARGO_BIN_EXE_fai"), "--message-format=json"]);
    assert!(registered.status.success(), "{registered:?}");
    let listed = fixture.run(&["toolchain", "list", "--message-format=json"]);
    assert!(listed.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(metadata["toolchains"].as_array().unwrap().len(), 1);
    assert!(!metadata["toolchains"][0]["buildId"].as_str().unwrap().is_empty());
}

#[test]
fn an_unavailable_compiler_pin_fails_without_creating_a_lock() {
    let fixture = Fixture::new();
    fixture.write("fai.toml", "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[toolchain]\nversion = '9.9.9'\n");
    let result = fixture.run(&["check", "--no-daemon", "--message-format=json"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("FAI0809"), "{result:?}");
    assert!(!fixture.root.join("fai.lock").exists());
}

#[cfg(unix)]
#[test]
fn exact_installed_pins_dispatch_before_opening_the_project() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let info: serde_json::Value =
        serde_json::from_slice(&fixture.run(&["build-info"]).stdout).unwrap();
    let metadata = serde_json::json!({"version": "9.9.9", "target": info["target"], "toolBuildId": "test-compiler"});
    fixture.write("compiler", &format!("#!/bin/sh\nif [ \"$1\" = build-info ]; then\n  printf '%s\\n' '{}'\nelse\n  printf 'selected installed compiler\\n'\nfi\n", metadata));
    let executable = fixture.root.join("compiler");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let registered = fixture.run(&["toolchain", "register", executable.to_str().unwrap()]);
    assert!(registered.status.success(), "{registered:?}");
    fixture.write("fai.toml", "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[toolchain]\nversion = '9.9.9'\n");
    let result = fixture.run(&["check"]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"selected installed compiler\n");
}
