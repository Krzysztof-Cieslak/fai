//! The command-line surface, modelled with clap's derive API.
//!
//! The full surface from `docs/CLI.md` is enumerated here, including every `query`
//! and `daemon` subcommand, so `fai --help` is complete and the interface is
//! locked even though command behavior is not implemented yet.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};

impl GlobalArgs {
    pub(crate) fn options(&self) -> fai_driver::ProjectOptions {
        fai_driver::ProjectOptions {
            no_local: self.no_local,
            locked: self.locked || self.frozen,
            offline: self.offline || self.frozen,
            no_vendor: self.no_vendor,
        }
    }
}

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
    /// Select one member of a manifest workspace.
    #[arg(long, short = 'p', global = true, value_name = "PACKAGE")]
    pub package: Option<String>,
    /// Ignore personal root overrides and use the shared project context.
    #[arg(long, global = true)]
    pub no_local: bool,
    /// Require a valid selected lock graph without changing selections.
    #[arg(long, global = true)]
    pub locked: bool,
    /// Use only cached source content and metadata.
    #[arg(long, global = true)]
    pub offline: bool,
    /// Combine locked and offline operation.
    #[arg(long, global = true)]
    pub frozen: bool,
    /// Bypass the configured vendor tree.
    #[arg(long, global = true)]
    pub no_vendor: bool,
    /// Metadata target matrix; compilation supports the host target only.
    #[arg(long = "target", global = true, value_name = "TRIPLE")]
    pub targets: Vec<String>,
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
    /// Validate and publish explicitly versioned package releases through Git.
    Publish(PublishArgs),
    /// Create an application or library package.
    Init(InitArgs),
    /// Add a Git or local dependency and repair the active lock.
    Add(AddArgs),
    /// Remove a declared dependency and repair the active lock.
    Remove(RemoveArgs),
    /// Conservatively prepare selected lock graphs.
    Resolve,
    /// Prepare selected graphs and materialize their sources.
    Fetch,
    /// Snapshot locked Git dependencies for cache-independent offline builds.
    Vendor {
        /// Project-relative source snapshot directory.
        #[arg(long = "dir")]
        directory: Option<String>,
    },
    /// Update all dependencies or the specified aliases/logical keys.
    Update { packages: Vec<String> },
    /// Inspect recorded dependency graphs without updating them.
    Deps {
        #[command(subcommand)]
        sub: Option<DepsCommand>,
    },
    /// Print compiler identity and build provenance as JSON.
    BuildInfo,
    /// Compile to a native executable (AOT).
    Build(BuildArgs),
    /// Build and run via the JIT.
    Run(RunArgs),
    /// Internal: JIT-run an entry file in this (worker) process. Hidden.
    #[command(name = "__run-worker", hide = true)]
    RunWorker(RunWorkerArgs),
    /// Internal: check contracts from a bundle in this (worker) process. Hidden.
    #[command(name = "__test-worker", hide = true)]
    TestWorker(TestWorkerArgs),
    /// Internal: run the per-workspace daemon in this process. Hidden.
    #[command(name = "__daemon-serve", hide = true)]
    DaemonServe,
    /// Typecheck only (fast inner loop).
    Check(CheckArgs),
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

/// Explicit release version intent and resumable publication options.
#[derive(Debug, Args)]
pub struct PublishArgs {
    /// Bump/exact version, or package=intent entries for a selected batch.
    pub intents: Vec<String>,
    /// Validate and report without changing working-tree files or refs.
    #[arg(long, conflicts_with = "resume")]
    pub plan: bool,
    /// Resume the prepared release without another version bump.
    #[arg(long, conflicts_with = "intents")]
    pub resume: bool,
    /// Configured Git remote name.
    #[arg(long)]
    pub remote: Option<String>,
    /// Destination branch.
    #[arg(long)]
    pub branch: Option<String>,
}

/// Arguments for package initialization.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// New or existing directory, relative to the selected project directory.
    #[arg(default_value = ".")]
    pub path: Utf8PathBuf,
    /// Package name; otherwise use the directory name.
    #[arg(long)]
    pub name: Option<String>,
    /// Create a library instead of an application.
    #[arg(long = "lib")]
    pub library: bool,
}

/// A dependency source with explicit optional version intent.
#[derive(Debug, Args)]
pub struct AddArgs {
    /// Git URL or github:owner/repository shorthand.
    #[arg(required_unless_present = "path", conflicts_with = "path")]
    pub source: Option<String>,
    /// Local package directory.
    #[arg(long, conflicts_with_all = ["source", "branch", "rev", "subdir"])]
    pub path: Option<Utf8PathBuf>,
    /// Package path within the Git repository.
    #[arg(long)]
    pub subdir: Option<String>,
    /// Authored version requirement; omitted Git versions default to a selected caret range.
    #[arg(long, conflicts_with_all = ["branch", "rev"])]
    pub version: Option<String>,
    /// Literal branch to pin in the active lock.
    #[arg(long, conflicts_with = "rev")]
    pub branch: Option<String>,
    /// Full commit ID.
    #[arg(long)]
    pub rev: Option<String>,
    /// Dependency import alias.
    #[arg(long = "as")]
    pub alias: Option<String>,
    /// Add to development dependencies.
    #[arg(long)]
    pub dev: bool,
}

/// Arguments for removing one declared dependency.
#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Declared import alias to remove.
    pub alias: String,
    /// Remove the development entry instead of the normal one.
    #[arg(long)]
    pub dev: bool,
}

/// Read-only dependency explanations.
#[derive(Debug, Subcommand)]
pub enum DepsCommand {
    /// Show recorded incoming dependency chains for an alias.
    Why { selector: String },
}

/// Arguments for `fai check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// A file or directory; defaults to the whole workspace.
    pub path: Option<Utf8PathBuf>,
    /// Type-check only: skip evaluating closed `example` contracts.
    #[arg(long)]
    pub no_examples: bool,
}

/// Arguments for `fai build`.
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// The entry file, or the manifest's default executable when omitted.
    #[arg(default_value = "")]
    pub path: Utf8PathBuf,
    /// Select a named executable declared by the package.
    #[arg(long, value_name = "NAME", conflicts_with = "path")]
    pub bin: Option<String>,
    /// Output executable path (defaults to the entry file's stem).
    #[arg(long, value_name = "FILE")]
    pub out: Option<Utf8PathBuf>,
    /// Optimize the build (accepted; no effect yet).
    #[arg(long)]
    pub release: bool,
}

/// Arguments for the hidden `__run-worker` subcommand.
#[derive(Debug, Args)]
pub struct RunWorkerArgs {
    /// Path to the serialized run bundle to JIT and execute.
    pub bundle: Utf8PathBuf,
    /// Application arguments, separate from worker-control arguments.
    #[arg(last = true)]
    pub args: Vec<String>,
}

/// Arguments for the hidden `__test-worker` subcommand.
#[derive(Debug, Args)]
pub struct TestWorkerArgs {
    /// Path to the serialized test bundle to JIT and check.
    pub bundle: Utf8PathBuf,
    /// The index of the first contract to check (to resume after a crash).
    pub start: usize,
}

/// Arguments for `fai run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The entry file, or the manifest's default executable when omitted.
    #[arg(default_value = "")]
    pub path: Utf8PathBuf,
    /// Select a named executable declared by the package.
    #[arg(long, value_name = "NAME", conflicts_with = "path")]
    pub bin: Option<String>,
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
    /// The initial PRNG seed (runs are deterministic for a given seed).
    #[arg(long, value_name = "N")]
    pub seed: Option<i64>,
    /// The number of random trials per property.
    #[arg(long, value_name = "N")]
    pub count: Option<i64>,
    /// The maximum generation size.
    #[arg(long, value_name = "N")]
    pub max_size: Option<i64>,
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
        /// Follow the reverse graph to its transitive closure.
        #[arg(long)]
        transitive: bool,
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
