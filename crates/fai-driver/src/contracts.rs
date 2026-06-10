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

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use fai_codegen::JitProgram;
use fai_contracts::{
    CONTRACT_ABORTED, CONTRACT_FAILED, CONTRACT_NOT_RUNNABLE, ContractInfo, ContractKind,
    run_contract, synthesize,
};
use fai_core::ir::{ExprKind, FnAbi, LoweredDef};
use fai_core::wire::def_to_wire;
use fai_core::{RebuiltTest, TestWireBundle, WireContract, WireDefId, from_wire_test};
use fai_db::{Db, SourceFile};
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{Diagnostic, DiagnosticCode, SCHEMA_VERSION, Severity, render_human};
use fai_rc::{BorrowSig, rc, rc_lowered};
use fai_resolve::{DefId, ModuleName, module_name};
use fai_span::{Span, SpanResolver};
use rustc_hash::FxHashSet;
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
#[derive(Debug, Clone, Copy)]
pub struct TestConfig {
    /// The initial PRNG seed.
    pub seed: i64,
    /// The number of random trials per `forall`.
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
}

/// A per-contract result in the JSON output and the `$/testEvent` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractEvent {
    /// The contract's position among its file's contracts.
    pub ordinal: usize,
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
    /// The number of trials the contract ran with.
    pub trials: i64,
    /// The maximum generation size the contract ran with.
    pub max_size: i64,
}

/// The render-side metadata for one contract: everything needed to build its
/// diagnostics and event that the database-free worker does not carry.
#[derive(Debug, Clone)]
pub struct ContractMeta {
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
    /// The PRNG seed the contract runs with.
    pub seed: i64,
    /// The number of trials the contract runs with.
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
    /// The PRNG seed for the run.
    pub seed: i64,
    /// Whether an error in `pre_diagnostics` blocks execution.
    pub blocked: bool,
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
    /// Whether the run succeeded (no errors, nothing failed, no leak).
    pub ok: bool,
}

/// The JSON envelope for `fai test`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestOutput {
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
            schema_version: SCHEMA_VERSION,
            total: self.total,
            passed: self.passed,
            not_run: self.not_run,
            seed: self.seed,
            events: self.events.clone(),
            diagnostics: to_wire(&self.diagnostics, resolver),
            ok: self.ok,
        }
    }

    /// Renders the outcome for humans (diagnostics, then a summary line).
    #[must_use]
    pub fn render_human(&self, resolver: &dyn SpanResolver, color: bool) -> String {
        use std::fmt::Write as _;
        let mut out = render_human(&self.diagnostics, resolver, color);
        let failed = self.total - self.passed - self.not_run;
        let _ = writeln!(
            out,
            "{} passed, {failed} failed, {} could not run (of {})",
            self.passed, self.not_run, self.total
        );
        out
    }
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
    build_plan(db, files, match_pat, config, |_| true)
}

/// Builds a prepared [`TestPlan`] containing only the closed `example` contracts
/// in `files` — the contracts `fai check` evaluates eagerly. `forall`s (which
/// need generated inputs) are excluded; they remain `fai test`'s responsibility.
#[must_use]
pub fn build_example_plan(db: &dyn Db, files: &[SourceFile], config: TestConfig) -> TestPlan {
    build_plan(db, files, None, config, |kind| kind == ContractKind::Example)
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
    // Collect contracts, keeping each with its file.
    let mut items: Vec<(SourceFile, ContractInfo)> = Vec::new();
    for &file in files {
        for info in fai_contracts::contracts(db, file) {
            if accept(info.kind) && matches_filter(db, file, &info, match_pat) {
                items.push((file, info));
            }
        }
    }
    let total = items.len();

    // Type errors of the selected files must be clean before running (a broken
    // body would JIT to nonsense). Reported, and they fail the run.
    let mut pre_diagnostics = Vec::new();
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
            Ok(s) if has_error_node(&s.prop) || has_error_node(&s.entry) => not_runnable.push((
                meta,
                "uses a construct the native backend does not support yet".to_owned(),
                CONTRACT_NOT_RUNNABLE,
            )),
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
        defs.push(def_to_wire(lowered, &module_of, *arity, FnAbi::default()));
    }
    for def in &reachable {
        if let Some(file) = db.source_file(def.file) {
            let lowered = rc(db, file, def.name);
            defs.push(def_to_wire(&lowered, &module_of, arity_of(db, *def), abi_of(db, *def)));
        }
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

    TestPlan {
        bundle: TestWireBundle { defs, contracts },
        runnable_meta,
        not_runnable,
        pre_diagnostics,
        total,
        seed: config.seed,
        blocked,
    }
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

    events.sort_by_key(|e| e.ordinal);
    diagnostics.sort_by(|a, b| {
        (a.primary.start().raw(), a.code.as_str()).cmp(&(b.primary.start().raw(), b.code.as_str()))
    });
    let ok = leaked == 0 && !diagnostics.iter().any(|d| d.severity == Severity::Error);
    TestOutcome {
        total: plan.total,
        passed,
        not_run: plan.not_runnable.len(),
        diagnostics,
        events,
        leaked,
        seed: plan.seed,
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
    let mut spawn = |start: usize| spawn_and_read(&bundle_path, start, timeout);
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
    mut spawn: impl FnMut(usize) -> (Vec<WorkerResult>, ExitKind),
    on_result: &mut dyn FnMut(&ContractResult),
) -> Vec<ContractResult> {
    let mut results: Vec<Option<ContractResult>> = (0..n).map(|_| None).collect();
    let mut start = 0;
    while start < n {
        let (received, exit) = spawn(start);
        for wr in received {
            let position = wr.position;
            if position < n && results[position].is_none() {
                let r = ContractResult::from_worker(wr);
                on_result(&r);
                results[position] = Some(r);
            }
        }
        match (start..n).find(|&i| results[i].is_none()) {
            None => break,
            Some(pos) => {
                let status = if matches!(exit, ExitKind::Timeout) {
                    ResultStatus::TimedOut
                } else {
                    ResultStatus::Crashed
                };
                let r =
                    ContractResult { position: pos, status, counterexample: None, live_delta: 0 };
                on_result(&r);
                results[pos] = Some(r);
                start = pos + 1;
            }
        }
    }
    results.into_iter().flatten().collect()
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
/// and enforcing `timeout` (killing the worker on expiry). Returns the results it
/// streamed and how it ended.
fn spawn_and_read(
    bundle_path: &Path,
    start: usize,
    timeout: Duration,
) -> (Vec<WorkerResult>, ExitKind) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return (Vec::new(), ExitKind::Crash),
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
        Err(_) => return (Vec::new(), ExitKind::Crash),
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<WorkerResult>();
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
        let mut sink = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut sink);
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
            Err(_) => ExitKind::Crash,
        };
        let _ = code_tx.send(kind);
    });

    let mut received = Vec::new();
    for result in rx {
        received.push(result);
    }
    let _ = reader.join();
    let _ = draining.join();
    let kind = code_rx.recv().unwrap_or(ExitKind::Crash);
    (received, kind)
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
    let mut program = JitProgram::compile(&rebuilt.defs, &namer, &arity, &abi);
    for position in start_index..rebuilt.contracts.len() {
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
    let symbol = info.subject.map(|subject| match module_name(db, file) {
        Some(ModuleName(m)) => format!("{}.{}", m.as_str(), subject.as_str()),
        None => subject.as_str().to_owned(),
    });
    ContractMeta {
        ordinal: info.ordinal,
        kind: info.kind,
        binders: info.binders.iter().map(|s| s.as_str().to_owned()).collect(),
        symbol,
        span: Span::new(file.source(db), info.span),
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
        ordinal: meta.ordinal,
        symbol: meta.symbol.clone(),
        kind: meta.kind.keyword().to_owned(),
        status,
        counterexample: result.counterexample.as_ref().map(|raw| binding_str(&meta.binders, raw)),
        seed: meta.seed,
        trials: meta.trials,
        max_size: meta.max_size,
    }
}

/// Renders one streamed contract event as a single human-readable progress line
/// (with a trailing newline). Shared by the in-process and daemon CLI paths so a
/// warm `fai test` prints the same live lines as `--no-daemon`.
#[must_use]
pub fn render_test_event_line(event: &ContractEvent) -> String {
    let label =
        event.symbol.clone().unwrap_or_else(|| format!("{} #{}", event.kind, event.ordinal));
    match event.status {
        ContractStatus::Passed => format!("ok    {label}\n"),
        ContractStatus::Failed => match &event.counterexample {
            Some(ce) => format!("FAIL  {label}: {ce}\n"),
            None => format!("FAIL  {label}\n"),
        },
        ContractStatus::Crashed => format!("ABORT {label}: runtime error\n"),
        ContractStatus::TimedOut => format!("ABORT {label}: timed out\n"),
        ContractStatus::NotRun => format!("skip  {label}\n"),
    }
}

/// The event for a contract that could not be run (an ungeneratable binder).
fn not_run_event(meta: &ContractMeta) -> ContractEvent {
    ContractEvent {
        ordinal: meta.ordinal,
        symbol: meta.symbol.clone(),
        kind: meta.kind.keyword().to_owned(),
        status: ContractStatus::NotRun,
        counterexample: None,
        seed: meta.seed,
        trials: meta.trials,
        max_size: meta.max_size,
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

/// Whether a lowered definition contains a lowering-error placeholder (so it
/// reached a construct the native backend does not support).
fn has_error_node(def: &LoweredDef) -> bool {
    fn scan(e: &fai_core::ir::CExpr) -> bool {
        match &e.kind {
            ExprKind::Error => true,
            ExprKind::Prim { args, .. } | ExprKind::MakeData { args, .. } => args.iter().any(scan),
            ExprKind::App { func, args } => scan(func) || args.iter().any(scan),
            ExprKind::If { cond, then, els } => scan(cond) || scan(then) || scan(els),
            ExprKind::Let { value, body, .. } | ExprKind::Reset { value, body, .. } => {
                scan(value) || scan(body)
            }
            ExprKind::DataTag(b) | ExprKind::DataField { base: b, .. } => scan(b),
            ExprKind::Dup { body, .. } | ExprKind::Drop { body, .. } => scan(body),
            ExprKind::Join { body, .. } | ExprKind::HoleStart { body, .. } => scan(body),
            ExprKind::Recur { args } => args.iter().any(scan),
            ExprKind::HoleFill { cell, .. } => scan(cell),
            ExprKind::HoleClose { base, .. } => scan(base),
            ExprKind::Lit(_)
            | ExprKind::Local(_)
            | ExprKind::Global(_)
            | ExprKind::MakeClosure { .. } => false,
        }
    }
    def.fns.iter().any(|f| scan(&f.body))
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

    fn meta(ordinal: usize) -> ContractMeta {
        ContractMeta {
            ordinal,
            kind: ContractKind::Forall,
            binders: vec!["n".to_owned()],
            symbol: Some(format!("M.f{ordinal}")),
            span: Span::new(SourceId::new(0), TextRange::empty(ByteOffset::ZERO)),
            seed: 0,
            trials: 100,
            max_size: 100,
        }
    }

    fn plan(n: usize) -> TestPlan {
        TestPlan {
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
            seed: 0,
            blocked: false,
        }
    }

    fn pass(position: usize) -> WorkerResult {
        WorkerResult { position, passed: true, counterexample: None, live_delta: 0 }
    }

    fn nop(_: &ContractResult) {}

    #[test]
    fn resume_loop_clean_run_records_every_contract() {
        let received = [pass(0), pass(1), pass(2)];
        let results =
            resume_loop(3, |start| (received[start..].to_vec(), ExitKind::Clean), &mut nop);
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.status == ResultStatus::Passed));
    }

    #[test]
    fn resume_loop_crash_marks_only_the_crasher_and_resumes() {
        // Worker completes 0 and 1, then aborts at 2; the resume completes 3.
        let mut spawns = 0;
        let results = resume_loop(
            4,
            |start| {
                spawns += 1;
                match start {
                    0 => (vec![pass(0), pass(1)], ExitKind::Crash),
                    3 => (vec![pass(3)], ExitKind::Clean),
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
            |start| match start {
                0 => (Vec::new(), ExitKind::Crash),
                1 => (vec![pass(1)], ExitKind::Clean),
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
            |start| {
                assert_eq!(start, 0);
                (Vec::new(), ExitKind::Timeout)
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
            |_start| {
                spawns += 1;
                (Vec::new(), ExitKind::Crash)
            },
            &mut nop,
        );
        assert_eq!(spawns, 3);
        assert!(results.iter().all(|r| r.status == ResultStatus::Crashed));
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
}
