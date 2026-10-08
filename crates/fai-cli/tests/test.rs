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
fn oversized_array_capacity_is_rejected_in_an_isolated_jit() {
    let source = "module Large\nexample: Array.length (Array.withCapacity 2305843009213693948) = 0\nexample: true\n";
    let dir = workspace("large-array", &[("Large.fai", source)]);
    let out =
        fai().args(["test", "--no-daemon", "-C"]).arg(&dir).arg("Large.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("FAI6003"), "{stdout}");
    assert!(stdout.contains("1 passed, 1 failed"), "{stdout}");
}

#[test]
fn a_fused_search_cannot_hide_a_contract_trap() {
    let source =
        "module Partial\nexample: List.any (fun x -> x = 1) (List.map (fun n -> 1 / n) [1, 0])\n";
    let dir = workspace("partial-fusion", &[("Partial.fai", source)]);
    let output =
        fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("Partial.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("FAI6003"), "{stdout}");
}

#[test]
fn an_unused_effectful_local_function_keeps_a_contract_pure() {
    let source = "module M\npublic retained : (Unit -> Unit / 'e) -> Int\nlet retained action =\n  let local u = action u\n  42\nexample: retained (fun u -> ()) = 42\n";
    let dir = workspace("local-purity", &[("M.fai", source)]);
    let output = fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("M.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed, 0 failed"), "{stdout}");
}

#[test]
fn user_option_type_gets_its_own_constructor_generator() {
    let source = indoc! {r#"
        module Main
        type Option 'a = | Only 'a
        valid : Option Int -> Bool
        let valid value =
          match value with
          | Only n -> Int.toString n <> ""
        forall value: valid value
    "#};
    let dir = workspace("user-option", &[("Main.fai", source)]);
    let output =
        fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[track_caller]
fn run_generator_case(name: &str, files: &[(&str, &str)], success: bool) -> String {
    let dir = workspace(name, files);
    let output =
        fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        output.status.success(),
        success,
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

#[test]
fn cross_module_shadowed_option_generates_valid_values() {
    run_generator_case(
        "external-option",
        &[
            (
                "Lib.fai",
                "module Lib\npublic type Option 'a = | Only 'a\npublic valid : Option Int -> Bool\nlet valid x =\n  match x with\n  | Only n -> Int.toString n <> \"\"\n",
            ),
            ("Main.fai", "module Main\nforall x: Lib.valid x\n"),
        ],
        true,
    );
}

#[test]
fn shadowed_result_uses_its_own_tags_and_arities() {
    let source = "module Main\ntype Result 'a 'b = | Loading | Failure 'b | Success 'a\nvalid : Result Int String -> Bool\nlet valid x =\n  match x with\n  | Loading -> x = Loading\n  | Failure message -> x = Failure message\n  | Success value -> x = Success value\nforall x: valid x\n";
    run_generator_case("user-result", &[("Main.fai", source)], true);
}

#[test]
fn shadowed_option_honors_custom_arbitrary() {
    let source = "module Main\ntype Option 'a = | Only 'a\narbOnly : Test.Arbitrary (Option Int)\nlet arbOnly = { gen = fun size seed -> (Only 9, seed), show = fun x -> \"only\", shrink = fun x -> [] }\nvalid : Option Int -> Bool\nlet valid x =\n  match x with\n  | Only n -> n = 9\nforall x: valid x\n";
    run_generator_case("user-option-override", &[("Main.fai", source)], true);
}

#[test]
fn shadowed_option_without_a_base_case_is_not_groundable() {
    let source = "module Main\ntype Option 'a = | Again 'a (Option 'a)\nvalid : Option Int -> Bool\nlet valid x = true\nforall x: valid x\n";
    let out = run_generator_case("user-option-no-base", &[("Main.fai", source)], false);
    assert!(out.contains("FAI6005"), "{out}");
}

#[test]
fn shadowed_option_counterexample_names_the_users_constructor() {
    let source = "module Main\ntype Option 'a = | Only 'a\nvalid : Option Int -> Bool\nlet valid x = false\nforall x: valid x\n";
    let out = run_generator_case("user-option-counterexample", &[("Main.fai", source)], false);
    assert!(out.contains("counterexample: x = Only 0"), "{out}");
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
