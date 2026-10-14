//! Shared human reporting and machine-readable summary counts for contract runs.

use std::fmt::Write as _;

use fai_diagnostics::render_human;
use fai_span::SpanResolver;
use serde::Serialize;

use super::{ContractEvent, ContractSourceOrigin, ContractStatus, TestOutcome, TestPlan};

/// Disjoint contract outcomes; their sum equals `total`. Filtered contracts are
/// outside that total, and configured property trials are not execution counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestSummary {
    /// Contracts selected for this run.
    pub total: usize,
    /// Selected example declarations.
    pub examples: usize,
    /// Selected property declarations.
    pub properties: usize,
    /// Contracts that held.
    pub passed: usize,
    /// Contracts that returned false.
    pub failed: usize,
    /// Contracts whose worker aborted.
    pub crashed: usize,
    /// Contracts whose worker exceeded its time limit.
    pub timed_out: usize,
    /// Contracts with unavailable generators or unsupported harnesses.
    pub not_run: usize,
    /// Runnable contracts not executed, for example because compilation failed.
    pub blocked: usize,
    /// Contracts in the selected files excluded by `--match`.
    pub filtered_out: usize,
    /// Monotonic wall time from plan preparation through execution, in milliseconds.
    pub elapsed_ms: u64,
}

impl TestSummary {
    pub(super) fn from_run(plan: &TestPlan, events: &[ContractEvent]) -> Self {
        let examples = plan.example_count();
        let mut summary = Self {
            total: plan.total,
            examples,
            properties: plan.total - examples,
            blocked: plan.total.saturating_sub(events.len()),
            filtered_out: plan.filtered_out,
            elapsed_ms: u64::try_from(plan.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..Self::default()
        };
        for event in events {
            match event.status {
                ContractStatus::Passed => summary.passed += 1,
                ContractStatus::Failed => summary.failed += 1,
                ContractStatus::Crashed => summary.crashed += 1,
                ContractStatus::TimedOut => summary.timed_out += 1,
                ContractStatus::NotRun => summary.not_run += 1,
            }
        }
        summary
    }
}

impl TestPlan {
    fn example_count(&self) -> usize {
        self.runnable_meta
            .iter()
            .chain(self.not_runnable.iter().map(|(meta, _, _)| meta))
            .filter(|meta| meta.kind == fai_contracts::ContractKind::Example)
            .count()
    }

    /// Renders the selection and generator settings before execution starts.
    #[must_use]
    pub fn render_header(&self) -> String {
        let examples = self.example_count();
        let properties = self.total - examples;
        let mut out = format!(
            "{} {} {} ({examples} {}, {properties} {})\n",
            if self.blocked { "selected" } else { "running" },
            self.total,
            if self.total == 1 { "contract" } else { "contracts" },
            if examples == 1 { "example" } else { "examples" },
            if properties == 1 { "property" } else { "properties" },
        );
        if self.total > examples {
            let _ = writeln!(
                out,
                "seed: {}; trials per property: {}; max size: {}",
                self.config.seed, self.config.trials, self.config.max_size
            );
        }
        if self.total == 0 {
            out.push_str(if self.filtered_out > 0 {
                "no contracts matched the filter\n"
            } else {
                "no contracts found in the selected files\n"
            });
        }
        out.push('\n');
        out
    }
}

fn styled(text: &str, codes: &str, color: bool) -> String {
    if color { format!("\x1b[{codes}m{text}\x1b[0m") } else { text.to_owned() }
}

/// Keep paths and labels on one line without stripping Unicode names.
fn printable(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn label(event: &ContractEvent) -> String {
    let origin = match event.source.origin {
        ContractSourceOrigin::User => "",
        ContractSourceOrigin::StandardLibrary => "std:",
    };
    let file = printable(&event.source.file);
    let location = match event.location {
        Some(at) => format!("{origin}{file}:{}:{}", at.line, at.column),
        None => format!("{origin}{file}: #{}", event.ordinal + 1),
    };
    match &event.symbol {
        Some(symbol) => format!("{} [{}, {location}]", printable(symbol), printable(&event.kind)),
        None => format!("{} [{location}]", printable(&event.kind)),
    }
}

/// Renders one completed contract without repeating its detailed diagnostics.
/// The daemon and direct CLI paths share this live, line-oriented format.
#[must_use]
pub fn render_test_event_line(event: &ContractEvent, color: bool) -> String {
    let (status, codes) = match event.status {
        ContractStatus::Passed => ("ok", "32"),
        ContractStatus::Failed => ("FAILED", "1;31"),
        ContractStatus::Crashed => ("CRASHED", "1;31"),
        ContractStatus::TimedOut => ("TIMED OUT", "1;31"),
        ContractStatus::NotRun => ("NOT RUN", "33"),
    };
    format!("test {} ... {}\n", label(event), styled(status, codes, color))
}

impl TestOutcome {
    /// Renders detailed failures followed by the authoritative run summary.
    #[must_use]
    pub fn render_human(&self, resolver: &dyn SpanResolver, color: bool) -> String {
        let mut out = String::new();
        if !self.diagnostics.is_empty() {
            out.push_str("\nfailures:\n\n");
            out.push_str(&render_human(&self.diagnostics, resolver, color));
        }
        if self.events.iter().any(|event| event.status != ContractStatus::Passed) {
            out.push_str("\nunsuccessful contracts:\n");
            for event in &self.events {
                if event.status != ContractStatus::Passed {
                    let _ = write!(out, "    {}", render_test_event_line(event, color));
                }
            }
        }
        if self.events.iter().any(|event| {
            event.kind == "forall"
                && matches!(
                    event.status,
                    ContractStatus::Failed | ContractStatus::Crashed | ContractStatus::TimedOut
                )
        }) {
            let _ = writeln!(
                out,
                "\nrerun with the same path/filter and --seed {} --count {} --max-size {}",
                self.config.seed, self.config.trials, self.config.max_size
            );
        }
        let summary = &self.summary;
        if summary.blocked > 0 {
            let _ = writeln!(out, "\n{} contracts blocked before execution", summary.blocked);
        }
        let status = styled(
            if self.ok { "ok" } else { "FAILED" },
            if self.ok { "32" } else { "1;31" },
            color,
        );
        let _ = write!(
            out,
            "\ntest result: {status}. {} passed; {} failed; {} crashed; {} timed out; {} not run",
            summary.passed, summary.failed, summary.crashed, summary.timed_out, summary.not_run
        );
        if summary.blocked > 0 {
            let _ = write!(out, "; {} blocked", summary.blocked);
        }
        let _ = writeln!(
            out,
            "; {} filtered out; finished in {}.{:02}s",
            summary.filtered_out,
            summary.elapsed_ms / 1000,
            (summary.elapsed_ms % 1000) / 10
        );
        out
    }
}
