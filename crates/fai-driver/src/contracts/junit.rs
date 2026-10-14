//! Portable JUnit XML exported from the same contract outcomes as console/JSON reports.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use fai_diagnostics::{Diagnostic, Severity, render_human};
use rustc_hash::FxHashMap;

use super::{ContractSourceOrigin, ContractStatus, TestOutcome, TestPlan};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Passed,
    Failure,
    Error,
}

struct Case {
    name: String,
    class: String,
    file: Option<String>,
    line: Option<u32>,
    status: Status,
    code: String,
    message: String,
    details: String,
}

/// Renders a completed or compilation-blocked contract run as JUnit XML.
/// Counts describe emitted cases, including any synthetic run-level error case.
#[must_use]
pub fn render_junit(plan: &TestPlan, outcome: &TestOutcome) -> String {
    let events: BTreeMap<_, _> =
        outcome.events.iter().map(|e| ((&e.source, e.ordinal), e)).collect();
    let mut by_span = FxHashMap::<_, Vec<&Diagnostic>>::default();
    for diagnostic in &outcome.diagnostics {
        by_span.entry(diagnostic.primary).or_default().push(diagnostic);
    }
    let mut metadata = plan
        .runnable_meta
        .iter()
        .chain(plan.not_runnable.iter().map(|(m, _, _)| m))
        .collect::<Vec<_>>();
    metadata.sort_by_key(|m| (&m.source, m.ordinal));
    let mut cases = Vec::with_capacity(metadata.len());
    for meta in metadata {
        let event = events.get(&(&meta.source, meta.ordinal));
        let diagnostics = by_span.remove(&meta.span).unwrap_or_default();
        let error = diagnostics.iter().find(|d| d.severity == Severity::Error);
        let (status, fallback) = match event.map(|e| e.status) {
            Some(ContractStatus::Passed) if error.is_none() => (Status::Passed, ""),
            Some(ContractStatus::Failed) => (Status::Failure, "contract does not hold"),
            Some(ContractStatus::Crashed) => (Status::Error, "contract crashed"),
            Some(ContractStatus::TimedOut) => (Status::Error, "contract timed out"),
            Some(ContractStatus::NotRun) => (Status::Error, "contract could not run"),
            Some(ContractStatus::Passed) => (Status::Error, "contract runtime error"),
            None => (Status::Error, "contract blocked before execution"),
        };
        let diagnostics = diagnostics.into_iter().cloned().collect::<Vec<_>>();
        let details = render_human(&diagnostics, &plan.render_spans, false);
        let error = diagnostics.iter().find(|d| d.severity == Severity::Error);
        cases.push(Case {
            name: format!(
                "{}::{}#{}",
                meta.symbol.as_deref().unwrap_or("contract"),
                meta.kind.keyword(),
                meta.ordinal + 1
            ),
            class: format!(
                "{}{}",
                if meta.source.origin == ContractSourceOrigin::StandardLibrary {
                    "std:"
                } else {
                    ""
                },
                meta.source.file
            ),
            file: Some(meta.source.file.clone()),
            line: meta.location.map(|location| location.line),
            status,
            code: error.map_or("contract", |d| d.code.as_str()).to_owned(),
            message: error.map_or(fallback, |d| &d.message).to_owned(),
            details,
        });
    }
    // Keep compiler/setup errors that are not attached to a contract visible to CI,
    // including an empty selection whose project fails before contracts can run.
    let remaining = outcome
        .diagnostics
        .iter()
        .filter(|d| by_span.contains_key(&d.primary))
        .cloned()
        .collect::<Vec<_>>();
    if remaining.iter().any(|d| d.severity == Severity::Error) {
        cases.push(run_error(&render_human(&remaining, &plan.render_spans, false)));
    } else if !outcome.ok && cases.iter().all(|case| case.status == Status::Passed) {
        cases.push(run_error("test run did not complete successfully"));
    }
    let properties = [
        ("seed", outcome.config.seed.to_string()),
        ("trials", outcome.config.trials.to_string()),
        ("maxSize", outcome.config.max_size.to_string()),
        ("selected", outcome.total.to_string()),
        ("filteredOut", outcome.summary.filtered_out.to_string()),
    ];
    document(&cases, outcome.summary.elapsed_ms, &properties)
}

/// Renders a preparation/transport failure when no test plan could be produced.
#[must_use]
pub fn junit_error_report(message: &str) -> String {
    // Preparation errors can arrive as already-rendered console text. Discard
    // only SGR color sequences, retaining the actual diagnostic and its location.
    let mut plain = String::new();
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            let mut candidate = chars.clone();
            candidate.next();
            while candidate.peek().is_some_and(|c| c.is_ascii_digit() || *c == ';') {
                candidate.next();
            }
            if candidate.next() == Some('m') {
                chars = candidate;
                continue;
            }
        }
        plain.push(c);
    }
    document(&[run_error(&plain)], 0, &[])
}

fn run_error(details: &str) -> Case {
    Case {
        name: "test run".to_owned(),
        class: "fai".to_owned(),
        file: None,
        line: None,
        status: Status::Error,
        code: "tooling".to_owned(),
        message: "test run could not complete".to_owned(),
        details: details.to_owned(),
    }
}

fn document(cases: &[Case], elapsed_ms: u64, properties: &[(&str, String)]) -> String {
    let failures = cases.iter().filter(|c| c.status == Status::Failure).count();
    let errors = cases.iter().filter(|c| c.status == Status::Error).count();
    let counts = format!(
        "tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"0\" time=\"{}.{:03}\"",
        cases.len(),
        elapsed_ms / 1000,
        elapsed_ms % 1000
    );
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites {counts}>\n  <testsuite name=\"fai test\" {counts}>\n"
    );
    if !properties.is_empty() {
        xml.push_str("    <properties>\n");
        for (name, value) in properties {
            let _ = writeln!(
                xml,
                "      <property name=\"{}\" value=\"{}\"/>",
                escape(name),
                escape(value)
            );
        }
        xml.push_str("    </properties>\n");
    }
    for case in cases {
        let _ = write!(
            xml,
            "    <testcase name=\"{}\" classname=\"{}\"",
            escape(&case.name),
            escape(&case.class)
        );
        if let Some(file) = &case.file {
            let _ = write!(xml, " file=\"{}\"", escape(file));
        }
        if let Some(line) = case.line {
            let _ = write!(xml, " line=\"{line}\"");
        }
        if case.status == Status::Passed && case.details.is_empty() {
            xml.push_str("/>\n");
            continue;
        }
        xml.push_str(">\n");
        if case.status == Status::Passed {
            let _ = writeln!(xml, "      <system-out>{}</system-out>", escape(&case.details));
        } else {
            let tag = if case.status == Status::Failure { "failure" } else { "error" };
            let _ = writeln!(
                xml,
                "      <{tag} type=\"{}\" message=\"{}\">{}</{tag}>",
                escape(&case.code),
                escape(&case.message),
                escape(&case.details)
            );
        }
        xml.push_str("    </testcase>\n");
    }
    xml.push_str("  </testsuite>\n</testsuites>\n");
    xml
}

// Numeric references preserve whitespace even in attributes. XML 1.0 cannot
// represent other controls, so keep their visible escaped spelling instead.
fn escape(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c if c < '\u{20}' || c == '\u{fffe}' || c == '\u{ffff}' => {
                out.extend(c.escape_unicode())
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        ResultStatus, assemble_outcome,
        tests::{meta, plan, result},
    };

    #[test]
    fn passing_report_has_real_suite_time_but_no_invented_case_time() {
        let p = plan(1);
        let mut outcome = assemble_outcome(&p, &[result(0, ResultStatus::Passed)]);
        outcome.summary.elapsed_ms = 1234;
        let xml = render_junit(&p, &outcome);
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let root = doc.root_element();
        assert_eq!(root.attribute("tests"), Some("1"));
        assert_eq!(root.attribute("failures"), Some("0"));
        assert_eq!(root.attribute("errors"), Some("0"));
        assert_eq!(root.attribute("time"), Some("1.234"));
        let case = root.descendants().find(|n| n.has_tag_name("testcase")).unwrap();
        assert_eq!(case.attribute("name"), Some("M.f0::forall#1"));
        assert_eq!(case.attribute("classname"), Some("M.fai"));
        assert_eq!(case.attribute("file"), Some("M.fai"));
        assert_eq!(case.attribute("line"), Some("3"));
        assert_eq!(case.attribute("time"), None);
        assert!(
            root.descendants()
                .any(|n| n.attribute("name") == Some("trials")
                    && n.attribute("value") == Some("100"))
        );
    }

    #[test]
    fn failed_property_retains_escaped_counterexample_and_diagnostic() {
        let mut p = plan(1);
        p.runnable_meta[0].symbol = Some("M.λ<&\"'".to_owned());
        let mut r = result(0, ResultStatus::Failed);
        r.counterexample = Some("\"<&>\nλ\"".to_owned());
        let xml = render_junit(&p, &assemble_outcome(&p, &[r]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("failures"), Some("1"));
        let failure = doc.descendants().find(|n| n.has_tag_name("failure")).unwrap();
        assert_eq!(failure.attribute("type"), Some("FAI6001"));
        assert!(failure.text().unwrap().contains("counterexample: n = \"<&>\nλ\""));
        assert_eq!(
            doc.descendants().find(|n| n.has_tag_name("testcase")).unwrap().attribute("name"),
            Some("M.λ<&\"'::forall#1")
        );
    }

    #[test]
    fn crash_is_an_error_not_an_assertion_failure() {
        let p = plan(1);
        let xml = render_junit(&p, &assemble_outcome(&p, &[result(0, ResultStatus::Crashed)]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("errors"), Some("1"));
        assert_eq!(doc.root_element().attribute("failures"), Some("0"));
        assert_eq!(
            doc.descendants().find(|n| n.has_tag_name("error")).unwrap().attribute("type"),
            Some("FAI6003")
        );
    }

    #[test]
    fn timeout_is_an_error_with_its_reason() {
        let p = plan(1);
        let xml = render_junit(&p, &assemble_outcome(&p, &[result(0, ResultStatus::TimedOut)]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert!(
            doc.descendants()
                .find(|n| n.has_tag_name("error"))
                .unwrap()
                .attribute("message")
                .unwrap()
                .contains("time limit")
        );
    }

    #[test]
    fn unavailable_generator_is_never_skipped() {
        let mut p = plan(0);
        p.total = 1;
        p.not_runnable.push((
            meta(0),
            "no generator".to_owned(),
            fai_contracts::CONTRACT_NOT_RUNNABLE,
        ));
        let xml = render_junit(&p, &assemble_outcome(&p, &[]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("errors"), Some("1"));
        assert!(!doc.descendants().any(|n| n.has_tag_name("skipped")));
        assert!(xml.contains("FAI6002"));
    }

    #[test]
    fn blocked_contracts_and_compiler_errors_cannot_form_a_green_report() {
        let mut p = plan(1);
        p.blocked = true;
        p.pre_diagnostics.push(Diagnostic::error(
            crate::WORKSPACE_ERROR,
            "cannot compile".to_owned(),
            crate::tooling_span(),
        ));
        let xml = render_junit(&p, &assemble_outcome(&p, &[]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("tests"), Some("2"));
        assert_eq!(doc.root_element().attribute("errors"), Some("2"));
        assert!(xml.contains("contract blocked before execution"));
        assert!(xml.contains("cannot compile"));
    }

    #[test]
    fn a_runtime_leak_marks_a_passing_contract_as_an_error() {
        let p = plan(1);
        let mut r = result(0, ResultStatus::Passed);
        r.live_delta = 1;
        let xml = render_junit(&p, &assemble_outcome(&p, &[r]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("errors"), Some("1"));
        assert!(xml.contains("leaked"));
    }

    #[test]
    fn empty_filtered_selection_is_a_valid_zero_case_report() {
        let mut p = plan(0);
        p.filtered_out = 4;
        let xml = render_junit(&p, &assemble_outcome(&p, &[]));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("tests"), Some("0"));
        assert_eq!(doc.root_element().attribute("errors"), Some("0"));
        assert!(doc.descendants().any(
            |n| n.attribute("name") == Some("filteredOut") && n.attribute("value") == Some("4")
        ));
    }

    #[test]
    fn setup_errors_escape_controls_and_strip_console_colors() {
        let xml = junit_error_report("\x1b[31mboom\x1b[0m \0\u{fffe}\r\n<&>λ");
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(doc.root_element().attribute("errors"), Some("1"));
        let text = doc.descendants().find(|n| n.has_tag_name("error")).unwrap().text().unwrap();
        assert_eq!(text, "boom \\u{0}\\u{fffe}\r\n<&>λ");
    }

    #[test]
    fn contract_names_are_unique_even_with_the_same_subject() {
        let mut p = plan(2);
        p.runnable_meta[1].symbol = p.runnable_meta[0].symbol.clone();
        let xml = render_junit(
            &p,
            &assemble_outcome(
                &p,
                &[result(0, ResultStatus::Passed), result(1, ResultStatus::Passed)],
            ),
        );
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let names = doc
            .descendants()
            .filter(|n| n.has_tag_name("testcase"))
            .map(|n| n.attribute("name").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["M.f0::forall#1", "M.f0::forall#2"]);
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #[test]
            fn arbitrary_diagnostic_text_stays_well_formed(chars in prop::collection::vec(any::<char>(), 0..200)) {
                let text = chars.into_iter().collect::<String>();
                let xml = junit_error_report(&text);
                let doc = roxmltree::Document::parse(&xml).unwrap();
                prop_assert_eq!(doc.root_element().attribute("errors"), Some("1"));
                prop_assert_eq!(doc.descendants().filter(|n| n.has_tag_name("testcase")).count(), 1);
            }
        }
    }
}
