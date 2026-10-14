//! End-to-end daemon tests, spawning the real `fai` binary so the auto-spawned
//! daemon and the worker subprocesses are exercised.
//!
//! Each test is isolated: a unique workspace, a unique `FAI_RUNTIME_DIR` (socket
//! directory) and `FAI_CACHE_DIR`, all inherited by the self-spawned daemon. A
//! [`Daemon`] guard stops its daemon on drop so none leak across the run.
//!
//! These run on Windows too: the spawn no longer lets the daemon inherit the
//! client's stdio pipes, so a piped client returns promptly instead of blocking
//! until the daemon's idle timeout.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use indoc::indoc;

fn unique(tag: &str) -> PathBuf {
    // Reserve the directory atomically: separate test processes can reuse a PID
    // while an earlier process's fixture directories still exist.
    tempfile::Builder::new().prefix(&format!("fai-daemon-{tag}-")).tempdir().unwrap().keep()
}

/// A short base directory for the daemon's Unix socket. macOS caps socket paths
/// at ~104 bytes, and its default temp dir (`/var/folders/…`) is long enough
/// that appending the per-workspace socket file overflows the limit — so the
/// socket directory uses a short, fixed base instead of [`unique`]. (On Windows
/// the endpoint is a namespaced pipe, so the path length is irrelevant.)
fn unique_socket_dir() -> PathBuf {
    let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    tempfile::Builder::new().prefix("fai-rt-").tempdir_in(base).unwrap().keep()
}

#[test]
fn fixture_directory_is_reserved_before_use() {
    let first = unique("reserved");
    let second = unique("reserved");
    assert!(first.is_dir());
    assert!(second.is_dir());
    assert_ne!(first, second);
    std::fs::remove_dir(first).unwrap();
    std::fs::remove_dir(second).unwrap();
}

#[test]
fn socket_directory_is_reserved_before_use() {
    let directory = unique_socket_dir();
    assert!(directory.is_dir());
    std::fs::remove_dir(directory).unwrap();
}

/// An isolated workspace with its own daemon endpoint and cache.
struct Daemon {
    workspace: PathBuf,
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
    run_timeout_ms: Option<u64>,
    run_as_bytes: Option<u64>,
    test_hold_ms: Option<u64>,
    idle_timeout_secs: u64,
}

impl Daemon {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let workspace = unique(&format!("{name}-ws"));
        std::fs::create_dir_all(&workspace).unwrap();
        for (file, contents) in files {
            std::fs::create_dir_all(workspace.join(file).parent().unwrap()).unwrap();
            std::fs::write(workspace.join(file), contents).unwrap();
        }
        Self {
            workspace,
            runtime_dir: unique_socket_dir(),
            cache_dir: unique(&format!("{name}-cache")),
            run_timeout_ms: None,
            run_as_bytes: None,
            test_hold_ms: None,
            idle_timeout_secs: 60,
        }
    }

    /// Sets the supervised-run wall-clock timeout (inherited by the daemon).
    fn with_run_timeout(mut self, ms: u64) -> Self {
        self.run_timeout_ms = Some(ms);
        self
    }

    /// Makes the daemon hold each off-lock read for `ms` (the test-only
    /// `FAI_DAEMON_TEST_HOLD_MS` hook), so a concurrent burst deterministically
    /// overlaps and `daemon status` reports a peak concurrency > 1.
    fn with_test_hold(mut self, ms: u64) -> Self {
        self.test_hold_ms = Some(ms);
        self
    }

    fn with_idle_timeout(mut self, seconds: u64) -> Self {
        self.idle_timeout_secs = seconds;
        self
    }

    /// Sets the worker's opt-in committed-memory cap (`FAI_RUN_AS_BYTES`),
    /// inherited by the daemon and thus the worker it spawns. Used to exercise
    /// the Windows Job-Object memory limit.
    #[cfg(windows)]
    fn with_run_as_bytes(mut self, bytes: u64) -> Self {
        self.run_as_bytes = Some(bytes);
        self
    }

    /// A `fai` command with this workspace's isolated environment.
    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .env("FAI_RUNTIME_DIR", &self.runtime_dir)
            .env("FAI_CACHE_DIR", &self.cache_dir)
            .env("FAI_DAEMON_IDLE_TIMEOUT", self.idle_timeout_secs.to_string());
        if let Some(ms) = self.run_timeout_ms {
            command.env("FAI_RUN_TIMEOUT_MS", ms.to_string());
        }
        if let Some(bytes) = self.run_as_bytes {
            command.env("FAI_RUN_AS_BYTES", bytes.to_string());
        }
        if let Some(ms) = self.test_hold_ms {
            command.env("FAI_DAEMON_TEST_HOLD_MS", ms.to_string());
        }
        command
    }

    /// Runs `fai <args> -C <workspace> <trailing>` and captures the output.
    fn run(&self, args: &[&str], trailing: &[&str]) -> Output {
        let mut command = self.cmd();
        command.args(args).arg("-C").arg(&self.workspace).args(trailing);
        command.output().unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Best-effort: stop the daemon so it doesn't linger past the test.
        let _ = self.run(&["daemon", "stop"], &[]);
    }
}

mod package_projects {
    use super::*;

    const ROOT: &str = "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[dependencies]\nlib = {path = 'lib'}\n";
    const LIB: &str = "manifest-version = 1\n[package]\nname = 'lib'\nversion = '1.0.0'\n";
    const VALUE: &str = "module Values\npublic value : Int\nlet value = 42\n";
    const MAIN: &str = "module Main\nimport lib.Values\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine (Int.toString Values.value)\nexample: Values.value = 42\n";

    fn project() -> Daemon {
        Daemon::new(
            "package",
            &[
                ("fai.toml", ROOT),
                ("src/Main.fai", MAIN),
                ("lib/fai.toml", LIB),
                ("lib/src/Values.fai", VALUE),
            ],
        )
    }

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let result = Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
        assert!(result.status.success(), "{args:?}: {result:?}");
        String::from_utf8(result.stdout).unwrap()
    }

    fn release(root: &std::path::Path, version: &str, value: i32) {
        std::fs::write(
            root.join("fai.toml"),
            format!("manifest-version = 1\n[package]\nname = 'tools-lib'\nversion = '{version}'\n"),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/Values.fai"),
            format!("module Values\npublic value : Int\nlet value = {value}\n"),
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "release fixture"]);
        git(root, &["tag", &format!("v{version}")]);
    }

    fn git_project() -> (Daemon, PathBuf, String) {
        let project = Daemon::new(
            "git-package",
            &[
                ("fai.toml", "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n"),
                (
                    "src/Main.fai",
                    "module Main\nimport tools_lib.Values\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine (Int.toString Values.value)\n",
                ),
            ],
        );
        let upstream = project.workspace.join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q"]);
        git(&upstream, &["config", "user.name", "Package Test"]);
        git(&upstream, &["config", "user.email", "packages@example.invalid"]);
        release(&upstream, "1.0.0", 42);
        let url = lsp_types::Url::from_directory_path(&upstream).unwrap().to_string();
        (project, upstream, url)
    }

    #[test]
    fn git_add_derives_the_alias_and_caret_then_pins_ordinary_runs() {
        let (project, upstream, url) = git_project();
        let added = project.run(&["add", &url, "--message-format=json"], &[]);
        assert!(added.status.success(), "{added:?}");
        let manifest = std::fs::read_to_string(project.workspace.join("fai.toml")).unwrap();
        assert!(manifest.contains("tools_lib"), "{manifest}");
        assert!(manifest.contains("^1.0.0"), "{manifest}");
        let lock = std::fs::read(project.workspace.join("fai.lock")).unwrap();
        release(&upstream, "1.1.0", 43);
        let pinned = project.run(&["run", "--frozen"], &[]);
        assert!(pinned.status.success(), "{pinned:?}");
        assert_eq!(pinned.stdout, b"42\n");
        assert_eq!(std::fs::read(project.workspace.join("fai.lock")).unwrap(), lock);
        let updated = project.run(&["update", "tools_lib", "--message-format=json"], &[]);
        assert!(updated.status.success(), "{updated:?}");
        let current = project.run(&["run", "--frozen"], &[]);
        assert!(current.status.success(), "{current:?}");
        assert_eq!(current.stdout, b"43\n");
    }

    #[test]
    fn resolve_retains_valid_cached_selections_without_refreshing_the_remote() {
        let (project, upstream, url) = git_project();
        let added = project.run(&["add", &url], &[]);
        assert!(added.status.success(), "{added:?}");
        let before = std::fs::read(project.workspace.join("fai.lock")).unwrap();
        std::fs::remove_dir_all(upstream).unwrap();
        let resolved = project.run(&["resolve", "--message-format=json"], &[]);
        assert!(resolved.status.success(), "{resolved:?}");
        assert_eq!(std::fs::read(project.workspace.join("fai.lock")).unwrap(), before);
    }

    #[test]
    fn changing_a_branchs_repository_discards_its_old_commit_binding() {
        let (project, upstream, url) = git_project();
        let branch = git(&upstream, &["symbolic-ref", "--short", "HEAD"]);
        let branch = branch.trim();
        let added = project.run(&["add", &url, "--branch", branch], &[]);
        assert!(added.status.success(), "{added:?}");
        let fork = project.workspace.join("fork");
        std::fs::create_dir(&fork).unwrap();
        git(&fork, &["init", "-q", "-b", branch]);
        git(&fork, &["config", "user.name", "Package Test"]);
        git(&fork, &["config", "user.email", "packages@example.invalid"]);
        release(&fork, "1.0.0", 99);
        let fork_url = lsp_types::Url::from_directory_path(&fork).unwrap();
        let path = project.workspace.join("fai.toml");
        let mut manifest = std::fs::read_to_string(&path).unwrap();
        manifest.push_str(&format!("\n[[overrides]]\nreplace = {{git = '{url}'}}\nwith = {{git = '{fork_url}', branch = '{branch}'}}\n"));
        std::fs::write(path, manifest).unwrap();
        let resolved = project.run(&["run", "--no-daemon"], &[]);
        assert!(resolved.status.success(), "{resolved:?}");
        assert_eq!(resolved.stdout, b"99\n");
    }

    #[test]
    fn failed_add_keeps_the_authored_manifest_and_lock() {
        let (project, _, url) = git_project();
        assert!(project.run(&["add", &url], &[]).status.success());
        let manifest = std::fs::read(project.workspace.join("fai.toml")).unwrap();
        let lock = std::fs::read(project.workspace.join("fai.lock")).unwrap();
        let rejected =
            project.run(&["add", &url, "--as", "tools_lib", "--version", "^99.0.0"], &[]);
        assert!(!rejected.status.success());
        assert_eq!(std::fs::read(project.workspace.join("fai.toml")).unwrap(), manifest);
        assert_eq!(std::fs::read(project.workspace.join("fai.lock")).unwrap(), lock);
    }

    #[test]
    fn dependency_inspection_does_not_restore_or_refresh_sources() {
        let (project, upstream, url) = git_project();
        assert!(project.run(&["add", &url], &[]).status.success());
        let lock = std::fs::read(project.workspace.join("fai.lock")).unwrap();
        std::fs::remove_dir_all(upstream).unwrap();
        std::fs::remove_dir_all(&project.cache_dir).unwrap();
        let inspected = project.run(&["deps", "why", "tools_lib", "--message-format=json"], &[]);
        assert!(inspected.status.success(), "{inspected:?}");
        assert_eq!(std::fs::read(project.workspace.join("fai.lock")).unwrap(), lock);
        let json: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
        assert!(!json["why"].as_array().unwrap().is_empty());
    }

    #[test]
    fn moved_release_tags_fail_without_changing_the_lock() {
        let (project, upstream, url) = git_project();
        assert!(project.run(&["add", &url], &[]).status.success());
        let lock = std::fs::read(project.workspace.join("fai.lock")).unwrap();
        std::fs::write(
            upstream.join("src/Values.fai"),
            "module Values\npublic value : Int\nlet value = 99\n",
        )
        .unwrap();
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", "altered fixture"]);
        git(&upstream, &["tag", "-f", "v1.0.0"]);
        let rejected = project.run(&["update", "--message-format=json"], &[]);
        assert!(!rejected.status.success(), "{rejected:?}");
        assert!(String::from_utf8_lossy(&rejected.stdout).contains("FAI0815"));
        assert_eq!(std::fs::read(project.workspace.join("fai.lock")).unwrap(), lock);
    }

    #[test]
    fn locked_missing_graph_fails_before_fetching_or_writing() {
        let (project, _, url) = git_project();
        std::fs::write(project.workspace.join("fai.toml"), format!("manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[dependencies]\ntools_lib = {{git = '{url}', version = '*'}}\n")).unwrap();
        let rejected = project.run(&["check", "--frozen", "--message-format=json"], &[]);
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stdout).contains("FAI0806"), "{rejected:?}");
        assert!(!project.workspace.join("fai.lock").exists());
        assert!(!project.cache_dir.join("packages/git").exists());
    }

    #[test]
    fn removal_prunes_the_selected_graph_without_rewriting_imports() {
        let (project, _, url) = git_project();
        assert!(project.run(&["add", &url], &[]).status.success());
        let source = std::fs::read(project.workspace.join("src/Main.fai")).unwrap();
        let removed = project.run(&["remove", "tools_lib", "--message-format=json"], &[]);
        assert!(removed.status.success(), "{removed:?}");
        let lock = std::fs::read_to_string(project.workspace.join("fai.lock")).unwrap();
        assert!(!lock.contains("tools_lib"));
        assert_eq!(std::fs::read(project.workspace.join("src/Main.fai")).unwrap(), source);
    }

    fn vendored_project() -> (Daemon, PathBuf) {
        let (project, upstream, url) = git_project();
        let added = project.run(&["add", &url], &[]);
        assert!(added.status.success(), "{added:?}");
        let vendored = project.run(&["vendor"], &[]);
        assert!(vendored.status.success(), "{vendored:?}");
        (project, upstream)
    }

    fn vendor_source(project: &Daemon) -> PathBuf {
        std::fs::read_dir(project.workspace.join("vendor/sources"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
            .join("src/Values.fai")
    }

    #[test]
    fn vendored_frozen_execution_needs_neither_git_cache_nor_remote() {
        let (project, upstream) = vendored_project();
        std::fs::remove_dir_all(upstream).unwrap();
        std::fs::remove_dir_all(&project.cache_dir).unwrap();
        let result = project.run(&["run", "--frozen", "--no-daemon"], &[]);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[test]
    fn altered_vendor_content_does_not_fall_back_to_an_available_cache() {
        let (project, _) = vendored_project();
        std::fs::write(vendor_source(&project), "modified").unwrap();
        let rejected = project.run(&["check", "--no-daemon", "--message-format=json"], &[]);
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stdout).contains("FAI0820"), "{rejected:?}");
    }

    #[test]
    fn no_vendor_explicitly_uses_the_verified_cache() {
        let (project, _) = vendored_project();
        std::fs::write(vendor_source(&project), "modified").unwrap();
        let result = project.run(&["run", "--no-daemon", "--no-vendor", "--frozen"], &[]);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[test]
    fn vendor_refresh_repairs_sources_and_retains_unowned_files() {
        let (project, _) = vendored_project();
        std::fs::write(project.workspace.join("vendor/notes.txt"), "keep").unwrap();
        std::fs::write(vendor_source(&project), "modified").unwrap();
        let refreshed = project.run(&["vendor"], &[]);
        assert!(refreshed.status.success(), "{refreshed:?}");
        assert_eq!(
            std::fs::read_to_string(project.workspace.join("vendor/notes.txt")).unwrap(),
            "keep"
        );
        let result = project.run(&["run", "--no-daemon", "--frozen"], &[]);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[cfg(unix)]
    #[test]
    fn contained_vendor_directory_aliases_keep_canonical_source_ownership() {
        let (project, _) = vendored_project();
        std::fs::rename(
            project.workspace.join("vendor"),
            project.workspace.join("source-snapshot"),
        )
        .unwrap();
        std::os::unix::fs::symlink("source-snapshot", project.workspace.join("vendor")).unwrap();
        let result = project.run(&["run", "--no-daemon", "--frozen"], &[]);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(result.stdout, b"42\n");
    }

    #[test]
    fn default_executable_agrees_between_daemon_and_direct_run() {
        let project = project();
        let direct = project.run(&["run", "--no-daemon"], &[]);
        let warm = project.run(&["run"], &[]);
        assert!(direct.status.success(), "{direct:?}");
        assert!(warm.status.success(), "{warm:?}");
        assert_eq!(direct.stdout, b"42\n");
        assert_eq!(direct.stdout, warm.stdout);
    }

    #[test]
    fn named_executable_builds_and_runs() {
        let project = project();
        let output = project.workspace.join(format!("program{}", std::env::consts::EXE_SUFFIX));
        let built = project
            .cmd()
            .args(["build", "--no-daemon", "--bin", "app", "-C"])
            .arg(&project.workspace)
            .arg("--out")
            .arg(&output)
            .output()
            .unwrap();
        assert!(built.status.success(), "{built:?}");
        let ran = Command::new(output).output().unwrap();
        assert!(ran.status.success(), "{ran:?}");
        assert_eq!(ran.stdout, b"42\n");
    }

    #[test]
    fn package_contracts_agree_between_daemon_and_direct_test() {
        let project = project();
        let direct = project.run(&["test", "--no-daemon", "--message-format=json"], &[]);
        let warm = project.run(&["test", "--message-format=json"], &[]);
        assert!(direct.status.success(), "{direct:?}");
        assert!(warm.status.success(), "{warm:?}");
        assert_eq!(test_report(&direct), test_report(&warm));
    }

    #[test]
    fn personal_and_shared_overrides_have_separate_warm_contexts() {
        let project = project();
        std::fs::write(project.workspace.join("fai.toml"), "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[dependencies]\nlib = {git = 'https://example.com/lib', version = '^1.0.0'}\n[[overrides]]\nreplace = {git = 'https://example.com/lib'}\nwith = {path = 'lib'}\n").unwrap();
        std::fs::create_dir_all(project.workspace.join("personal/src")).unwrap();
        std::fs::write(project.workspace.join("personal/fai.toml"), LIB).unwrap();
        std::fs::write(project.workspace.join("personal/src/Values.fai"), VALUE.replace("42", "7"))
            .unwrap();
        std::fs::write(project.workspace.join("fai.local.toml"), "manifest-version = 1\n[[overrides]]\nreplace = {git = 'https://example.com/lib'}\nwith = {path = 'personal'}\n").unwrap();
        let local = project.run(&["run"], &[]);
        let shared = project.run(&["run", "--no-local"], &[]);
        let local_again = project.run(&["run"], &[]);
        assert!(local.status.success(), "{local:?}");
        assert!(shared.status.success(), "{shared:?}");
        assert_eq!(local.stdout, b"7\n");
        assert_eq!(shared.stdout, b"42\n");
        assert_eq!(local.stdout, local_again.stdout);
        assert!(String::from_utf8_lossy(&local.stderr).contains("local context"));
        assert!(shared.stderr.is_empty());
    }

    #[test]
    fn malformed_manifests_report_the_same_located_json_in_both_paths() {
        let project = project();
        std::fs::write(
            project.workspace.join("fai.toml"),
            "manifest-version = 1\n[package]\nname = 'app'\nversion = 'bad'\n",
        )
        .unwrap();
        let direct = project.run(&["check", "--no-daemon", "--message-format=json"], &[]);
        let warm = project.run(&["check", "--message-format=json"], &[]);
        assert_eq!(direct.status.code(), Some(3), "{direct:?}");
        assert_eq!(warm.status.code(), direct.status.code(), "{warm:?}");
        assert_eq!(direct.stdout, warm.stdout);
        let json: serde_json::Value = serde_json::from_slice(&warm.stdout).unwrap();
        assert_eq!(json["diagnostics"][0]["primary"]["file"], "fai.toml");
    }

    #[test]
    fn explicit_member_avoids_an_unselected_broken_dependency_graph() {
        let project = Daemon::new(
            "members",
            &[
                ("fai.toml", "manifest-version = 1\n[workspace]\nmembers = ['a', 'b']\n"),
                ("a/fai.toml", LIB),
                ("a/src/Values.fai", VALUE),
                ("b/fai.toml", ROOT),
            ],
        );
        let result = project.run(&["check", "--no-daemon", "--package", "a", "--no-examples"], &[]);
        assert!(result.status.success(), "{result:?}");
    }
}

#[track_caller]
fn missing_selection(command: &str, path: &str, json: bool) {
    let daemon = Daemon::new("missing-selection", &[]);
    let format = if json { "--message-format=json" } else { "--message-format=human" };
    let cold = daemon.run(&[command, "--no-daemon", format, "--color=never"], &[path]);
    let warm = daemon.run(&[command, format, "--color=never"], &[path]);
    assert_eq!(
        cold.status.code(),
        Some(3),
        "{}{}",
        String::from_utf8_lossy(&cold.stdout),
        String::from_utf8_lossy(&cold.stderr)
    );
    assert_eq!(warm.status.code(), cold.status.code());
    assert_eq!(warm.stdout, cold.stdout);
    assert_eq!(warm.stderr, cold.stderr);
    if json {
        let output: serde_json::Value = serde_json::from_slice(&cold.stdout).unwrap();
        assert_eq!(output["ok"], false);
        assert_eq!(output["diagnostics"][0]["code"], "FAI0002");
    } else {
        assert!(String::from_utf8_lossy(&cold.stdout).contains("FAI0002"));
    }
}

#[test]
fn check_rejects_a_missing_file_selection() {
    missing_selection("check", "missing.fai", true);
}
#[test]
fn fmt_rejects_a_missing_file_selection() {
    missing_selection("fmt", "missing.fai", true);
}
#[test]
fn test_rejects_a_missing_file_selection() {
    missing_selection("test", "missing.fai", true);
}
#[test]
fn check_rejects_a_missing_directory_selection() {
    missing_selection("check", "missing/", false);
}
#[test]
fn fmt_rejects_a_missing_directory_selection() {
    missing_selection("fmt", "missing/", false);
}
#[test]
fn test_rejects_a_missing_directory_selection() {
    missing_selection("test", "missing/", false);
}

#[track_caller]
fn empty_selection(command: &str) {
    let daemon = Daemon::new(&format!("empty-selection-{command}"), &[]);
    std::fs::create_dir(daemon.workspace.join("empty")).unwrap();
    let cold = daemon.run(&[command, "--no-daemon"], &["./empty/"]);
    let warm = daemon.run(&[command], &["./empty/"]);
    assert!(
        cold.status.success(),
        "{}{}",
        String::from_utf8_lossy(&cold.stdout),
        String::from_utf8_lossy(&cold.stderr)
    );
    assert_eq!(warm.status.code(), cold.status.code());
    assert_eq!(test_report(&warm), test_report(&cold));
    assert_eq!(warm.stderr, cold.stderr);
}

#[test]
fn check_accepts_an_existing_empty_selection() {
    empty_selection("check");
}
#[test]
fn fmt_accepts_an_existing_empty_selection() {
    empty_selection("fmt");
}
#[test]
fn test_accepts_an_existing_empty_selection() {
    empty_selection("test");
}

#[test]
fn dot_prefixed_test_selection_executes_the_contract() {
    let daemon = Daemon::new("dot-selection", &[("Main.fai", "module Main\nexample: false\n")]);
    let cold = daemon.run(&["test", "--no-daemon"], &["./Main.fai"]);
    let warm = daemon.run(&["test"], &["./Main.fai"]);
    assert_eq!(cold.status.code(), Some(1));
    assert_eq!(warm.status.code(), cold.status.code());
    assert_eq!(test_report(&warm), test_report(&cold));
    assert!(String::from_utf8_lossy(&cold.stdout).contains("FAI6001"));
}

#[test]
fn dot_prefixed_check_selection_reports_the_files_error() {
    let daemon = Daemon::new("dot-check", &[("Main.fai", "module Main\nlet value = unknown\n")]);
    let cold = daemon.run(&["check", "--no-daemon"], &["./Main.fai"]);
    let warm = daemon.run(&["check"], &["./Main.fai"]);
    assert_eq!(cold.status.code(), Some(1));
    assert_eq!(warm.status.code(), cold.status.code());
    assert_eq!(warm.stdout, cold.stdout);
    assert!(String::from_utf8_lossy(&cold.stdout).contains("unknown"));
}

#[test]
fn dot_prefixed_fmt_selection_reports_and_formats_the_file() {
    let daemon = Daemon::new("dot-fmt", &[("Main.fai", "module Main\nlet value=1\n")]);
    let cold = daemon.run(&["fmt", "--check", "--no-daemon"], &["./Main.fai"]);
    let warm = daemon.run(&["fmt", "--check"], &["./Main.fai"]);
    assert_eq!(cold.status.code(), Some(1));
    assert_eq!(warm.status.code(), cold.status.code());
    assert_eq!(warm.stdout, cold.stdout);
    assert!(daemon.run(&["fmt"], &["./Main.fai"]).status.success());
    assert!(
        std::fs::read_to_string(daemon.workspace.join("Main.fai"))
            .unwrap()
            .contains("let value = 1")
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Only elapsed time is nondeterministic between equivalent test runs.
fn test_report(output: &Output) -> String {
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        if let Some(summary) = value.get_mut("summary").and_then(serde_json::Value::as_object_mut) {
            assert!(summary.remove("elapsedMs").unwrap().is_u64());
        }
        return serde_json::to_string(&value).unwrap();
    }
    stdout(output)
        .lines()
        .map(|line| line.split_once("; finished in ").map_or(line, |(prefix, _)| prefix))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_contract_progress(line: &str) -> bool {
    line.starts_with("test ") && !line.starts_with("test result:")
}

/// Parses the pid from `daemon status` output, or `None` if no daemon is running.
fn status_pid(daemon: &Daemon) -> Option<u32> {
    let text = stdout(&daemon.run(&["daemon", "status"], &[]));
    let after = text.split("pid ").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Parses "peak concurrency: N" from `daemon status` output.
fn status_peak_concurrency(daemon: &Daemon) -> Option<u64> {
    let text = stdout(&daemon.run(&["daemon", "status"], &[]));
    let after = text.split("peak concurrency: ").nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[test]
fn warm_check_matches_no_daemon() {
    let daemon = Daemon::new(
        "parity",
        &[(
            "Ok.fai",
            indoc! {r#"
                module Ok

                let x = 1
            "#},
        )],
    );

    // First check auto-spawns the daemon; second is warm. Both must match a
    // one-shot --no-daemon run byte-for-byte.
    let warm1 = daemon.run(&["check"], &["--message-format=json"]);
    assert!(warm1.status.success(), "stderr: {}", String::from_utf8_lossy(&warm1.stderr));
    let warm2 = daemon.run(&["check"], &["--message-format=json"]);
    let cold = daemon.run(&["check", "--no-daemon"], &["--message-format=json"]);

    assert_eq!(stdout(&warm1), stdout(&cold), "warm output must equal --no-daemon output");
    assert_eq!(stdout(&warm2), stdout(&cold), "a second warm run must also match");
}

#[test]
fn active_read_outlives_the_daemon_idle_timeout() {
    let daemon = Daemon::new("active-read", &[("Main.fai", "module Main\nlet value = 1\n")])
        .with_idle_timeout(1)
        .with_test_hold(6000);
    let output = daemon.run(&["check", "--no-examples"], &["Main.fai"]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(status_pid(&daemon).is_some(), "the daemon must survive active work");
}

#[test]
fn a_silent_run_worker_outlives_the_daemon_idle_timeout() {
    let source = "module Main\npublic main : Runtime -> Unit / { Clock, Console }\nlet main r =\n  let _ = r.clock.sleep 6000\n  r.console.writeLine \"finished\"\n";
    let daemon = Daemon::new("active-run", &[("Main.fai", source)])
        .with_idle_timeout(1)
        .with_run_timeout(15000);
    let output = daemon.run(&["run"], &["Main.fai"]);
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "finished\n");
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(status_pid(&daemon).is_some());
}

#[test]
fn a_test_batch_outlives_the_daemon_idle_timeout() {
    let source = "module Main\nlet spin n = spin (n + 1)\nexample: spin 0 = 0\nexample: true\n";
    let daemon = Daemon::new("active-test", &[("Main.fai", source)]).with_idle_timeout(1);
    let output = daemon
        .cmd()
        .env("FAI_TEST_TIMEOUT_MS", "6500")
        .args(["test", "-C"])
        .arg(&daemon.workspace)
        .arg("Main.fai")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout(&output).contains("1 passed; 0 failed; 0 crashed; 1 timed out"),
        "{}",
        stdout(&output)
    );
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(status_pid(&daemon).is_some());
}

#[test]
fn warm_check_tracks_source_addition_and_removal() {
    let daemon = Daemon::new("membership", &[("A.fai", "module A\nlet value = B.value\n")]);
    let missing = daemon.run(&["check"], &["--message-format=json"]);
    assert_eq!(missing.status.code(), Some(1));
    std::fs::write(daemon.workspace.join("B.fai"), "module B\npublic value : Int\nlet value = 7\n")
        .unwrap();
    let added = daemon.run(&["check"], &["--message-format=json"]);
    assert!(added.status.success(), "{}", stdout(&added));
    std::fs::remove_file(daemon.workspace.join("B.fai")).unwrap();
    let removed = daemon.run(&["check"], &["--message-format=json"]);
    let clean = daemon.run(&["check", "--no-daemon"], &["--message-format=json"]);
    assert_eq!(removed.status.code(), Some(1));
    assert_eq!(stdout(&removed), stdout(&clean));
}

#[test]
fn oversized_syntax_keeps_the_daemon_alive_and_formatting_non_destructive() {
    let text = format!("module Deep\nlet value = {}1{}\n", "(".repeat(10_000), ")".repeat(10_000));
    let daemon = Daemon::new("deep-syntax", &[("Deep.fai", &text)]);
    let checked = daemon.run(&["check", "--no-examples"], &["--message-format=json"]);
    assert_eq!(checked.status.code(), Some(1));
    assert!(stdout(&checked).contains("FAI1023"));
    let pid = status_pid(&daemon).unwrap();
    let formatted = daemon.run(&["fmt"], &["--message-format=json"]);
    assert_eq!(formatted.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(daemon.workspace.join("Deep.fai")).unwrap(), text);
    std::fs::write(daemon.workspace.join("Deep.fai"), "module Deep\nlet value = 1\n").unwrap();
    assert!(daemon.run(&["check", "--no-examples"], &[]).status.success());
    assert_eq!(status_pid(&daemon), Some(pid));
}

#[test]
fn warm_check_reports_a_failing_example() {
    let daemon = Daemon::new(
        "checkexample",
        &[(
            "Bad.fai",
            indoc! {r#"
                module Bad

                example: 1 = 2
            "#},
        )],
    );

    // The daemon evaluates closed `example` contracts (in an isolated worker) and
    // reports a failure as FAI6001 — byte-for-byte matching the --no-daemon run.
    let warm = daemon.run(&["check"], &["--message-format=json", "Bad.fai"]);
    let cold = daemon.run(&["check", "--no-daemon"], &["--message-format=json", "Bad.fai"]);
    assert_eq!(warm.status.code(), Some(1), "stderr: {}", String::from_utf8_lossy(&warm.stderr));
    let warm_out = stdout(&warm);
    assert!(warm_out.contains("FAI6001"), "expected FAI6001 in warm output: {warm_out}");
    assert_eq!(warm_out, stdout(&cold), "warm output must equal --no-daemon output");
}

/// A passing example, a `forall` that divides by a runtime zero so it aborts on
/// the first generated input, then a passing `forall`.
const CRASH: &str = indoc! {r#"
    module Crash

    example: 1 + 1 = 2
    forall n: 1 / (n - n) = 0
    forall xs: List.length xs >= 0
"#};

#[test]
fn warm_test_matches_no_daemon() {
    let daemon = Daemon::new("testparity", &[("Crash.fai", CRASH)]);

    // The daemon supervises isolated workers and streams per-contract events;
    // Its semantic report must equal the one-shot run, apart from elapsed time,
    // for both JSON and human output, even with a contract that aborts.
    let warm_json = daemon.run(&["test"], &["--message-format=json", "Crash.fai"]);
    let cold_json = daemon.run(&["test", "--no-daemon"], &["--message-format=json", "Crash.fai"]);
    assert_eq!(
        warm_json.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&warm_json.stderr)
    );
    assert_eq!(
        test_report(&warm_json),
        test_report(&cold_json),
        "warm JSON must equal --no-daemon"
    );

    let warm_human = daemon.run(&["test"], &["Crash.fai"]);
    let cold_human = daemon.run(&["test", "--no-daemon"], &["Crash.fai"]);
    assert_eq!(
        test_report(&warm_human),
        test_report(&cold_human),
        "warm human must equal --no-daemon"
    );
}

#[test]
fn subjectless_test_sources_survive_streaming_and_worker_resume() {
    let daemon = Daemon::new(
        "event-identity",
        &[
            ("A.fai", "module A\nexample: true\n"),
            ("B.fai", "module B\nforall n: 1 / (n - n) = 0\nexample: true\n"),
            (
                "C.fai",
                "module C\nforall f: call f\ncall : (Int -> Int) -> Bool\nlet call f = f 0 = 0\n",
            ),
        ],
    );
    let warm = daemon.run(&["test"], &["--message-format=json"]);
    let cold = daemon.run(&["test", "--no-daemon"], &["--message-format=json"]);
    assert_eq!(warm.status.code(), Some(1));
    assert_eq!(test_report(&warm), test_report(&cold));
    let report: serde_json::Value = serde_json::from_slice(&warm.stdout).unwrap();
    let events = report["events"].as_array().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|e| (e["source"].clone(), e["ordinal"].clone(), e["status"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                serde_json::json!({"file":"A.fai","origin":"user"}),
                serde_json::json!(0),
                serde_json::json!("passed")
            ),
            (
                serde_json::json!({"file":"B.fai","origin":"user"}),
                serde_json::json!(0),
                serde_json::json!("crashed")
            ),
            (
                serde_json::json!({"file":"B.fai","origin":"user"}),
                serde_json::json!(1),
                serde_json::json!("passed")
            ),
            (
                serde_json::json!({"file":"C.fai","origin":"user"}),
                serde_json::json!(0),
                serde_json::json!("notRun")
            ),
        ]
    );
    let warm = daemon.run(&["test"], &[]);
    let cold = daemon.run(&["test", "--no-daemon"], &[]);
    let text = stdout(&warm);
    assert_eq!(test_report(&warm), test_report(&cold));
    assert!(text.contains("test example [A.fai:2:1] ... ok"), "{text}");
    assert!(text.contains("test forall [B.fai:2:1] ... CRASHED"), "{text}");
    assert!(text.contains("test forall [C.fai:2:1] ... NOT RUN"), "{text}");
}

#[cfg(debug_assertions)]
#[track_caller]
fn live_contract_progress(no_daemon: bool, tag: &str) {
    use wait_timeout::ChildExt;

    struct ReleaseGate(PathBuf);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "release");
        }
    }

    let daemon = Daemon::new(tag, &[("Crash.fai", CRASH)]);
    let gate = ReleaseGate(daemon.workspace.join("continue"));
    let _ = std::fs::remove_file(&gate.0);
    let mut command = daemon.cmd();
    command
        .env("FAI_TEST_EVENT_GATE", &gate.0)
        .env("FAI_TEST_TIMEOUT_MS", "30000")
        .args(["test", "--color=never", "-C"])
        .arg(&daemon.workspace)
        .arg("Crash.fai")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_daemon {
        command.arg("--no-daemon");
    }
    let mut child = KillOnDrop(command.spawn().unwrap());
    let lines = read_lines(child.0.stdout.take().unwrap());
    let header = lines.recv_timeout(Duration::from_secs(20)).expect("selection header");
    assert!(header.starts_with("running 3 contracts"), "{header}");
    let first = loop {
        let line = lines.recv_timeout(Duration::from_secs(20)).expect("first live contract event");
        if is_contract_progress(&line) {
            break line;
        }
    };
    assert!(first.ends_with("... ok"), "{first}");
    assert!(child.0.try_wait().unwrap().is_none(), "the worker is still gated");
    std::fs::write(&gate.0, "release").unwrap();
    let status = child.0.wait_timeout(Duration::from_secs(20)).unwrap().expect("test run ends");
    assert_eq!(status.code(), Some(1));
    let mut output = vec![first];
    while let Ok(line) = lines.recv_timeout(Duration::from_secs(5)) {
        output.push(line);
    }
    let events: Vec<_> = output.iter().filter(|line| is_contract_progress(line)).collect();
    assert_eq!(events.len(), 3, "{output:?}");
    assert!(events[1].ends_with("... CRASHED"), "{output:?}");
    assert!(events[2].ends_with("... ok"), "{output:?}");
    assert!(output.iter().any(|line| line.contains("2 passed; 0 failed; 1 crashed")), "{output:?}");
}

#[cfg(debug_assertions)]
#[test]
fn daemon_contract_progress_arrives_before_the_next_contract_finishes() {
    live_contract_progress(false, "live-contract-daemon");
}

#[cfg(debug_assertions)]
#[test]
fn local_contract_progress_arrives_before_the_next_contract_finishes() {
    live_contract_progress(true, "live-contract-local");
}

#[cfg(debug_assertions)]
#[test]
fn test_report_keeps_the_source_revision_that_ran() {
    use wait_timeout::ChildExt;
    struct ReleaseGate(PathBuf);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "release");
        }
    }
    let source = "module M\nexample: true\nexample: false\n";
    let daemon = Daemon::new("test-revision", &[("M.fai", source)]);
    let gate = ReleaseGate(daemon.workspace.join("continue"));
    let _ = std::fs::remove_file(&gate.0);
    let mut child = KillOnDrop(
        daemon
            .cmd()
            .env("FAI_TEST_EVENT_GATE", &gate.0)
            .env("FAI_TEST_TIMEOUT_MS", "30000")
            .args(["test", "--color=never", "-C"])
            .arg(&daemon.workspace)
            .arg("M.fai")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let lines = read_lines(child.0.stdout.take().unwrap());
    loop {
        let line = lines.recv_timeout(Duration::from_secs(20)).unwrap();
        if is_contract_progress(&line) {
            assert!(line.ends_with("... ok"), "{line}");
            break;
        }
    }
    std::fs::write(daemon.workspace.join("M.fai"), format!("// 🌍 inserted\n\n\n{source}"))
        .unwrap();
    assert!(daemon.run(&["check", "--no-examples"], &["M.fai"]).status.success());
    std::fs::write(&gate.0, "release").unwrap();
    assert_eq!(child.0.wait_timeout(Duration::from_secs(20)).unwrap().unwrap().code(), Some(1));
    let mut output = String::new();
    while let Ok(line) = lines.recv_timeout(Duration::from_secs(5)) {
        output.push_str(&line);
        output.push('\n');
    }
    assert!(output.contains("M.fai:3:1"), "{output}");
}

#[test]
fn daemon_survives_a_trapping_contract() {
    let daemon = Daemon::new("survive", &[("Crash.fai", CRASH)]);

    // A contract that traps aborts its isolated worker, not the daemon.
    let out = daemon.run(&["test"], &["Crash.fai"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("FAI6003"), "expected a located abort: {}", stdout(&out));

    // The daemon is still alive and serving after the worker aborted.
    assert!(status_pid(&daemon).is_some(), "daemon must survive a trapping contract");
    let again = daemon.run(&["test"], &["--message-format=json", "Crash.fai"]);
    assert_eq!(again.status.code(), Some(1), "a second warm test still works");
}

#[test]
fn status_reports_running_then_stopped() {
    let daemon = Daemon::new(
        "lifecycle",
        &[(
            "Ok.fai",
            indoc! {r#"
                module Ok

                let x = 1
            "#},
        )],
    );

    // No daemon yet.
    let before = daemon.run(&["daemon", "status"], &[]);
    assert!(stdout(&before).contains("no daemon running"), "got: {}", stdout(&before));

    // A check spawns one; status then reports it.
    let _ = daemon.run(&["check"], &[]);
    let running = daemon.run(&["daemon", "status"], &[]);
    assert!(stdout(&running).contains("daemon running"), "got: {}", stdout(&running));

    // Stop it; status reports none again.
    let stopped = daemon.run(&["daemon", "stop"], &[]);
    assert!(stdout(&stopped).contains("daemon stopped"), "got: {}", stdout(&stopped));
    let after = daemon.run(&["daemon", "status"], &[]);
    assert!(stdout(&after).contains("no daemon running"), "got: {}", stdout(&after));
}

#[test]
fn status_reports_command_latency() {
    let daemon = Daemon::new(
        "latency",
        &[(
            "Ok.fai",
            indoc! {r#"
                module Ok

                let x = 1
            "#},
        )],
    );

    // Each `check` is a Command request the daemon serves and times; `status`
    // itself is not a Command, so it is not counted.
    let _ = daemon.run(&["check"], &[]);
    let _ = daemon.run(&["check"], &[]);

    let status = stdout(&daemon.run(&["daemon", "status"], &[]));
    assert!(status.contains("commands served:"), "got: {status}");
    assert!(
        !status.contains("commands served: 0"),
        "expected a nonzero command count, got: {status}"
    );
}

#[test]
fn warm_check_reflects_an_edit() {
    let daemon = Daemon::new(
        "filesync",
        &[(
            "Main.fai",
            indoc! {r#"
                module Main

                let x = 1
            "#},
        )],
    );

    // Warm up: clean.
    let clean = daemon.run(&["check"], &["--message-format=json"]);
    assert!(clean.status.success(), "stderr: {}", String::from_utf8_lossy(&clean.stderr));

    // Introduce a type error on disk; the warm daemon must re-sync and see it.
    std::fs::write(
        daemon.workspace.join("Main.fai"),
        indoc! {r#"
            module Main

            public f : Int -> Bool
            let f x = x + 1
        "#},
    )
    .unwrap();
    let dirty = daemon.run(&["check"], &["--message-format=json"]);
    assert_eq!(dirty.status.code(), Some(1), "expected a type error after the edit");
    assert!(stdout(&dirty).contains("FAI3004"), "got: {}", stdout(&dirty));
}

#[test]
fn daemon_survives_a_missing_signature_cycle_and_its_repair() {
    let a = "module A\npublic x : Int\nlet x = 1\n";
    let b = "module B\npublic y : Int\nlet y = A.x\n";
    let daemon = Daemon::new("missing-signatures", &[("A.fai", a), ("B.fai", b)]);
    assert!(daemon.run(&["check", "--no-examples"], &[]).status.success());
    let pid = status_pid(&daemon).unwrap();
    std::fs::write(daemon.workspace.join("A.fai"), "module A\npublic let x = B.y\n").unwrap();
    std::fs::write(daemon.workspace.join("B.fai"), "module B\npublic let y = A.x\n").unwrap();
    let bad = daemon.run(&["check", "--no-examples", "--message-format=json"], &[]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(bad.stderr.is_empty(), "{}", String::from_utf8_lossy(&bad.stderr));
    assert_eq!(stdout(&bad).matches("FAI3003").count(), 2);
    assert_eq!(status_pid(&daemon), Some(pid));
    std::fs::write(daemon.workspace.join("A.fai"), a).unwrap();
    std::fs::write(daemon.workspace.join("B.fai"), b).unwrap();
    assert!(daemon.run(&["check", "--no-examples"], &[]).status.success());
    assert_eq!(status_pid(&daemon), Some(pid));
}

const HELLO: &str = indoc! {r#"
    module Hello

    public main : Runtime -> Unit / { Console }
    let main runtime = runtime.console.writeLine "hi from run"
"#};

#[test]
fn run_streams_output_via_daemon() {
    let daemon = Daemon::new("runstream", &[("Hello.fai", HELLO)]);
    let out = daemon.run(&["run"], &["Hello.fai"]);
    assert_eq!(stdout(&out), "hi from run\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0));
}

const STDIN_ECHO: &str = "module Main\necho : Console -> Unit / { Console }\nlet echo c =\n  match c.readLine () with\n  | Err e -> c.writeError e\n  | Ok None -> ()\n  | Ok (Some line) ->\n    let ignored = c.writeLine line\n    echo c\npublic main : Runtime -> Unit / { Console }\nlet main r = echo r.console\n";

#[track_caller]
fn input_output(daemon: &Daemon, bytes: Vec<u8>, no_daemon: bool) -> Vec<u8> {
    let mut command = daemon.cmd();
    command.env("FAI_RUN_TIMEOUT_MS", "20000");
    command
        .args(["run", "-C"])
        .arg(&daemon.workspace)
        .arg("Main.fai")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_daemon {
        command.arg("--no-daemon");
    }
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || input.write_all(&bytes));
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    writer.join().unwrap().unwrap();
    output.stdout
}

#[test]
fn daemon_stdin_matches_local_unicode_lines_and_final_eof() {
    let daemon = Daemon::new("stdin-unicode", &[("Main.fai", STDIN_ECHO)]);
    let input = format!("{}🌍\nzażółć\nlast", "x".repeat(8191)).into_bytes();
    let local = input_output(&daemon, input.clone(), true);
    let warm = input_output(&daemon, input.clone(), false);
    assert_eq!(warm, local);
    assert_eq!(warm, [input, b"\n".to_vec()].concat());
}

#[test]
fn daemon_stdin_empty_input_closes_the_worker_pipe() {
    let daemon = Daemon::new("stdin-empty", &[("Main.fai", STDIN_ECHO)]);
    assert!(input_output(&daemon, Vec::new(), false).is_empty());
}

#[test]
fn daemon_stdin_feeds_lazy_line_streams() {
    let source = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  match Stream.toStdoutLines r (Stream.stdinLines r) with\n  | Ok u -> ()\n  | Err e -> r.console.writeError e\n";
    let daemon = Daemon::new("stdin-lazy", &[("Main.fai", source)]);
    let input = "one\n世界\nthree\n".as_bytes().to_vec();
    assert_eq!(input_output(&daemon, input.clone(), false), input);
}

#[test]
fn daemon_stdin_and_stdout_progress_together_for_large_streams() {
    let daemon = Daemon::new("stdin-large", &[("Main.fai", STDIN_ECHO)]);
    let input = ("z".repeat(8190) + "\n").repeat(64).into_bytes();
    assert_eq!(input_output(&daemon, input.clone(), false), input);
}

#[test]
fn daemon_stdin_is_duplex_before_eof() {
    use wait_timeout::ChildExt;
    let daemon = Daemon::new("stdin-duplex", &[("Main.fai", STDIN_ECHO)]);
    let mut child = KillOnDrop(
        daemon
            .cmd()
            .args(["run", "-C"])
            .arg(&daemon.workspace)
            .arg("Main.fai")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let output = read_lines(child.0.stdout.take().unwrap());
    input.write_all(b"first\n").unwrap();
    assert_eq!(output.recv_timeout(Duration::from_secs(20)).unwrap(), "first");
    input.write_all("second🌍\n".as_bytes()).unwrap();
    assert_eq!(output.recv_timeout(Duration::from_secs(5)).unwrap(), "second🌍");
    drop(input);
    assert!(child.0.wait_timeout(Duration::from_secs(10)).unwrap().unwrap().success());
}

#[test]
fn a_finished_run_does_not_wait_for_client_stdin() {
    use wait_timeout::ChildExt;
    let daemon = Daemon::new(
        "stdin-open-exit",
        &[("Main.fai", "module Main\npublic main : Runtime -> Unit\nlet main r = ()\n")],
    );
    let mut child = KillOnDrop(
        daemon
            .cmd()
            .args(["run", "-C"])
            .arg(&daemon.workspace)
            .arg("Main.fai")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let _held_open = child.0.stdin.take().unwrap();
    assert!(
        child
            .0
            .wait_timeout(Duration::from_secs(20))
            .unwrap()
            .expect("stdin does not delay exit")
            .success()
    );
}

#[test]
fn a_timed_out_run_does_not_wait_for_client_stdin() {
    use wait_timeout::ChildExt;
    let daemon = Daemon::new("stdin-open-timeout", &[("Main.fai", "module Main\nlet spin n = spin (n + 1)\npublic main : Runtime -> Unit\nlet main r = spin 0\n")]).with_run_timeout(500);
    let mut child = KillOnDrop(
        daemon
            .cmd()
            .args(["run", "-C"])
            .arg(&daemon.workspace)
            .arg("Main.fai")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let _held_open = child.0.stdin.take().unwrap();
    assert_eq!(
        child
            .0
            .wait_timeout(Duration::from_secs(20))
            .unwrap()
            .expect("stdin does not delay timeout")
            .code(),
        Some(124)
    );
}

#[test]
fn a_mid_run_daemon_disconnect_does_not_repeat_side_effects() {
    use wait_timeout::ChildExt;
    let daemon = Daemon::new("run-no-replay", &[]);
    let counter = daemon.workspace.join("count.txt");
    let path = serde_json::to_string(counter.to_str().unwrap()).unwrap();
    let source = format!(
        "module Main\npublic main : Runtime -> Unit / {{ Clock, Console, FileSystem }}\nlet main r =\n  let path = Result.withDefault Path.empty (Path.fromString {path})\n  let previous = Result.withDefault \"\" (r.fs.readFile path)\n  let written = r.fs.writeFile path (previous ++ \"x\")\n  let printed = r.console.writeLine \"READY\"\n  r.clock.sleep 500\n"
    );
    std::fs::write(daemon.workspace.join("Main.fai"), source).unwrap();
    let mut client = KillOnDrop(
        daemon
            .cmd()
            .args(["run", "-C"])
            .arg(&daemon.workspace)
            .arg("Main.fai")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let lines = read_lines(client.0.stdout.take().unwrap());
    assert_eq!(lines.recv_timeout(Duration::from_secs(15)).unwrap(), "READY");
    assert!(daemon.run(&["daemon", "stop"], &[]).status.success());
    let status = client
        .0
        .wait_timeout(Duration::from_secs(10))
        .unwrap()
        .expect("disconnected run terminates");
    assert!(!status.success());
    let mut error = String::new();
    client.0.stderr.take().unwrap().read_to_string(&mut error).unwrap();
    assert!(error.contains("command was not restarted"), "{error}");
    assert!(error.contains("FAI0008"), "{error}");
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "x");
}

#[test]
fn disconnecting_a_run_client_reaps_its_silent_worker() {
    let source = "module Main\npublic main : Runtime -> Unit / { Clock, Console, Net }\nlet main r =\n  match r.net.listen 0 with\n  | Err e -> r.console.writeLine e\n  | Ok listener ->\n    let printed = r.console.writeLine (Int.toString (r.net.localPort listener))\n    let waited = r.clock.sleep 60000\n    r.console.writeLine (Int.toString (r.net.localPort listener))\n";
    let daemon = Daemon::new("run-disconnect-reap", &[("Main.fai", source)]);
    let mut client = KillOnDrop(
        daemon
            .cmd()
            .args(["run", "-C"])
            .arg(&daemon.workspace)
            .arg("Main.fai")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let lines = read_lines(client.0.stdout.take().unwrap());
    let port: u16 = lines.recv_timeout(Duration::from_secs(15)).unwrap().parse().unwrap();
    client.0.kill().unwrap();
    client.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(listener) = std::net::TcpListener::bind(("0.0.0.0", port)) {
            drop(listener);
            break;
        }
        assert!(Instant::now() < deadline, "worker retained its listener after client disconnect");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(status_pid(&daemon).is_some());
}

#[cfg(unix)]
#[test]
fn pre_submission_connection_failure_still_runs_the_fallback() {
    let daemon = Daemon::new("run-pre-submit", &[("Hello.fai", HELLO)]);
    let blocked = daemon.workspace.join("blocked-runtime-dir");
    std::fs::write(&blocked, "not a directory").unwrap();
    let output = daemon
        .cmd()
        .env("FAI_RUNTIME_DIR", blocked)
        .args(["run", "-C"])
        .arg(&daemon.workspace)
        .arg("Hello.fai")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"hi from run\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("running in-process"));
}

#[test]
fn run_timeout_is_reaped_and_daemon_survives() {
    // A tail loop cannot finish or exhaust the stack, regardless of CPU speed.
    // The daemon must reap it (exit 124) and continue serving requests.
    let spin = indoc! {r#"
        module Main

        spin : Int -> Int
        let spin n = spin n

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine (Int.toString (spin 0))
    "#};
    let daemon = Daemon::new("timeout", &[("Main.fai", spin)]).with_run_timeout(500);

    let run = daemon.run(&["run"], &["Main.fai"]);
    assert_eq!(
        run.status.code(),
        Some(124),
        "expected a timeout exit; stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    // The daemon survived the reaped worker: a later command still works.
    let check = daemon.run(&["check"], &["--message-format=json"]);
    assert!(check.status.success(), "daemon must survive a reaped run worker");
}

/// On Windows the worker's resource limits are enforced by a Job Object (the peer
/// of the Unix `setrlimit` guard). A run that commits far past a low
/// `FAI_RUN_AS_BYTES` cap is terminated by the OS before it can exhaust host
/// memory, and the daemon survives to serve the next command.
#[cfg(windows)]
#[test]
fn run_memory_limit_is_enforced_and_daemon_survives() {
    // Build a list of ~100M cons cells: several GiB, dwarfing the 128 MiB cap, so
    // the committed-memory limit (not the generous wall-clock reaper) is what stops
    // the worker, and it does so within a fraction of a second.
    let hog = indoc! {r#"
        module Main

        let build n acc = if n = 0 then acc else build (n - 1) (n :: acc)

        public main : Runtime -> Unit / { Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (List.length (build 100000000 [])))
    "#};
    let daemon = Daemon::new("memlimit", &[("Main.fai", hog)]).with_run_as_bytes(128 * 1024 * 1024);

    let run = daemon.run(&["run"], &["Main.fai"]);
    assert_ne!(run.status.code(), Some(0), "a worker past the memory cap must not succeed");

    // The daemon survived the killed worker: a later command still works.
    assert!(status_pid(&daemon).is_some(), "daemon must survive a memory-limited worker");
    let check = daemon.run(&["check"], &["--message-format=json"]);
    assert!(check.status.success(), "daemon must serve after a memory-limited worker");
}

#[test]
fn query_via_daemon_matches_no_daemon() {
    let daemon = Daemon::new(
        "query",
        &[(
            "Calc.fai",
            indoc! {r#"
                module Calc

                public add : Int -> Int -> Int
                let add x y = x + y
            "#},
        )],
    );
    let warm = daemon.run(&["query", "type", "Calc.add"], &[]);
    assert!(warm.status.success(), "stderr: {}", String::from_utf8_lossy(&warm.stderr));
    let cold = daemon.run(&["query", "type", "Calc.add", "--no-daemon"], &[]);
    assert_eq!(stdout(&warm), stdout(&cold), "warm query must match --no-daemon");
}

#[test]
fn unicode_character_positions_match_through_the_daemon() {
    let daemon = Daemon::new(
        "unicode-query",
        &[("Unicode.fai", "module Unicode\r\n(* é😀 *)let answer = 42\r\n")],
    );
    let warm = daemon.run(&["query", "def", "Unicode.fai:2:9"], &[]);
    let cold = daemon.run(&["query", "def", "Unicode.fai:2:9", "--no-daemon"], &[]);
    assert!(warm.status.success(), "{}{}", stdout(&warm), String::from_utf8_lossy(&warm.stderr));
    assert!(cold.status.success());
    assert_eq!(warm.stdout, cold.stdout);
    let value: serde_json::Value = serde_json::from_slice(&warm.stdout).unwrap();
    assert_eq!(value["target"]["name"], "answer");
}

const TWO_MODULES: &[(&str, &str)] = &[
    ("A.fai", "module A\n\npublic a : Int\nlet a = 1\n"),
    ("B.fai", "module B\n\npublic b : Int\nlet b = 2\n"),
];

#[test]
fn concurrent_checks_match_no_daemon() {
    let daemon = Daemon::new("concurrent", TWO_MODULES);
    // A one-shot baseline, plus a warm-up that spawns the daemon so the burst
    // below races neither the spawn nor a cold cache.
    let cold = stdout(&daemon.run(&["check", "--no-daemon"], &["--message-format=json"]));
    let _ = daemon.run(&["check"], &["--message-format=json"]);

    // Fire several checks at the one daemon at once; each must return exactly the
    // result a one-shot run would (the daemon serves them on per-connection
    // threads, concurrently, on independent snapshots).
    const N: usize = 6;
    let outs: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..N)
            .map(|_| {
                scope.spawn(|| {
                    let out = daemon.run(&["check"], &["--message-format=json"]);
                    assert!(
                        out.status.success(),
                        "stderr: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    stdout(&out)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for out in &outs {
        assert_eq!(*out, cold, "each concurrent warm check must equal --no-daemon output");
    }
}

#[test]
fn peak_concurrency_exceeds_one_under_load() {
    // The test-only hold makes each off-lock read linger, so a concurrent burst is
    // guaranteed to overlap; `daemon status` then reports a peak concurrency > 1,
    // proving reads are served in parallel rather than strictly serialized.
    let daemon = Daemon::new("peak", &[("Ok.fai", "module Ok\n\nlet x = 1\n")]).with_test_hold(300);
    let _ = daemon.run(&["check"], &["--message-format=json"]); // spawn (with the hold env)

    const N: usize = 4;
    std::thread::scope(|scope| {
        for _ in 0..N {
            scope.spawn(|| {
                let out = daemon.run(&["check"], &["--message-format=json"]);
                assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
            });
        }
    });

    let peak = status_peak_concurrency(&daemon).expect("status reports peak concurrency");
    assert!(peak >= 2, "expected reads served in parallel (peak >= 2), got {peak}");
}

#[test]
fn concurrent_reads_with_interleaved_edit_stay_valid() {
    let daemon = Daemon::new("interleave", &[("M.fai", "module M\n\nlet x = 1\n")]);
    let _ = daemon.run(&["check"], &["--message-format=json"]); // spawn

    let clean = "module M\n\nlet x = 1\n";
    let bad = "module M\n\npublic f : Int -> Bool\nlet f x = x + 1\n";

    std::thread::scope(|scope| {
        // An editor toggling the file on disk under the readers.
        scope.spawn(|| {
            for i in 0..20 {
                let content = if i % 2 == 0 { bad } else { clean };
                std::fs::write(daemon.workspace.join("M.fai"), content).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            std::fs::write(daemon.workspace.join("M.fai"), clean).unwrap();
        });
        // Readers checking throughout: every result is a well-formed check (a clean
        // exit 0 or a typed exit 1), never a crash, hang, or torn output.
        for _ in 0..2 {
            scope.spawn(|| {
                for _ in 0..8 {
                    let out = daemon.run(&["check"], &["--message-format=json"]);
                    let code = out.status.code();
                    assert!(
                        matches!(code, Some(0) | Some(1)),
                        "unexpected exit {code:?}; stderr: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    let value: serde_json::Value = serde_json::from_slice(&out.stdout)
                        .expect("a concurrent check must emit valid JSON");
                    assert!(value.get("ok").is_some(), "check output has an `ok` field: {value}");
                }
            });
        }
    });

    // The daemon is healthy after the storm and reflects the final (clean) file.
    let final_check = daemon.run(&["check"], &["--message-format=json"]);
    assert!(
        final_check.status.success(),
        "final check must be clean; stderr: {}",
        String::from_utf8_lossy(&final_check.stderr)
    );
}

#[test]
fn fmt_via_daemon_writes_the_file() {
    let daemon = Daemon::new("fmt", &[("W.fai", "module W\nlet   x=1\n")]);
    let out = daemon.run(&["fmt", "--message-format=json"], &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    // The daemon (not the client) performed the rewrite on disk.
    let on_disk = std::fs::read_to_string(daemon.workspace.join("W.fai")).unwrap();
    assert_eq!(on_disk, "module W\n\nlet x = 1\n");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(value["changed"][0], "W.fai");
}

#[test]
fn restart_replaces_the_daemon() {
    let daemon = Daemon::new(
        "restart",
        &[(
            "Ok.fai",
            indoc! {r#"
                module Ok

                let x = 1
            "#},
        )],
    );
    let _ = daemon.run(&["check"], &[]); // spawn
    let mut prev = status_pid(&daemon).expect("a daemon should be running");

    // Restart is synchronous: it stops the old daemon (waiting until its endpoint
    // refuses) and only returns once a fresh, distinct one is connectable. Repeat
    // it to stress the stop→start handoff — a regression races the old shutdown
    // and the status that follows finds no (or the old) daemon.
    for _ in 0..5 {
        let restart = daemon.run(&["daemon", "restart"], &[]);
        assert!(stdout(&restart).contains("restarted"), "got: {}", stdout(&restart));

        let next = status_pid(&daemon).expect("a daemon should be running after restart");
        assert_ne!(prev, next, "restart must replace the daemon process");
        prev = next;
    }
}

#[test]
fn run_compile_error_exits_four_via_daemon() {
    // `writeLine` expects a String; passing an Int is a type error, so the bundle
    // never builds: the daemon streams the diagnostic and the run exits 4.
    let bad = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
        let main r = r.console.writeLine (1 + 2)
    "#};
    let daemon = Daemon::new("runbad", &[("Main.fai", bad)]);
    let out = daemon.run(&["run"], &["Main.fai"]);
    assert_eq!(out.status.code(), Some(4), "a compile error exits 4");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("FAI3"),
        "expected a type diagnostic; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn build_via_daemon_produces_a_runnable_binary() {
    let src = indoc! {r#"
        module Calc

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine (Int.toString (40 + 2))
    "#};
    let daemon = Daemon::new("build", &[("Calc.fai", src)]);
    let exe = daemon.workspace.join("calc");

    let build = daemon.run(&["build"], &["Calc.fai", "--out", exe.to_str().unwrap()]);
    assert!(build.status.success(), "stderr: {}", String::from_utf8_lossy(&build.stderr));

    let run = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    assert_eq!(run.status.code(), Some(0));
}

/// Kills the wrapped child on drop, so a panicking assertion never leaks the
/// long-lived `daemon tap` process.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reads `reader` line by line on a background thread, forwarding each line over
/// a channel so a test can poll with a timeout instead of blocking on the pipe.
fn read_lines<R: Read + Send + 'static>(reader: R) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

#[test]
fn a_passive_tap_does_not_prevent_genuine_idle_shutdown() {
    let daemon = Daemon::new("idle-tap", &[]).with_idle_timeout(1);
    let mut child = daemon
        .cmd()
        .args(["daemon", "tap", "-C"])
        .arg(&daemon.workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let frames = read_lines(child.stdout.take().unwrap());
    let notices = read_lines(child.stderr.take().unwrap());
    let _tap = KillOnDrop(child);
    let ready = notices.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(ready.contains("tapping daemon traffic"), "{ready}");
    assert_eq!(
        frames.recv_timeout(Duration::from_secs(15)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
    assert!(status_pid(&daemon).is_none());
}

#[test]
fn tap_streams_decoded_traffic() {
    let daemon = Daemon::new(
        "tap",
        &[(
            "Ok.fai",
            indoc! {r#"
                module Ok

                let x = 1
            "#},
        )],
    );

    // Spawn `fai daemon tap` as a child: it auto-spawns the daemon, prints a
    // readiness notice on stderr, then streams a JSON decode of traffic on stdout.
    let mut command = daemon.cmd();
    command
        .args(["daemon", "tap"])
        .arg("-C")
        .arg(&daemon.workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let frames = read_lines(child.stdout.take().unwrap());
    let notices = read_lines(child.stderr.take().unwrap());
    let _tap = KillOnDrop(child);

    // Wait until the subscription is acknowledged (the readiness notice), so the
    // traffic generated next cannot slip past a not-yet-registered tap.
    let ready = notices.recv_timeout(Duration::from_secs(10));
    assert!(
        matches!(&ready, Ok(line) if line.contains("tapping daemon traffic")),
        "expected a readiness notice, got: {ready:?}"
    );

    // Generate traffic on a separate connection; the tap must decode it.
    let check = daemon.run(&["check"], &["--message-format=json"]);
    assert!(check.status.success(), "stderr: {}", String::from_utf8_lossy(&check.stderr));

    // Collect tapped lines until the decoded `check` command appears, or time out.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen: Vec<String> = Vec::new();
    let command_line = loop {
        match frames.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                let is_command = line.contains(r#""Command""#);
                seen.push(line.clone());
                if is_command {
                    break Some(line);
                }
            }
            Err(_) if Instant::now() >= deadline => break None,
            Err(_) => {}
        }
    };
    let command_line = command_line
        .unwrap_or_else(|| panic!("tap did not decode the check command; saw: {seen:#?}"));

    // A tapped line is `#<conn> <arrow> <json>`: the arrow shows direction and the
    // remainder is the valid JSON decode of the frame.
    assert!(command_line.contains("->"), "the check request is inbound: {command_line}");
    let json = command_line.splitn(3, ' ').nth(2).expect("a frame carries a json payload");
    let value: serde_json::Value = serde_json::from_str(json).expect("tap payload is valid JSON");
    assert!(value.get("Command").is_some(), "decode names the request variant: {value}");
}
