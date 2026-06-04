//! Command orchestration for the Fai CLI and daemon.
//!
//! This crate is the seam between the thin clients (the CLI today, the daemon
//! later) and the query database. It defines the result envelopes (the stable
//! JSON schemas), the workspace [`Session`], and one entry point per command.
//!
//! Every command is a stub for now: it returns a single [`NOT_IMPLEMENTED`]
//! diagnostic. The signatures already take `&dyn Db` so the warm-database daemon
//! can call the very same functions once the commands gain real behavior.

mod session;

use fai_db::Db;
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{
    CodeInfo, Diagnostic, DiagnosticCode, SCHEMA_VERSION, Severity, render_human,
};
use fai_span::{ByteOffset, SourceId, Span, SpanResolver, TextRange};
use serde::Serialize;

pub use session::Session;

/// A command is not implemented yet.
pub const NOT_IMPLEMENTED: DiagnosticCode = DiagnosticCode::new("FAI0001");
/// A workspace or I/O error prevented the command from running.
pub const WORKSPACE_ERROR: DiagnosticCode = DiagnosticCode::new("FAI0002");

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
fn tooling_span() -> Span {
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

/// `fai check` — typecheck the workspace.
#[must_use]
pub fn check(db: &dyn Db) -> CommandResult {
    not_implemented(db, "check")
}

/// `fai build` — compile to a native executable.
#[must_use]
pub fn build(db: &dyn Db) -> CommandResult {
    not_implemented(db, "build")
}

/// `fai run` — build and run via the JIT.
#[must_use]
pub fn run(db: &dyn Db) -> CommandResult {
    not_implemented(db, "run")
}

/// `fai test` — run example/forall contracts.
#[must_use]
pub fn test(db: &dyn Db) -> CommandResult {
    not_implemented(db, "test")
}

/// `fai fmt` — canonically format sources.
#[must_use]
pub fn fmt(db: &dyn Db) -> CommandResult {
    not_implemented(db, "fmt")
}

/// `fai lsp` — start the language server.
#[must_use]
pub fn lsp(db: &dyn Db) -> CommandResult {
    not_implemented(db, "lsp")
}

/// `fai query <name>` — read-only code intelligence.
#[must_use]
pub fn query(db: &dyn Db, name: &str) -> CommandResult {
    not_implemented(db, &format!("query {name}"))
}

/// `fai daemon <name>` — daemon lifecycle management.
#[must_use]
pub fn daemon(db: &dyn Db, name: &str) -> CommandResult {
    not_implemented(db, &format!("daemon {name}"))
}

#[cfg(test)]
mod tests {
    use fai_db::FaiDatabase;

    use super::*;

    #[test]
    fn not_implemented_reports_fai0001() {
        let db = FaiDatabase::new();
        let result = check(&db);
        assert!(!result.ok);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, NOT_IMPLEMENTED);
        assert!(result.diagnostics[0].message.contains("check"));
    }

    #[test]
    fn output_envelope_shape() {
        let db = FaiDatabase::new();
        let result = check(&db);
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
    fn codes_are_well_formed() {
        for info in CODES {
            assert!(info.code.has_valid_format(), "bad code: {}", info.code);
        }
    }
}
