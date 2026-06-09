//! End-to-end tests of the `fai` binary's `check` command evaluating closed
//! `example` contracts, spawning the real executable so the isolated worker that
//! runs the examples is exercised. The headline guarantees: a wrong example is
//! reported as `FAI6001` by `fai check` (not only by `fai test`), `--no-examples`
//! restores a pure type-check, and an example that *traps* fails safely (the
//! worker is isolated, so `fai check` neither crashes nor reports it).

use std::path::PathBuf;
use std::process::Command;

fn fai() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fai"))
}

fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fai-cli-check-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, contents) in files {
        std::fs::write(dir.join(file), contents).unwrap();
    }
    dir
}

/// Runs `fai check --no-daemon` over a one-file workspace, returning the process
/// output. Going through the real binary exercises the isolated example worker.
fn check(name: &str, file: &str, src: &str, extra: &[&str]) -> std::process::Output {
    let dir = workspace(name, &[(file, src)]);
    fai()
        .args(["check", "--no-daemon", "--color=never", "-C"])
        .arg(&dir)
        .args(extra)
        .arg(file)
        .output()
        .unwrap()
}

#[test]
fn wrong_example_is_reported_by_check() {
    let out = check("wrong", "Bad.fai", "module Bad\nexample: 1 = 2\n", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("FAI6001"), "expected FAI6001 in: {stdout}");
    assert!(stdout.contains("example does not hold"), "expected the message in: {stdout}");
}

#[test]
fn correct_example_passes_check() {
    let out = check("correct", "Ok.fai", "module Ok\nexample: 1 + 1 = 2\n", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!stdout.contains("FAI6001"), "no failure expected: {stdout}");
}

#[test]
fn no_examples_flag_restores_a_pure_type_check() {
    // The example is false, but `--no-examples` skips evaluating it, so the
    // type-clean file checks successfully.
    let out = check("opt-out", "Bad.fai", "module Bad\nexample: 1 = 2\n", &["--no-examples"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    assert!(!stdout.contains("FAI6001"), "examples must not run under --no-examples: {stdout}");
}

#[test]
fn trapping_example_is_isolated_and_check_succeeds() {
    // Integer division by zero traps at runtime: it kills the isolated worker,
    // not `fai check`. Since check reports only definite failures, the trapping
    // example is dropped (left to `fai test`) and the run succeeds cleanly.
    let out = check("trap", "Trap.fai", "module Trap\nexample: 1 / 0 = 0\n", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a trapping example must not fail or crash check; stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!stdout.contains("FAI6001"), "a trap is not a definite failure: {stdout}");
    assert!(!stdout.contains("FAI6003"), "aborts are left to `fai test`: {stdout}");
}

#[test]
fn json_output_reports_the_example_failure() {
    let dir = workspace("json", &[("Bad.fai", "module Bad\nexample: 2 + 2 = 5\n")]);
    let out = fai()
        .args(["check", "--no-daemon", "--message-format=json", "-C"])
        .arg(&dir)
        .arg("Bad.fai")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON envelope");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["ok"], false);
    let codes: Vec<&str> = value["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"FAI6001"), "expected FAI6001 in {codes:?}");
}

#[test]
fn example_in_an_imported_module_is_evaluated() {
    // A wrong example whose body calls into the standard library is still caught.
    let out = check(
        "callee",
        "M.fai",
        "module M\nexample: List.map (fun x -> x * 2) [1, 2] = [2, 3]\n",
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("FAI6001"), "expected FAI6001 in: {stdout}");
}
