//! Discovery and isolated execution of ordinary Fai `Test.Suite` values.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fai_codegen::JitProgram;
use fai_core::{WireBundle, WireDefId, from_wire};
use fai_db::{Db, SourceFile, SourceOrigin, SourceRole};
use fai_diagnostics::{Diagnostic, Severity};
use fai_resolve::{DefId, ModuleName};
use fai_runtime::{
    Value, apply, data_tag_of, fai_data_field, fai_drop, fai_dup, make_int, make_str, read_int,
    read_string,
};
use fai_span::{Span, SpanResolver};
use fai_syntax::Symbol;
use fai_types::{EffEnd, Ty};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use super::{ContractLocation, ContractSource, ContractSourceOrigin, TestConfig, TestPlan};

fn discovery_timeout() -> Duration {
    Duration::from_millis(
        std::env::var("FAI_TEST_DISCOVERY_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000),
    )
}

fn cleanup_timeout() -> Duration {
    Duration::from_millis(
        std::env::var("FAI_TEST_CLEANUP_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5_000),
    )
}

/// Named test runner selection controls.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestOptions {
    /// Discover and list selected cases without executing bodies or fixtures.
    pub list: bool,
    /// Reject every focus marker in the requested sources, before name filtering.
    pub deny_focused: bool,
}

#[derive(Debug, Clone)]
/// A typed root and its immutable worker input and source coordinates.
pub struct PreparedSuite {
    /// Source handle used only while attaching native project configuration.
    pub file: SourceFile,
    /// Working directory shared by direct and daemon execution.
    pub working_directory: Option<String>,
    /// Logical source label reported to users.
    pub source: ContractSource,
    /// The root binding's source span.
    pub span: Span,
    /// Reusable lowered program, not process-local closures.
    pub bundle: WireBundle,
    /// Pure manifest function.
    pub manifest: WireDefId,
    /// Selected-case executor.
    pub execute: WireDefId,
    /// Structured result decoder entry.
    pub issues: WireDefId,
    /// Stable source keys resolved against this compilation revision.
    pub locations: BTreeMap<String, Span>,
}

/// A discovered tree node. Identity uses the complete, unnormalized name path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// Focus inherited from this node or an ancestor.
    pub focused: bool,
    /// Length-prefixed path key used inside the worker.
    pub id: String,
    /// Case, group, or skip marker.
    pub kind: String,
    /// Exact local name.
    pub name: String,
    /// Exact ancestor and local names.
    pub path: Vec<String>,
    /// Explicit skip reason.
    pub reason: String,
    /// Whether an ancestor or this node is skipped.
    pub skipped: bool,
    /// Compiler-issued source-site key.
    pub source: String,
}

/// A failure returned by Fai; `source` is resolved against captured coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    /// Rendered actual value.
    pub actual: String,
    /// Additional contextual messages.
    pub context: Vec<String>,
    /// Rendered expected value.
    pub expected: String,
    /// Assertion or operation failure.
    pub kind: String,
    /// Primary explanation.
    pub message: String,
    /// Compiler-issued source-site key.
    pub source: String,
}

#[derive(Debug, Clone)]
/// One selected named case, referring to its prepared root.
pub struct NamedCase {
    /// Index of the immutable prepared suite.
    pub suite: usize,
    /// Its discovered identity and selection metadata.
    pub metadata: Metadata,
}

#[derive(Debug, Default)]
/// Complete discovery before any contract or named body starts.
pub struct Selection {
    /// Selected leaves, including explicit skips.
    pub cases: Vec<NamedCase>,
    /// Discovery errors that block execution.
    pub diagnostics: Vec<Diagnostic>,
    /// Cases excluded by filters or focus.
    pub filtered: usize,
    /// Whether an active focus narrowed this selection.
    pub focused: bool,
    /// Complete manifests used to verify reconstruction in each fresh worker.
    pub manifests: BTreeMap<usize, Vec<Metadata>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    source: ContractSource,
    bundle: WireBundle,
    manifest: WireDefId,
    execute: WireDefId,
    issues: WireDefId,
    selected: Option<String>,
    expected: Vec<Metadata>,
    config: TestConfig,
    directory: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    metadata: Vec<Metadata>,
    issues: Vec<Issue>,
}

#[derive(Debug)]
/// The worker's returned checks or a supervisor-detected failure.
pub enum Completion {
    /// Normal return, potentially containing failed checks.
    Passed(Vec<Issue>),
    /// A worker preparation or protocol error, rather than a process crash.
    Errored(String),
    /// Abnormal worker termination and captured diagnostics.
    Crashed(String),
    /// Deadline expiration, retaining any checks returned during cleanup grace.
    TimedOut(String, Vec<Issue>),
}

fn std_def(db: &dyn Db, name: &str) -> Option<DefId> {
    let file = fai_resolve::std_module_file(db, ModuleName(Symbol::intern("Test")))?;
    Some(DefId::new(file.source(db), Symbol::intern(name)))
}

fn suite_parts(ty: &Ty) -> Option<(&Ty, &fai_types::EffectRow, fai_resolve::AdtRef)> {
    let Ty::App(base, effect) = ty else { return None };
    let Ty::App(head, context) = base.as_ref() else { return None };
    let (Ty::Adt(adt), Ty::EffectArg(effect)) = (head.as_ref(), effect.as_ref()) else {
        return None;
    };
    Some((context, effect, *adt))
}

/// Find and type-check named roots without running them.
pub fn prepare(db: &dyn Db, files: &[SourceFile]) -> (Vec<PreparedSuite>, Vec<Diagnostic>) {
    let mut suites = Vec::new();
    let mut diagnostics = Vec::new();
    let candidates: Vec<_> = files
        .iter()
        .copied()
        .filter(|file| {
            if matches!(file.origin(db), SourceOrigin::StandardLibrary)
                || matches!(file.origin(db), SourceOrigin::Package(_))
                    && file.role(db) != &SourceRole::Test
            {
                return false;
            }
            let parsed = fai_syntax::parse(db, *file);
            parsed.module.roots.iter().any(|item| {
                matches!(&parsed.module.items[item.index()].kind,
            fai_syntax::ast::ItemKind::Binding { name, .. } if name.as_str() == "tests")
            })
        })
        .collect();
    if candidates.is_empty() {
        return (suites, diagnostics);
    }
    let Some(manifest) = std_def(db, "manifest") else { return (suites, diagnostics) };
    let execute = std_def(db, "execute").expect("embedded Test.execute");
    let issues = std_def(db, "issues").expect("embedded Test.issues");
    let test_file = db.source_file(execute.file).expect("embedded Test file");
    let execute_ty = fai_types::def_type(db, test_file, execute.name);
    let Ty::Arrow(expected_suite, _, _) = &execute_ty.ty else { return (suites, diagnostics) };
    let Some((expected_context, _, expected_adt)) = suite_parts(expected_suite) else {
        return (suites, diagnostics);
    };
    let prelude = fai_resolve::std_module_file(db, ModuleName(Symbol::intern("Prelude")))
        .expect("embedded Prelude");
    let runtime = DefId::new(prelude.source(db), Symbol::intern("defaultRuntime"));
    for file in candidates {
        if matches!(file.origin(db), SourceOrigin::StandardLibrary)
            || matches!(file.origin(db), SourceOrigin::Package(_))
                && file.role(db) != &SourceRole::Test
        {
            continue;
        }
        let parsed = fai_syntax::parse(db, file);
        let Some(def) = fai_resolve::module_defs(db, file).get(Symbol::intern("tests")).cloned()
        else {
            continue;
        };
        let item = &parsed.module.items[def.binding.index()];
        let span = Span::new(file.source(db), item.span);
        diagnostics.extend(crate::semantic_diagnostics(db, file));
        let declared = fai_types::declared_scheme(db, file, Symbol::intern("tests"));
        let valid = declared.as_ref().and_then(|s| suite_parts(&s.ty)).is_some_and(
            |(context, effect, adt)| {
                adt == expected_adt && context == expected_context && effect.tail == EffEnd::Closed
            },
        ) && matches!(&item.kind, fai_syntax::ast::ItemKind::Binding { params, .. } if params.is_empty())
            && fai_types::def_effect(db, file, Symbol::intern("tests")).is_pure();
        if !valid {
            diagnostics.push(Diagnostic::error(fai_contracts::INVALID_SUITE,
                "tests must be a pure value with an explicit Test.Suite Test.Context signature and closed effect row".to_owned(), span));
            continue;
        }
        let root = DefId::new(file.source(db), Symbol::intern("tests"));
        let roots = [root, manifest, execute, issues, runtime];
        let reachable = crate::backend::reachable_from_roots(db, &roots, &FxHashSet::default());
        let found = crate::backend::precompile_diagnostics(db, &reachable);
        let invalid = found.iter().any(|d| d.severity == Severity::Error);
        diagnostics.extend(found);
        if invalid {
            continue;
        }
        let module_of = |d| crate::backend::module_label(db, d);
        let wire_id =
            |d: DefId| WireDefId { module: module_of(d), name: d.name.as_str().to_owned() };
        let mut defs = Vec::new();
        let mut locations = BTreeMap::new();
        let mut seen_files = FxHashSet::default();
        for def in &reachable {
            let Some(source) = db.source_file(def.file) else { continue };
            defs.push(fai_core::wire::def_to_wire(
                &fai_rc::rc(db, source, def.name),
                &module_of,
                crate::backend::arity_of(db, *def),
                crate::backend::abi_of(db, *def),
                Vec::new(),
                (*fai_rc::entry_bounds(db, source, def.name)).clone(),
                (*fai_rc::result_facts(db, source, def.name)).clone(),
            ));
            if source.origin(db) != SourceOrigin::StandardLibrary
                && seen_files.insert(source.source(db))
            {
                let parsed = fai_syntax::parse(db, source);
                let resolved = fai_resolve::resolve(db, source);
                for (&expr, resolution) in &resolved.by_expr {
                    if let fai_resolve::Res::Def(def) = resolution
                        && fai_core::test_location_target(db, source, *def).is_some()
                    {
                        let key = fai_core::test_location_key(db, source, expr);
                        locations.insert(
                            key,
                            Span::new(source.source(db), parsed.module.expr(expr).span),
                        );
                    }
                }
            }
        }
        let fusion = crate::backend::program_fusion(db, &reachable);
        for lowered in &fusion.loops {
            defs.push(fai_core::wire::def_to_wire(
                lowered,
                &module_of,
                fusion.arity.get(&lowered.def).copied().unwrap_or(0),
                fusion.abi.get(&lowered.def).cloned().unwrap_or_default(),
                Vec::new(),
                crate::backend::bounds_entry_of(db, lowered.def),
                crate::backend::bounds_result_of(db, lowered.def),
            ));
        }
        suites.push(PreparedSuite {
            file,
            working_directory: None,
            source: ContractSource {
                file: file.path(db).replace(std::path::MAIN_SEPARATOR, "/"),
                origin: ContractSourceOrigin::User,
            },
            span,
            bundle: WireBundle {
                entry: wire_id(root),
                runtime: wire_id(runtime),
                defs,
                libraries: Vec::new(),
                concurrent: true,
            },
            manifest: wire_id(manifest),
            execute: wire_id(execute),
            issues: wire_id(issues),
            locations,
        });
    }
    suites.sort_by(|a, b| {
        (&a.source.file, &a.bundle.entry.module).cmp(&(&b.source.file, &b.bundle.entry.module))
    });
    (suites, diagnostics)
}

/// Discover complete trees, validate all metadata, then apply selection policy.
pub fn discover(plan: &TestPlan, filter: Option<&str>, options: TestOptions) -> Selection {
    let mut selection = Selection::default();
    for (index, suite) in plan.suites.iter().enumerate() {
        match invoke(suite, None, &[], plan.config, discovery_timeout()) {
            Ok((response, _, _, _)) => {
                selection.manifests.insert(index, response.metadata.clone());
                let mut names = BTreeSet::new();
                let invalid = response.metadata.iter().any(|m| {
                    m.name.is_empty()
                        || m.path.iter().any(String::is_empty)
                        || m.kind != "skip" && !names.insert(m.path.clone())
                        || m.skipped && m.reason.trim().is_empty()
                });
                if invalid || options.deny_focused && response.metadata.iter().any(|m| m.focused) {
                    let message = if invalid {
                        "suite names must be nonempty and unique among siblings, and skipped tests require a reason"
                    } else {
                        "focused tests are forbidden by --deny-focused"
                    };
                    selection.diagnostics.push(Diagnostic::error(
                        fai_contracts::SUITE_DISCOVERY,
                        message.to_owned(),
                        suite.span,
                    ));
                    continue;
                }
                for metadata in response.metadata.into_iter().filter(|m| m.kind == "case") {
                    if filter.is_some_and(|f| {
                        !metadata.path.join("::").contains(f) && !suite.source.file.contains(f)
                    }) {
                        selection.filtered += 1;
                    } else {
                        selection.cases.push(NamedCase { suite: index, metadata });
                    }
                }
            }
            Err(completion) => selection.diagnostics.push(Diagnostic::error(
                fai_contracts::SUITE_DISCOVERY,
                format!("could not discover test suite: {completion:?}"),
                suite.span,
            )),
        }
    }
    selection.focused = selection.cases.iter().any(|c| c.metadata.focused && !c.metadata.skipped);
    if selection.focused {
        let before = selection.cases.len();
        selection.cases.retain(|c| c.metadata.focused);
        selection.filtered += before - selection.cases.len();
    }
    selection
}

/// Run one selected case, retaining output and cleanup errors after termination.
pub fn run(plan: &TestPlan, case: &NamedCase, expected: &[Metadata]) -> (Completion, String, u64) {
    let suite = &plan.suites[case.suite];
    let started = Instant::now();
    match invoke(suite, Some(&case.metadata.id), expected, plan.config, super::test_timeout()) {
        Ok((response, output, timed_out, cleanup)) => {
            let mut issues = response.issues;
            if let Some(message) = cleanup {
                issues.push(Issue {
                    actual: String::new(),
                    expected: String::new(),
                    context: Vec::new(),
                    kind: "operation".to_owned(),
                    message,
                    source: String::new(),
                });
            }
            let completion = if timed_out {
                Completion::TimedOut("test exceeded its deadline".to_owned(), issues)
            } else {
                Completion::Passed(issues)
            };
            (completion, output, started.elapsed().as_millis() as u64)
        }
        Err(completion) => (completion, String::new(), started.elapsed().as_millis() as u64),
    }
}

fn capture(
    mut stream: fai_runtime::HostOutput,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> String {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    let mut draining = None;
    loop {
        if stopped.load(std::sync::atomic::Ordering::Acquire) {
            let since = draining.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_millis(100) {
                truncated = true;
                break;
            }
        }
        let n = match stream.try_read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let _ = stream.wait(Duration::from_millis(10));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let take = n.min((512 * 1024usize).saturating_sub(kept.len()));
        truncated |= take != n;
        kept.extend_from_slice(&buffer[..take]);
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        output.push_str("\n[output truncated]\n");
    }
    output
}

fn invoke(
    suite: &PreparedSuite,
    selected: Option<&str>,
    expected: &[Metadata],
    config: TestConfig,
    timeout: Duration,
) -> Result<(Response, String, bool, Option<String>), Completion> {
    let directory = tempfile::Builder::new()
        .prefix("fai-case-")
        .tempdir()
        .map_err(|e| Completion::Errored(e.to_string()))?;
    let input = directory.path().join("request.json");
    let output = directory.path().join("result.json");
    let resources = directory.path().join("resources");
    std::fs::create_dir(&resources).map_err(|e| Completion::Errored(e.to_string()))?;
    let request = Request {
        source: suite.source.clone(),
        bundle: suite.bundle.clone(),
        manifest: suite.manifest.clone(),
        execute: suite.execute.clone(),
        issues: suite.issues.clone(),
        selected: selected.map(str::to_owned),
        expected: expected.to_vec(),
        config,
        directory: resources.to_string_lossy().into_owned(),
    };
    let data = serde_json::to_vec(&request).map_err(|e| Completion::Errored(e.to_string()))?;
    std::fs::write(&input, data).map_err(|e| Completion::Errored(e.to_string()))?;
    let mut command =
        Command::new(std::env::current_exe().map_err(|e| Completion::Errored(e.to_string()))?);
    if let Some(directory) = &suite.working_directory {
        command.current_dir(directory);
    }
    let (stdout, child_stdout) =
        fai_runtime::host_output().map_err(|e| Completion::Errored(e.to_string()))?;
    let (stderr, child_stderr) =
        fai_runtime::host_output().map_err(|e| Completion::Errored(e.to_string()))?;
    let mut child = command
        .arg("__suite-worker")
        .arg(&input)
        .arg(&output)
        .env("FAI_RUN_CPU_SECS", (timeout.as_secs() + 10).to_string())
        .stdin(Stdio::null())
        .stdout(child_stdout)
        .stderr(child_stderr)
        .spawn()
        .map_err(|e| Completion::Errored(e.to_string()))?;
    drop(command);
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let out_stop = stopped.clone();
    let err_stop = stopped.clone();
    let out = std::thread::spawn(move || capture(stdout, out_stop));
    let err = std::thread::spawn(move || capture(stderr, err_stop));
    let preparation = Instant::now();
    let mut execution = None;
    let mut exit_status = None;
    let ready = output.with_extension("ready");
    let timed_out = loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            exit_status = Some(status);
            break false;
        }
        if selected.is_some() && execution.is_none() && ready.exists() {
            execution = Some(Instant::now());
        }
        let expired = match execution {
            Some(start) => start.elapsed() >= timeout,
            None => {
                preparation.elapsed()
                    >= if selected.is_some() { discovery_timeout() } else { timeout }
            }
        };
        if expired {
            break true;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if timed_out {
        let _ = std::fs::write(output.with_extension("cancel"), b"cancel");
        if let Some(status) = child.wait_timeout(cleanup_timeout()).ok().flatten() {
            exit_status = Some(status);
        } else {
            let _ = child.kill();
            exit_status = child.wait().ok();
        }
    }
    stopped.store(true, std::sync::atomic::Ordering::Release);
    let mut text = format!("{}{}", out.join().unwrap_or_default(), err.join().unwrap_or_default());
    let response = std::fs::metadata(&output)
        .ok()
        .filter(|m| m.len() <= 16 * 1024 * 1024)
        .and_then(|_| std::fs::read(&output).ok())
        .and_then(|bytes| serde_json::from_slice::<Result<Response, String>>(&bytes).ok());
    let cleanup = directory.close().err().map(|e| format!("fixture cleanup: {e}"));
    if !matches!(&response, Some(Ok(_)))
        && let Some(message) = &cleanup
    {
        text.push_str(&format!("\n{message}"));
    }
    match response {
        Some(Ok(_)) if !timed_out && exit_status.is_some_and(|s| !s.success()) => {
            Err(Completion::Crashed(text))
        }
        Some(Ok(response)) => Ok((response, text, timed_out, cleanup)),
        Some(Err(message)) if timed_out => {
            Err(Completion::TimedOut(format!("{message}\n{text}"), Vec::new()))
        }
        Some(Err(message)) => Err(Completion::Errored(format!("{message}\n{text}"))),
        None if timed_out => Err(Completion::TimedOut(text, Vec::new())),
        None if exit_status.is_some_and(|status| status.success()) => {
            Err(Completion::Errored(format!("worker returned no valid result\n{text}")))
        }
        None => Err(Completion::Crashed(text)),
    }
}

fn text(value: Value) -> String {
    let result = String::from_utf8_lossy(&read_string(value)).into_owned();
    fai_drop(value);
    result
}

fn list(value: Value) -> Vec<Value> {
    let mut result = Vec::new();
    let mut cursor = value;
    while data_tag_of(cursor) != 0 {
        result.push(fai_data_field(cursor, 0));
        let tail = fai_data_field(cursor, 1);
        fai_drop(cursor);
        cursor = tail;
    }
    fai_drop(cursor);
    result
}

fn string_list(value: Value) -> Vec<String> {
    list(value).into_iter().map(text).collect()
}
fn boolean(value: Value) -> bool {
    let result = read_int(value) != 0;
    fai_drop(value);
    result
}

fn metadata(value: Value) -> Metadata {
    let result = Metadata {
        focused: boolean(fai_data_field(value, 0)),
        id: text(fai_data_field(value, 1)),
        kind: text(fai_data_field(value, 2)),
        name: text(fai_data_field(value, 3)),
        path: string_list(fai_data_field(value, 4)),
        reason: text(fai_data_field(value, 5)),
        skipped: boolean(fai_data_field(value, 6)),
        source: text(fai_data_field(value, 7)),
    };
    fai_drop(value);
    result
}

fn issue(value: Value) -> Issue {
    let result = Issue {
        actual: text(fai_data_field(value, 0)),
        context: string_list(fai_data_field(value, 1)),
        expected: text(fai_data_field(value, 2)),
        kind: text(fai_data_field(value, 3)),
        message: text(fai_data_field(value, 4)),
        source: text(fai_data_field(value, 5)),
    };
    fai_drop(value);
    result
}

/// Database-free worker entry. Only typed, compiler-generated bundles reach it.
pub fn worker(input: &std::path::Path, output: &std::path::Path) -> i32 {
    crate::backend::apply_run_limits();
    let mut result = fai_runtime::with_program_args(&[], || -> Result<Response, String> {
        let bytes = std::fs::read(input).map_err(|e| e.to_string())?;
        let mut decoder = serde_json::Deserializer::from_slice(&bytes);
        decoder.disable_recursion_limit();
        let request = Request::deserialize(&mut decoder).map_err(|e| e.to_string())?;
        let rebuilt = from_wire(&request.bundle);
        let namer = |def: DefId| {
            crate::backend::mangle(
                rebuilt.module_labels.get(&def.file).map_or("M", String::as_str),
                def.name.as_str(),
            )
        };
        let arity = |def| rebuilt.arities.get(&def).copied().unwrap_or(0);
        let abi = |def| rebuilt.abis.get(&def).cloned().unwrap_or_default();
        let borrows: FxHashMap<_, _> =
            rebuilt.defs.iter().map(|d| (d.def, d.entry_borrowed.clone())).collect();
        let borrow = |def| borrows.get(&def).cloned().unwrap_or_default();
        let entry_of = |def| rebuilt.bounds_entry.get(&def).cloned().unwrap_or_default();
        let result_of = |def| rebuilt.bounds_result.get(&def).cloned().unwrap_or_default();
        let bce = fai_codegen::Bce {
            entry_of: &entry_of,
            result_of: &result_of,
            shadow: false,
            concurrent: true,
        };
        let lookup = if request.selected.is_none() {
            Some(Box::new(|_: &str| Some(fai_runtime::discovery_foreign_address()))
                as fai_codegen::ForeignLookup)
        } else {
            crate::backend::load_foreign_libraries(&request.bundle.libraries)?
        };
        let mut program = JitProgram::compile_with_foreign(
            &rebuilt.defs,
            &namer,
            &arity,
            &abi,
            &borrow,
            &bce,
            lookup,
        );
        let resolve = |wire: &WireDefId| -> Result<DefId, String> {
            let source = rebuilt
                .module_labels
                .iter()
                .find(|(_, name)| **name == wire.module)
                .map(|(id, _)| *id)
                .ok_or("missing worker module")?;
            Ok(DefId::new(source, Symbol::intern(&wire.name)))
        };
        let root = apply(program.closure_value(&namer, rebuilt.entry), &[]);
        let manifest_fn = program.closure_value(&namer, resolve(&request.manifest)?);
        let found = list(apply(manifest_fn, &[fai_dup(root)]))
            .into_iter()
            .map(metadata)
            .collect::<Vec<_>>();
        if request.selected.is_none() {
            fai_drop(root);
            return Ok(Response { metadata: found, issues: Vec::new() });
        }
        if !request.expected.is_empty() && request.expected != found {
            fai_drop(root);
            return Err("suite changed between discovery and execution".to_owned());
        }
        let execute = program.closure_value(&namer, resolve(&request.execute)?);
        let issues_fn = program.closure_value(&namer, resolve(&request.issues)?);
        let runtime_fn = program.closure_value(&namer, rebuilt.runtime);
        let id = request.selected.unwrap_or_default();
        let path = found
            .iter()
            .find(|m| m.kind == "case" && m.id == id)
            .ok_or("selected case disappeared")?
            .path
            .clone();
        let identity = identity(&request.source, &request.bundle.entry.module, &path);
        std::fs::write(output.with_extension("ready"), b"ready").map_err(|e| e.to_string())?;
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel();
        let cancellation = output.with_extension("cancel");
        let monitor = std::thread::spawn(move || {
            loop {
                if cancellation.exists() {
                    let _ = tx.send(());
                    break;
                }
                match stopped.recv_timeout(Duration::from_millis(10)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        let checked = fai_runtime::block_on_cancellable(
            Box::new(move || {
                let runtime = apply(runtime_fn, &[]);
                apply(
                    execute,
                    &[
                        root,
                        make_str(&id),
                        make_str(&identity),
                        runtime,
                        make_int(request.config.seed),
                        make_int(request.config.trials),
                        make_int(request.config.max_size),
                        make_str(&request.directory),
                    ],
                )
            }),
            Some(rx),
        );
        let _ = stop.send(());
        let _ = monitor.join();
        let failures = list(apply(issues_fn, &[checked])).into_iter().map(issue).collect();
        Ok(Response { metadata: found, issues: failures })
    });
    let live = fai_runtime::live_count();
    if result.is_ok() && live != 0 {
        result = Err(format!("test worker leaked {live} live values"));
    }
    let success = result.is_ok();
    if let Ok(bytes) = serde_json::to_vec(&result) {
        let _ = std::fs::write(output, bytes);
    }
    if success { 0 } else { 1 }
}

/// Resolve an assertion key, falling back to the root when no site was captured.
pub fn location(
    plan: &TestPlan,
    case: &NamedCase,
    source: &str,
) -> (Span, Option<ContractLocation>) {
    let suite = &plan.suites[case.suite];
    let span = suite.locations.get(source).copied().unwrap_or(suite.span);
    let location = plan
        .render_spans
        .resolve(span)
        .map(|s| ContractLocation { line: s.start.line, column: s.start.column });
    (span, location)
}

pub(super) fn identity(source: &ContractSource, suite: &str, path: &[String]) -> String {
    serde_json::to_string(&(source, suite, "tests", path)).expect("test identity is serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_stops_after_the_worker_even_if_a_writer_remains_open() {
        let (reader, writer) = fai_runtime::host_output().unwrap();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let output = capture(reader, stopped);
        drop(writer);
        assert!(output.contains("output truncated"));
    }

    #[test]
    fn names_and_package_scopes_cannot_alias_in_test_identities() {
        let source = ContractSource {
            file: "tests/Checks.fai".to_owned(),
            origin: ContractSourceOrigin::User,
        };
        assert_ne!(
            identity(&source, "package-a", &["one".to_owned()]),
            identity(&source, "package-b", &["one".to_owned()])
        );
        assert_ne!(
            identity(&source, "package-a", &["one::two".to_owned()]),
            identity(&source, "package-a", &["one".to_owned(), "two".to_owned()])
        );
    }
}
