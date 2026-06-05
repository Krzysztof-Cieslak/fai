//! End-to-end tests of the `fai` binary's `run` and `build` commands, spawning
//! the real executable (so the `fai run` worker subprocess is exercised).

use std::path::PathBuf;
use std::process::Command;

fn fai() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fai"))
}

fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fai-cli-run-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, contents) in files {
        std::fs::write(dir.join(file), contents).unwrap();
    }
    dir
}

const HELLO: &str = "module Hello\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime \"hi from run\"\n";

#[test]
fn run_prints_via_console_capability() {
    let dir = workspace("run", &[("Hello.fai", HELLO)]);
    let out = fai().args(["run", "-C"]).arg(&dir).arg("Hello.fai").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi from run\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn build_produces_a_runnable_binary() {
    let src = "module Calc\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (40 + 2))\n";
    let dir = workspace("build", &[("Calc.fai", src)]);
    let exe = dir.join("calc");

    let build = fai()
        .args(["build", "-C"])
        .arg(&dir)
        .arg("Calc.fai")
        .arg("--out")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(build.status.success(), "build stderr: {}", String::from_utf8_lossy(&build.stderr));

    let run = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    assert_eq!(run.status.code(), Some(0), "the produced binary should exit cleanly");
}

#[test]
fn run_without_main_reports_no_entry_point() {
    let dir = workspace("nomain", &[("M.fai", "module M\n\nlet x = 1\n")]);
    let out = fai().args(["run", "-C"]).arg(&dir).arg("M.fai").output().unwrap();
    assert_eq!(out.status.code(), Some(4), "a compile failure exits 4");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("entry point"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
