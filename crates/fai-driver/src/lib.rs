//! Command orchestration for the Fai CLI and daemon.
//!
//! This crate is the seam between the thin clients (the CLI and the daemon) and
//! the query database. It defines the result envelopes (the stable JSON schemas),
//! the workspace [`Session`], and one entry point per command. Entry points take
//! `&dyn Db` so the warm-database daemon and the one-shot CLI share them.

#[allow(unsafe_code)]
mod backend;
#[cfg(test)]
mod build_tests;
mod cache;
mod command;
mod contracts;
mod query;
mod session;

use std::fmt::Write as _;

use camino::Utf8PathBuf;
pub use fai_db::Db;
use fai_db::SourceFile;
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{
    CodeInfo, Diagnostic, DiagnosticCode, SCHEMA_VERSION, Severity, render_human,
};
use fai_span::{ByteOffset, SourceId, Span, SpanResolver, TextRange};
use serde::Serialize;

pub use backend::{
    BuildOutcome, BuildOutput, CompiledProgram, RunBundleResult, RunOutcome, build_native,
    build_run_bundle, jit_compile, jit_run_bundle, jit_run_program, object_code, reachable_defs,
    set_object_cache_capacity,
};
pub use cache::{cache_stats, reset_stats, set_cache_dir};
pub use command::{
    CommandSpec, DirtyFile, EXIT_FAILURES, EXIT_INTERNAL, EXIT_OK, EXIT_WORKSPACE, OutputFormat,
    RenderOpts, Rendered, run_command,
};
pub use contracts::{
    ContractEvent, ContractResult, ContractStatus, TestConfig, TestOutcome, TestOutput, TestPlan,
    assemble_outcome, build_example_plan, build_test_plan, check_examples,
    check_examples_in_process, example_failures, jit_test_bundle, render_test_event_line,
    run_test_workers, run_test_workers_with_timeout, run_tests,
};
pub use fai_core::{TestWireBundle, WireBundle};
pub use query::{QueryRequest, QueryResult, run_query};
pub use session::Session;

/// A command is not implemented yet.
pub const NOT_IMPLEMENTED: DiagnosticCode = DiagnosticCode::new("FAI0001");
/// A workspace or I/O error prevented the command from running.
pub const WORKSPACE_ERROR: DiagnosticCode = DiagnosticCode::new("FAI0002");
/// The linker failed while producing a native executable.
pub const LINK_FAILED: DiagnosticCode = DiagnosticCode::new("FAI0003");
/// The entry file has no `main` to build or run.
pub const NO_ENTRY_POINT: DiagnosticCode = DiagnosticCode::new("FAI0004");
/// The daemon could not be reached; the command ran in-process instead.
pub const DAEMON_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new("FAI0005");
/// A `run` worker exceeded its time limit and was terminated.
pub const RUN_TIMEOUT: DiagnosticCode = DiagnosticCode::new("FAI0006");

/// Diagnostic codes owned by the tooling/driver layer (the `FAI0xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: NOT_IMPLEMENTED,
        title: "command not implemented",
        default_severity: Severity::Error,
        explanation: "The requested CLI command has no behavior in this build. It is a \
                      placeholder for a command that lands in a later release.",
    },
    CodeInfo {
        code: WORKSPACE_ERROR,
        title: "workspace or I/O error",
        default_severity: Severity::Error,
        explanation: "The workspace could not be read: the root is not a directory, a file \
                      could not be read, or a path was not valid UTF-8. Check the path passed \
                      to `-C`/the entry file and filesystem permissions.",
    },
    CodeInfo {
        code: LINK_FAILED,
        title: "linker failed",
        default_severity: Severity::Error,
        explanation: "The system linker returned an error while producing the native \
                      executable. The linker's own output accompanies this diagnostic; a \
                      missing toolchain or linker is the usual cause.",
    },
    CodeInfo {
        code: NO_ENTRY_POINT,
        title: "no entry point",
        default_severity: Severity::Error,
        explanation: "`fai build`/`fai run` need an entry file defining \
                      `public main : Runtime -> Unit`, but none was found.",
    },
    CodeInfo {
        code: DAEMON_UNAVAILABLE,
        title: "daemon unavailable; ran in-process",
        default_severity: Severity::Warning,
        explanation: "The per-workspace daemon could not be reached, so the command ran \
                      in-process (correct, just without the warm-cache speedup). Run \
                      `fai daemon status` to investigate, or pass `--no-daemon` to silence it.",
    },
    CodeInfo {
        code: RUN_TIMEOUT,
        title: "run timed out",
        default_severity: Severity::Error,
        explanation: "A program under `fai run` exceeded its wall-clock limit and was \
                      terminated (exit 124). Raise `FAI_RUN_TIMEOUT_MS` for a longer-running \
                      program.",
    },
];

/// A workspace or I/O failure that prevents a command from running.
///
/// These are hard failures (exit code 3), distinct from in-band diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The workspace root is missing or not a directory.
    #[error("workspace root is not a directory: {0}")]
    NotADirectory(camino::Utf8PathBuf),
    /// A filesystem error while reading the workspace.
    #[error("failed to read {path}: {source}")]
    Io {
        /// The path being read.
        path: camino::Utf8PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A path under the workspace was not valid UTF-8.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
}

/// The outcome of running a command: its diagnostics and whether it succeeded.
///
/// Diagnostics are held in their in-memory form; rendering resolves spans via a
/// [`SpanResolver`] supplied by the caller.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Diagnostics produced by the command.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether the command completed without error diagnostics.
    pub ok: bool,
}

impl CommandResult {
    /// Builds the JSON wire envelope (`{ schemaVersion, diagnostics, ok }`).
    #[must_use]
    pub fn to_output(&self, resolver: &dyn SpanResolver) -> CommandOutput {
        CommandOutput {
            schema_version: SCHEMA_VERSION,
            diagnostics: to_wire(&self.diagnostics, resolver),
            ok: self.ok,
        }
    }

    /// Renders the diagnostics for human consumption.
    #[must_use]
    pub fn render_human(&self, resolver: &dyn SpanResolver, color: bool) -> String {
        render_human(&self.diagnostics, resolver, color)
    }
}

/// The JSON envelope shared by command results (`docs/CLI.md` §5).
///
/// This is the stable shape `fai check --message-format=json` emits; the same
/// type is reused by the not-yet-implemented commands until they gain richer
/// envelopes of their own.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    /// Output schema version.
    pub schema_version: u32,
    /// The command's diagnostics, in wire form.
    pub diagnostics: Vec<DiagnosticWire>,
    /// Whether the command succeeded.
    pub ok: bool,
}

/// A span used by tooling-level diagnostics that have no source location. It
/// refers to no registered file, so resolvers report it as `<unknown>`.
pub(crate) fn tooling_span() -> Span {
    Span::new(SourceId::new(u32::MAX), TextRange::empty(ByteOffset::ZERO))
}

/// Runs a read `f` over a session snapshot, returning `None` if a concurrent
/// input mutation cancelled it mid-flight.
///
/// salsa cancels outstanding snapshots when an input is written: the in-flight
/// read unwinds with a cancellation payload, which this catches and reports as
/// `None` so the caller (the daemon) can retry against a fresh snapshot at the
/// new revision. Any other panic propagates unchanged. The closure is taken as
/// [`std::panic::UnwindSafe`] because a cancellation discards its work entirely —
/// the snapshot it reads is dropped and re-taken on retry, so no partially
/// observed state escapes; callers wrap the read in
/// [`std::panic::AssertUnwindSafe`].
pub fn catch_cancellation<T>(f: impl FnOnce() -> T + std::panic::UnwindSafe) -> Option<T> {
    fai_db::salsa::Cancelled::catch(f).ok()
}

/// Renders diagnostics to a human-readable string, for paths that report errors
/// outside the normal result envelope (e.g. a failed `run` bundle).
#[must_use]
pub fn render_diagnostics(diagnostics: &[Diagnostic], resolver: &dyn SpanResolver) -> String {
    render_human(diagnostics, resolver, false)
}

/// Builds a result describing a hard driver error (rendered as `FAI0002`).
#[must_use]
pub fn error_result(error: &DriverError) -> CommandResult {
    let diagnostic = Diagnostic::error(WORKSPACE_ERROR, error.to_string(), tooling_span());
    CommandResult { diagnostics: vec![diagnostic], ok: false }
}

/// Collects the diagnostics produced while parsing `file` (front-end only).
fn file_diagnostics(db: &dyn Db, file: SourceFile) -> Vec<Diagnostic> {
    fai_syntax::parse::accumulated::<fai_db::Diag>(db, file)
        .into_iter()
        .map(|diag| diag.0.clone())
        .collect()
}

/// Collects the resolution + type diagnostics that belong to `file`.
///
/// Accumulators are transitive (a query collects everything emitted by the
/// queries it calls), and workspace-level queries (e.g. duplicate-module
/// detection) touch every file. So we filter to diagnostics whose primary span
/// is in `file`, ensuring only the checked file's own diagnostics are reported.
pub(crate) fn semantic_diagnostics(db: &dyn Db, file: SourceFile) -> Vec<Diagnostic> {
    let source = file.source(db);
    let mut out = Vec::new();
    out.extend(
        fai_resolve::resolve::accumulated::<fai_db::Diag>(db, file)
            .into_iter()
            .map(|d| d.0.clone()),
    );
    out.extend(
        fai_types::check_file::accumulated::<fai_db::Diag>(db, file)
            .into_iter()
            .map(|d| d.0.clone()),
    );
    out.retain(|d| d.primary.source() == source);
    dedup_diagnostics(&mut out);
    out
}

/// Removes exact-duplicate diagnostics (transitive accumulation can surface the
/// same diagnostic via more than one path).
fn dedup_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|d| {
        seen.insert((
            d.code.as_str(),
            d.primary.start().raw(),
            d.primary.end().raw(),
            d.message.clone(),
        ))
    });
}

/// Sorts diagnostics deterministically by (byte start, code).
pub(crate) fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        (a.primary.start().raw(), a.code.as_str()).cmp(&(b.primary.start().raw(), b.code.as_str()))
    });
}

/// `fai check` — parse, resolve, and type-check `files`, reporting diagnostics.
///
/// Resolution and inference run against the whole workspace held by `db` (so
/// cross-module references resolve), but only the selected `files`' diagnostics
/// are reported. A file that does not parse skips its semantic passes (so a parse
/// error does not cascade into spurious resolution/type errors).
#[must_use]
pub fn check(db: &dyn Db, files: &[SourceFile]) -> CommandResult {
    let mut diagnostics = Vec::new();
    for &file in files {
        let parse_diags = file_diagnostics(db, file);
        let has_parse_error = parse_diags.iter().any(|d| d.severity == Severity::Error);
        diagnostics.extend(parse_diags);
        if !has_parse_error {
            diagnostics.extend(semantic_diagnostics(db, file));
        }
    }
    sort_diagnostics(&mut diagnostics);
    let ok = !diagnostics.iter().any(|diag| diag.severity == Severity::Error);
    CommandResult { diagnostics, ok }
}

/// `fai test` — run example/forall contracts over `files`, filtered by
/// `match_pat` (against the subject symbol / module), with generator `config`.
#[must_use]
pub fn test(
    db: &dyn Db,
    files: &[SourceFile],
    match_pat: Option<&str>,
    config: TestConfig,
) -> TestOutcome {
    run_tests(db, files, match_pat, config)
}

/// One file's formatting outcome.
#[derive(Debug, Clone)]
pub struct FormattedFile {
    /// The file's workspace-relative path.
    pub path: Utf8PathBuf,
    /// The canonical text (only meaningful when the file parsed cleanly).
    pub formatted: String,
    /// Whether `formatted` differs from the file on disk.
    pub changed: bool,
}

/// The outcome of `fai fmt`: the per-file results and any diagnostics for files
/// that could not be formatted (parse errors).
#[derive(Debug, Clone)]
pub struct FmtResult {
    /// Files that parsed cleanly and were formatted.
    pub files: Vec<FormattedFile>,
    /// Diagnostics for files skipped because of parse errors.
    pub diagnostics: Vec<Diagnostic>,
}

impl FmtResult {
    /// The workspace-relative paths whose contents would change.
    #[must_use]
    pub fn changed_paths(&self) -> Vec<String> {
        self.files.iter().filter(|f| f.changed).map(|f| f.path.to_string()).collect()
    }

    /// Whether any file's formatting differs from disk.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        self.files.iter().any(|f| f.changed)
    }

    /// Whether any file could not be formatted because of an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|diag| diag.severity == Severity::Error)
    }

    /// Builds the JSON envelope (`{ schemaVersion, changed, diagnostics }`).
    #[must_use]
    pub fn to_output(&self, resolver: &dyn SpanResolver) -> FmtOutput {
        FmtOutput {
            schema_version: SCHEMA_VERSION,
            changed: self.changed_paths(),
            diagnostics: to_wire(&self.diagnostics, resolver),
        }
    }

    /// Renders the outcome for humans. `check` selects "would reformat" wording.
    #[must_use]
    pub fn render_human(&self, resolver: &dyn SpanResolver, color: bool, check: bool) -> String {
        let mut out = render_human(&self.diagnostics, resolver, color);
        let verb = if check { "would reformat" } else { "reformatted" };
        for path in self.changed_paths() {
            let _ = writeln!(out, "{verb} {path}");
        }
        out
    }
}

/// The JSON envelope for `fai fmt` (`docs/CLI.md` §5, plus `diagnostics`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FmtOutput {
    /// Output schema version.
    pub schema_version: u32,
    /// Workspace-relative paths that changed (or would change under `--check`).
    pub changed: Vec<String>,
    /// Diagnostics for files that could not be formatted.
    pub diagnostics: Vec<DiagnosticWire>,
}

/// `fai fmt` — format `files`. The driver computes the canonical text; writing it
/// to disk is the client's job.
#[must_use]
pub fn fmt(db: &dyn Db, files: &[SourceFile]) -> FmtResult {
    let mut formatted = Vec::new();
    let mut diagnostics = Vec::new();
    for &file in files {
        let diags = file_diagnostics(db, file);
        if diags.iter().any(|diag| diag.severity == Severity::Error) {
            diagnostics.extend(diags);
            continue; // a file that does not parse cannot be formatted
        }
        let parsed = fai_syntax::parse(db, file);
        let source = file.text(db);
        let text = fai_fmt::format(&parsed.module, &parsed.comments, source);
        let changed = text != *source;
        formatted.push(FormattedFile {
            path: Utf8PathBuf::from(file.path(db).as_str()),
            formatted: text,
            changed,
        });
    }
    FmtResult { files: formatted, diagnostics }
}

#[cfg(test)]
mod tests {
    use fai_db::FaiDatabase;

    use super::*;

    #[test]
    fn test_with_no_files_is_ok() {
        let db = FaiDatabase::new();
        let outcome = test(&db, &[], None, TestConfig::default());
        assert!(outcome.ok);
        assert_eq!(outcome.total, 0);
        assert_eq!(outcome.passed, 0);
        let resolver = fai_db::DbSpanResolver::new(&db);
        let output = outcome.to_output(&resolver);
        assert_eq!(output.schema_version, 1);
        assert!(output.ok);
    }

    #[test]
    fn check_with_no_files_is_ok() {
        let db = FaiDatabase::new();
        let result = check(&db, &[]);
        assert!(result.ok);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn codes_are_well_formed() {
        for info in CODES {
            assert!(info.code.has_valid_format(), "bad code: {}", info.code);
        }
    }
}
