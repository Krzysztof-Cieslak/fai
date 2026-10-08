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

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

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
    run_as_bytes: Option<u64>,
    test_hold_ms: Option<u64>,
    idle_timeout_secs: u64,
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
    assert_eq!(warm.stdout, cold.stdout);
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
    assert_eq!(warm.stdout, cold.stdout);
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
    assert!(stdout(&output).contains("1 passed, 1 failed"), "{}", stdout(&output));
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
    // its rendered report must equal the one-shot --no-daemon run byte-for-byte,
    // for both JSON and human output, even with a contract that aborts.
    let warm_json = daemon.run(&["test"], &["--message-format=json", "Crash.fai"]);
    let cold_json = daemon.run(&["test", "--no-daemon"], &["--message-format=json", "Crash.fai"]);
    assert_eq!(
        warm_json.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&warm_json.stderr)
    );
    assert_eq!(stdout(&warm_json), stdout(&cold_json), "warm JSON must equal --no-daemon");

    let warm_human = daemon.run(&["test"], &["Crash.fai"]);
    let cold_human = daemon.run(&["test", "--no-daemon"], &["Crash.fai"]);
    assert_eq!(stdout(&warm_human), stdout(&cold_human), "warm human must equal --no-daemon");
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

#[test]
fn a_mid_run_daemon_disconnect_does_not_repeat_side_effects() {
    use wait_timeout::ChildExt;
    let daemon = Daemon::new("run-no-replay", &[]);
    let counter = daemon.workspace.join("count.txt");
    let path = serde_json::to_string(counter.to_str().unwrap()).unwrap();
    let source = format!(
        "module Main\npublic main : Runtime -> Unit / {{ Clock, Console, FileSystem }}\nlet main r =\n  let previous = Result.withDefault \"\" (r.fs.readFile {path})\n  let written = r.fs.writeFile {path} (previous ++ \"x\")\n  let printed = r.console.writeLine \"READY\"\n  r.clock.sleep 500\n"
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
