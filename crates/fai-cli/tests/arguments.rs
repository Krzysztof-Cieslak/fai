//! Application argv parity across native binaries and both JIT worker paths.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    root: PathBuf,
    endpoint: PathBuf,
}

impl Fixture {
    fn new(concurrent: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name =
            format!("fai-argv-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let root = std::env::temp_dir().join(&name);
        let endpoint = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() }
            .join(format!("{name}-rt"));
        std::fs::create_dir_all(&root).unwrap();
        let (effects, body) = if concurrent {
            (
                "Console, Concurrency, Env",
                "runtime.concurrency.scope (fun nursery -> runtime.concurrency.await (runtime.concurrency.spawn nursery (fun u -> runtime.env.args ())))",
            )
        } else {
            ("Console, Env", "runtime.env.args ()")
        };
        let source = format!(
            "module Main\npublic main : Runtime -> Unit / {{ {effects} }}\nlet main runtime =\n  let args = {body}\n  runtime.console.writeLine (Int.toString (List.length args) ++ \":\" ++ String.join \"|\" args)\n"
        );
        std::fs::write(root.join("Main.fai"), source).unwrap();
        Self { root, endpoint }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .arg("-C")
            .arg(&self.root)
            .env("FAI_RUNTIME_DIR", &self.endpoint)
            .env("FAI_CACHE_DIR", self.root.join("cache"))
            .env("FAI_DAEMON_IDLE_TIMEOUT", "60");
        command
    }

    fn run(&self, mode: Mode, args: &[&str]) -> Output {
        match mode {
            Mode::Native => {
                let executable = self.root.join(format!("program{}", std::env::consts::EXE_SUFFIX));
                let built = self
                    .command()
                    .args(["build", "--no-daemon", "Main.fai", "--out"])
                    .arg(&executable)
                    .output()
                    .unwrap();
                assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
                Command::new(executable).args(args).output().unwrap()
            }
            Mode::Worker | Mode::Daemon => {
                let mut command = self.command();
                command.arg("run");
                if matches!(mode, Mode::Worker) {
                    command.arg("--no-daemon");
                }
                command.arg("Main.fai").arg("--").args(args).output().unwrap()
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.command().args(["daemon", "stop"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.endpoint);
    }
}

enum Mode {
    Native,
    Worker,
    Daemon,
}

#[track_caller]
fn assert_output(output: Output, args: &[&str]) {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, format!("{}:{}\n", args.len(), args.join("|")).as_bytes());
}

#[track_caller]
fn check_args(mode: Mode, args: &[&str]) {
    let fixture = Fixture::new(false);
    assert_output(fixture.run(mode, args), args);
}

const MIXED: &[&str] = &["hello world", "", "café😀", "--help", "--", "\"quoted\"", "line\nbreak"];

#[test]
fn native_preserves_application_arguments() {
    check_args(Mode::Native, MIXED);
}

#[test]
fn worker_preserves_application_arguments() {
    check_args(Mode::Worker, MIXED);
}

#[test]
fn daemon_preserves_application_arguments() {
    check_args(Mode::Daemon, MIXED);
}

#[test]
fn native_without_arguments_sees_an_empty_list() {
    check_args(Mode::Native, &[]);
}

#[test]
fn worker_without_arguments_sees_an_empty_list() {
    check_args(Mode::Worker, &[]);
}

#[test]
fn daemon_without_arguments_sees_an_empty_list() {
    check_args(Mode::Daemon, &[]);
}

#[test]
fn daemon_arguments_are_visible_in_spawned_tasks() {
    let fixture = Fixture::new(true);
    assert_output(fixture.run(Mode::Daemon, MIXED), MIXED);
}

#[test]
fn warm_daemon_uses_each_requests_own_arguments() {
    let fixture = Fixture::new(false);
    assert_output(fixture.run(Mode::Daemon, MIXED), MIXED);
    assert_output(fixture.run(Mode::Daemon, &[]), &[]);
}
