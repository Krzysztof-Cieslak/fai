//! End-to-end tests of the `fai` binary's `test` command, spawning the real
//! executable so the isolated `__test-worker` subprocess and its supervision are
//! exercised. The headline guarantee: a contract whose body traps on a generated
//! input (here, integer division by zero) fails *that* contract and the run
//! continues — the supervisor records it and resumes the rest.

use std::path::PathBuf;
use std::process::Command;

use indoc::indoc;

fn fai() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fai"))
}

fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fai-cli-test-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, contents) in files {
        std::fs::write(dir.join(file), contents).unwrap();
    }
    dir
}

/// A passing example, a `forall` that divides by a runtime zero (`n - n`) so it
/// aborts on the first generated input, then a passing `forall`. The middle
/// contract must abort in isolation while the others still run.
const CRASH: &str = indoc! {r#"
    module Crash

    example: 1 + 1 = 2
    forall n: 1 / (n - n) = 0
    forall xs: List.length xs >= 0
"#};

#[test]
fn trapping_contract_is_isolated_and_the_run_continues() {
    let dir = workspace("isolate", &[("Crash.fai", CRASH)]);
    let out =
        fai().args(["test", "--no-daemon", "-C"]).arg(&dir).arg("Crash.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The run failed (the aborted contract), but it did not crash the process.
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The trap is reported as a located FAI6003, not a process abort.
    assert!(stdout.contains("FAI6003"), "expected FAI6003 in: {stdout}");
    assert!(stdout.contains("aborted while running"), "expected the abort message in: {stdout}");
    // The contracts on either side of the crasher still ran and passed.
    assert!(stdout.contains("2 passed, 1 failed"), "expected the rest to run: {stdout}");
}

#[test]
fn trapping_contract_streams_live_lines_in_order() {
    let dir = workspace("livelines", &[("Crash.fai", CRASH)]);
    let out =
        fai().args(["test", "--no-daemon", "-C"]).arg(&dir).arg("Crash.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The example passes, the divide-by-zero aborts, the last forall passes —
    // each emits a live line as it completes (human mode).
    let abort = stdout.find("ABORT").expect("an ABORT line");
    let oks: Vec<_> = stdout.match_indices("ok    ").map(|(i, _)| i).collect();
    assert_eq!(oks.len(), 2, "two passing contracts each emit a line: {stdout}");
    assert!(oks[0] < abort && abort < oks[1], "lines stream in source order: {stdout}");
}

#[test]
fn json_output_has_per_contract_events_and_seed() {
    let dir = workspace("json", &[("Crash.fai", CRASH)]);
    let out = fai()
        .args(["test", "--no-daemon", "--message-format=json", "-C"])
        .arg(&dir)
        .arg("Crash.fai")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON envelope");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["total"], 3);
    assert_eq!(value["passed"], 2);
    assert_eq!(value["seed"], 0);
    assert_eq!(value["ok"], false);

    let events = value["events"].as_array().expect("events array");
    assert_eq!(events.len(), 3);
    let status_of = |ordinal: i64| -> String {
        events
            .iter()
            .find(|e| e["ordinal"] == ordinal)
            .and_then(|e| e["status"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    assert_eq!(status_of(0), "passed");
    assert_eq!(status_of(1), "crashed");
    assert_eq!(status_of(2), "passed");
    // Every event reports the generator configuration it ran with.
    assert_eq!(events[0]["trials"], 100);
    assert_eq!(events[0]["maxSize"], 100);
}

#[test]
fn passing_contracts_exit_zero() {
    let src = indoc! {r#"
        module Ok

        forall xs: List.reverse (List.reverse xs) = xs
        forall n: n + 0 = n
    "#};
    let dir = workspace("ok", &[("Ok.fai", src)]);
    let out = fai().args(["test", "--no-daemon", "-C"]).arg(&dir).arg("Ok.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("2 passed, 0 failed"), "got: {stdout}");
}

#[test]
fn false_property_reports_a_shrunk_counterexample() {
    let dir = workspace("shrink", &[("Bad.fai", "module Bad\n\nforall n: n = n + 1\n")]);
    let out = fai()
        .args(["test", "--no-daemon", "--message-format=json", "-C"])
        .arg(&dir)
        .arg("Bad.fai")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let event = &value["events"][0];
    assert_eq!(event["status"], "failed");
    assert_eq!(event["counterexample"], "n = 0");
}
