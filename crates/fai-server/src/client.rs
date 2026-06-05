//! The thin client: connect to (or spawn) the workspace daemon, perform the
//! handshake, and exchange framed requests.
//!
//! [`connect_or_spawn`] returns a ready [`Client`] (handshake done). A daemon
//! whose version doesn't match is told to exit and a fresh one is spawned
//! (version-stamped socket paths already keep different compiler versions apart,
//! so this is a defensive backstop).

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use fai_driver::{CommandSpec, DirtyFile, RenderOpts, Rendered};
use interprocess::local_socket::Stream;

use crate::protocol::{
    CommandRequest, InitParams, OutputStream, PROTOCOL_VERSION, Request, Response, RunRequest,
    ServerMessage, StatusInfo, frame_to_json, read_frame, write_frame,
};
use crate::transport;

/// The compiler version, sent in the handshake.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait for a freshly spawned daemon to accept connections.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A client-side failure talking to the daemon.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// An I/O error on the connection or while spawning.
    #[error("daemon i/o error: {0}")]
    Io(#[from] io::Error),
    /// The daemon returned an error or an unexpected message.
    #[error("daemon protocol error: {0}")]
    Protocol(String),
    /// A freshly spawned daemon did not accept connections in time.
    #[error("daemon did not start within the timeout")]
    SpawnTimeout,
    /// The `fai` executable could not be located for spawning.
    #[error("cannot locate the fai executable: {0}")]
    NoExecutable(io::Error),
}

/// A connected, handshaken client.
pub struct Client {
    stream: Stream,
    log: Option<File>,
}

impl Client {
    fn new(stream: Stream, log_path: Option<&PathBuf>) -> Self {
        let log = log_path.and_then(|p| File::options().create(true).append(true).open(p).ok());
        Self { stream, log }
    }

    fn log(&mut self, direction: &str, json: &str) {
        if let Some(file) = self.log.as_mut() {
            let _ = writeln!(file, "{direction} {json}");
        }
    }

    fn send(&mut self, request: &Request) -> io::Result<()> {
        self.log("->", &frame_to_json(request));
        write_frame(&mut self.stream, request)
    }

    fn next_message(&mut self) -> io::Result<ServerMessage> {
        let message: ServerMessage = read_frame(&mut self.stream)?;
        self.log("<-", &frame_to_json(&message));
        Ok(message)
    }

    /// Sends a request and returns the terminal response, draining any streamed
    /// `Output` frames (none for non-`run` requests).
    fn request(&mut self, request: &Request) -> io::Result<Response> {
        self.send(request)?;
        loop {
            match self.next_message()? {
                ServerMessage::Output { .. } => {}
                ServerMessage::Result(response) => return Ok(response),
            }
        }
    }

    /// Runs a command and returns the daemon's rendered output.
    pub fn command(
        &mut self,
        spec: CommandSpec,
        opts: RenderOpts,
        dirty: Vec<DirtyFile>,
    ) -> Result<Rendered, DaemonError> {
        match self.request(&Request::Command(CommandRequest { spec, opts, dirty }))? {
            Response::Command(rendered) => Ok(rendered),
            Response::Error(message) => Err(DaemonError::Protocol(message)),
            other => Err(DaemonError::Protocol(format!("unexpected response: {other:?}"))),
        }
    }

    /// Runs a program under daemon supervision, streaming the worker's output to
    /// `out`/`err`, and returns its exit code.
    pub fn stream_run(
        &mut self,
        path: &str,
        args: &[String],
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<i32, DaemonError> {
        self.send(&Request::Run(RunRequest {
            path: path.to_owned(),
            args: args.to_vec(),
            dirty: Vec::new(),
        }))?;
        loop {
            match self.next_message()? {
                ServerMessage::Output { stream: OutputStream::Stdout, chunk } => {
                    let _ = out.write_all(&chunk);
                    let _ = out.flush();
                }
                ServerMessage::Output { stream: OutputStream::Stderr, chunk } => {
                    let _ = err.write_all(&chunk);
                    let _ = err.flush();
                }
                ServerMessage::Result(Response::RunExit(code)) => return Ok(code),
                ServerMessage::Result(Response::Error(message)) => {
                    return Err(DaemonError::Protocol(message));
                }
                ServerMessage::Result(other) => {
                    return Err(DaemonError::Protocol(format!("unexpected response: {other:?}")));
                }
            }
        }
    }

    /// Queries the daemon's status.
    pub fn status(&mut self) -> Result<StatusInfo, DaemonError> {
        match self.request(&Request::Status)? {
            Response::Status(info) => Ok(info),
            other => Err(DaemonError::Protocol(format!("unexpected response: {other:?}"))),
        }
    }

    /// Asks the daemon to shut down.
    pub fn shutdown(&mut self) -> Result<(), DaemonError> {
        match self.request(&Request::Shutdown)? {
            Response::Ok => Ok(()),
            other => Err(DaemonError::Protocol(format!("unexpected response: {other:?}"))),
        }
    }

    fn handshake(&mut self, root: &Utf8Path) -> io::Result<HandshakeOutcome> {
        let params = InitParams {
            protocol_version: PROTOCOL_VERSION,
            compiler_version: VERSION.to_owned(),
            workspace_root: root.as_str().to_owned(),
        };
        match self.request(&Request::Initialize(params))? {
            Response::Initialized(result)
                if result.protocol_version == PROTOCOL_VERSION
                    && result.compiler_version == VERSION =>
            {
                Ok(HandshakeOutcome::Ready)
            }
            _ => Ok(HandshakeOutcome::Mismatch),
        }
    }
}

/// The result of a handshake attempt.
enum HandshakeOutcome {
    /// Versions match; the client is usable.
    Ready,
    /// The daemon is stale/mismatched and should be replaced.
    Mismatch,
}

/// Connects to the workspace daemon, spawning and waiting for one if needed.
pub fn connect_or_spawn(root: &Utf8Path, log: Option<PathBuf>) -> Result<Client, DaemonError> {
    if let Some(client) = try_handshake(root, log.as_ref())? {
        return Ok(client);
    }

    spawn_daemon(root)?;
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    loop {
        if let Some(client) = try_handshake(root, log.as_ref())? {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err(DaemonError::SpawnTimeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Connects to an existing daemon (no spawn). `Ok(None)` if none is running.
pub fn try_connect(root: &Utf8Path, log: Option<PathBuf>) -> Result<Option<Client>, DaemonError> {
    try_handshake(root, log.as_ref())
}

/// Attempts one connect + handshake. `Ok(None)` means no usable daemon is
/// reachable (none running, or a stale one we just told to exit).
fn try_handshake(root: &Utf8Path, log: Option<&PathBuf>) -> Result<Option<Client>, DaemonError> {
    let Ok(stream) = transport::connect(root) else {
        return Ok(None);
    };
    let mut client = Client::new(stream, log);
    match client.handshake(root) {
        Ok(HandshakeOutcome::Ready) => Ok(Some(client)),
        Ok(HandshakeOutcome::Mismatch) => {
            // Tell the stale daemon to exit; the caller will spawn a fresh one.
            let _ = client.request(&Request::Exit);
            Ok(None)
        }
        // A daemon that died mid-handshake is simply not usable.
        Err(_) => Ok(None),
    }
}

/// Spawns a detached daemon for `root` (same binary, hidden subcommand).
fn spawn_daemon(root: &Utf8Path) -> Result<(), DaemonError> {
    let exe = std::env::current_exe().map_err(DaemonError::NoExecutable)?;
    let mut command = Command::new(exe);
    command
        .arg("__daemon-serve")
        .arg("--project")
        .arg(root.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command.spawn()?;
    Ok(())
}

/// Applies platform flags to fully detach the spawned daemon.
fn detach(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(windows))]
    {
        // On Unix the daemon calls setsid() itself at startup.
        let _ = command;
    }
}
