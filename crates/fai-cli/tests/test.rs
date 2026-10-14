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

#[track_caller]
fn impure_helper_is_not_executed(command: &str) {
    let source = "module Main\npublic noisy : Int -> Int / { Console }\nlet noisy n =\n  let ignored = stdConsole.writeLine \"EFFECT_EXECUTED\"\n  n\nexample: noisy 1 = 1\n";
    let dir = workspace(&format!("effect-{command}"), &[("Main.fai", source)]);
    let output = fai()
        .args([command, "--no-daemon", "--message-format=json", "-C"])
        .arg(dir)
        .arg("Main.fai")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("only protocol output");
    assert!(
        result["diagnostics"].as_array().unwrap().iter().any(|error| error["code"] == "FAI6004"),
        "{result}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("EFFECT_EXECUTED"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("EFFECT_EXECUTED"));
}

#[test]
fn check_rejects_a_hidden_helper_effect_before_eager_execution() {
    impure_helper_is_not_executed("check");
}

#[test]
fn test_rejects_a_hidden_helper_effect_before_starting_a_worker() {
    impure_helper_is_not_executed("test");
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
    assert!(stdout.contains("2 passed; 0 failed; 1 crashed"), "expected the rest to run: {stdout}");
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
    assert!(stdout.contains("1 passed; 0 failed; 1 crashed"), "{stdout}");
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
fn private_reader_contract_keeps_its_array_check() {
    let source = "module Main\nlet at depth i xs = if depth <= 0 then Array.unsafeGet i xs else at (depth - 1) i xs\nlet ordinary () = at 0 0 [| 42 |]\nexample: at 0 (-1) [| 42 |] = 0\nexample: true\n";
    let dir = workspace("contract-reader-bounds", &[("Main.fai", source)]);
    let output =
        fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("FAI6003"), "{stdout}");
    assert!(stdout.contains("Main.fai:4:1"), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed; 1 crashed"), "{stdout}");
}

#[test]
fn property_reader_keeps_generated_arguments_conservative() {
    let source = "module Main\nlet at depth i xs = if depth <= 0 then Array.unsafeGet i xs else at (depth - 1) i xs\nlet ordinary () = at 0 0 [| 42 |]\nforall i: at 0 (i - i - 1) [| 42 |] = 0\nexample: true\n";
    let dir = workspace("property-reader-bounds", &[("Main.fai", source)]);
    let output =
        fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("FAI6003"), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed; 1 crashed"), "{stdout}");
}

#[test]
fn private_wrapper_propagates_contract_input_conservatism() {
    let source = "module Main\nlet at depth i xs = if depth <= 0 then Array.unsafeGet i xs else at (depth - 1) i xs\nlet lookup i = at 0 i [| 42 |]\nlet ordinary () = lookup 0\nexample: lookup (-1) = 0\nexample: true\n";
    let dir = workspace("wrapped-reader-bounds", &[("Main.fai", source)]);
    let output =
        fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("FAI6003"), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed; 1 crashed"), "{stdout}");
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
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
}

#[test]
fn wrapping_a_live_property_binder_in_some_preserves_its_value() {
    let source = "module M\nforall x: Option.withDefault 0 (Some x) = x\n";
    let dir = workspace("niche-payload", &[("M.fai", source)]);
    let output = fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("M.fai").output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn direct_match_contracts_run_in_the_worker() {
    let source = include_str!("../../../samples/MatchContracts.fai");
    let dir = workspace("match-contracts", &[("MatchContracts.fai", source)]);
    let output = fai()
        .args(["test", "--no-daemon", "-C"])
        .arg(dir)
        .arg("MatchContracts.fai")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("2 passed; 0 failed"), "{stdout}");
}

#[test]
fn check_evaluates_a_direct_match_example() {
    let source = include_str!("../../../samples/MatchContracts.fai");
    let dir = workspace("check-match", &[("MatchContracts.fai", source)]);
    let output = fai()
        .args(["check", "--no-daemon", "-C"])
        .arg(dir)
        .arg("MatchContracts.fai")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn direct_match_preserves_a_live_property_binder() {
    let source =
        "module M\nforall x: (match Some x with | None -> false | Some value -> value = x)\n";
    let dir = workspace("live-match-payload", &[("M.fai", source)]);
    let output = fai().args(["test", "--no-daemon", "-C"]).arg(dir).arg("M.fai").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
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
    let events: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("test ") && !line.starts_with("test result:"))
        .collect();
    assert_eq!(events.len(), 3, "each contract emits a live line: {stdout}");
    assert!(events[0].ends_with("... ok"), "{stdout}");
    assert!(events[1].ends_with("... CRASHED"), "{stdout}");
    assert!(events[2].ends_with("... ok"), "{stdout}");
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
    assert_eq!(value["config"], serde_json::json!({"seed": 0, "trials": 100, "maxSize": 100}));
    let mut summary = value["summary"].clone();
    assert!(summary.as_object_mut().unwrap().remove("elapsedMs").unwrap().is_u64());
    assert_eq!(
        summary,
        serde_json::json!({
            "total": 3, "examples": 1, "properties": 2,
            "passed": 2, "failed": 0, "crashed": 1, "timedOut": 0,
            "notRun": 0, "blocked": 0, "filteredOut": 0
        })
    );

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
    assert_eq!(events[0]["location"], serde_json::json!({"line": 3, "column": 1}));
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
    assert!(stdout.contains("2 passed; 0 failed"), "got: {stdout}");
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

#[test]
fn human_report_distinguishes_contracts_attached_to_the_same_binding() {
    let source = "module Named\n// 🌍\nlet add n = n + 1\nexample: add 1 = 2\nexample: add 2 = 3\nforall n: add n = n + 1\n";
    let dir = workspace("named-report", &[("Named.fai", source)]);
    let output =
        fai().args(["test", "--no-daemon", "--color=never", "-C"]).arg(dir).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.starts_with("running 3 contracts (2 examples, 1 property)\n"), "{text}");
    assert!(text.contains("test Named.add [example, Named.fai:4:1] ... ok"), "{text}");
    assert!(text.contains("test Named.add [example, Named.fai:5:1] ... ok"), "{text}");
    assert!(text.contains("test Named.add [forall, Named.fai:6:1] ... ok"), "{text}");
    assert!(text.contains("test result: ok. 3 passed; 0 failed"), "{text}");
    assert!(!text.contains('\x1b'), "{text}");
}

#[test]
fn human_property_failure_groups_diagnostics_and_reproduction_settings() {
    let dir = workspace("failure-report", &[("Bad.fai", "module Bad\nforall n: n = n + 1\n")]);
    let output = fai()
        .args(["test", "--no-daemon", "--seed=-9", "--count=23", "--max-size=42", "-C"])
        .arg(dir)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("seed: -9; trials per property: 23; max size: 42"), "{text}");
    assert!(text.contains("test forall [Bad.fai:2:1] ... FAILED"), "{text}");
    assert_eq!(text.matches("counterexample: n = 0").count(), 1, "{text}");
    assert!(text.contains("\nfailures:\n"), "{text}");
    assert!(text.contains("same path/filter and --seed -9 --count 23 --max-size 42"), "{text}");
    assert!(text.contains("test result: FAILED. 0 passed; 1 failed"), "{text}");
}

#[test]
fn compilation_errors_report_blocked_contracts_in_json() {
    let dir = workspace(
        "blocked-report",
        &[("Bad.fai", "module Bad\nlet value = unknown\nexample: true\n")],
    );
    let output = fai()
        .args(["test", "--no-daemon", "--message-format=json", "-C"])
        .arg(dir)
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["summary"]["blocked"], 1);
    assert_eq!(report["summary"]["failed"], 0);
    assert_eq!(report["events"], serde_json::json!([]));
}

#[test]
fn filtered_summary_counts_only_contracts_in_selected_files() {
    let source =
        "module Named\nlet first = 1\nexample: first = 1\nlet second = 2\nexample: second = 2\n";
    let dir = workspace(
        "filtered-report",
        &[("Named.fai", source), ("Other.fai", "module Other\nexample: true\n")],
    );
    let output = fai()
        .args(["test", "--no-daemon", "--message-format=json", "--match=first", "-C"])
        .arg(dir)
        .arg("Named.fai")
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report}");
    assert_eq!(report["total"], 1);
    assert_eq!(report["summary"]["filteredOut"], 1);
    assert_eq!(report["events"][0]["ordinal"], 0);
}

#[test]
fn unmatched_filter_reports_zero_selected_without_failure() {
    let dir = workspace("unmatched-report", &[("Only.fai", "module Only\nexample: true\n")]);
    let output =
        fai().args(["test", "--no-daemon", "--match=missing", "-C"]).arg(dir).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("no contracts matched the filter"), "{text}");
    assert!(text.contains("test result: ok. 0 passed; 0 failed"), "{text}");
    assert!(text.contains("1 filtered out"), "{text}");
}

#[test]
fn empty_file_reports_no_contracts() {
    let dir = workspace("empty-report", &[("Empty.fai", "module Empty\n")]);
    let output = fai().args(["test", "--no-daemon", "-C"]).arg(dir).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("no contracts found in the selected files"), "{text}");
    assert!(text.contains("test result: ok. 0 passed; 0 failed"), "{text}");
}

#[test]
fn ungeneratable_property_is_not_reported_as_ignored_or_passed() {
    let source =
        "module Missing\nforall f: call f\ncall : (Int -> Int) -> Bool\nlet call f = f 0 = 0\n";
    let dir = workspace("not-run-report", &[("Missing.fai", source)]);
    let output = fai().args(["test", "--no-daemon", "-C"]).arg(dir).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("test forall [Missing.fai:2:1] ... NOT RUN"), "{text}");
    assert!(text.contains("0 passed; 0 failed; 0 crashed; 0 timed out; 1 not run"), "{text}");
    assert!(text.contains("test result: FAILED."), "{text}");
}

#[test]
fn forced_color_applies_to_progress_and_summary() {
    let dir = workspace("colored-report", &[("Color.fai", "module Color\nexample: true\n")]);
    let output =
        fai().args(["test", "--no-daemon", "--color=always", "-C"]).arg(dir).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("... \x1b[32mok\x1b[0m"), "{text:?}");
    assert!(text.contains("test result: \x1b[32mok\x1b[0m"), "{text:?}");
}
