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
fn exported_signature_cannot_hide_effects_from_a_dependent_contract() {
    let dir = workspace(
        "signature-effects",
        &[
            (
                "Lib.fai",
                "module Lib\npublic invoke : (Unit -> Unit / 'e) -> Unit\nlet invoke action = action ()\n",
            ),
            (
                "Main.fai",
                "module Main\nhidden : Console -> Unit\nlet hidden c = Lib.invoke (fun u -> c.writeLine \"must not run\")\nexample: hidden stdConsole = ()\n",
            ),
        ],
    );
    let output = fai()
        .args(["check", "--no-daemon", "--message-format=json", "-C"])
        .arg(dir)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = parsed["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics.iter().any(|d| d["code"] == "FAI3004" && d["primary"]["file"] == "Lib.fai"),
        "{parsed}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("must not run"));
}

#[test]
fn invalid_unicode_escape_is_rejected_without_running_examples() {
    let out = check(
        "unicode-scalar",
        "Bad.fai",
        "module Bad\nlet value = \"\\u{110000}\"\n",
        &["--no-examples"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("FAI1006"), "expected invalid-escape diagnostic: {stdout}");
}

#[test]
fn formatter_preserves_a_file_with_an_invalid_unicode_escape() {
    let src = "module Bad\nlet value = '\\u{D800}'\n";
    let dir = workspace("unicode-fmt", &[("Bad.fai", src)]);
    let out = fai()
        .args(["fmt", "--no-daemon", "--color=never", "-C"])
        .arg(&dir)
        .arg("Bad.fai")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("FAI1006"));
    assert_eq!(std::fs::read_to_string(dir.join("Bad.fai")).unwrap(), src);
}

#[track_caller]
fn assert_trailing_tokens_preserved(name: &str, source: &str) {
    let dir = workspace(name, &[("Bad.fai", source)]);
    let output = fai()
        .args(["fmt", "--no-daemon", "--message-format=json", "-C"])
        .arg(&dir)
        .arg("Bad.fai")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["changed"].as_array().unwrap().is_empty());
    assert!(result["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI1020"));
    assert_eq!(std::fs::read_to_string(dir.join("Bad.fai")).unwrap(), source);
}

#[test]
fn formatter_keeps_a_same_line_declaration_in_the_original_file() {
    assert_trailing_tokens_preserved("trailing-declaration", "module Bad\nlet x = 1 let y = 2\n");
}

#[test]
fn formatter_keeps_a_stray_delimiter_in_a_nested_module() {
    assert_trailing_tokens_preserved(
        "trailing-nested",
        "module Bad\nmodule Inner =\n  let x = \"é😀\" )\n",
    );
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
fn non_exhaustive_contract_is_rejected_with_examples_disabled() {
    let out = check(
        "contract-match",
        "Bad.fai",
        "module Bad\nexample: match true with | true -> true\n",
        &["--no-examples"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("FAI4001"), "expected exhaustiveness diagnostic: {stdout}");
}

#[test]
fn non_exhaustive_array_element_is_rejected_before_execution() {
    let out = check(
        "array-match",
        "Bad.fai",
        "module Bad\nlet value = [| (match false with | true -> 1) |]\n",
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("FAI4001"), "expected exhaustiveness diagnostic: {stdout}");
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
