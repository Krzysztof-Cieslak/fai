//! The command-line surface, modelled with clap's derive API.
//!
//! The full surface from `cli.md` is enumerated here, including every `query`
//! and `daemon` subcommand, so `fai --help` is complete and the interface is
//! locked even though command behavior is not implemented yet.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};

/// The `fai` command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "fai",
    version,
    about = "The Fai compiler and toolchain",
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Flags accepted before or after the subcommand.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Global flags shared by every subcommand.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Output format. Defaults to `human` for build/dev commands and `json` for
    /// `query`.
    #[arg(long, global = true, value_name = "FORMAT")]
    pub message_format: Option<MessageFormat>,
    /// Workspace root. Defaults to the current directory.
    #[arg(long = "project", short = 'C', global = true, value_name = "DIR")]
    pub project: Option<Utf8PathBuf>,
    /// Run in-process; do not spawn or connect to a daemon.
    #[arg(long, global = true)]
    pub no_daemon: bool,
    /// Colorize human output.
    #[arg(long, global = true, value_name = "WHEN", default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,
    /// Increase log verbosity (repeatable).
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Decrease log verbosity to errors only.
    #[arg(long, short, global = true)]
    pub quiet: bool,
    /// Append a JSON decode of daemon traffic to this file (debug).
    #[arg(long, global = true, value_name = "FILE")]
    pub protocol_log: Option<Utf8PathBuf>,
}

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MessageFormat {
    /// Human-readable text.
    Human,
    /// Machine-readable JSON.
    Json,
}

/// When to colorize human output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Colorize when writing to a terminal.
    Auto,
    /// Always colorize.
    Always,
    /// Never colorize.
    Never,
}

impl std::fmt::Display for ColorChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ColorChoice::Auto => "auto",
            ColorChoice::Always => "always",
            ColorChoice::Never => "never",
        };
        f.write_str(s)
    }
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Compile to a native executable (AOT).
    Build(PathArgs),
    /// Build and run via the JIT.
    Run(RunArgs),
    /// Typecheck only (fast inner loop).
    Check(PathArgs),
    /// Run example/forall contracts.
    Test(TestArgs),
    /// Canonically format sources.
    Fmt(FmtArgs),
    /// Start the language server on stdio.
    Lsp,
    /// Read-only code intelligence.
    Query {
        /// The query to run.
        #[command(subcommand)]
        sub: QueryCommand,
    },
    /// Manage the per-workspace daemon.
    Daemon {
        /// The daemon action.
        #[command(subcommand)]
        sub: DaemonCommand,
    },
}

/// Arguments for commands taking an optional path.
#[derive(Debug, Args)]
pub struct PathArgs {
    /// A file or directory; defaults to the whole workspace.
    pub path: Option<Utf8PathBuf>,
}

/// Arguments for `fai run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The entry file.
    pub path: Utf8PathBuf,
    /// Arguments passed to the program after `--`.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// Arguments for `fai test`.
#[derive(Debug, Args)]
pub struct TestArgs {
    /// A file or directory; defaults to the whole workspace.
    pub path: Option<Utf8PathBuf>,
    /// Run only contracts whose symbol matches this pattern.
    #[arg(long, value_name = "PAT")]
    pub r#match: Option<String>,
}

/// Arguments for `fai fmt`.
#[derive(Debug, Args)]
pub struct FmtArgs {
    /// A file or directory; defaults to the whole workspace.
    pub path: Option<Utf8PathBuf>,
    /// Do not write; exit non-zero if any file would change.
    #[arg(long)]
    pub check: bool,
}

/// `fai query` subcommands.
#[derive(Debug, Subcommand)]
pub enum QueryCommand {
    /// List/search symbols.
    Symbols,
    /// Resolve to definition site(s).
    Def {
        /// Symbol or `file:line:col`.
        target: String,
    },
    /// Find all use sites.
    Refs {
        /// Symbol or `file:line:col`.
        target: String,
    },
    /// The type at a symbol or position.
    Type {
        /// Symbol or `file:line:col`.
        target: String,
    },
    /// Docs and attached contracts.
    Docs {
        /// Symbol or `file:line:col`.
        target: String,
    },
    /// Nested symbol outline.
    Outline {
        /// A file or module.
        target: String,
    },
    /// A module's public interface.
    Api {
        /// The module.
        module: String,
    },
    /// Reverse dependencies (blast radius).
    Dependents {
        /// Symbol or module.
        target: String,
    },
    /// Inbound call edges.
    Callers {
        /// The symbol.
        symbol: String,
    },
    /// Outbound call edges.
    Callees {
        /// The symbol.
        symbol: String,
    },
    /// Hoogle-style search by type.
    Search {
        /// A type pattern.
        pattern: String,
    },
    /// Capability footprint of a function.
    Caps {
        /// The symbol.
        symbol: String,
    },
}

impl QueryCommand {
    /// The subcommand name, for diagnostics.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            QueryCommand::Symbols => "symbols",
            QueryCommand::Def { .. } => "def",
            QueryCommand::Refs { .. } => "refs",
            QueryCommand::Type { .. } => "type",
            QueryCommand::Docs { .. } => "docs",
            QueryCommand::Outline { .. } => "outline",
            QueryCommand::Api { .. } => "api",
            QueryCommand::Dependents { .. } => "dependents",
            QueryCommand::Callers { .. } => "callers",
            QueryCommand::Callees { .. } => "callees",
            QueryCommand::Search { .. } => "search",
            QueryCommand::Caps { .. } => "caps",
        }
    }
}

/// `fai daemon` subcommands.
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Print daemon status.
    Status,
    /// Start the daemon (idempotent).
    Start,
    /// Gracefully shut down the daemon.
    Stop,
    /// Restart the daemon.
    Restart,
    /// Stream a JSON decode of daemon traffic.
    Tap,
}

impl DaemonCommand {
    /// The subcommand name, for diagnostics.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            DaemonCommand::Status => "status",
            DaemonCommand::Start => "start",
            DaemonCommand::Stop => "stop",
            DaemonCommand::Restart => "restart",
            DaemonCommand::Tap => "tap",
        }
    }
}
