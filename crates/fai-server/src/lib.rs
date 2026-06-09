//! The per-workspace daemon and its thin client.
//!
//! The daemon ([`serve`]) holds one warm [`fai_driver::Session`] and serves
//! framed MessagePack JSON-RPC requests over a per-workspace endpoint
//! ([`mod@transport`]). The [`Client`] connects to (or spawns) it and exchanges
//! [`mod@protocol`] messages. The free functions here are the high-level surface
//! the CLI's routing layer uses: run a command warm, or manage the daemon's
//! lifecycle. Every entry point degrades gracefully — callers fall back to
//! in-process execution when the daemon is unreachable.

mod client;
pub mod protocol;
mod server;
mod transport;

use std::io::Write;
use std::path::PathBuf;

use camino::Utf8Path;
use fai_driver::{CommandSpec, DirtyFile, RenderOpts, Rendered};

pub use client::{Client, DaemonError, connect_or_spawn};
pub use protocol::StatusInfo;
pub use server::serve;

use crate::protocol::TestRequest;

/// Runs a command through the workspace daemon (spawning one if needed),
/// returning its rendered output.
pub fn run_command(
    root: &Utf8Path,
    spec: CommandSpec,
    opts: RenderOpts,
    dirty: Vec<DirtyFile>,
    log: Option<PathBuf>,
) -> Result<Rendered, DaemonError> {
    let mut client = connect_or_spawn(root, log)?;
    client.command(spec, opts, dirty)
}

/// Runs a program under daemon supervision (spawning the daemon if needed),
/// streaming the worker's output to `out`/`err`, and returns its exit code.
pub fn run(
    root: &Utf8Path,
    path: &str,
    args: &[String],
    log: Option<PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, DaemonError> {
    let mut client = connect_or_spawn(root, log)?;
    client.stream_run(path, args, out, err)
}

/// Runs `example`/`forall` contracts under daemon supervision (spawning the
/// daemon if needed), streaming live per-contract lines to `out` and returning
/// the run's exit code.
#[allow(clippy::too_many_arguments)]
pub fn test(
    root: &Utf8Path,
    path: Option<&Utf8Path>,
    r#match: Option<&str>,
    seed: Option<i64>,
    count: Option<i64>,
    max_size: Option<i64>,
    opts: RenderOpts,
    log: Option<PathBuf>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, DaemonError> {
    let mut client = connect_or_spawn(root, log)?;
    let request = TestRequest {
        path: path.map(|p| p.as_str().to_owned()),
        r#match: r#match.map(str::to_owned),
        seed,
        count,
        max_size,
        opts,
        dirty: Vec::new(),
    };
    client.stream_test(request, out, err)
}

/// Returns the daemon's status, or `None` if no daemon is running for `root`.
pub fn status(root: &Utf8Path, log: Option<PathBuf>) -> Result<Option<StatusInfo>, DaemonError> {
    match client::try_connect(root, log)? {
        Some(mut client) => client.status().map(Some),
        None => Ok(None),
    }
}

/// Ensures a daemon is running for `root` (idempotent).
pub fn start(root: &Utf8Path, log: Option<PathBuf>) -> Result<(), DaemonError> {
    // `connect_or_spawn` both connects and spawns as needed; we don't keep the
    // connection (it closes on drop, which the daemon tolerates).
    connect_or_spawn(root, log).map(drop)
}

/// Stops the daemon for `root`, returning whether one was running.
pub fn stop(root: &Utf8Path, log: Option<PathBuf>) -> Result<bool, DaemonError> {
    match client::try_connect(root, log)? {
        Some(mut client) => {
            client.shutdown()?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Restarts the daemon for `root`.
pub fn restart(root: &Utf8Path, log: Option<PathBuf>) -> Result<(), DaemonError> {
    let _ = stop(root, log.clone())?;
    start(root, log)
}
