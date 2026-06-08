//! End-to-end daemon tests, spawning the real `fai` binary so the auto-spawned
//! daemon and the worker subprocesses are exercised.
//!
//! Each test is isolated: a unique workspace, a unique `FAI_RUNTIME_DIR` (socket
//! directory) and `FAI_CACHE_DIR`, all inherited by the self-spawned daemon. A
//! [`Daemon`] guard stops its daemon on drop so none leak across the run.
//!
//! Disabled on Windows: the spawned daemon inherits and holds the client's
//! stdio pipes, so a client that captures output blocks until the daemon's idle
//! timeout instead of returning promptly, and `daemon status`/`restart` cannot
//! reach the running daemon. The daemon path needs a Windows fix (spawn the
//! daemon without inheriting the client's handles) before these run there.
#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use indoc::indoc;

fn unique(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-daemon-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A short base directory for the daemon's Unix socket. macOS caps socket paths
/// at ~104 bytes, and its default temp dir (`/var/folders/…`) is long enough
/// that appending the per-workspace socket file overflows the limit — so the
/// socket directory uses a short, fixed base instead of [`unique`]. (On Windows
/// the endpoint is a namespaced pipe, so the path length is irrelevant.)
fn unique_socket_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    base.join(format!("fai-rt-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)))
}

/// An isolated workspace with its own daemon endpoint and cache.
struct Daemon {
    workspace: PathBuf,
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
    run_timeout_ms: Option<u64>,
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
            runtime_dir: unique_socket_dir(),
            cache_dir: unique(&format!("{name}-cache")),
            run_timeout_ms: None,
        }
    }

    /// Sets the supervised-run wall-clock timeout (inherited by the daemon).
    fn with_run_timeout(mut self, ms: u64) -> Self {
        self.run_timeout_ms = Some(ms);
        self
    }

    /// A `fai` command with this workspace's isolated environment.
    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .env("FAI_RUNTIME_DIR", &self.runtime_dir)
            .env("FAI_CACHE_DIR", &self.cache_dir)
            .env("FAI_DAEMON_IDLE_TIMEOUT", "60");
        if let Some(ms) = self.run_timeout_ms {
            command.env("FAI_RUN_TIMEOUT_MS", ms.to_string());
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Parses the pid from `daemon status` output, or `None` if no daemon is running.
fn status_pid(daemon: &Daemon) -> Option<u32> {
    let text = stdout(&daemon.run(&["daemon", "status"], &[]));
    let after = text.split("pid ").nth(1)?;
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

const HELLO: &str = indoc! {r#"
    module Hello

    public main : Runtime -> Unit
    let main runtime = runtime.console.writeLine "hi from run"
"#};

#[test]
fn run_streams_output_via_daemon() {
    let daemon = Daemon::new("runstream", &[("Hello.fai", HELLO)]);
    let out = daemon.run(&["run"], &["Hello.fai"]);
    assert_eq!(stdout(&out), "hi from run\n", "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn run_timeout_is_reaped_and_daemon_survives() {
    // Naive fib is exponential-time but shallow-stack, so it runs well past the
    // timeout without crashing — the daemon must reap it (exit 124) and live on.
    let fib = indoc! {r#"
        module Main

        let fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (fib 40))
    "#};
    let daemon = Daemon::new("timeout", &[("Main.fai", fib)]).with_run_timeout(500);

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
    let pid1 = status_pid(&daemon).expect("a daemon should be running");

    let restart = daemon.run(&["daemon", "restart"], &[]);
    assert!(stdout(&restart).contains("restarted"), "got: {}", stdout(&restart));

    let pid2 = status_pid(&daemon).expect("a daemon should be running after restart");
    assert_ne!(pid1, pid2, "restart must replace the daemon process");
}

#[test]
fn run_compile_error_exits_four_via_daemon() {
    // `writeLine` expects a String; passing an Int is a type error, so the bundle
    // never builds: the daemon streams the diagnostic and the run exits 4.
    let bad = indoc! {r#"
        module Main

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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
