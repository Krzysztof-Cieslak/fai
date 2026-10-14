//! Named suite discovery, selection, effects, failures and worker isolation.

use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);
impl Workspace {
    fn new(source: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("tests")).unwrap();
        std::fs::write(root.path().join("tests/Suite.fai"), source).unwrap();
        Self(root)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fai"))
            .args(["test", "--color=never", "--message-format=json", "-C"])
            .arg(self.0.path())
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.run(args);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {output:?}"))
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_fai"))
            .args(["daemon", "stop", "-C"])
            .arg(self.0.path())
            .output();
    }
}

#[test]
fn named_tests_return_structured_failures_and_source_locations() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"math\" [\n  Test.test \"sum\" (fun _ -> Expect.equal Int.toString 4 (2 + 2)),\n  Test.test \"two failures\" (fun _ -> Expect.all [Expect.equal Int.toString 1 2, Expect.fail \"second\"])\n]\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["schemaVersion"], 2);
    assert_eq!(report["total"], 2, "{report}");
    assert_eq!(report["passed"], 1, "{report}");
    assert_eq!(report["events"][1]["failures"].as_array().unwrap().len(), 2);
    assert_eq!(report["events"][1]["failures"][0]["expected"], "1");
    assert_eq!(report["events"][1]["failures"][0]["actual"], "2");
    assert_eq!(report["events"][1]["failures"][0]["location"]["line"], 5);
}

#[test]
fn focus_filters_contracts_and_skip_prevents_execution() {
    let workspace = Workspace::new(
        "module Suite\nexample: false\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"group\" [Test.test \"ordinary\" (fun _ -> Expect.fail \"not selected\"), Test.focus (Test.test \"focused\" (fun _ -> Test.pass)), Test.focus (Test.skip \"later\" (Test.test \"skipped\" (fun _ -> Expect.fail \"not executed\")))]\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["focused"], true);
    assert_eq!(report["summary"]["passed"], 1);
    assert_eq!(report["summary"]["skipped"], 1);
    assert_eq!(report["summary"]["filteredOut"], 2);
}

#[test]
fn deny_focused_checks_even_filtered_and_skipped_branches() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"g\" [Test.test \"chosen\" (fun _ -> Test.pass), Test.skip \"later\" (Test.focus (Test.test \"hidden\" (fun _ -> Test.pass)))]\n",
    );
    let report = workspace.json(&["--no-daemon", "--match=chosen", "--deny-focused"]);
    assert_eq!(report["ok"], false, "{report}");
    assert!(report["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI6008"));
}

#[test]
fn listing_never_runs_test_bodies() {
    let workspace = Workspace::new(
        "module Suite\nbody : Test.Context -> Test.Check / { Console }\nlet body ctx =\n  let printed = ctx.runtime.console.writeLine \"BODY_RAN\"\n  Test.pass\ntests : Test.Suite Test.Context { Console }\nlet tests = Test.test \"deferred\" body\n",
    );
    let report = workspace.json(&["--no-daemon", "--list"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["summary"]["listed"], 1);
    assert_eq!(report["events"][0]["output"], serde_json::Value::Null);
}

#[test]
fn mixed_effects_execute_on_fresh_worker_contexts() {
    let workspace = Workspace::new(
        "module Suite\nconsole : Test.Context -> Test.Check / { Console }\nlet console ctx =\n  let printed = ctx.runtime.console.writeLine \"CAPTURED\"\n  Test.pass\nclock : Test.Context -> Test.Check / { Clock }\nlet clock ctx =\n  let slept = ctx.runtime.clock.sleep 1\n  Test.pass\ntests : Test.Suite Test.Context { Console, Clock }\nlet tests = Test.describe \"effects\" [Test.test \"pure\" (fun _ -> Test.pass), Test.test \"console\" console, Test.test \"clock\" clock]\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["passed"], 3);
    assert!(report["events"][1]["output"].as_str().unwrap().contains("CAPTURED"));
}

#[test]
fn a_crashed_named_case_does_not_prevent_the_next_case() {
    let workspace = Workspace::new(
        "module Suite\nlet divide n = 1 / n\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"crash\" [Test.test \"bad\" (fun ctx -> Expect.equal Int.toString 0 (divide (ctx.seed - ctx.seed))), Test.test \"good\" (fun _ -> Test.pass)]\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], false);
    assert_eq!(report["summary"]["crashed"], 1, "{report}");
    assert_eq!(report["passed"], 1, "{report}");
}

#[test]
fn duplicate_names_are_discovery_errors() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"g\" [Test.test \"same\" (fun _ -> Test.pass), Test.test \"same\" (fun _ -> Test.pass)]\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], false, "{report}");
    assert!(report["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI6008"));
}

#[test]
fn discarded_checks_are_diagnosed() {
    let workspace = Workspace::new(
        "module Suite\nlet body _ =\n  let ignored = Expect.fail \"lost\"\n  Test.pass\ntests : Test.Suite Test.Context {}\nlet tests = Test.test \"discard\" body\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert!(
        report["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI6011"),
        "{report}"
    );
}

#[test]
fn suite_initializers_must_be_pure() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests =\n  let printed = stdConsole.writeLine \"DISCOVERY_EFFECT\"\n  Test.test \"case\" (fun _ -> Test.pass)\n",
    );
    let output = workspace.run(&["--no-daemon"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert!(
        report["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI6007"),
        "{report}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).starts_with("DISCOVERY_EFFECT"));
}

#[test]
fn fixture_cleanup_retains_body_and_cleanup_failures() {
    let workspace = Workspace::new(
        "module Suite\nrelease : Test.Context -> Result Unit String / { Console }\nlet release ctx =\n  let printed = ctx.runtime.console.writeLine \"CLEANED\"\n  Err \"release failed\"\ntests : Test.Suite Test.Context { Concurrency, Console }\nlet tests = Test.bracket (fun ctx -> Ok ctx) release (Test.test \"fixture\" (fun _ -> Expect.fail \"body failed\"))\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["total"], 1, "{report}");
    assert_eq!(report["events"][0]["failures"].as_array().unwrap().len(), 2);
    assert_eq!(report["summary"]["errored"], 1);
    assert!(report["events"][0]["output"].as_str().unwrap().contains("CLEANED"));
}

#[test]
fn daemon_and_direct_named_reports_match_except_time() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"g\" [Test.test \"ok\" (fun _ -> Test.pass), Test.skip \"later\" (Test.test \"skip\" (fun _ -> Test.pass))]\n",
    );
    let mut warm = workspace.json(&[]);
    let mut direct = workspace.json(&["--no-daemon"]);
    warm["summary"].as_object_mut().unwrap().remove("elapsedMs");
    direct["summary"].as_object_mut().unwrap().remove("elapsedMs");
    assert_eq!(warm, direct);
}

#[test]
fn filesystem_sample_runs_in_per_test_temporary_storage() {
    let workspace = Workspace::new(include_str!("../../../samples/NamedTests.fai"));
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["passed"], 2);
    assert_eq!(report["summary"]["skipped"], 1);
}

#[test]
fn named_test_id_is_unchanged_by_a_comment_edit() {
    let source = "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.test \"same\" (fun _ -> Test.pass)\n";
    let workspace = Workspace::new(source);
    let before = workspace.json(&["--no-daemon"]);
    std::fs::write(workspace.0.path().join("tests/Suite.fai"), format!("// comment\n{source}"))
        .unwrap();
    let after = workspace.json(&["--no-daemon"]);
    assert_eq!(before["events"][0]["id"], after["events"][0]["id"]);
    assert_ne!(before["events"][0]["location"], after["events"][0]["location"]);
}

#[test]
fn a_timed_out_case_is_cancelled_and_the_next_case_runs() {
    let workspace = Workspace::new(
        "module Suite\nslow : Test.Context -> Test.Check / { Clock }\nlet slow ctx =\n  let slept = ctx.runtime.clock.sleep 10000\n  Test.pass\ntests : Test.Suite Test.Context { Clock }\nlet tests = Test.describe \"deadline\" [Test.test \"slow\" slow, Test.test \"next\" (fun _ -> Test.pass)]\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fai"))
        .args(["test", "--no-daemon", "--message-format=json", "-C"])
        .arg(workspace.0.path())
        .env("FAI_TEST_TIMEOUT_MS", "1000")
        .env("FAI_TEST_CLEANUP_TIMEOUT_MS", "500")
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["timedOut"], 1, "{report}");
    assert_eq!(report["passed"], 1, "{report}");
}

#[test]
fn pure_discovery_loops_are_bounded() {
    let workspace = Workspace::new(
        "module Suite\nlet forever n = forever (n + 1)\ntests : Test.Suite Test.Context {}\nlet tests = forever 0\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fai"))
        .args(["test", "--no-daemon", "--message-format=json", "-C"])
        .arg(workspace.0.path())
        .env("FAI_TEST_DISCOVERY_TIMEOUT_MS", "100")
        .env("FAI_TEST_CLEANUP_TIMEOUT_MS", "50")
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false, "{report}");
    assert!(
        report["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI6008"),
        "{report}"
    );
}

#[test]
fn fixture_contexts_can_have_a_different_type() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.withFixture (fun _ body -> body 7) (Test.test \"value\" (Expect.equal Int.toString 7))\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["passed"], 1);
}

#[test]
fn context_identity_matches_the_reported_case_identity() {
    let workspace = Workspace::new(
        "module Suite\nbody : Test.Context -> Test.Check / { Console }\nlet body ctx =\n  let printed = ctx.runtime.console.writeLine ctx.id\n  Test.pass\ntests : Test.Suite Test.Context { Console }\nlet tests = Test.test \"identity\" body\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(
        report["events"][0]["output"].as_str().unwrap().trim(),
        report["events"][0]["id"].as_str().unwrap()
    );
}

#[test]
fn runner_owned_temporary_roots_are_removed_after_execution() {
    let workspace = Workspace::new(
        "module Suite\nbody : Test.Context -> Test.Check / { Console }\nlet body ctx =\n  let printed = ctx.runtime.console.writeLine (Result.withDefault \"\" (Path.toString ctx.tempRoot))\n  Test.pass\ntests : Test.Suite Test.Context { Console }\nlet tests = Test.test \"temporary\" body\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
    let directory = report["events"][0]["output"].as_str().unwrap().trim();
    assert!(!directory.is_empty());
    assert!(!std::path::Path::new(directory).exists());
}

#[test]
fn junit_retains_named_failure_error_and_skip_categories() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"g\" [Test.test \"pass\" (fun _ -> Test.pass), Test.test \"fail\" (fun _ -> Expect.fail \"expected failure\"), Test.test \"error\" (fun _ -> Test.error \"operation error\"), Test.skip \"later\" (Test.test \"skip\" (fun _ -> Test.pass))]\n",
    );
    let path = workspace.0.path().join("report.xml");
    let output = workspace.run(&["--no-daemon", "--junit", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let xml = std::fs::read_to_string(path).unwrap();
    let doc = roxmltree::Document::parse(&xml).unwrap();
    let root = doc.root_element();
    assert_eq!(root.attribute("tests"), Some("4"), "{xml}");
    assert_eq!(root.attribute("failures"), Some("1"));
    assert_eq!(root.attribute("errors"), Some("1"));
    assert_eq!(root.attribute("skipped"), Some("1"));
}

#[test]
fn worker_control_messages_are_not_exposed_as_application_stdin() {
    let workspace = Workspace::new(
        "module Suite\nbody : Test.Context -> Test.Check / { Console }\nlet body ctx = Expect.equal (fun _ -> \"stdin\") (Ok None) (ctx.runtime.console.readLine ())\ntests : Test.Suite Test.Context { Console }\nlet tests = Test.test \"stdin\" body\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], true, "{report}");
}

#[test]
fn cancellation_masks_fixture_release_operations() {
    let workspace = Workspace::new(
        "module Suite\nbody : Test.Context -> Test.Check / { Clock }\nlet body ctx =\n  let slept = ctx.runtime.clock.sleep 10000\n  Test.pass\nrelease : Test.Context -> Result Unit String / { FileSystem, Console }\nlet release ctx =\n  let path = Path.join ctx.tempRoot (Result.withDefault Path.empty (Path.fromString \"cleanup.txt\"))\n  match ctx.runtime.fs.writeFile path \"released\" with\n  | Err error -> Err (FileSystem.errorToString error)\n  | Ok _ ->\n    let printed = ctx.runtime.console.writeLine \"RELEASED\"\n    Ok ()\ntests : Test.Suite Test.Context { Clock, Console, FileSystem, Concurrency }\nlet tests = Test.bracket (fun ctx -> Ok ctx) release (Test.test \"cancel\" body)\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_fai"))
        .args(["test", "--no-daemon", "--message-format=json", "-C"])
        .arg(workspace.0.path())
        .env("FAI_TEST_TIMEOUT_MS", "1000")
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["timedOut"], 1, "{report}");
    assert!(report["events"][0]["output"].as_str().unwrap_or("").contains("RELEASED"), "{report}");
}

#[test]
fn nested_skip_cannot_hide_an_empty_reason() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.skip \"outer\" (Test.skip \"\" (Test.test \"case\" (fun _ -> Test.pass)))\n",
    );
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], false, "{report}");
    assert!(report["diagnostics"].as_array().unwrap().iter().any(|d| d["code"] == "FAI6008"));
}

#[test]
fn junit_names_do_not_collide_when_names_contain_delimiters() {
    let workspace = Workspace::new(
        "module Suite\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"root\" [Test.describe \"a::b\" [Test.test \"c\" (fun _ -> Test.pass)], Test.describe \"a\" [Test.test \"b::c\" (fun _ -> Test.pass)]]\n",
    );
    let path = workspace.0.path().join("report.xml");
    let output = workspace.run(&["--no-daemon", "--junit", path.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    let xml = std::fs::read_to_string(path).unwrap();
    let doc = roxmltree::Document::parse(&xml).unwrap();
    let names: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("testcase"))
        .map(|n| n.attribute("name").unwrap())
        .collect();
    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
}

#[test]
fn malformed_suite_sources_cannot_report_an_empty_success() {
    let workspace =
        Workspace::new("module Suite\ntests : Test.Suite Test.Context {}\nlet tests = [\n");
    let report = workspace.json(&["--no-daemon"]);
    assert_eq!(report["ok"], false, "{report}");
    assert!(!report["diagnostics"].as_array().unwrap().is_empty());
}
