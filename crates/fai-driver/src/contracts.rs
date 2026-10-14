//! Running `example`/`forall` contracts for `fai test`.
//!
//! The warm front end ([`build_test_plan`]) collects each selected file's
//! contracts, synthesizes a runnable harness per contract ([`fai_contracts`]),
//! reference-counts the synthesized defs together with their reachable callees,
//! and serializes them into a portable [`TestWireBundle`] — plus the render-side
//! metadata (`ContractMeta`) the worker does not need. Each contract is then
//! checked in an **isolated worker subprocess** ([`jit_test_bundle`], driven by
//! [`run_test_workers`]): the worker JIT-compiles the bundle once and applies
//! each harness, streaming a per-contract result; if a generated input drives a
//! body into a runtime trap (e.g. division by zero) the worker aborts, and the
//! supervisor records *that* contract as aborted and re-spawns to resume after
//! it, so one bad contract never aborts the whole run. [`assemble_outcome`] turns
//! the per-contract results plus the plan's metadata into the final
//! [`TestOutcome`]: failures become `FAI6001` (with a shrunk counterexample),
//! aborts `FAI6003`, ungeneratable binders `FAI6002`, and the runtime's
//! per-contract live-object count is asserted to return to baseline.
//!
//! The same execution path ([`run_contracts`]) backs the in-process [`run_tests`]
//! library entry point, which runs the batch in one process (no isolation) for
//! callers that only test known-safe corpora.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod junit;
mod named;
mod report;
pub use junit::{junit_error_report, render_junit};
pub use named::{TestOptions, worker as named_suite_worker};
pub use report::{TestSummary, render_test_event_line};

use fai_codegen::JitProgram;
use fai_contracts::{
    CONTRACT_ABORTED, CONTRACT_FAILED, ContractInfo, ContractKind, run_contract, synthesize,
};
use fai_core::ir::{FnAbi, LoweredDef};
use fai_core::wire::def_to_wire;
use fai_core::{RebuiltTest, TestWireBundle, WireContract, WireDefId, from_wire_test};
use fai_db::{Db, SourceFile};
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{Diagnostic, DiagnosticCode, Severity};
use fai_rc::{BorrowSig, entry_bounds, rc, rc_lowered, result_facts};
use fai_resolve::{DefId, ModuleName, module_name};
use fai_span::{Span, SpanResolver};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::backend::{
    abi_of, apply_run_limits, arity_of, mangle, module_label, precompile_diagnostics,
    reachable_from_roots,
};
use crate::{WORKSPACE_ERROR, semantic_diagnostics, tooling_span};

/// Default wall-clock limit for a single supervised test-worker batch.
const DEFAULT_TEST_TIMEOUT_MS: u64 = 300_000;

/// Default wall-clock limit for `fai check`'s eager example evaluation. Shorter
/// than the test limit: the daemon holds the session lock while checking, so a
/// runaway example must not stall it for long (a timed-out example is dropped,
/// since `fai check` reports only definite failures — see [`example_failures`]).
const DEFAULT_CHECK_TIMEOUT_MS: u64 = 10_000;

/// Generator configuration for a `fai test` run (fixed defaults are deterministic).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestConfig {
    /// The initial PRNG seed.
    pub seed: i64,
    /// The configured random-trial limit per `forall` (failures stop early).
    pub trials: i64,
    /// The maximum generation size.
    pub max_size: i64,
}

impl Default for TestConfig {
    fn default() -> Self {
        TestConfig { seed: 0, trials: 100, max_size: 100 }
    }
}

/// The status of a single contract in the JSON output and `$/testEvent` stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContractStatus {
    /// The contract held.
    Passed,
    /// The contract did not hold (a failing example, or a `forall` counterexample).
    Failed,
    /// The body raised a runtime trap on a generated input (e.g. division by zero).
    Crashed,
    /// The contract did not finish within the time limit.
    TimedOut,
    /// The contract could not be exercised (an ungeneratable binder).
    NotRun,
    /// A named test returned an operation or fixture error.
    Errored,
    /// Intentionally ignored with an explicit reason.
    Skipped,
    /// Discovered for --list without execution.
    Listed,
}

/// The loader-controlled origin of a contract's source, independent of its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContractSourceOrigin {
    /// A workspace or editor-supplied source.
    User,
    /// A source embedded in the compiler's standard library.
    StandardLibrary,
}

/// A portable source key. Together with a file-local ordinal this identifies a
/// contract across filtering, worker restarts, and separate compiler processes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractSource {
    /// The logical source path (workspace-relative for user files, `/` separators).
    pub file: String,
    /// Separates embedded sources from identically spelled user paths.
    pub origin: ContractSourceOrigin,
}

/// A per-contract result in the JSON output and the `$/testEvent` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractEvent {
    /// The contract's source key; identity is `(source, ordinal)`, not `symbol`.
    pub source: ContractSource,
    /// The contract's position among its file's contracts.
    pub ordinal: usize,
    /// One-based coordinates in the source revision compiled for this run.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub location: Option<ContractLocation>,
    /// The subject binding the contract describes (module-qualified), if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub symbol: Option<String>,
    /// `"example"` or `"forall"`.
    pub kind: String,
    /// The outcome.
    pub status: ContractStatus,
    /// The shrunk counterexample for a failing `forall` (binder names + values).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub counterexample: Option<String>,
    /// The PRNG seed the contract ran with.
    pub seed: i64,
    /// The configured trial limit, not the number of predicate evaluations.
    pub trials: i64,
    /// The maximum generation size the contract ran with.
    pub max_size: i64,
    /// Stable named-test identity (source, suite binding and exact name path).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
    /// Stable package/suite identity, independent of database registration order.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub suite: Option<String>,
    /// Exact group/case name components for a named test.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub path: Vec<String>,
    /// Returned structured assertion or operation failures.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<TestFailure>,
    /// An explicit skip reason.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    /// Bounded captured test stdout/stderr.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output: Option<String>,
}

/// An expectation failure or operation error reported by a named test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestFailure {
    /// `assertion` or `operation`.
    pub kind: String,
    /// Human-readable intent or error.
    pub message: String,
    /// Rendered expected value, if applicable.
    pub expected: String,
    /// Rendered actual value, if applicable.
    pub actual: String,
    /// Additional contextual messages.
    pub context: Vec<String>,
    /// Compiled source file for the assertion.
    pub file: String,
    /// One-based coordinates in that source.
    pub location: Option<ContractLocation>,
}

/// One-based source coordinates for a contract (Unicode-scalar columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractLocation {
    /// Line containing the contract declaration.
    pub line: u32,
    /// Column containing the contract declaration.
    pub column: u32,
}

/// The render-side metadata for one contract: everything needed to build its
/// diagnostics and event that the database-free worker does not carry.
#[derive(Debug, Clone)]
pub struct ContractMeta {
    /// The contract's portable source key.
    pub source: ContractSource,
    /// The contract's position among its file's contracts.
    pub ordinal: usize,
    /// `example` or `forall`.
    pub kind: ContractKind,
    /// The `forall` binder names, in order (empty for an `example`).
    pub binders: Vec<String>,
    /// The subject binding (module-qualified), if any.
    pub symbol: Option<String>,
    /// The contract's source span.
    pub span: Span,
    /// Coordinates captured from the same source revision as the harness.
    pub location: Option<ContractLocation>,
    /// The PRNG seed the contract runs with.
    pub seed: i64,
    /// The configured trial limit for this contract.
    pub trials: i64,
    /// The maximum generation size the contract runs with.
    pub max_size: i64,
}

/// A prepared `fai test` run: the portable bundle to ship to the worker, the
/// render-side metadata for the runnable contracts (parallel to
/// `bundle.contracts`), the contracts that cannot be run, the diagnostics that
/// must be clean before running, and the totals.
#[derive(Debug, Clone)]
pub struct TestPlan {
    /// Prepared named suite roots from the selected test source scopes.
    pub suites: Vec<named::PreparedSuite>,
    /// Named-suite discovery and selection policy.
    pub options: TestOptions,
    /// Original name filter, also applied to discovered named tests.
    pub match_pattern: Option<String>,
    /// Immutable coordinates from the revision compiled for this run.
    pub render_spans: TestSpans,
    /// The portable program (defs + contract entries) for the worker.
    pub bundle: TestWireBundle,
    /// Render-side metadata for each runnable contract, in bundle order.
    pub runnable_meta: Vec<ContractMeta>,
    /// Contracts that cannot be run, with the reason and the code to report it
    /// under (a non-groundable type / ambiguous generator use a specific code).
    pub not_runnable: Vec<(ContractMeta, String, DiagnosticCode)>,
    /// Diagnostics that must be clean before running (type/callee errors).
    pub pre_diagnostics: Vec<Diagnostic>,
    /// Total contracts considered (runnable + not-runnable).
    pub total: usize,
    /// Generator settings for the run.
    pub config: TestConfig,
    /// Contracts excluded by the match filter within the selected files.
    pub filtered_out: usize,
    /// Monotonic start of preparation, before synthesis and worker execution.
    pub started: Instant,
    /// Whether an error in `pre_diagnostics` blocks execution.
    pub blocked: bool,
}

/// A detached resolver for all spans a prepared test run can report. It does not
/// hold a database snapshot while workers execute or while inputs are edited.
#[derive(Debug, Clone, Default)]
pub struct TestSpans {
    spans: FxHashMap<Span, fai_span::ResolvedSpan>,
}

impl SpanResolver for TestSpans {
    fn resolve(&self, span: Span) -> Option<fai_span::ResolvedSpan> {
        self.spans.get(&span).cloned()
    }
}

impl TestSpans {
    fn capture(db: &dyn Db, plan: &TestPlan) -> Self {
        let resolver = fai_db::DbSpanResolver::new(db);
        let mut spans = FxHashMap::default();
        let mut record = |span| {
            if let Some(resolved) = resolver.resolve(span) {
                spans.insert(span, resolved);
            }
        };
        for meta in &plan.runnable_meta {
            record(meta.span);
        }
        for (meta, _, _) in &plan.not_runnable {
            record(meta.span);
        }
        for diagnostic in &plan.pre_diagnostics {
            record(diagnostic.primary);
            for label in &diagnostic.secondary {
                record(label.span);
            }
            for suggestion in &diagnostic.suggestions {
                record(suggestion.span);
            }
        }
        Self { spans }
    }
}

/// A single contract's outcome, as determined by the worker or the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractResult {
    /// The contract's position in the runnable list.
    pub position: usize,
    /// The outcome.
    pub status: ResultStatus,
    /// The raw rendered counterexample value (for a failing `forall`).
    pub counterexample: Option<String>,
    /// Net change in the runtime's live-object count while running this contract.
    pub live_delta: i64,
}

impl ContractResult {
    /// Maps a worker frame (which only reports pass/fail) to a result.
    fn from_worker(wr: WorkerResult) -> Self {
        let status = if wr.passed { ResultStatus::Passed } else { ResultStatus::Failed };
        ContractResult {
            position: wr.position,
            status,
            counterexample: wr.counterexample,
            live_delta: wr.live_delta,
        }
    }
}

/// The outcome of running one contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    /// The contract held.
    Passed,
    /// The contract did not hold.
    Failed,
    /// The body aborted at runtime (a trap) before producing a result.
    Crashed,
    /// The contract did not finish within the time limit.
    TimedOut,
}

/// One contract's result as emitted by the worker on its stdout (newline-delimited
/// JSON). The worker is database-free, so it reports only the position, the
/// pass/fail outcome, the raw counterexample, and the live-object delta; the
/// supervisor resolves the rest from the plan's metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerResult {
    /// The contract's position in the runnable list.
    position: usize,
    /// Whether the contract held.
    passed: bool,
    /// The raw rendered counterexample (for a failing `forall`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    counterexample: Option<String>,
    /// Net change in the live-object count while running this contract.
    live_delta: i64,
}

/// The outcome of a `fai test` run.
#[derive(Debug, Clone)]
pub struct TestOutcome {
    /// Whether focus filtered the whole selected invocation.
    pub focused: bool,
    /// Contracts excluded by an active named focus selection.
    pub contracts_suppressed: bool,
    /// Total contracts considered.
    pub total: usize,
    /// Contracts that passed.
    pub passed: usize,
    /// Contracts that could not be run (ungeneratable binders / unsupported).
    pub not_run: usize,
    /// All diagnostics: type errors of selected files plus per-contract results.
    pub diagnostics: Vec<Diagnostic>,
    /// Per-contract results, in source order.
    pub events: Vec<ContractEvent>,
    /// Net change in the runtime's live-object count (should be 0).
    pub leaked: i64,
    /// The PRNG seed the run used.
    pub seed: i64,
    /// Generator settings, including the configured trial limit.
    pub config: TestConfig,
    /// Disjoint outcome counts and elapsed preparation/execution time.
    pub summary: TestSummary,
    /// Whether the run succeeded (no errors, nothing failed, no leak).
    pub ok: bool,
}

/// The JSON envelope for `fai test`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestOutput {
    /// Whether this was a focused run.
    pub focused: bool,
    /// Output schema version.
    pub schema_version: u32,
    /// Total contracts considered.
    pub total: usize,
    /// Contracts that passed.
    pub passed: usize,
    /// Contracts that could not be run.
    pub not_run: usize,
    /// The PRNG seed the run used.
    pub seed: i64,
    /// Generator settings; trials are a configured limit, not observed executions.
    pub config: TestConfig,
    /// Disjoint counts and elapsed time for the run.
    pub summary: TestSummary,
    /// Per-contract results, in source order.
    pub events: Vec<ContractEvent>,
    /// The run's diagnostics, in wire form.
    pub diagnostics: Vec<DiagnosticWire>,
    /// Whether the run succeeded.
    pub ok: bool,
}

impl TestOutcome {
    /// Builds the JSON wire envelope.
    #[must_use]
    pub fn to_output(&self, resolver: &dyn SpanResolver) -> TestOutput {
        TestOutput {
            schema_version: 2,
            focused: self.focused,
            total: self.total,
            passed: self.passed,
            not_run: self.not_run,
            seed: self.seed,
            config: self.config,
            summary: self.summary.clone(),
            events: self.events.clone(),
            diagnostics: to_wire(&self.diagnostics, resolver),
            ok: self.ok,
        }
    }
}

/// Results of one complete contract and named-suite invocation.
pub struct TestExecution {
    contracts: Vec<ContractResult>,
    named: Vec<(ContractEvent, Vec<Diagnostic>)>,
    selection: named::Selection,
    suppressed: bool,
    listed: bool,
}

/// Discover suites before running anything, so focus and CI policy apply to the
/// whole invocation. Named cases execute in fresh workers, in discovery order.
#[must_use]
pub fn execute_tests(plan: &TestPlan, on_event: &mut dyn FnMut(&ContractEvent)) -> TestExecution {
    let selection = if plan.blocked {
        named::Selection::default()
    } else {
        named::discover(plan, plan.match_pattern.as_deref(), plan.options)
    };
    let blocked =
        plan.blocked || selection.diagnostics.iter().any(|d| d.severity == Severity::Error);
    let suppressed = selection.focused;
    let listed = plan.options.list;
    let contracts =
        if blocked || suppressed || listed { Vec::new() } else { run_test_workers(plan, on_event) };
    let mut named_results = Vec::new();
    if !blocked {
        if listed && !suppressed {
            for meta in
                plan.runnable_meta.iter().chain(plan.not_runnable.iter().map(|(meta, _, _)| meta))
            {
                let mut event = not_run_event(meta);
                event.status = ContractStatus::Listed;
                on_event(&event);
            }
        }
        for case in &selection.cases {
            let (completion, output, _) = if listed || case.metadata.skipped {
                (named::Completion::Passed(Vec::new()), String::new(), 0)
            } else {
                named::run(
                    plan,
                    case,
                    selection.manifests.get(&case.suite).map(Vec::as_slice).unwrap_or(&[]),
                )
            };
            let mut status = if listed {
                ContractStatus::Listed
            } else if case.metadata.skipped {
                ContractStatus::Skipped
            } else {
                ContractStatus::Passed
            };
            let mut issues = match completion {
                named::Completion::Passed(issues) => issues,
                named::Completion::Errored(message) => {
                    status = ContractStatus::Errored;
                    vec![named::Issue {
                        kind: "operation".to_owned(),
                        message,
                        actual: String::new(),
                        expected: String::new(),
                        context: Vec::new(),
                        source: case.metadata.source.clone(),
                    }]
                }
                named::Completion::Crashed(message) => {
                    status = ContractStatus::Crashed;
                    vec![named::Issue {
                        kind: "operation".to_owned(),
                        message: format!("test worker crashed: {message}"),
                        actual: String::new(),
                        expected: String::new(),
                        context: Vec::new(),
                        source: case.metadata.source.clone(),
                    }]
                }
                named::Completion::TimedOut(message, mut issues) => {
                    status = ContractStatus::TimedOut;
                    issues.push(named::Issue {
                        kind: "operation".to_owned(),
                        message: format!("test worker timed out: {message}"),
                        actual: String::new(),
                        expected: String::new(),
                        context: Vec::new(),
                        source: case.metadata.source.clone(),
                    });
                    issues
                }
            };
            if status == ContractStatus::Passed && !issues.is_empty() {
                status = if issues.iter().any(|i| i.kind != "assertion") {
                    ContractStatus::Errored
                } else {
                    ContractStatus::Failed
                };
            }
            let suite = &plan.suites[case.suite];
            let (_, location) = named::location(plan, case, &case.metadata.source);
            let mut failures = Vec::new();
            let mut diagnostics = Vec::new();
            for issue in issues.drain(..) {
                let (span, at) = named::location(plan, case, &issue.source);
                let file = plan
                    .render_spans
                    .resolve(span)
                    .map(|s| s.path.to_string())
                    .unwrap_or_else(|| suite.source.file.clone());
                let code = if issue.kind == "assertion" {
                    fai_contracts::NAMED_TEST_FAILED
                } else {
                    fai_contracts::NAMED_TEST_ERROR
                };
                let mut help = issue.context.join("\n");
                if !issue.expected.is_empty() || !issue.actual.is_empty() {
                    help.push_str(&format!(
                        "\nexpected: {}\nactual: {}",
                        issue.expected, issue.actual
                    ));
                }
                let mut diagnostic = Diagnostic::error(code, issue.message.clone(), span);
                if !help.is_empty() {
                    diagnostic = diagnostic.with_help(help);
                }
                diagnostics.push(diagnostic);
                failures.push(TestFailure {
                    kind: issue.kind,
                    message: issue.message,
                    expected: issue.expected,
                    actual: issue.actual,
                    context: issue.context,
                    file,
                    location: at,
                });
            }
            let event = ContractEvent {
                source: suite.source.clone(),
                ordinal: 0,
                location,
                symbol: Some(case.metadata.path.join("::")),
                kind: "test".to_owned(),
                status,
                counterexample: None,
                seed: plan.config.seed,
                trials: plan.config.trials,
                max_size: plan.config.max_size,
                id: Some(named::identity(
                    &suite.source,
                    &suite.bundle.entry.module,
                    &case.metadata.path,
                )),
                suite: Some(suite.bundle.entry.module.clone()),
                path: case.metadata.path.clone(),
                failures,
                reason: case.metadata.skipped.then(|| case.metadata.reason.clone()),
                output: (!output.is_empty()).then_some(output),
            };
            on_event(&event);
            named_results.push((event, diagnostics));
        }
    }
    TestExecution { contracts, named: named_results, selection, suppressed, listed }
}

/// Combine both test kinds into a single versioned report.
#[must_use]
pub fn assemble_execution(plan: &TestPlan, execution: &TestExecution) -> TestOutcome {
    let mut outcome = assemble_outcome(plan, &execution.contracts);
    if execution.suppressed || execution.listed {
        outcome.events.clear();
        outcome.diagnostics = plan.pre_diagnostics.clone();
        if execution.suppressed {
            outcome.total = 0;
        } else {
            for meta in
                plan.runnable_meta.iter().chain(plan.not_runnable.iter().map(|(meta, _, _)| meta))
            {
                let mut event = not_run_event(meta);
                event.status = ContractStatus::Listed;
                outcome.events.push(event);
            }
        }
        outcome.not_run = 0;
        outcome.passed = 0;
        outcome.summary.examples = if execution.suppressed { 0 } else { outcome.summary.examples };
        outcome.summary.properties =
            if execution.suppressed { 0 } else { outcome.summary.properties };
    }
    outcome.focused = execution.suppressed;
    outcome.contracts_suppressed = execution.suppressed;
    outcome.diagnostics.extend(execution.selection.diagnostics.clone());
    for (event, diagnostics) in &execution.named {
        outcome.events.push(event.clone());
        outcome.diagnostics.extend(diagnostics.clone());
    }
    outcome.total += execution.named.len();
    outcome.passed = outcome.events.iter().filter(|e| e.status == ContractStatus::Passed).count();
    outcome.not_run = outcome.events.iter().filter(|e| e.status == ContractStatus::NotRun).count();
    let summary = &mut outcome.summary;
    summary.total = outcome.total;
    summary.named = execution.named.len();
    summary.passed = outcome.passed;
    summary.failed = outcome.events.iter().filter(|e| e.status == ContractStatus::Failed).count();
    summary.crashed = outcome.events.iter().filter(|e| e.status == ContractStatus::Crashed).count();
    summary.timed_out =
        outcome.events.iter().filter(|e| e.status == ContractStatus::TimedOut).count();
    summary.not_run = outcome.not_run;
    summary.errored = outcome.events.iter().filter(|e| e.status == ContractStatus::Errored).count();
    summary.skipped = outcome.events.iter().filter(|e| e.status == ContractStatus::Skipped).count();
    summary.listed = outcome.events.iter().filter(|e| e.status == ContractStatus::Listed).count();
    summary.blocked = outcome.total.saturating_sub(outcome.events.len());
    summary.filtered_out = plan.filtered_out
        + execution.selection.filtered
        + if execution.suppressed { plan.total } else { 0 };
    summary.elapsed_ms = plan.started.elapsed().as_millis() as u64;
    outcome.ok = outcome.leaked == 0
        && !outcome.diagnostics.iter().any(|d| d.severity == Severity::Error)
        && summary.blocked == 0;
    outcome
}

/// Runs the contracts in `files` (filtered by `match_pat` against the subject
/// symbol / module) in this process, returning their outcome. The whole batch
/// runs in one process with no isolation — a contract that traps aborts the
/// caller — so this is for callers testing known-safe corpora (the CLI/daemon
/// use the supervised worker via [`run_test_workers`]).
#[must_use]
pub fn run_tests(
    db: &dyn Db,
    files: &[SourceFile],
    match_pat: Option<&str>,
    config: TestConfig,
) -> TestOutcome {
    let plan = build_test_plan(db, files, match_pat, config);
    let results = if plan.blocked || plan.bundle.contracts.is_empty() {
        Vec::new()
    } else {
        let rebuilt = from_wire_test(&plan.bundle);
        let mut results = Vec::new();
        let mut sink = |wr: WorkerResult| results.push(ContractResult::from_worker(wr));
        run_contracts(&rebuilt, 0, &mut sink);
        results
    };
    assemble_outcome(&plan, &results)
}

/// `fai check`'s eager example check: evaluates the closed `example` contracts in
/// `files` in an isolated worker — so a trapping or looping example cannot crash
/// or hang the checker — and returns only the located `FAI6001` failures. Aborts,
/// timeouts, and leaks are dropped here ([`example_failures`]); `fai test` stays
/// authoritative for those and for `forall`s. Returns nothing when there is no
/// example to run or a reachable callee fails to compile (the plan is blocked).
#[must_use]
pub fn check_examples(db: &dyn Db, files: &[SourceFile]) -> Vec<Diagnostic> {
    let plan = build_example_plan(db, files, TestConfig::default());
    if plan.blocked || plan.bundle.contracts.is_empty() {
        return Vec::new();
    }
    let results = run_test_workers_with_timeout(&plan, check_timeout(), &mut |_| {});
    example_failures(&plan, &results)
}

/// Like [`check_examples`], but evaluates the examples in this process with no
/// worker isolation — for callers testing known-safe corpora (a trapping example
/// would abort the caller). The CLI, daemon, and LSP use [`check_examples`].
#[must_use]
pub fn check_examples_in_process(db: &dyn Db, files: &[SourceFile]) -> Vec<Diagnostic> {
    let plan = build_example_plan(db, files, TestConfig::default());
    if plan.blocked || plan.bundle.contracts.is_empty() {
        return Vec::new();
    }
    let rebuilt = from_wire_test(&plan.bundle);
    let mut results = Vec::new();
    let mut sink = |wr: WorkerResult| results.push(ContractResult::from_worker(wr));
    run_contracts(&rebuilt, 0, &mut sink);
    example_failures(&plan, &results)
}

/// The located `FAI6001` diagnostics for the failed `example`s in `results`.
/// Crashes/timeouts (`FAI6003`), live-object leaks, and not-runnable binders
/// (`FAI6002`) are deliberately omitted: `fai check` reports only definite
/// example failures and defers the rest to `fai test`.
#[must_use]
pub fn example_failures(plan: &TestPlan, results: &[ContractResult]) -> Vec<Diagnostic> {
    assemble_outcome(plan, results)
        .diagnostics
        .into_iter()
        .filter(|d| d.code == CONTRACT_FAILED)
        .collect()
}

/// Builds a prepared [`TestPlan`] for the contracts in `files` (filtered by
/// `match_pat`): collects, synthesizes, reference-counts, and serializes the
/// runnable contracts into a portable bundle, alongside the render-side metadata
/// and the diagnostics that must be clean before running.
#[must_use]
pub fn build_test_plan(
    db: &dyn Db,
    files: &[SourceFile],
    match_pat: Option<&str>,
    config: TestConfig,
) -> TestPlan {
    build_test_plan_with_options(db, files, match_pat, config, TestOptions::default())
}

/// Prepares contracts and pure named-suite roots without executing either.
#[must_use]
pub fn build_test_plan_with_options(
    db: &dyn Db,
    files: &[SourceFile],
    match_pat: Option<&str>,
    config: TestConfig,
    options: TestOptions,
) -> TestPlan {
    let mut plan = build_plan(db, files, match_pat, config, |_| true);
    // A malformed test root may not survive parsing as a binding. Validate the
    // selected source scope even when it contains no discoverable contracts.
    for &file in files {
        plan.pre_diagnostics.extend(crate::semantic_diagnostics(db, file));
    }
    let (suites, diagnostics) = named::prepare(db, files);
    plan.suites = suites;
    plan.options = options;
    plan.match_pattern = match_pat.map(str::to_owned);
    plan.pre_diagnostics.extend(diagnostics);
    plan.pre_diagnostics.sort_by(|a, b| {
        (a.primary.source(), a.primary.start(), a.code.as_str(), &a.message).cmp(&(
            b.primary.source(),
            b.primary.start(),
            b.code.as_str(),
            &b.message,
        ))
    });
    plan.pre_diagnostics.dedup();
    plan.blocked = plan.pre_diagnostics.iter().any(|d| d.severity == Severity::Error);
    plan.render_spans = TestSpans::capture(db, &plan);
    let resolver = fai_db::DbSpanResolver::new(db);
    for suite in &plan.suites {
        for span in std::iter::once(&suite.span).chain(suite.locations.values()) {
            if let Some(resolved) = resolver.resolve(*span) {
                plan.render_spans.spans.insert(*span, resolved);
            }
        }
    }
    plan
}

/// Builds a prepared [`TestPlan`] containing only the closed `example` contracts
/// in `files` — the contracts `fai check` evaluates eagerly. `forall`s (which
/// need generated inputs) are excluded; they remain `fai test`'s responsibility.
#[must_use]
pub fn build_example_plan(db: &dyn Db, files: &[SourceFile], config: TestConfig) -> TestPlan {
    build_plan(db, files, None, config, |kind| kind == ContractKind::Example)
}

/// Attach project-native dependencies and the working directory before dropping
/// the preparation snapshot. Discovery itself never loads native libraries.
pub fn configure_test_plan(plan: &mut TestPlan, session: &crate::Session) {
    for suite in &mut plan.suites {
        suite.working_directory = Some(session.root().to_string());
        match session.native_dependencies(suite.file) {
            Ok(native) => suite.bundle.libraries = native.dynamic_library_paths(),
            Err(error) => {
                plan.pre_diagnostics.push(Diagnostic::error(
                    crate::WORKSPACE_ERROR,
                    error.to_string(),
                    suite.span,
                ));
                plan.blocked = true;
            }
        }
    }
}

/// The shared body of [`build_test_plan`]/[`build_example_plan`]: collects the
/// contracts accepted by `accept` (and the `match_pat` filter), synthesizes,
/// reference-counts, and serializes them into a portable bundle.
fn build_plan(
    db: &dyn Db,
    files: &[SourceFile],
    match_pat: Option<&str>,
    config: TestConfig,
    accept: impl Fn(ContractKind) -> bool,
) -> TestPlan {
    let started = Instant::now();
    // Collect contracts, keeping each with its file.
    let mut items: Vec<(SourceFile, ContractInfo)> = Vec::new();
    let mut filtered_out = 0;
    for &file in files {
        for info in fai_contracts::contracts(db, file) {
            if accept(info.kind) {
                if matches_filter(db, file, &info, match_pat) {
                    items.push((file, info));
                } else {
                    filtered_out += 1;
                }
            }
        }
    }
    let total = items.len();

    // Type errors of the selected files must be clean before running (a broken
    // body would JIT to nonsense). Reported, and they fail the run.
    let mut pre_diagnostics =
        if files.iter().any(|file| matches!(file.origin(db), fai_db::SourceOrigin::Package(_))) {
            crate::check(db, files).diagnostics
        } else {
            Vec::new()
        };
    let mut seen_files = FxHashSet::default();
    for (file, _) in &items {
        if seen_files.insert(file.source(db)) {
            pre_diagnostics.extend(semantic_diagnostics(db, *file));
        }
    }

    // Synthesize each contract into a harness, or record why it cannot run.
    let mut runnable: Vec<(fai_contracts::SynthContract, ContractMeta)> = Vec::new();
    let mut not_runnable: Vec<(ContractMeta, String, DiagnosticCode)> = Vec::new();
    for (file, info) in &items {
        let meta = contract_meta(db, *file, info, config);
        match synthesize(db, *file, info) {
            Ok(s) => runnable.push((s, meta)),
            Err(nr) => not_runnable.push((meta, nr.reason, nr.code)),
        }
    }

    // Reference-count and gather the synthesized defs (in deterministic order)
    // and the real callees reachable from them.
    let mut synth_list: Vec<(LoweredDef, usize)> = Vec::new();
    let mut synth_ids: FxHashSet<DefId> = FxHashSet::default();
    let mut roots: Vec<DefId> = Vec::new();
    let all_owned = |n: usize| BorrowSig(vec![false; n]);
    for (s, _) in &runnable {
        let entry = rc_lowered(db, &s.entry, &all_owned(s.entry_arity));
        let prop = rc_lowered(db, &s.prop, &all_owned(s.prop_arity));
        roots.extend(entry.referenced_globals());
        roots.extend(prop.referenced_globals());
        synth_ids.insert(s.entry.def);
        synth_ids.insert(s.prop.def);
        synth_list.push((entry, s.entry_arity));
        synth_list.push((prop, s.prop_arity));
        for (extra, arity) in &s.extra {
            let rcd = rc_lowered(db, extra, &all_owned(*arity));
            roots.extend(rcd.referenced_globals());
            synth_ids.insert(extra.def);
            synth_list.push((rcd, *arity));
        }
    }
    let reachable = reachable_from_roots(db, &roots, &synth_ids);

    // Callees must compile cleanly too.
    pre_diagnostics.append(&mut precompile_diagnostics(db, &reachable));
    let blocked = pre_diagnostics.iter().any(|d| d.severity == Severity::Error);

    // Serialize the synthesized defs (deterministic order) then the callees.
    let module_of = |d: DefId| module_label(db, d);
    let mut defs = Vec::with_capacity(synth_list.len() + reachable.len());
    for (lowered, arity) in &synth_list {
        // Synthesized harnesses/generators/properties have no source signature;
        // they are reached only via `apply_n`, so the uniform (boxed) ABI is both
        // correct and what that path requires.
        defs.push(def_to_wire(
            lowered,
            &module_of,
            *arity,
            FnAbi::default(),
            Vec::new(),
            fai_core::BoundSig::default(),
            fai_core::ResultSig::default(),
        ));
    }
    for def in &reachable {
        if let Some(file) = db.source_file(def.file) {
            let lowered = rc(db, file, def.name);
            defs.push(def_to_wire(
                &lowered,
                &module_of,
                arity_of(db, *def),
                abi_of(db, *def),
                Vec::new(),
                (*entry_bounds(db, file, def.name)).clone(),
                (*result_facts(db, file, def.name)).clone(),
            ));
        }
    }
    // The reachable bodies call synthesized deforestation loops (introduced by
    // fusion in `rc`), so the harness must carry them too — with their own
    // raw-scalar register ABI so direct calls into them marshal correctly.
    let fusion = crate::backend::program_fusion(db, &reachable);
    for fused in &fusion.loops {
        let arity = fusion.arity.get(&fused.def).copied().unwrap_or(0);
        let abi = fusion.abi.get(&fused.def).cloned().unwrap_or_default();
        defs.push(def_to_wire(
            fused,
            &module_of,
            arity,
            abi,
            Vec::new(),
            crate::backend::bounds_entry_of(db, fused.def),
            crate::backend::bounds_result_of(db, fused.def),
        ));
    }

    // The contract entries to apply, parallel to their render-side metadata.
    let mut contracts = Vec::with_capacity(runnable.len());
    let mut runnable_meta = Vec::with_capacity(runnable.len());
    for (s, meta) in &runnable {
        contracts.push(WireContract {
            id: WireDefId {
                module: module_of(s.entry.def),
                name: s.entry.def.name.as_str().to_owned(),
            },
            ordinal: meta.ordinal,
            seed: meta.seed,
            trials: meta.trials,
            max_size: meta.max_size,
        });
        runnable_meta.push(meta.clone());
    }

    let mut plan = TestPlan {
        suites: Vec::new(),
        options: TestOptions::default(),
        match_pattern: match_pat.map(str::to_owned),
        render_spans: TestSpans::default(),
        bundle: TestWireBundle { defs, contracts },
        runnable_meta,
        not_runnable,
        pre_diagnostics,
        total,
        config,
        filtered_out,
        started,
        blocked,
    };
    plan.render_spans = TestSpans::capture(db, &plan);
    plan
}

/// Assembles the final outcome from a plan and the per-contract results (empty
/// when execution was blocked or there was nothing to run): builds each
/// contract's diagnostic (`FAI6001`/`FAI6003`) and event, appends the
/// not-runnable diagnostics (`FAI6002`) and events, and computes the totals.
#[must_use]
pub fn assemble_outcome(plan: &TestPlan, results: &[ContractResult]) -> TestOutcome {
    let mut diagnostics = plan.pre_diagnostics.clone();
    let mut events = Vec::with_capacity(results.len() + plan.not_runnable.len());
    let mut passed = 0;
    let mut leaked = 0i64;

    for r in results {
        let meta = &plan.runnable_meta[r.position];
        events.push(resolve_event(meta, r));
        match r.status {
            ResultStatus::Passed => passed += 1,
            ResultStatus::Failed => {
                diagnostics.push(failed_diagnostic(meta, r.counterexample.as_deref()));
            }
            ResultStatus::Crashed | ResultStatus::TimedOut => {
                diagnostics.push(aborted_diagnostic(meta, r.status));
            }
        }
        if r.live_delta != 0 {
            leaked += r.live_delta;
            diagnostics.push(leak_diagnostic(meta, r.live_delta));
        }
    }

    for (meta, reason, code) in &plan.not_runnable {
        diagnostics.push(not_runnable_diagnostic(meta, reason, *code));
        events.push(not_run_event(meta));
    }

    events.sort_by(|a, b| (&a.source, a.ordinal).cmp(&(&b.source, b.ordinal)));
    diagnostics.sort_by(|a, b| {
        (a.primary.start().raw(), a.code.as_str()).cmp(&(b.primary.start().raw(), b.code.as_str()))
    });
    let summary = TestSummary::from_run(plan, &events);
    let ok = summary.blocked == 0
        && leaked == 0
        && !diagnostics.iter().any(|d| d.severity == Severity::Error);
    TestOutcome {
        focused: false,
        contracts_suppressed: false,
        total: plan.total,
        passed,
        not_run: plan.not_runnable.len(),
        diagnostics,
        events,
        leaked,
        seed: plan.config.seed,
        config: plan.config,
        summary,
        ok,
    }
}

/// Supervises the contracts in `plan` in isolated worker subprocesses, returning
/// each contract's result. The first spawn runs the whole batch; if a worker
/// aborts (a trap) or times out, the offending contract is recorded as
/// aborted/timed-out and a fresh worker resumes after it, so one bad contract
/// never takes down the run. `on_event` is called with each resolved event as it
/// is determined (the daemon forwards these as `$/testEvent`).
#[must_use]
pub fn run_test_workers(
    plan: &TestPlan,
    on_event: &mut dyn FnMut(&ContractEvent),
) -> Vec<ContractResult> {
    run_test_workers_with_timeout(plan, test_timeout(), on_event)
}

/// Like [`run_test_workers`], but with a caller-chosen per-worker wall-clock
/// `timeout`. `fai check`'s eager example evaluation uses a shorter limit than
/// `fai test` so a runaway example cannot hold the daemon session lock for long.
#[must_use]
pub fn run_test_workers_with_timeout(
    plan: &TestPlan,
    timeout: Duration,
    on_event: &mut dyn FnMut(&ContractEvent),
) -> Vec<ContractResult> {
    for (meta, _, _) in &plan.not_runnable {
        on_event(&not_run_event(meta));
    }
    let n = plan.bundle.contracts.len();
    if n == 0 {
        return Vec::new();
    }
    let bundle_path = match write_test_bundle(&plan.bundle) {
        Ok(path) => path,
        Err(_) => {
            // The bundle could not be shipped: report every contract as aborted.
            return (0..n)
                .map(|position| {
                    let r = ContractResult {
                        position,
                        status: ResultStatus::Crashed,
                        counterexample: None,
                        live_delta: 0,
                    };
                    on_event(&resolve_event(&plan.runnable_meta[position], &r));
                    r
                })
                .collect();
        }
    };
    let mut spawn = |start: usize, sink: &mut dyn FnMut(WorkerResult)| {
        spawn_and_read(&bundle_path, start, timeout, sink)
    };
    let mut on_result =
        |r: &ContractResult| on_event(&resolve_event(&plan.runnable_meta[r.position], r));
    let results = resume_loop(n, &mut spawn, &mut on_result);
    let _ = std::fs::remove_file(&bundle_path);
    results
}

/// The resume state machine: spawn a worker for the contracts from `start`, fold
/// in the results it streamed, and — if it died before finishing — record the
/// first un-acked contract as aborted (or timed out) and resume after it. Pure
/// over the `spawn` closure, so it is unit-tested with a mock spawner. Each spawn
/// advances past at least one contract, so it terminates in at most `n` spawns.
fn resume_loop(
    n: usize,
    mut spawn: impl FnMut(usize, &mut dyn FnMut(WorkerResult)) -> ExitKind,
    on_result: &mut dyn FnMut(&ContractResult),
) -> Vec<ContractResult> {
    let mut results = Vec::with_capacity(n);
    while results.len() < n {
        let exit = spawn(results.len(), &mut |wr| {
            // Only the next expected identity can be acknowledged. Duplicated,
            // out-of-range, and out-of-order frames never alter earlier results.
            if wr.position == results.len() && results.len() < n {
                let r = ContractResult::from_worker(wr);
                on_result(&r);
                results.push(r);
            }
        });
        if results.len() < n {
            let pos = results.len();
            let status = if matches!(exit, ExitKind::Timeout) {
                ResultStatus::TimedOut
            } else {
                ResultStatus::Crashed
            };
            let r = ContractResult { position: pos, status, counterexample: None, live_delta: 0 };
            on_result(&r);
            results.push(r);
        }
    }
    results
}

/// How a worker process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitKind {
    /// Exited successfully (ran every contract it was asked to).
    Clean,
    /// Killed for exceeding the wall-clock limit.
    Timeout,
    /// Exited abnormally (a signal/abort) before finishing.
    Crash,
}

/// Spawns the `__test-worker` subprocess on `bundle_path` starting at contract
/// `start`, reading its newline-delimited [`WorkerResult`] frames until it exits,
/// and enforcing `timeout` (killing the worker on expiry). Delivers each frame
/// immediately to `on_result`, then returns how the worker ended.
fn spawn_and_read(
    bundle_path: &Path,
    start: usize,
    timeout: Duration,
    on_result: &mut dyn FnMut(WorkerResult),
) -> ExitKind {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return ExitKind::Crash,
    };
    let cpu_secs = timeout.as_secs().max(1);
    let child = Command::new(exe)
        .arg("__test-worker")
        .arg(bundle_path)
        .arg(start.to_string())
        .env("FAI_RUN_CPU_SECS", cpu_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(_) => return ExitKind::Crash,
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::sync_channel::<WorkerResult>(1);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(result) = serde_json::from_str::<WorkerResult>(&line)
                && tx.send(result).is_err()
            {
                break;
            }
        }
    });
    // Drain stderr so a chatty worker (e.g. the abort message) never blocks on a
    // full pipe; its content is diagnostic noise the supervisor discards.
    let draining = std::thread::spawn(move || {
        let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
    });

    let (code_tx, code_rx) = mpsc::channel::<ExitKind>();
    std::thread::spawn(move || {
        let kind = match child.wait_timeout(timeout) {
            Ok(Some(status)) => {
                if status.success() {
                    ExitKind::Clean
                } else {
                    ExitKind::Crash
                }
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                ExitKind::Timeout
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                ExitKind::Crash
            }
        };
        let _ = code_tx.send(kind);
    });

    for result in rx {
        on_result(result);
    }
    let _ = reader.join();
    let _ = draining.join();
    code_rx.recv().unwrap_or(ExitKind::Crash)
}

/// The worker side of `fai test`: reconstructs a bundle, JIT-compiles it once,
/// and applies each contract from `start_index`, writing one [`WorkerResult`]
/// frame per contract to `out`. Applies any requested resource limits first.
pub fn jit_test_bundle(bundle: &TestWireBundle, start_index: usize, out: &mut dyn Write) -> i32 {
    apply_run_limits();
    let rebuilt = from_wire_test(bundle);
    let mut sink = |wr: WorkerResult| {
        if let Ok(line) = serde_json::to_string(&wr) {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    };
    run_contracts(&rebuilt, start_index, &mut sink);
    0
}

/// Compiles `rebuilt`'s definitions into one JIT image and applies each contract
/// entry from `start_index`, reporting each result (and its live-object delta)
/// through `sink`. Shared by the in-process runner and the worker.
fn run_contracts(rebuilt: &RebuiltTest, start_index: usize, sink: &mut dyn FnMut(WorkerResult)) {
    let labels = &rebuilt.module_labels;
    let arities = &rebuilt.arities;
    let abis = &rebuilt.abis;
    let namer = |d: DefId| mangle(labels.get(&d.file).map_or("M", String::as_str), d.name.as_str());
    let arity = |d: DefId| arities.get(&d).copied().unwrap_or(0);
    let abi = |d: DefId| abis.get(&d).cloned().unwrap_or_default();
    let borrows: FxHashMap<DefId, Vec<bool>> =
        rebuilt.defs.iter().map(|d| (d.def, d.entry_borrowed.clone())).collect();
    let borrow = |d: DefId| borrows.get(&d).cloned().unwrap_or_default();
    let entry_of = |d: DefId| rebuilt.bounds_entry.get(&d).cloned().unwrap_or_default();
    let result_of = |d: DefId| rebuilt.bounds_result.get(&d).cloned().unwrap_or_default();
    // Contracts are pure (no `Runtime`, so no capabilities), so they never use
    // concurrency: keep the single-threaded code paths.
    let bce = fai_codegen::Bce {
        entry_of: &entry_of,
        result_of: &result_of,
        shadow: false,
        concurrent: false,
    };
    let mut program = JitProgram::compile(&rebuilt.defs, &namer, &arity, &abi, &borrow, &bce);
    for position in start_index..rebuilt.contracts.len() {
        #[cfg(debug_assertions)]
        wait_for_event_test_gate(position);
        let c = &rebuilt.contracts[position];
        // The live-object counter is compiled in only under `debug_assertions`, so
        // `live_count` is zero (and `live_delta` always zero) in a release-built
        // toolchain: per-contract leak detection is a debug/test-build feature.
        let base = fai_runtime::live_count();
        let closure = program.closure_value(&namer, c.def);
        let outcome = run_contract(closure, c.seed, c.trials, c.max_size);
        let live_delta = fai_runtime::live_count() - base;
        sink(WorkerResult {
            position,
            passed: outcome.passed,
            counterexample: outcome.counterexample,
            live_delta,
        });
    }
}

/// Debug-build synchronization for streaming tests: the second contract waits
/// until the observer acknowledges receipt of the first contract's event.
#[cfg(debug_assertions)]
fn wait_for_event_test_gate(position: usize) {
    if position != 1 {
        return;
    }
    if let Some(path) = std::env::var_os("FAI_TEST_EVENT_GATE") {
        while !Path::new(&path).exists() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The supervised-test wall-clock limit (`FAI_TEST_TIMEOUT_MS`, default 300s).
fn test_timeout() -> Duration {
    let ms = std::env::var("FAI_TEST_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TEST_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// The eager-example-check wall-clock limit (`FAI_CHECK_TIMEOUT_MS`, default 10s).
fn check_timeout() -> Duration {
    let ms = std::env::var("FAI_CHECK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_CHECK_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// Serializes a test bundle to a unique temp file (JSON), returning its path.
fn write_test_bundle(bundle: &TestWireBundle) -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fai-test-bundle-{}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let json = serde_json::to_vec(bundle).map_err(|e| format!("serializing test bundle: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("writing test bundle: {e}"))?;
    Ok(path)
}

/// Builds the render-side metadata for one contract.
fn contract_meta(
    db: &dyn Db,
    file: SourceFile,
    info: &ContractInfo,
    config: TestConfig,
) -> ContractMeta {
    let span = Span::new(file.source(db), info.span);
    let location = fai_db::DbSpanResolver::new(db).resolve(span).map(|resolved| ContractLocation {
        line: resolved.start.line,
        column: resolved.start.column,
    });
    let symbol = info.subject.map(|subject| match module_name(db, file) {
        Some(ModuleName(m)) => format!("{}.{}", m.as_str(), subject.as_str()),
        None => subject.as_str().to_owned(),
    });
    ContractMeta {
        source: ContractSource {
            file: file.path(db).replace(std::path::MAIN_SEPARATOR, "/"),
            origin: match file.origin(db) {
                fai_db::SourceOrigin::User | fai_db::SourceOrigin::Package(_) => {
                    ContractSourceOrigin::User
                }
                fai_db::SourceOrigin::StandardLibrary => ContractSourceOrigin::StandardLibrary,
            },
        },
        ordinal: info.ordinal,
        kind: info.kind,
        binders: info.binders.iter().map(|s| s.as_str().to_owned()).collect(),
        symbol,
        span,
        location,
        seed: config.seed,
        trials: config.trials,
        max_size: config.max_size,
    }
}

/// Resolves a raw result to the public per-contract event using the metadata.
fn resolve_event(meta: &ContractMeta, result: &ContractResult) -> ContractEvent {
    let status = match result.status {
        ResultStatus::Passed => ContractStatus::Passed,
        ResultStatus::Failed => ContractStatus::Failed,
        ResultStatus::Crashed => ContractStatus::Crashed,
        ResultStatus::TimedOut => ContractStatus::TimedOut,
    };
    ContractEvent {
        source: meta.source.clone(),
        ordinal: meta.ordinal,
        location: meta.location,
        symbol: meta.symbol.clone(),
        kind: meta.kind.keyword().to_owned(),
        status,
        counterexample: result.counterexample.as_ref().map(|raw| binding_str(&meta.binders, raw)),
        seed: meta.seed,
        trials: meta.trials,
        max_size: meta.max_size,
        id: None,
        suite: None,
        path: Vec::new(),
        failures: Vec::new(),
        reason: None,
        output: None,
    }
}

/// The event for a contract that could not be run (an ungeneratable binder).
fn not_run_event(meta: &ContractMeta) -> ContractEvent {
    ContractEvent {
        source: meta.source.clone(),
        ordinal: meta.ordinal,
        location: meta.location,
        symbol: meta.symbol.clone(),
        kind: meta.kind.keyword().to_owned(),
        status: ContractStatus::NotRun,
        counterexample: None,
        seed: meta.seed,
        trials: meta.trials,
        max_size: meta.max_size,
        id: None,
        suite: None,
        path: Vec::new(),
        failures: Vec::new(),
        reason: None,
        output: None,
    }
}

/// Whether a contract passes the `--match` filter (against its subject or module).
fn matches_filter(db: &dyn Db, file: SourceFile, info: &ContractInfo, pat: Option<&str>) -> bool {
    let Some(pat) = pat else { return true };
    if let Some(subject) = info.subject
        && subject.as_str().contains(pat)
    {
        return true;
    }
    fai_resolve::module_name(db, file).is_some_and(|m| m.0.as_str().contains(pat))
}

/// Builds the `FAI6001` diagnostic for a failed contract.
fn failed_diagnostic(meta: &ContractMeta, counterexample: Option<&str>) -> Diagnostic {
    match meta.kind {
        ContractKind::Example => {
            Diagnostic::error(CONTRACT_FAILED, "this example does not hold".to_owned(), meta.span)
        }
        ContractKind::Forall => {
            let mut diag = Diagnostic::error(
                CONTRACT_FAILED,
                "this property does not hold".to_owned(),
                meta.span,
            );
            if let Some(ce) = counterexample {
                diag =
                    diag.with_help(format!("counterexample: {}", binding_str(&meta.binders, ce)));
            }
            diag
        }
    }
}

/// Builds the `FAI6003` diagnostic for a contract that aborted at runtime.
fn aborted_diagnostic(meta: &ContractMeta, status: ResultStatus) -> Diagnostic {
    let keyword = meta.kind.keyword();
    let (message, help) = match status {
        ResultStatus::TimedOut => (
            format!("this {keyword} did not finish within the time limit"),
            "it may loop forever on a generated input; each contract runs in isolation, so the \
             rest of the run continued",
        ),
        _ => (
            format!("this {keyword} aborted while running"),
            "a generated input drove the body into a runtime trap (e.g. integer division by \
             zero); each contract runs in isolation, so the rest of the run continued",
        ),
    };
    Diagnostic::error(CONTRACT_ABORTED, message, meta.span).with_help(help.to_owned())
}

/// Renders a counterexample binding: `n = 3` for one binder, `(a, b) = (1, 2)`
/// for several (the value `ce` is the generated tuple's rendering).
fn binding_str(binders: &[String], ce: &str) -> String {
    match binders {
        [one] => format!("{one} = {ce}"),
        many => format!("({}) = {ce}", many.join(", ")),
    }
}

/// Builds the "cannot be run" diagnostic for a contract, under `code` (usually
/// `FAI6002`, but a more specific code for a non-groundable type or an ambiguous
/// custom generator).
fn not_runnable_diagnostic(meta: &ContractMeta, reason: &str, code: DiagnosticCode) -> Diagnostic {
    Diagnostic::error(
        code,
        format!("this {} cannot be run: {reason}", meta.kind.keyword()),
        meta.span,
    )
}

/// Builds the internal-error diagnostic for a contract that leaked live objects
/// (a reference-counting soundness failure), located at the contract.
fn leak_diagnostic(meta: &ContractMeta, leaked: i64) -> Diagnostic {
    let span = if meta.span.source().raw() == u32::MAX { tooling_span() } else { meta.span };
    Diagnostic::error(
        WORKSPACE_ERROR,
        format!(
            "internal error: {leaked} live object(s) leaked while running this {}",
            meta.kind.keyword()
        ),
        span,
    )
}

#[cfg(test)]
mod tests {
    use fai_span::{ByteOffset, SourceId, TextRange};

    use super::*;

    pub(super) fn meta(ordinal: usize) -> ContractMeta {
        ContractMeta {
            source: ContractSource { file: "M.fai".to_owned(), origin: ContractSourceOrigin::User },
            ordinal,
            kind: ContractKind::Forall,
            binders: vec!["n".to_owned()],
            symbol: Some(format!("M.f{ordinal}")),
            span: Span::new(SourceId::new(0), TextRange::empty(ByteOffset::ZERO)),
            location: Some(ContractLocation { line: 3, column: 1 }),
            seed: 0,
            trials: 100,
            max_size: 100,
        }
    }

    pub(super) fn plan(n: usize) -> TestPlan {
        TestPlan {
            suites: Vec::new(),
            options: TestOptions::default(),
            match_pattern: None,
            render_spans: TestSpans::default(),
            bundle: TestWireBundle {
                defs: Vec::new(),
                contracts: (0..n)
                    .map(|i| WireContract {
                        id: WireDefId { module: "M".to_owned(), name: format!("contract#{i}") },
                        ordinal: i,
                        seed: 0,
                        trials: 100,
                        max_size: 100,
                    })
                    .collect(),
            },
            runnable_meta: (0..n).map(meta).collect(),
            not_runnable: Vec::new(),
            pre_diagnostics: Vec::new(),
            total: n,
            config: TestConfig::default(),
            filtered_out: 0,
            started: Instant::now(),
            blocked: false,
        }
    }

    fn pass(position: usize) -> WorkerResult {
        WorkerResult { position, passed: true, counterexample: None, live_delta: 0 }
    }

    fn nop(_: &ContractResult) {}

    #[test]
    fn resumed_events_retain_source_identity_for_each_status() {
        let mut p = plan(4);
        p.runnable_meta[0].source.file = "A.fai".to_owned();
        p.runnable_meta[1].source.file = "B.fai".to_owned();
        p.runnable_meta[2].source.file = "C.fai".to_owned();
        p.runnable_meta[3].source.file = "D.fai".to_owned();
        let mut events = Vec::new();
        let results = resume_loop(
            4,
            |start, sink| match start {
                0 => {
                    sink(pass(0));
                    ExitKind::Crash
                }
                2 => ExitKind::Timeout,
                3 => {
                    sink(WorkerResult { passed: false, ..pass(3) });
                    ExitKind::Clean
                }
                _ => unreachable!(),
            },
            &mut |result| events.push(resolve_event(&p.runnable_meta[result.position], result)),
        );
        assert_eq!(events, assemble_outcome(&p, &results).events);
        assert_eq!(
            events.iter().map(|e| (e.source.file.as_str(), e.status)).collect::<Vec<_>>(),
            vec![
                ("A.fai", ContractStatus::Passed),
                ("B.fai", ContractStatus::Crashed),
                ("C.fai", ContractStatus::TimedOut),
                ("D.fai", ContractStatus::Failed),
            ]
        );
    }

    #[test]
    fn not_runnable_only_plans_stream_the_same_identified_event_as_the_report() {
        let mut p = plan(0);
        p.total = 1;
        p.not_runnable.push((
            meta(7),
            "function generator unavailable".to_owned(),
            fai_contracts::CONTRACT_NOT_RUNNABLE,
        ));
        let mut events = Vec::new();
        let results = run_test_workers(&p, &mut |event| events.push(event.clone()));
        assert!(results.is_empty());
        assert_eq!(events, assemble_outcome(&p, &results).events);
        assert_eq!(events[0].ordinal, 7);
        assert_eq!(events[0].source.file, "M.fai");
        assert_eq!(events[0].status, ContractStatus::NotRun);
    }

    #[test]
    fn resume_loop_clean_run_records_every_contract() {
        let received = [pass(0), pass(1), pass(2)];
        let results = resume_loop(
            3,
            |start, sink| {
                for result in &received[start..] {
                    sink(result.clone());
                }
                ExitKind::Clean
            },
            &mut nop,
        );
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.status == ResultStatus::Passed));
    }

    #[test]
    fn resume_loop_crash_marks_only_the_crasher_and_resumes() {
        // Worker completes 0 and 1, then aborts at 2; the resume completes 3.
        let mut spawns = 0;
        let results = resume_loop(
            4,
            |start, sink| {
                spawns += 1;
                match start {
                    0 => {
                        sink(pass(0));
                        sink(pass(1));
                        ExitKind::Crash
                    }
                    3 => {
                        sink(pass(3));
                        ExitKind::Clean
                    }
                    other => panic!("unexpected resume start {other}"),
                }
            },
            &mut nop,
        );
        assert_eq!(spawns, 2, "one initial spawn plus one resume");
        assert_eq!(results[0].status, ResultStatus::Passed);
        assert_eq!(results[1].status, ResultStatus::Passed);
        assert_eq!(results[2].status, ResultStatus::Crashed);
        assert_eq!(results[3].status, ResultStatus::Passed);
    }

    #[test]
    fn resume_loop_crash_on_first_contract_resumes_after_it() {
        let results = resume_loop(
            2,
            |start, sink| match start {
                0 => ExitKind::Crash,
                1 => {
                    sink(pass(1));
                    ExitKind::Clean
                }
                other => panic!("unexpected resume start {other}"),
            },
            &mut nop,
        );
        assert_eq!(results[0].status, ResultStatus::Crashed);
        assert_eq!(results[1].status, ResultStatus::Passed);
    }

    #[test]
    fn resume_loop_timeout_marks_timed_out() {
        let results = resume_loop(
            1,
            |start, _sink| {
                assert_eq!(start, 0);
                ExitKind::Timeout
            },
            &mut nop,
        );
        assert_eq!(results[0].status, ResultStatus::TimedOut);
    }

    #[test]
    fn resume_loop_every_contract_crashes_terminates() {
        // A worker that always aborts immediately still terminates, one per spawn.
        let mut spawns = 0;
        let results = resume_loop(
            3,
            |_start, _sink| {
                spawns += 1;
                ExitKind::Crash
            },
            &mut nop,
        );
        assert_eq!(spawns, 3);
        assert!(results.iter().all(|r| r.status == ResultStatus::Crashed));
    }

    #[test]
    fn results_are_delivered_before_the_worker_returns() {
        let seen = std::cell::Cell::new(0);
        let results = resume_loop(
            2,
            |start, sink| {
                assert_eq!(start, 0);
                sink(pass(0));
                assert_eq!(seen.get(), 1, "first event must be live");
                sink(pass(1));
                ExitKind::Clean
            },
            &mut |_| seen.set(seen.get() + 1),
        );
        assert_eq!(seen.get(), 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn duplicate_and_out_of_order_frames_do_not_change_acknowledged_identity() {
        let mut seen = Vec::new();
        let results = resume_loop(
            4,
            |start, sink| match start {
                0 => {
                    sink(pass(0));
                    sink(pass(0));
                    sink(pass(2));
                    sink(pass(1));
                    ExitKind::Timeout
                }
                3 => {
                    sink(pass(3));
                    ExitKind::Clean
                }
                _ => panic!("unexpected resume position"),
            },
            &mut |result| seen.push((result.position, result.status)),
        );
        assert_eq!(
            seen,
            vec![
                (0, ResultStatus::Passed),
                (1, ResultStatus::Passed),
                (2, ResultStatus::TimedOut),
                (3, ResultStatus::Passed)
            ]
        );
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn assemble_reports_failure_with_named_counterexample() {
        let p = plan(1);
        let results = vec![ContractResult {
            position: 0,
            status: ResultStatus::Failed,
            counterexample: Some("0".to_owned()),
            live_delta: 0,
        }];
        let outcome = assemble_outcome(&p, &results);
        assert!(!outcome.ok);
        assert_eq!(outcome.passed, 0);
        let d = outcome.diagnostics.iter().find(|d| d.code.as_str() == "FAI6001").expect("FAI6001");
        assert_eq!(d.help.as_deref(), Some("counterexample: n = 0"));
        assert_eq!(outcome.events[0].status, ContractStatus::Failed);
        assert_eq!(outcome.events[0].counterexample.as_deref(), Some("n = 0"));
    }

    #[test]
    fn example_failures_keep_only_contract_failures() {
        // `example_failures` filters the assembled diagnostics to FAI6001 (a
        // definite contract failure), dropping crashes/timeouts and leaks — what
        // `fai check` reports, leaving the rest to `fai test`.
        let p = plan(3);
        let results = vec![
            ContractResult {
                position: 0,
                status: ResultStatus::Failed,
                counterexample: Some("0".to_owned()),
                live_delta: 0,
            },
            ContractResult {
                position: 1,
                status: ResultStatus::Crashed,
                counterexample: None,
                live_delta: 0,
            },
            ContractResult {
                position: 2,
                status: ResultStatus::Passed,
                counterexample: None,
                live_delta: 1,
            },
        ];
        let diags = example_failures(&p, &results);
        assert_eq!(diags.len(), 1, "only the failure becomes a diagnostic: {diags:?}");
        assert_eq!(diags[0].code.as_str(), "FAI6001");
    }

    #[test]
    fn example_failures_empty_when_all_pass() {
        let p = plan(2);
        let results = vec![
            ContractResult {
                position: 0,
                status: ResultStatus::Passed,
                counterexample: None,
                live_delta: 0,
            },
            ContractResult {
                position: 1,
                status: ResultStatus::Passed,
                counterexample: None,
                live_delta: 0,
            },
        ];
        assert!(example_failures(&p, &results).is_empty());
    }

    #[test]
    fn assemble_reports_crash_as_fai6003() {
        let p = plan(1);
        let results = vec![ContractResult {
            position: 0,
            status: ResultStatus::Crashed,
            counterexample: None,
            live_delta: 0,
        }];
        let outcome = assemble_outcome(&p, &results);
        assert!(!outcome.ok);
        assert!(outcome.diagnostics.iter().any(|d| d.code.as_str() == "FAI6003"));
        assert_eq!(outcome.events[0].status, ContractStatus::Crashed);
    }

    #[test]
    fn assemble_reports_leak_as_internal_error() {
        let p = plan(1);
        let results = vec![ContractResult {
            position: 0,
            status: ResultStatus::Passed,
            counterexample: None,
            live_delta: 1,
        }];
        let outcome = assemble_outcome(&p, &results);
        assert_eq!(outcome.leaked, 1);
        assert!(!outcome.ok);
        assert!(outcome.diagnostics.iter().any(|d| d.message.contains("leaked")));
    }

    #[test]
    fn assemble_counts_passed_and_seed() {
        let p = plan(2);
        let results = vec![
            ContractResult {
                position: 0,
                status: ResultStatus::Passed,
                counterexample: None,
                live_delta: 0,
            },
            ContractResult {
                position: 1,
                status: ResultStatus::Passed,
                counterexample: None,
                live_delta: 0,
            },
        ];
        let outcome = assemble_outcome(&p, &results);
        assert!(outcome.ok);
        assert_eq!(outcome.passed, 2);
        assert_eq!(outcome.seed, 0);
        assert_eq!(outcome.events.len(), 2);
    }

    pub(super) fn result(position: usize, status: ResultStatus) -> ContractResult {
        ContractResult { position, status, counterexample: None, live_delta: 0 }
    }

    #[test]
    fn summary_partitions_every_selected_contract() {
        let mut p = plan(5);
        p.runnable_meta[0].kind = ContractKind::Example;
        p.total = 6;
        p.filtered_out = 2;
        p.not_runnable.push((
            meta(5),
            "no generator".to_owned(),
            fai_contracts::CONTRACT_NOT_RUNNABLE,
        ));
        let results = [
            result(0, ResultStatus::Passed),
            result(1, ResultStatus::Failed),
            result(2, ResultStatus::Crashed),
            result(3, ResultStatus::TimedOut),
        ];
        let mut outcome = assemble_outcome(&p, &results);
        outcome.summary.elapsed_ms = 125;
        assert_eq!(
            serde_json::to_value(&outcome.summary).unwrap(),
            serde_json::json!({
                "total": 6, "examples": 1, "properties": 5,
                "named": 0, "errored": 0, "skipped": 0, "listed": 0,
                "passed": 1, "failed": 1, "crashed": 1, "timedOut": 1,
                "notRun": 1, "blocked": 1, "filteredOut": 2, "elapsedMs": 125
            })
        );
    }

    #[test]
    fn compilation_blocked_contracts_are_not_assertion_failures() {
        let mut p = plan(2);
        p.blocked = true;
        p.pre_diagnostics.push(Diagnostic::error(
            WORKSPACE_ERROR,
            "cannot compile".to_owned(),
            p.runnable_meta[0].span,
        ));
        let outcome = assemble_outcome(&p, &[]);
        assert!(!outcome.ok);
        assert_eq!(outcome.summary.blocked, 2);
        assert_eq!(outcome.summary.failed, 0);
        let report = outcome.render_human(&p.render_spans, false);
        assert!(report.contains("2 contracts blocked before execution"), "{report}");
        assert!(report.contains("0 passed; 0 failed"), "{report}");
    }

    #[test]
    fn an_incomplete_run_cannot_report_success() {
        let p = plan(1);
        assert!(!assemble_outcome(&p, &[]).ok);
    }

    #[test]
    fn report_header_distinguishes_examples_and_properties() {
        let mut p = plan(2);
        p.runnable_meta[0].kind = ContractKind::Example;
        p.config = TestConfig { seed: -17, trials: 50, max_size: 20 };
        assert_eq!(
            p.render_header(),
            "running 2 contracts (1 example, 1 property)\nseed: -17; trials per property: 50; max size: 20\n\n"
        );
    }

    #[test]
    fn empty_selection_is_explicit_and_successful() {
        let p = plan(0);
        assert!(p.render_header().contains("no contracts found in the selected files"));
        let outcome = assemble_outcome(&p, &[]);
        assert!(outcome.ok);
        assert_eq!(outcome.summary.total, 0);
    }

    #[test]
    fn unmatched_filter_is_distinct_from_no_contracts() {
        let mut p = plan(0);
        p.filtered_out = 3;
        assert!(p.render_header().contains("no contracts matched the filter"));
        let outcome = assemble_outcome(&p, &[]);
        assert_eq!(outcome.summary.filtered_out, 3);
        assert_eq!(outcome.summary.total, 0);
        assert!(outcome.ok);
    }

    #[test]
    fn passing_report_has_a_fixed_time_and_no_failure_sections() {
        let p = plan(1);
        let mut outcome = assemble_outcome(&p, &[result(0, ResultStatus::Passed)]);
        outcome.summary.elapsed_ms = 1234;
        assert_eq!(
            outcome.render_human(&p.render_spans, false),
            "\ntest result: ok. 1 passed; 0 failed; 0 crashed; 0 timed out; 0 not run; 0 filtered out; finished in 1.23s\n"
        );
    }

    #[test]
    fn property_failure_has_one_counterexample_and_reproduction_settings() {
        let mut p = plan(1);
        p.config = TestConfig { seed: -9, trials: 23, max_size: 42 };
        let outcome = assemble_outcome(
            &p,
            &[ContractResult {
                counterexample: Some("0".to_owned()),
                ..result(0, ResultStatus::Failed)
            }],
        );
        let report = outcome.render_human(&p.render_spans, false);
        assert!(report.contains("failures:\n"), "{report}");
        assert_eq!(report.matches("counterexample: n = 0").count(), 1, "{report}");
        assert!(report.contains("--seed -9 --count 23 --max-size 42"), "{report}");
        assert!(report.contains("test result: FAILED."), "{report}");
    }

    #[test]
    fn event_labels_disambiguate_the_same_subject() {
        let first = resolve_event(&meta(0), &result(0, ResultStatus::Passed));
        let mut second = first.clone();
        second.ordinal = 1;
        second.location = Some(ContractLocation { line: 4, column: 1 });
        assert_eq!(render_test_event_line(&first, false), "test M.f0 [forall, M.fai:3:1] ... ok\n");
        assert_ne!(render_test_event_line(&first, false), render_test_event_line(&second, false));
    }

    #[test]
    fn subjectless_event_retains_unicode_source_location() {
        let mut event = resolve_event(&meta(0), &result(0, ResultStatus::Crashed));
        event.symbol = None;
        event.source.file = "λ.fai".to_owned();
        assert_eq!(render_test_event_line(&event, false), "test forall [λ.fai:3:1] ... CRASHED\n");
    }

    #[test]
    fn event_without_coordinates_uses_one_based_ordinal() {
        let mut event = resolve_event(&meta(7), &result(0, ResultStatus::Passed));
        event.location = None;
        event.source.origin = ContractSourceOrigin::StandardLibrary;
        assert_eq!(
            render_test_event_line(&event, false),
            "test M.f7 [forall, std:M.fai: #8] ... ok\n"
        );
    }

    #[test]
    fn events_without_the_additive_location_field_still_decode() {
        let event = resolve_event(&meta(0), &result(0, ResultStatus::Passed));
        let mut json = serde_json::to_value(&event).unwrap();
        json.as_object_mut().unwrap().remove("location");
        let decoded: ContractEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.location, None);
        assert_eq!(decoded.source, event.source);
        assert_eq!(decoded.ordinal, event.ordinal);
    }

    #[test]
    fn live_report_escapes_control_characters_in_paths() {
        let mut event = resolve_event(&meta(0), &result(0, ResultStatus::Passed));
        event.source.file = "bad\n\x1b.fai".to_owned();
        let line = render_test_event_line(&event, false);
        assert!(line.contains("bad\\n\\u{1b}.fai"), "{line:?}");
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\x1b'));
    }

    #[test]
    fn timed_out_event_is_distinct_from_a_crash() {
        let event = resolve_event(&meta(0), &result(0, ResultStatus::TimedOut));
        assert_eq!(
            render_test_event_line(&event, false),
            "test M.f0 [forall, M.fai:3:1] ... TIMED OUT\n"
        );
    }

    #[test]
    fn not_runnable_event_is_not_an_ignored_success() {
        assert!(render_test_event_line(&not_run_event(&meta(0)), false).ends_with("... NOT RUN\n"));
    }

    #[test]
    fn color_is_opt_in_for_live_status_and_final_result() {
        let p = plan(1);
        let outcome = assemble_outcome(&p, &[result(0, ResultStatus::Passed)]);
        assert!(render_test_event_line(&outcome.events[0], true).contains("\x1b[32mok\x1b[0m"));
        assert!(outcome.render_human(&p.render_spans, true).contains("\x1b[32mok\x1b[0m"));
        assert!(!outcome.render_human(&p.render_spans, false).contains('\x1b'));
    }

    #[test]
    fn leak_failure_does_not_invent_an_assertion_failure() {
        let p = plan(1);
        let outcome = assemble_outcome(
            &p,
            &[ContractResult { live_delta: 1, ..result(0, ResultStatus::Passed) }],
        );
        let report = outcome.render_human(&p.render_spans, false);
        assert!(report.contains("test result: FAILED. 1 passed; 0 failed"), "{report}");
        assert!(report.contains("leaked"), "{report}");
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            #[test]
            fn summary_outcomes_partition_selection(
                statuses in prop::collection::vec(0u8..4, 0..50),
                blocked in 0usize..10,
                not_run in 0usize..10,
                filtered in 0usize..10,
            ) {
                let mut p = plan(statuses.len() + blocked);
                p.total += not_run;
                p.filtered_out = filtered;
                for ordinal in 0..not_run {
                    p.not_runnable.push((meta(ordinal + p.runnable_meta.len()), "no generator".to_owned(), fai_contracts::CONTRACT_NOT_RUNNABLE));
                }
                let results = statuses.iter().enumerate().map(|(position, status)| {
                    result(position, match status {
                        0 => ResultStatus::Passed,
                        1 => ResultStatus::Failed,
                        2 => ResultStatus::Crashed,
                        _ => ResultStatus::TimedOut,
                    })
                }).collect::<Vec<_>>();
                let summary = assemble_outcome(&p, &results).summary;
                prop_assert_eq!(summary.passed + summary.failed + summary.crashed + summary.timed_out + summary.not_run + summary.blocked, summary.total);
                prop_assert_eq!(summary.examples + summary.properties, summary.total);
                prop_assert_eq!(summary.blocked, blocked);
                prop_assert_eq!(summary.not_run, not_run);
                prop_assert_eq!(summary.filtered_out, filtered);
            }
        }
    }
}
