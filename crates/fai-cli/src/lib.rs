//! The `fai` command-line client.
//!
//! [`run`] is the in-process entry point: it parses arguments, dispatches to the
//! driver, and writes output to the provided streams, returning a process exit
//! code. `main` is a thin wrapper around it; tests call it directly with
//! captured buffers.

mod cli;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use clap::error::ErrorKind;
use fai_driver::{CommandResult, DriverError, QueryRequest, Session};
use fai_span::{SourceMap, SpanResolver};

use crate::cli::{
    BuildArgs, Cli, ColorChoice, Command, FmtArgs, GlobalArgs, MessageFormat, QueryCommand, RunArgs,
};

/// Success: no errors.
const EXIT_OK: i32 = 0;
/// The operation completed but reported failures.
const EXIT_FAILURES: i32 = 1;
/// Usage error (bad arguments/flags).
const EXIT_USAGE: i32 = 2;
/// Workspace/IO error.
const EXIT_WORKSPACE: i32 = 3;
/// Internal error (should not happen).
const EXIT_INTERNAL: i32 = 4;
/// A run worker terminated abnormally (e.g. by a signal).
const EXIT_CRASH: i32 = 134;

/// Parses `args`, runs the requested command, and returns a process exit code.
///
/// Output is written to `out`/`err` rather than the process streams so the whole
/// CLI is testable in-process. `--help`/`--version` are rendered to `out` with
/// exit `0`; other argument errors go to `err` with exit `2`.
pub fn run<I, T>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(parsed) => dispatch(parsed, out, err),
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                let _ = write!(out, "{error}");
                EXIT_OK
            }
            _ => {
                let _ = write!(err, "{error}");
                EXIT_USAGE
            }
        },
    }
}

/// Runs a successfully parsed command.
fn dispatch(parsed: Cli, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    init_tracing(parsed.global.verbose, parsed.global.quiet);

    let format = parsed.global.message_format.unwrap_or_else(|| default_format(&parsed.command));
    let color = use_color(parsed.global.color);

    let root = match workspace_root(parsed.global.project.clone()) {
        Ok(root) => root,
        Err(error) => return emit_error(&error, format, color, out, err),
    };
    let session = match Session::open(root) {
        Ok(session) => session,
        Err(error) => return emit_error(&error, format, color, out, err),
    };

    let db = session.db();
    let result = match &parsed.command {
        Command::Check(args) => fai_driver::check(db, &session.select_files(args.path.as_deref())),
        Command::Fmt(args) => return run_fmt(&session, args, format, color, out, err),
        Command::Build(args) => return run_build(&session, args, format, color, out, err),
        Command::Run(args) => return run_program(&parsed.global, args, err),
        Command::RunWorker(args) => return run_worker(&session, &args.path, err),
        Command::Test(_) => fai_driver::test(db),
        Command::Lsp => fai_driver::lsp(db),
        Command::Query { sub } => return run_query(&session, sub, format, out, err),
        Command::Daemon { sub } => fai_driver::daemon(db, sub.name()),
    };

    let resolver = session.resolver();
    match print_result(&result, &resolver, format, color, out, err) {
        Some(code) => code,
        None if result.ok => EXIT_OK,
        None => EXIT_FAILURES,
    }
}

/// Runs `fai fmt`: formats the selected files, writes changed ones (unless
/// `--check`), and reports the result.
fn run_fmt(
    session: &Session,
    args: &FmtArgs,
    format: MessageFormat,
    color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let files = session.select_files(args.path.as_deref());
    let result = fai_driver::fmt(session.db(), &files);

    if !args.check {
        for file in &result.files {
            if file.changed {
                let path = session.root().join(&file.path);
                if let Err(error) = std::fs::write(&path, &file.formatted) {
                    let _ = writeln!(err, "error: failed to write {path}: {error}");
                    return EXIT_WORKSPACE;
                }
            }
        }
    }

    let resolver = session.resolver();
    match format {
        MessageFormat::Json => match serde_json::to_string_pretty(&result.to_output(&resolver)) {
            Ok(json) => {
                let _ = writeln!(out, "{json}");
            }
            Err(error) => {
                let _ = writeln!(err, "internal error: failed to serialize output: {error}");
                return EXIT_INTERNAL;
            }
        },
        MessageFormat::Human => {
            let _ = write!(out, "{}", result.render_human(&resolver, color, args.check));
        }
    }

    if result.has_errors() || (args.check && result.has_changes()) {
        EXIT_FAILURES
    } else {
        EXIT_OK
    }
}

/// Runs `fai build`: compiles the entry file to a native executable.
fn run_build(
    session: &Session,
    args: &BuildArgs,
    format: MessageFormat,
    color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let files = session.select_files(Some(&args.path));
    let Some(entry) = files.first().copied() else {
        let _ = writeln!(err, "error: no such file in workspace: {}", args.path);
        return EXIT_WORKSPACE;
    };

    let stem = Utf8Path::new(entry.path(session.db())).file_stem().unwrap_or("a.out").to_owned();
    let requested = args.out.clone().unwrap_or_else(|| Utf8PathBuf::from(stem));
    let artifact = if requested.is_absolute() { requested } else { session.root().join(requested) };

    let outcome = fai_driver::build_native(session.db(), entry, &artifact);
    let resolver = session.resolver();
    match format {
        MessageFormat::Json => match serde_json::to_string_pretty(&outcome.to_output(&resolver)) {
            Ok(json) => {
                let _ = writeln!(out, "{json}");
            }
            Err(error) => {
                let _ = writeln!(err, "internal error: failed to serialize output: {error}");
                return EXIT_INTERNAL;
            }
        },
        MessageFormat::Human => {
            let _ = write!(out, "{}", outcome.render_human(&resolver, color));
        }
    }
    if outcome.ok { EXIT_OK } else { EXIT_FAILURES }
}

/// Runs `fai run`: spawns an isolated worker process that JIT-compiles and runs
/// the program, returning the program's exit code.
fn run_program(global: &GlobalArgs, args: &RunArgs, err: &mut dyn Write) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(err, "error: cannot locate the fai executable: {error}");
            return EXIT_WORKSPACE;
        }
    };
    let mut command = std::process::Command::new(exe);
    command.arg("__run-worker");
    if let Some(project) = &global.project {
        command.arg("--project").arg(project.as_str());
    }
    command.arg(args.path.as_str());
    // stdin/stdout/stderr are inherited so the program streams directly.
    match command.status() {
        Ok(status) => status.code().unwrap_or(EXIT_CRASH),
        Err(error) => {
            let _ = writeln!(err, "error: failed to start the run worker: {error}");
            EXIT_WORKSPACE
        }
    }
}

/// The worker side of `fai run`: JIT-compiles and runs the entry file in this
/// process, returning the program's exit code.
fn run_worker(session: &Session, path: &Utf8Path, err: &mut dyn Write) -> i32 {
    let files = session.select_files(Some(path));
    let Some(entry) = files.first().copied() else {
        let _ = writeln!(err, "error: no such file in workspace: {path}");
        return EXIT_WORKSPACE;
    };
    let outcome = fai_driver::jit_run_program(session.db(), entry);
    if !outcome.diagnostics.is_empty() {
        let resolver = session.resolver();
        let _ = write!(err, "{}", outcome.render_human(&resolver, false));
    }
    outcome.exit_code
}

/// Maps a clap `QueryCommand` to the driver's `QueryRequest`. Commands outside
/// M2's scope map to `Unsupported`.
fn to_request(sub: &QueryCommand) -> QueryRequest {
    match sub {
        QueryCommand::Symbols => QueryRequest::Symbols { module: None, limit: None },
        QueryCommand::Def { target } => QueryRequest::Def { target: target.clone() },
        QueryCommand::Refs { target } => QueryRequest::Refs { target: target.clone(), limit: None },
        QueryCommand::Type { target } => QueryRequest::Type { target: target.clone() },
        QueryCommand::Docs { target } => QueryRequest::Docs { target: target.clone() },
        QueryCommand::Outline { target } => QueryRequest::Outline { target: target.clone() },
        QueryCommand::Api { module } => QueryRequest::Api { module: module.clone() },
        QueryCommand::Dependents { target } => {
            QueryRequest::Dependents { target: target.clone(), limit: None }
        }
        QueryCommand::Callers { .. }
        | QueryCommand::Callees { .. }
        | QueryCommand::Search { .. }
        | QueryCommand::Caps { .. } => QueryRequest::Unsupported { name: sub.name().to_owned() },
    }
}

/// Runs `fai query`: dispatches to the IDE engine and writes JSON (or, with
/// `--human`, the readable rendering).
fn run_query(
    session: &Session,
    sub: &QueryCommand,
    format: MessageFormat,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let request = to_request(sub);
    let result = fai_driver::run_query(session, &request);
    let _ = err;
    match format {
        MessageFormat::Json => {
            let _ = writeln!(out, "{}", result.json);
        }
        MessageFormat::Human => {
            let _ = writeln!(out, "{}", result.human);
        }
    }
    if result.ok { EXIT_OK } else { EXIT_FAILURES }
}

/// Writes `result` in the chosen format. Returns `Some(exit)` only on an
/// internal failure (e.g. serialization), otherwise `None`.
fn print_result(
    result: &CommandResult,
    resolver: &dyn SpanResolver,
    format: MessageFormat,
    color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<i32> {
    match format {
        MessageFormat::Json => match serde_json::to_string_pretty(&result.to_output(resolver)) {
            Ok(json) => {
                let _ = writeln!(out, "{json}");
                None
            }
            Err(error) => {
                let _ = writeln!(err, "internal error: failed to serialize output: {error}");
                Some(EXIT_INTERNAL)
            }
        },
        MessageFormat::Human => {
            let _ = write!(out, "{}", result.render_human(resolver, color));
            None
        }
    }
}

/// Renders a hard driver error and returns the workspace exit code.
fn emit_error(
    error: &DriverError,
    format: MessageFormat,
    color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let result = fai_driver::error_result(error);
    let resolver = SourceMap::new();
    if let Some(code) = print_result(&result, &resolver, format, color, out, err) {
        return code;
    }
    EXIT_WORKSPACE
}

/// The default output format for a command (`json` for `query`, else `human`).
fn default_format(command: &Command) -> MessageFormat {
    match command {
        Command::Query { .. } => MessageFormat::Json,
        _ => MessageFormat::Human,
    }
}

/// Resolves the workspace root from `--project`, defaulting to the current dir.
fn workspace_root(project: Option<Utf8PathBuf>) -> Result<Utf8PathBuf, DriverError> {
    match project {
        Some(path) => Ok(path),
        None => {
            let cwd = std::env::current_dir()
                .map_err(|source| DriverError::Io { path: Utf8PathBuf::from("."), source })?;
            Utf8PathBuf::from_path_buf(cwd)
                .map_err(|p| DriverError::NonUtf8Path(p.to_string_lossy().into_owned()))
        }
    }
}

/// Decides whether to colorize, honoring `--color`, `NO_COLOR`, and the tty.
fn use_color(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
        }
    }
}

/// Installs a stderr tracing subscriber at a verbosity-derived level.
fn init_tracing(verbose: u8, quiet: bool) {
    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let _ =
        tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_capture(args: &[&str]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(args.iter().copied(), &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn help_goes_to_stdout_exit_zero() {
        let (code, out, _err) = run_capture(&["fai", "--help"]);
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("Usage"));
        assert!(out.contains("check"));
        assert!(out.contains("query"));
    }

    #[test]
    fn version_goes_to_stdout_exit_zero() {
        let (code, out, _err) = run_capture(&["fai", "--version"]);
        assert_eq!(code, EXIT_OK);
        assert!(out.contains("fai"));
    }

    #[test]
    fn unknown_command_is_usage_error() {
        let (code, _out, err) = run_capture(&["fai", "frobnicate"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(!err.is_empty());
    }

    #[test]
    fn no_args_shows_help_as_usage_error() {
        let (code, _out, err) = run_capture(&["fai"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(err.contains("Usage"));
    }

    fn workspace_with(name: &str, file: &str, contents: &str) -> String {
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), contents).unwrap();
        dir.to_str().unwrap().to_owned()
    }

    #[test]
    fn check_clean_workspace_succeeds() {
        let dir = workspace_with("fai-cli-check-clean", "Ok.fai", "module Ok\nlet x = 1");
        let (code, out, _err) = run_capture(&["fai", "check", "-C", &dir, "--message-format=json"]);
        assert_eq!(code, EXIT_OK);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn check_reports_syntax_errors() {
        let dir = workspace_with("fai-cli-check-bad", "Bad.fai", "module");
        let (code, out, _err) = run_capture(&["fai", "check", "-C", &dir, "--message-format=json"]);
        assert_eq!(code, EXIT_FAILURES);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["ok"], false);
        assert_eq!(value["diagnostics"][0]["code"], "FAI1022");
    }

    #[test]
    fn fmt_check_reports_drift() {
        let dir = workspace_with("fai-cli-fmt-check", "Drift.fai", "module Drift\nlet   x=1");
        let (code, out, _err) =
            run_capture(&["fai", "fmt", "-C", &dir, "--check", "--message-format=json"]);
        assert_eq!(code, EXIT_FAILURES);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["changed"][0], "Drift.fai");
        // `--check` must not rewrite the file.
        let on_disk = std::fs::read_to_string(
            std::env::temp_dir().join("fai-cli-fmt-check").join("Drift.fai"),
        )
        .unwrap();
        assert_eq!(on_disk, "module Drift\nlet   x=1");
    }

    #[test]
    fn fmt_rewrites_files_in_place() {
        let dir = workspace_with("fai-cli-fmt-write", "W.fai", "module W\nlet   x=1");
        let (code, _out, _err) = run_capture(&["fai", "fmt", "-C", &dir, "--message-format=json"]);
        assert_eq!(code, EXIT_OK);
        let on_disk =
            std::fs::read_to_string(std::env::temp_dir().join("fai-cli-fmt-write").join("W.fai"))
                .unwrap();
        assert_eq!(on_disk, "module W\n\nlet x = 1\n");
    }
}
