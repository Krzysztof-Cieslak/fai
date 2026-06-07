//! Running `example`/`forall` contracts for `fai test`.
//!
//! For each selected file we collect its contracts ([`fai_contracts`]), synthesize
//! a runnable harness per contract, JIT-compile them together with their reachable
//! callees, then apply each harness and decode the result. Failures become
//! `FAI6001` diagnostics (with a shrunk counterexample); contracts whose binders
//! cannot be generated become `FAI6002`. The runtime's live-object count is
//! checked afterward, so a reference-counting bug surfaces as a failed run.

use fai_codegen::JitProgram;
use fai_contracts::{
    CONTRACT_FAILED, CONTRACT_NOT_RUNNABLE, ContractInfo, ContractKind, run_contract, synthesize,
};
use fai_core::ir::{ExprKind, LoweredDef};
use fai_db::{Db, SourceFile};
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{Diagnostic, SCHEMA_VERSION, Severity, render_human};
use fai_rc::{BorrowSig, rc, rc_lowered};
use fai_resolve::DefId;
use fai_span::{Span, SpanResolver};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

use crate::backend::{arity_of, precompile_diagnostics, reachable_from_roots, symbol_base};
use crate::semantic_diagnostics;

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
    /// Net change in the runtime's live-object count (should be 0).
    pub leaked: i64,
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
/// symbol / module), returning their outcome.
#[must_use]
pub fn run_tests(
    db: &dyn Db,
    files: &[SourceFile],
    match_pat: Option<&str>,
    config: TestConfig,
) -> TestOutcome {
    // Collect contracts, keeping each with its file.
    let mut items: Vec<(SourceFile, ContractInfo)> = Vec::new();
    for &file in files {
        for info in fai_contracts::contracts(db, file) {
            if matches_filter(db, file, &info, match_pat) {
                items.push((file, info));
            }
        }
    }
    let total = items.len();

    // Type errors of the selected files must be clean before running (a broken
    // body would JIT to nonsense). Reported, and they fail the run.
    let mut diagnostics = Vec::new();
    let mut seen_files = FxHashSet::default();
    for (file, _) in &items {
        if seen_files.insert(file.source(db)) {
            diagnostics.extend(semantic_diagnostics(db, *file));
        }
    }

    // Synthesize each contract into a harness, or record why it cannot run.
    let mut synths: Vec<(usize, fai_contracts::SynthContract)> = Vec::new();
    let mut not_runnable: Vec<(usize, String)> = Vec::new();
    for (i, (file, info)) in items.iter().enumerate() {
        match synthesize(db, *file, info) {
            Ok(s) if has_error_node(&s.prop) || has_error_node(&s.entry) => not_runnable
                .push((i, "uses a construct the native backend does not support yet".to_owned())),
            Ok(s) => synths.push((i, s)),
            Err(nr) => not_runnable.push((i, nr.reason)),
        }
    }

    // Reference-count and gather the synthesized defs and their real callees.
    let mut synth_defs: FxHashMap<DefId, (LoweredDef, usize)> = FxHashMap::default();
    let mut roots: Vec<DefId> = Vec::new();
    for (_, s) in &synths {
        let all_owned = |n: usize| BorrowSig(vec![false; n]);
        let entry = rc_lowered(db, &s.entry, &all_owned(s.entry_arity));
        let prop = rc_lowered(db, &s.prop, &all_owned(s.prop_arity));
        roots.extend(entry.referenced_globals());
        roots.extend(prop.referenced_globals());
        synth_defs.insert(s.entry.def, (entry, s.entry_arity));
        synth_defs.insert(s.prop.def, (prop, s.prop_arity));
    }
    let synth_ids: FxHashSet<DefId> = synth_defs.keys().copied().collect();
    let reachable = reachable_from_roots(db, &roots, &synth_ids);

    // Callees must compile cleanly too.
    let mut pre = precompile_diagnostics(db, &reachable);
    diagnostics.append(&mut pre);
    let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);

    let mut passed = 0;
    if !has_errors && !synths.is_empty() {
        let mut all_defs: Vec<LoweredDef> = Vec::with_capacity(reachable.len() + synth_defs.len());
        for def in &reachable {
            if let Some(file) = db.source_file(def.file) {
                all_defs.push((*rc(db, file, def.name)).clone());
            }
        }
        for (lowered, _) in synth_defs.values() {
            all_defs.push(lowered.clone());
        }

        let namer = |d: DefId| symbol_base(db, d);
        let arity = |d: DefId| synth_defs.get(&d).map_or_else(|| arity_of(db, d), |(_, a)| *a);
        let mut program = JitProgram::compile(&all_defs, &namer, &arity);

        let base_live = fai_runtime::live_count();
        for (i, s) in &synths {
            let closure = program.closure_value(&namer, s.entry.def);
            let outcome = run_contract(closure, config.seed, config.trials, config.max_size);
            let (file, info) = &items[*i];
            if outcome.passed {
                passed += 1;
            } else {
                diagnostics.push(failed_diagnostic(db, *file, info, outcome.counterexample));
            }
        }
        let leaked = fai_runtime::live_count() - base_live;
        drop(program);
        finish(db, total, passed, &not_runnable, &items, diagnostics, leaked)
    } else {
        // Errors (type or callee) block running; contracts are not executed.
        finish(db, total, passed, &not_runnable, &items, diagnostics, 0)
    }
}

/// Assembles the final outcome: appends not-runnable diagnostics, sorts, and
/// computes `ok`.
fn finish(
    db: &dyn Db,
    total: usize,
    passed: usize,
    not_runnable: &[(usize, String)],
    items: &[(SourceFile, ContractInfo)],
    mut diagnostics: Vec<Diagnostic>,
    leaked: i64,
) -> TestOutcome {
    for (i, reason) in not_runnable {
        let (file, info) = &items[*i];
        diagnostics.push(not_runnable_diagnostic(db, *file, info, reason));
    }
    if leaked != 0 {
        diagnostics.push(Diagnostic::error(
            crate::WORKSPACE_ERROR,
            format!("internal error: {leaked} live object(s) leaked while running contracts"),
            crate::tooling_span(),
        ));
    }
    diagnostics.sort_by(|a, b| {
        (a.primary.start().raw(), a.code.as_str()).cmp(&(b.primary.start().raw(), b.code.as_str()))
    });
    let ok = leaked == 0 && !diagnostics.iter().any(|d| d.severity == Severity::Error);
    TestOutcome { total, passed, not_run: not_runnable.len(), diagnostics, leaked, ok }
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
            ExprKind::Lit(_)
            | ExprKind::Local(_)
            | ExprKind::Global(_)
            | ExprKind::MakeClosure { .. } => false,
        }
    }
    def.fns.iter().any(|f| scan(&f.body))
}

/// Builds the `FAI6001` diagnostic for a failed contract.
fn failed_diagnostic(
    db: &dyn Db,
    file: SourceFile,
    info: &ContractInfo,
    counterexample: Option<String>,
) -> Diagnostic {
    let span = Span::new(file.source(db), info.span);
    match info.kind {
        ContractKind::Example => {
            Diagnostic::error(CONTRACT_FAILED, "this example does not hold".to_owned(), span)
        }
        ContractKind::Forall => {
            let mut diag =
                Diagnostic::error(CONTRACT_FAILED, "this property does not hold".to_owned(), span);
            if let Some(ce) = counterexample {
                diag = diag.with_help(format!("counterexample: {}", binding_str(info, &ce)));
            }
            diag
        }
    }
}

/// Renders a counterexample binding: `n = 3` for one binder, `(a, b) = (1, 2)` for
/// several (the value `ce` is the generated tuple's rendering).
fn binding_str(info: &ContractInfo, ce: &str) -> String {
    let names: Vec<&str> = info.binders.iter().map(|s| s.as_str()).collect();
    match names.as_slice() {
        [one] => format!("{one} = {ce}"),
        many => format!("({}) = {ce}", many.join(", ")),
    }
}

/// Builds the `FAI6002` diagnostic for a contract that cannot be run.
fn not_runnable_diagnostic(
    db: &dyn Db,
    file: SourceFile,
    info: &ContractInfo,
    reason: &str,
) -> Diagnostic {
    Diagnostic::error(
        CONTRACT_NOT_RUNNABLE,
        format!("this {} cannot be run: {reason}", info.kind.keyword()),
        Span::new(file.source(db), info.span),
    )
}
