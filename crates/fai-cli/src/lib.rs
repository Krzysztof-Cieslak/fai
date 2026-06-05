//! The `fai` command-line client.
//!
//! [`run`] is the in-process entry point: it parses arguments, dispatches to the
//! driver (directly, or through the per-workspace daemon), and writes output to
//! the provided streams, returning a process exit code. `main` is a thin wrapper
//! around it; tests call it directly with captured buffers.
//!
//! Build/dev/query commands route through the warm daemon by default; `--no-daemon`
//! (and the hidden worker/daemon subcommands) run in-process. The daemon and the
//! in-process path share `fai_driver::run_command`, so their output is identical.

mod cli;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use clap::error::ErrorKind;
use fai_driver::{
    CommandResult, CommandSpec, DriverError, OutputFormat, QueryRequest, RenderOpts, Rendered,
    Session,
};
use fai_span::{SourceMap, SpanResolver};

use crate::cli::{
    BuildArgs, Cli, ColorChoice, Command, DaemonCommand, GlobalArgs, MessageFormat, QueryCommand,
    RunArgs,
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
    let log = parsed.global.protocol_log.clone().map(Utf8PathBuf::into_std_path_buf);

    // Subcommands that always run in this process, never through the daemon.
    match &parsed.command {
        Command::RunWorker(args) => return run_worker(&args.bundle, err),
        Command::DaemonServe => return run_daemon_serve(&parsed.global, err),
        _ => {}
    }

    let root = match workspace_root(parsed.global.project.clone()) {
        Ok(root) => root,
        Err(error) => return emit_error(&error, format, color, out, err),
    };

    match &parsed.command {
        Command::Check(args) => {
            let spec = CommandSpec::Check { path: args.path.clone() };
            route(&parsed.global, &root, spec, format, color, log, out, err)
        }
        Command::Fmt(args) => {
            let spec = CommandSpec::Fmt { path: args.path.clone(), check: args.check };
            route(&parsed.global, &root, spec, format, color, log, out, err)
        }
        Command::Build(args) => {
            let spec = build_spec(args, &root);
            route(&parsed.global, &root, spec, format, color, log, out, err)
        }
        Command::Query { sub } => {
            let spec = CommandSpec::Query(to_request(sub));
            route(&parsed.global, &root, spec, format, color, log, out, err)
        }
        Command::Run(args) => run_program(&parsed.global, &root, args, log, out, err),
        Command::Test(_) => run_in_process_result(&root, fai_driver::test, format, color, out, err),
        Command::Lsp => run_in_process_result(&root, fai_driver::lsp, format, color, out, err),
        Command::Daemon { sub } => run_daemon_command(&root, sub, log, out, err),
        Command::RunWorker(_) | Command::DaemonServe => unreachable!("handled above"),
    }
}

/// Runs a daemon-eligible command: through the warm daemon by default, in-process
/// under `--no-daemon` or as a fallback when the daemon is unreachable.
#[allow(clippy::too_many_arguments)]
fn route(
    global: &GlobalArgs,
    root: &Utf8Path,
    spec: CommandSpec,
    format: MessageFormat,
    color: bool,
    log: Option<PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let opts = RenderOpts { format: to_output_format(format), color };

    let rendered = if global.no_daemon {
        match run_in_process(root, &spec, opts) {
            Ok(rendered) => rendered,
            Err(error) => return emit_error(&error, format, color, out, err),
        }
    } else {
        match fai_server::run_command(root, spec.clone(), opts, Vec::new(), log) {
            Ok(rendered) => rendered,
            Err(daemon_error) => {
                let _ = writeln!(
                    err,
                    "warning [{}]: daemon unavailable ({daemon_error}); running in-process",
                    fai_driver::DAEMON_UNAVAILABLE
                );
                match run_in_process(root, &spec, opts) {
                    Ok(rendered) => rendered,
                    Err(error) => return emit_error(&error, format, color, out, err),
                }
            }
        }
    };

    let _ = out.write_all(rendered.stdout.as_bytes());
    let _ = err.write_all(rendered.stderr.as_bytes());
    rendered.exit
}

/// Runs `spec` against a freshly opened session in this process.
fn run_in_process(
    root: &Utf8Path,
    spec: &CommandSpec,
    opts: RenderOpts,
) -> Result<Rendered, DriverError> {
    let session = Session::open(root.to_owned())?;
    Ok(fai_driver::run_command(&session, spec, opts))
}

/// Runs an in-process command that returns a [`CommandResult`] (`test`/`lsp`).
fn run_in_process_result(
    root: &Utf8Path,
    command: fn(&dyn fai_driver::Db) -> CommandResult,
    format: MessageFormat,
    color: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let session = match Session::open(root.to_owned()) {
        Ok(session) => session,
        Err(error) => return emit_error(&error, format, color, out, err),
    };
    let result = command(session.db());
    let resolver = session.resolver();
    match print_result(&result, &resolver, format, color, out, err) {
        Some(code) => code,
        None if result.ok => EXIT_OK,
        None => EXIT_FAILURES,
    }
}

/// Builds the `build` command spec, resolving the output path to absolute.
fn build_spec(args: &BuildArgs, root: &Utf8Path) -> CommandSpec {
    let stem = Utf8Path::new(args.path.as_str()).file_stem().unwrap_or("a.out").to_owned();
    let requested = args.out.clone().unwrap_or_else(|| Utf8PathBuf::from(stem));
    let out = if requested.is_absolute() { requested } else { root.join(requested) };
    CommandSpec::Build { path: args.path.clone(), out, release: args.release }
}

/// Runs the per-workspace daemon in this process (the `__daemon-serve` worker).
fn run_daemon_serve(global: &GlobalArgs, err: &mut dyn Write) -> i32 {
    let root = match workspace_root(global.project.clone()) {
        Ok(root) => root,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return EXIT_WORKSPACE;
        }
    };
    match fai_server::serve(root) {
        Ok(()) => EXIT_OK,
        Err(error) => {
            let _ = writeln!(err, "daemon error: {error}");
            EXIT_WORKSPACE
        }
    }
}

/// Manages the per-workspace daemon (`fai daemon ...`).
fn run_daemon_command(
    root: &Utf8Path,
    sub: &DaemonCommand,
    log: Option<PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match sub {
        DaemonCommand::Status => match fai_server::status(root, log) {
            Ok(Some(info)) => {
                let _ = writeln!(
                    out,
                    "daemon running: pid {}, version {}, protocol {}, uptime {}s",
                    info.pid, info.compiler_version, info.protocol_version, info.uptime_secs
                );
                EXIT_OK
            }
            Ok(None) => {
                let _ = writeln!(out, "no daemon running for this workspace");
                EXIT_OK
            }
            Err(error) => daemon_error(err, &error),
        },
        DaemonCommand::Start => match fai_server::start(root, log) {
            Ok(()) => {
                let _ = writeln!(out, "daemon running");
                EXIT_OK
            }
            Err(error) => daemon_error(err, &error),
        },
        DaemonCommand::Stop => match fai_server::stop(root, log) {
            Ok(true) => {
                let _ = writeln!(out, "daemon stopped");
                EXIT_OK
            }
            Ok(false) => {
                let _ = writeln!(out, "no daemon running for this workspace");
                EXIT_OK
            }
            Err(error) => daemon_error(err, &error),
        },
        DaemonCommand::Restart => match fai_server::restart(root, log) {
            Ok(()) => {
                let _ = writeln!(out, "daemon restarted");
                EXIT_OK
            }
            Err(error) => daemon_error(err, &error),
        },
        DaemonCommand::Tap => {
            let _ = writeln!(err, "`fai daemon tap` is not implemented yet");
            EXIT_FAILURES
        }
    }
}

/// Reports a daemon-control error and returns the workspace exit code.
fn daemon_error(err: &mut dyn Write, error: &fai_server::DaemonError) -> i32 {
    let _ = writeln!(err, "error: {error}");
    EXIT_WORKSPACE
}

/// Exit code for a program that failed to compile.
const EXIT_COMPILE_ERROR: i32 = 4;

/// Runs `fai run`: through the daemon (which supervises an isolated worker and
/// streams its output) by default, or in-process under `--no-daemon` / as a
/// fallback when the daemon is unreachable.
fn run_program(
    global: &GlobalArgs,
    root: &Utf8Path,
    args: &RunArgs,
    log: Option<PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if global.no_daemon {
        return run_in_process_worker(root, &args.path, &args.args, err);
    }
    match fai_server::run(root, args.path.as_str(), &args.args, log, out, err) {
        Ok(exit) => exit,
        Err(daemon_error) => {
            let _ = writeln!(
                err,
                "warning [{}]: daemon unavailable ({daemon_error}); running in-process",
                fai_driver::DAEMON_UNAVAILABLE
            );
            run_in_process_worker(root, &args.path, &args.args, err)
        }
    }
}

/// Builds the run bundle in this process and runs it in an isolated worker with
/// inherited stdio (the `--no-daemon` / fallback path).
fn run_in_process_worker(
    root: &Utf8Path,
    path: &Utf8Path,
    program_args: &[String],
    err: &mut dyn Write,
) -> i32 {
    let session = match Session::open(root.to_owned()) {
        Ok(session) => session,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return EXIT_WORKSPACE;
        }
    };
    let files = session.select_files(Some(path));
    let Some(entry) = files.first().copied() else {
        let _ = writeln!(err, "error: no such file in workspace: {path}");
        return EXIT_WORKSPACE;
    };

    let result = fai_driver::build_run_bundle(session.db(), entry);
    let Some(bundle) = result.bundle else {
        let resolver = session.resolver();
        let _ = write!(err, "{}", fai_driver::render_diagnostics(&result.diagnostics, &resolver));
        return EXIT_COMPILE_ERROR;
    };

    let bundle_path = match write_bundle_file(&bundle) {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return EXIT_WORKSPACE;
        }
    };
    let exit = spawn_worker(&bundle_path, &[], err);
    let _ = std::fs::remove_file(&bundle_path);
    let _ = program_args; // program arguments are accepted but unused in this subset
    exit
}

/// Spawns the `__run-worker` subprocess on `bundle_path` with inherited stdio,
/// applying any `env` (e.g. resource limits). Returns the program's exit code.
fn spawn_worker(bundle_path: &std::path::Path, env: &[(&str, String)], err: &mut dyn Write) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            let _ = writeln!(err, "error: cannot locate the fai executable: {error}");
            return EXIT_WORKSPACE;
        }
    };
    let mut command = std::process::Command::new(exe);
    command.arg("__run-worker").arg(bundle_path);
    for (key, value) in env {
        command.env(key, value);
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(EXIT_CRASH),
        Err(error) => {
            let _ = writeln!(err, "error: failed to start the run worker: {error}");
            EXIT_WORKSPACE
        }
    }
}

/// Serializes a run bundle to a unique temp file (JSON), returning its path.
fn write_bundle_file(bundle: &fai_driver::WireBundle) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fai-run-bundle-{}-{}.json",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let json = serde_json::to_vec(bundle).map_err(|e| format!("serializing run bundle: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("writing run bundle: {e}"))?;
    Ok(path)
}

/// The worker side of `fai run`: reads a serialized bundle, JIT-compiles it, and
/// runs it in this process, returning the program's exit code.
fn run_worker(bundle_path: &Utf8Path, err: &mut dyn Write) -> i32 {
    let bytes = match std::fs::read(bundle_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = writeln!(err, "error: failed to read run bundle {bundle_path}: {error}");
            return EXIT_WORKSPACE;
        }
    };
    let bundle: fai_driver::WireBundle = match serde_json::from_slice(&bytes) {
        Ok(bundle) => bundle,
        Err(error) => {
            let _ = writeln!(err, "error: malformed run bundle: {error}");
            return EXIT_WORKSPACE;
        }
    };
    fai_driver::jit_run_bundle(&bundle)
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

/// Maps the CLI message format onto the driver's output format.
fn to_output_format(format: MessageFormat) -> OutputFormat {
    match format {
        MessageFormat::Json => OutputFormat::Json,
        MessageFormat::Human => OutputFormat::Human,
    }
}

/// Resolves the workspace root from `--project`, defaulting to the current dir,
/// and makes it absolute (so the client and daemon agree on the endpoint).
fn workspace_root(project: Option<Utf8PathBuf>) -> Result<Utf8PathBuf, DriverError> {
    let cwd = || {
        std::env::current_dir()
            .map_err(|source| DriverError::Io { path: Utf8PathBuf::from("."), source })
            .and_then(|p| {
                Utf8PathBuf::from_path_buf(p)
                    .map_err(|p| DriverError::NonUtf8Path(p.to_string_lossy().into_owned()))
            })
    };
    match project {
        Some(path) if path.is_absolute() => Ok(path),
        Some(path) => Ok(cwd()?.join(path)),
        None => cwd(),
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
        let (code, out, _err) =
            run_capture(&["fai", "check", "--no-daemon", "-C", &dir, "--message-format=json"]);
        assert_eq!(code, EXIT_OK);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn check_reports_syntax_errors() {
        let dir = workspace_with("fai-cli-check-bad", "Bad.fai", "module");
        let (code, out, _err) =
            run_capture(&["fai", "check", "--no-daemon", "-C", &dir, "--message-format=json"]);
        assert_eq!(code, EXIT_FAILURES);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(value["ok"], false);
        assert_eq!(value["diagnostics"][0]["code"], "FAI1022");
    }

    #[test]
    fn fmt_check_reports_drift() {
        let dir = workspace_with("fai-cli-fmt-check", "Drift.fai", "module Drift\nlet   x=1");
        let (code, out, _err) = run_capture(&[
            "fai",
            "fmt",
            "--no-daemon",
            "-C",
            &dir,
            "--check",
            "--message-format=json",
        ]);
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
        let (code, _out, _err) =
            run_capture(&["fai", "fmt", "--no-daemon", "-C", &dir, "--message-format=json"]);
        assert_eq!(code, EXIT_OK);
        let on_disk =
            std::fs::read_to_string(std::env::temp_dir().join("fai-cli-fmt-write").join("W.fai"))
                .unwrap();
        assert_eq!(on_disk, "module W\n\nlet x = 1\n");
    }
}
