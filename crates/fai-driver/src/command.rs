//! Unified command execution: run a command against a [`Session`] and produce
//! the exact bytes a client should print.
//!
//! Both the in-process CLI and the daemon call [`run_command`], so warm
//! (daemon) output is identical to the one-shot (`--no-daemon`) path by
//! construction — there is a single rendering and I/O implementation. The result
//! is a [`Rendered`] (`stdout`, `stderr`, `exit`); any file I/O a command implies
//! (formatting in place, linking an artifact) is performed here, since whoever
//! runs the command (the client or the daemon) has workspace access.

use std::fmt::Write as _;

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::query::{QueryRequest, run_query};
use crate::session::Session;
use crate::{build_native, check, check_examples, fmt};

/// Success: no errors.
pub const EXIT_OK: i32 = 0;
/// The operation completed but reported failures.
pub const EXIT_FAILURES: i32 = 1;
/// Workspace/IO error.
pub const EXIT_WORKSPACE: i32 = 3;
/// Internal error (e.g. serialization).
pub const EXIT_INTERNAL: i32 = 4;

/// A client-declared changed file (the dirty-set fast path, CLI.md §7.5).
///
/// `content`, when present, is the authoritative already-written text (the daemon
/// uses it directly); otherwise the daemon re-reads `path` from disk. `hash` is
/// advisory. The CLI does not populate this; it exists for editor/LSP clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyFile {
    /// Workspace-relative path.
    pub path: String,
    /// Optional `blake3:<hex>` content hash (advisory).
    pub hash: Option<String>,
    /// Optional inline content (already written to disk).
    pub content: Option<String>,
}

/// The output format a command renders to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    /// Human-readable text.
    Human,
    /// Machine-readable JSON.
    Json,
}

/// Rendering options carried from the client (it knows its own terminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderOpts {
    /// The output format.
    pub format: OutputFormat,
    /// Whether to colorize human output.
    pub color: bool,
}

/// A command to run against a workspace session.
///
/// Paths are workspace-relative or absolute; the build output path is resolved by
/// the client before it is placed here (so the daemon writes to the right place
/// regardless of its working directory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandSpec {
    /// Typecheck the selection (or the whole workspace).
    Check {
        /// File/dir to check; `None` = the whole workspace.
        path: Option<Utf8PathBuf>,
        /// Evaluate closed `example` contracts and report failures (`FAI6001`).
        /// `false` restores a pure type-check (`fai check --no-examples`).
        examples: bool,
    },
    /// Format the selection; `check` reports drift without writing.
    Fmt {
        /// File/dir to format; `None` = the whole workspace.
        path: Option<Utf8PathBuf>,
        /// Report-only: do not write changed files.
        check: bool,
    },
    /// Build a native executable from `path` to `out`.
    Build {
        /// The entry file.
        path: Utf8PathBuf,
        /// The output executable path (absolute, or resolved against the root).
        out: Utf8PathBuf,
        /// Optimize (accepted; no effect yet).
        release: bool,
    },
    /// A read-only code-intelligence query.
    Query(QueryRequest),
}

/// The bytes a command produced, plus its process exit code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rendered {
    /// Bytes for the client's stdout.
    pub stdout: String,
    /// Bytes for the client's stderr.
    pub stderr: String,
    /// The process exit code.
    pub exit: i32,
}

/// Runs `spec` against `session` and renders the result to bytes.
#[must_use]
pub fn run_command(session: &Session, spec: &CommandSpec, opts: RenderOpts) -> Rendered {
    match spec {
        CommandSpec::Check { path, examples } => {
            run_check(session, path.as_deref(), *examples, opts)
        }
        CommandSpec::Fmt { path, check } => run_fmt(session, path.as_deref(), *check, opts),
        CommandSpec::Build { path, out, release } => run_build(session, path, out, *release, opts),
        CommandSpec::Query(request) => run_query_command(session, request, opts),
    }
}

fn run_check(
    session: &Session,
    path: Option<&camino::Utf8Path>,
    examples: bool,
    opts: RenderOpts,
) -> Rendered {
    let files = session.select_files(path);
    let mut result = check(session.db(), &files);
    // Once the selection type-checks cleanly, evaluate its closed `example`
    // contracts and fold in any failures (located `FAI6001`). A type error skips
    // this (the examples could not be compiled soundly anyway).
    if examples && result.ok {
        let failures = check_examples(session.db(), &files);
        if !failures.is_empty() {
            result.diagnostics.extend(failures);
            crate::sort_diagnostics(&mut result.diagnostics);
            result.ok = false;
        }
    }
    let resolver = session.resolver();
    let mut r = Rendered::default();
    match opts.format {
        OutputFormat::Json => match serde_json::to_string_pretty(&result.to_output(&resolver)) {
            Ok(json) => {
                let _ = writeln!(r.stdout, "{json}");
            }
            Err(error) => {
                let _ = writeln!(r.stderr, "internal error: failed to serialize output: {error}");
                r.exit = EXIT_INTERNAL;
                return r;
            }
        },
        OutputFormat::Human => {
            let _ = write!(r.stdout, "{}", result.render_human(&resolver, opts.color));
        }
    }
    r.exit = if result.ok { EXIT_OK } else { EXIT_FAILURES };
    r
}

fn run_fmt(
    session: &Session,
    path: Option<&camino::Utf8Path>,
    check_only: bool,
    opts: RenderOpts,
) -> Rendered {
    let files = session.select_files(path);
    let result = fmt(session.db(), &files);
    let mut r = Rendered::default();

    if !check_only {
        for file in &result.files {
            if file.changed {
                let target = session.root().join(&file.path);
                if let Err(error) = std::fs::write(&target, &file.formatted) {
                    let _ = writeln!(r.stderr, "error: failed to write {target}: {error}");
                    r.exit = EXIT_WORKSPACE;
                    return r;
                }
            }
        }
    }

    let resolver = session.resolver();
    match opts.format {
        OutputFormat::Json => match serde_json::to_string_pretty(&result.to_output(&resolver)) {
            Ok(json) => {
                let _ = writeln!(r.stdout, "{json}");
            }
            Err(error) => {
                let _ = writeln!(r.stderr, "internal error: failed to serialize output: {error}");
                r.exit = EXIT_INTERNAL;
                return r;
            }
        },
        OutputFormat::Human => {
            let _ = write!(r.stdout, "{}", result.render_human(&resolver, opts.color, check_only));
        }
    }
    r.exit = if result.has_errors() || (check_only && result.has_changes()) {
        EXIT_FAILURES
    } else {
        EXIT_OK
    };
    r
}

fn run_build(
    session: &Session,
    path: &camino::Utf8Path,
    out: &camino::Utf8Path,
    release: bool,
    opts: RenderOpts,
) -> Rendered {
    let _ = release; // accepted; no effect yet
    let mut r = Rendered::default();
    let files = session.select_files(Some(path));
    let Some(entry) = files.first().copied() else {
        let _ = writeln!(r.stderr, "error: no such file in workspace: {path}");
        r.exit = EXIT_WORKSPACE;
        return r;
    };
    let artifact = if out.is_absolute() { out.to_owned() } else { session.root().join(out) };

    let outcome = build_native(session.db(), entry, &artifact);
    let resolver = session.resolver();
    match opts.format {
        OutputFormat::Json => match serde_json::to_string_pretty(&outcome.to_output(&resolver)) {
            Ok(json) => {
                let _ = writeln!(r.stdout, "{json}");
            }
            Err(error) => {
                let _ = writeln!(r.stderr, "internal error: failed to serialize output: {error}");
                r.exit = EXIT_INTERNAL;
                return r;
            }
        },
        OutputFormat::Human => {
            let _ = write!(r.stdout, "{}", outcome.render_human(&resolver, opts.color));
        }
    }
    r.exit = if outcome.ok { EXIT_OK } else { EXIT_FAILURES };
    r
}

fn run_query_command(session: &Session, request: &QueryRequest, opts: RenderOpts) -> Rendered {
    let result = run_query(session, request);
    let mut r = Rendered::default();
    match opts.format {
        OutputFormat::Json => {
            let _ = writeln!(r.stdout, "{}", result.json);
        }
        OutputFormat::Human => {
            let _ = writeln!(r.stdout, "{}", result.human);
        }
    }
    r.exit = if result.ok { EXIT_OK } else { EXIT_FAILURES };
    r
}
