//! Command orchestration for the Fai CLI and daemon.
//!
//! This crate is the seam between the thin clients (the CLI today, the daemon
//! later) and the query database. It defines the result envelopes (the stable
//! JSON schemas), the workspace [`Session`], and one entry point per command.
//!
//! Every command is a stub for now: it returns a single [`NOT_IMPLEMENTED`]
//! diagnostic. The signatures already take `&dyn Db` so the warm-database daemon
//! can call the very same functions once the commands gain real behavior.

#[allow(unsafe_code)]
mod backend;
#[cfg(test)]
mod build_tests;
mod cache;
mod command;
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
    BuildOutcome, BuildOutput, RunOutcome, build_native, jit_run_program, object_code,
    reachable_defs,
};
pub use cache::{cache_stats, reset_stats, set_cache_dir};
pub use command::{
    CommandSpec, DirtyFile, EXIT_FAILURES, EXIT_INTERNAL, EXIT_OK, EXIT_WORKSPACE, OutputFormat,
    RenderOpts, Rendered, run_command,
};
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
    },
    CodeInfo {
        code: WORKSPACE_ERROR,
        title: "workspace or I/O error",
        default_severity: Severity::Error,
    },
    CodeInfo { code: LINK_FAILED, title: "linker failed", default_severity: Severity::Error },
    CodeInfo { code: NO_ENTRY_POINT, title: "no entry point", default_severity: Severity::Error },
    CodeInfo {
        code: DAEMON_UNAVAILABLE,
        title: "daemon unavailable; ran in-process",
        default_severity: Severity::Warning,
    },
    CodeInfo { code: RUN_TIMEOUT, title: "run timed out", default_severity: Severity::Error },
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

/// Builds the standard "not implemented" result for a command.
fn not_implemented(_db: &dyn Db, command: &str) -> CommandResult {
    let diagnostic = Diagnostic::error(
        NOT_IMPLEMENTED,
        format!("`fai {command}` is not implemented yet"),
        tooling_span(),
    )
    .with_help("this command has no behavior in the current build");
    CommandResult { diagnostics: vec![diagnostic], ok: false }
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
fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
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

/// `fai test` — run example/forall contracts.
#[must_use]
pub fn test(db: &dyn Db) -> CommandResult {
    not_implemented(db, "test")
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

/// `fai lsp` — start the language server.
#[must_use]
pub fn lsp(db: &dyn Db) -> CommandResult {
    not_implemented(db, "lsp")
}

#[cfg(test)]
mod tests {
    use fai_db::FaiDatabase;

    use super::*;

    #[test]
    fn not_implemented_reports_fai0001() {
        let db = FaiDatabase::new();
        let result = test(&db); // `test` is still a stub
        assert!(!result.ok);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, NOT_IMPLEMENTED);
        assert!(result.diagnostics[0].message.contains("test"));
    }

    #[test]
    fn output_envelope_shape() {
        let db = FaiDatabase::new();
        let result = test(&db);
        let resolver = fai_db::DbSpanResolver::new(&db);
        let output = result.to_output(&resolver);
        assert_eq!(output.schema_version, 1);
        assert!(!output.ok);
        assert_eq!(output.diagnostics.len(), 1);
        // Tooling diagnostics have no real source location.
        assert_eq!(output.diagnostics[0].primary.file, "<unknown>");
        assert_eq!(output.diagnostics[0].code, "FAI0001");
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
