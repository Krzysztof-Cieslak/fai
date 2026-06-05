//! End-to-end daemon tests, spawning the real `fai` binary so the auto-spawned
//! daemon and the worker subprocesses are exercised.
//!
//! Each test is isolated: a unique workspace, a unique `FAI_RUNTIME_DIR` (socket
//! directory) and `FAI_CACHE_DIR`, all inherited by the self-spawned daemon. A
//! [`Daemon`] guard stops its daemon on drop so none leak across the run.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn unique(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-daemon-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// An isolated workspace with its own daemon endpoint and cache.
struct Daemon {
    workspace: PathBuf,
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Daemon {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let workspace = unique(&format!("{name}-ws"));
        std::fs::create_dir_all(&workspace).unwrap();
        for (file, contents) in files {
            std::fs::write(workspace.join(file), contents).unwrap();
        }
        Self {
            workspace,
            runtime_dir: unique(&format!("{name}-rt")),
            cache_dir: unique(&format!("{name}-cache")),
        }
    }

    /// A `fai` command with this workspace's isolated environment.
    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .env("FAI_RUNTIME_DIR", &self.runtime_dir)
            .env("FAI_CACHE_DIR", &self.cache_dir)
            .env("FAI_DAEMON_IDLE_TIMEOUT", "60");
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn warm_check_matches_no_daemon() {
    let daemon = Daemon::new("parity", &[("Ok.fai", "module Ok\n\nlet x = 1\n")]);

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
fn status_reports_running_then_stopped() {
    let daemon = Daemon::new("lifecycle", &[("Ok.fai", "module Ok\n\nlet x = 1\n")]);

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
fn warm_check_reflects_an_edit() {
    let daemon = Daemon::new("filesync", &[("Main.fai", "module Main\n\nlet x = 1\n")]);

    // Warm up: clean.
    let clean = daemon.run(&["check"], &["--message-format=json"]);
    assert!(clean.status.success(), "stderr: {}", String::from_utf8_lossy(&clean.stderr));

    // Introduce a type error on disk; the warm daemon must re-sync and see it.
    std::fs::write(
        daemon.workspace.join("Main.fai"),
        "module Main\n\npublic f : Int -> Bool\nlet f x = x + 1\n",
    )
    .unwrap();
    let dirty = daemon.run(&["check"], &["--message-format=json"]);
    assert_eq!(dirty.status.code(), Some(1), "expected a type error after the edit");
    assert!(stdout(&dirty).contains("FAI3004"), "got: {}", stdout(&dirty));
}

#[test]
fn build_via_daemon_produces_a_runnable_binary() {
    let src = "module Calc\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (40 + 2))\n";
    let daemon = Daemon::new("build", &[("Calc.fai", src)]);
    let exe = daemon.workspace.join("calc");

    let build = daemon.run(&["build"], &["Calc.fai", "--out", exe.to_str().unwrap()]);
    assert!(build.status.success(), "stderr: {}", String::from_utf8_lossy(&build.stderr));

    let run = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    assert_eq!(run.status.code(), Some(0));
}
