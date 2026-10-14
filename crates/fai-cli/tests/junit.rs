//! JUnit artifacts accompany ordinary test output in direct and daemon execution.

use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new(source: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("M.fai"), source).unwrap();
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fai"))
            .args(["test", "--color=never", "-C"])
            .arg(self.0.path())
            .args(args)
            .output()
            .unwrap()
    }

    fn report(&self, path: &str) -> String {
        std::fs::read_to_string(self.0.path().join(path)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_fai"))
            .args(["daemon", "stop", "-C"])
            .arg(self.0.path())
            .output();
    }
}

fn counts(xml: &str) -> (u64, u64, u64) {
    let doc = roxmltree::Document::parse(xml).unwrap();
    let root = doc.root_element();
    (
        root.attribute("tests").unwrap().parse().unwrap(),
        root.attribute("failures").unwrap().parse().unwrap(),
        root.attribute("errors").unwrap().parse().unwrap(),
    )
}

#[test]
fn passing_junit_artifact_keeps_live_console_output() {
    let fixture = Fixture::new("module M\nexample: true\nforall n: n = n\n");
    let output = fixture.run(&["--no-daemon", "--junit", "reports/pass.xml"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("test result: ok. 2 passed"), "{text}");
    assert!(!text.contains("<testsuites"), "{text}");
    assert_eq!(counts(&fixture.report("reports/pass.xml")), (2, 0, 0));
}

#[test]
fn failed_and_crashed_contracts_still_publish_an_artifact() {
    let fixture =
        Fixture::new("module M\nforall n: n = n + 1\nforall n: 1 / (n - n) = 0\nexample: true\n");
    let output = fixture.run(&["--no-daemon", "--color=always", "--junit", "failed.xml"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let xml = fixture.report("failed.xml");
    assert_eq!(counts(&xml), (3, 1, 1));
    assert!(xml.contains("counterexample: n = 0"), "{xml}");
    assert!(!xml.contains('\x1b'), "{xml}");
    let doc = roxmltree::Document::parse(&xml).unwrap();
    assert_eq!(doc.descendants().filter(|n| n.has_tag_name("testcase")).count(), 3);
}

#[test]
fn json_output_remains_a_single_envelope_with_junit_enabled() {
    let fixture = Fixture::new("module M\nforall n: n = n\n");
    let output = fixture.run(&[
        "--no-daemon",
        "--message-format=json",
        "--seed=-7",
        "--count=12",
        "--max-size=8",
        "--junit",
        "report.xml",
    ]);
    assert!(output.status.success(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["passed"], 1);
    let xml = fixture.report("report.xml");
    let doc = roxmltree::Document::parse(&xml).unwrap();
    assert!(
        doc.descendants()
            .any(|n| n.attribute("name") == Some("seed") && n.attribute("value") == Some("-7"))
    );
    assert!(
        doc.descendants()
            .any(|n| n.attribute("name") == Some("trials") && n.attribute("value") == Some("12"))
    );
    assert!(
        doc.descendants()
            .any(|n| n.attribute("name") == Some("maxSize") && n.attribute("value") == Some("8"))
    );
}

#[test]
fn compile_errors_publish_errors_instead_of_a_green_report() {
    let fixture = Fixture::new("module M\nlet bad = unknown\nexample: true\n");
    let output = fixture.run(&["--no-daemon", "--junit", "report.xml"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let xml = fixture.report("report.xml");
    let (_, failures, errors) = counts(&xml);
    assert_eq!(failures, 0);
    assert!(errors > 0, "{xml}");
    assert!(xml.contains("unknown"), "{xml}");
}

#[test]
fn missing_selection_publishes_a_setup_error() {
    let fixture = Fixture::new("module M\nexample: true\n");
    let output = fixture.run(&["--no-daemon", "missing.fai", "--junit", "report.xml"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let xml = fixture.report("report.xml");
    assert_eq!(counts(&xml), (1, 0, 1));
    assert!(xml.contains("missing.fai"), "{xml}");
}

#[test]
fn unavailable_generators_are_errors_not_skips() {
    let fixture = Fixture::new(
        "module M\nforall f: call f\ncall : (Int -> Int) -> Bool\nlet call f = f 0 = 0\n",
    );
    let output = fixture.run(&["--no-daemon", "--junit", "report.xml"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let xml = fixture.report("report.xml");
    assert_eq!(counts(&xml), (1, 0, 1));
    assert!(!xml.contains("<skipped"), "{xml}");
}

#[test]
fn empty_filter_publishes_a_valid_empty_suite() {
    let fixture = Fixture::new("module M\nexample: true\n");
    let output = fixture.run(&["--no-daemon", "--match=absent", "--junit", "report.xml"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(counts(&fixture.report("report.xml")), (0, 0, 0));
}

#[test]
fn artifact_write_failure_is_nonzero_without_corrupting_json_stdout() {
    let fixture = Fixture::new("module M\nexample: true\n");
    std::fs::create_dir(fixture.0.path().join("report.xml")).unwrap();
    std::fs::write(fixture.0.path().join("report.xml/keep"), "keep").unwrap();
    let output = fixture.run(&["--no-daemon", "--message-format=json", "--junit", "report.xml"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to write JUnit report"));
    assert_eq!(fixture.report("report.xml/keep"), "keep");
}

type XmlElement = (String, Vec<(String, String)>, String);

fn semantic_xml(xml: &str) -> Vec<XmlElement> {
    let doc = roxmltree::Document::parse(xml).unwrap();
    doc.descendants()
        .filter(|n| n.is_element())
        .map(|node| {
            let attrs = node
                .attributes()
                .filter(|a| a.name() != "time")
                .map(|a| (a.name().to_owned(), a.value().to_owned()))
                .collect();
            (node.tag_name().name().to_owned(), attrs, node.text().unwrap_or("").to_owned())
        })
        .collect()
}

#[test]
fn daemon_and_direct_artifacts_have_identical_semantics() {
    let fixture =
        Fixture::new("module M\nlet value = 1\nexample: value = 1\nforall n: n = n + 1\n");
    let warm = fixture.run(&["--junit", "warm.xml"]);
    let direct = fixture.run(&["--no-daemon", "--junit", "direct.xml"]);
    assert_eq!(warm.status.code(), Some(1), "{warm:?}");
    assert_eq!(warm.status.code(), direct.status.code());
    assert_eq!(
        semantic_xml(&fixture.report("warm.xml")),
        semantic_xml(&fixture.report("direct.xml"))
    );
}

#[test]
fn daemon_setup_error_produces_a_report_with_the_original_diagnostic() {
    let fixture = Fixture::new("module M\nexample: true\n");
    let output = fixture.run(&["missing.fai", "--junit", "report.xml"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let xml = fixture.report("report.xml");
    assert_eq!(counts(&xml), (1, 0, 1));
    assert!(xml.contains("missing.fai"), "{xml}");
}
