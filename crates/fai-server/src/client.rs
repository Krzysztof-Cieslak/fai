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
use fai_driver::{
    CommandSpec, DirtyFile, OutputFormat, RenderOpts, Rendered, render_test_event_line,
};
use interprocess::local_socket::Stream;

use crate::protocol::{
    CommandRequest, InitParams, OutputStream, PROTOCOL_VERSION, Request, Response, RunRequest,
    ServerMessage, StatusInfo, TestRequest, frame_to_json, read_frame, render_tap, write_frame,
};
use crate::transport;

/// The compiler version, sent in the handshake.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait for a freshly spawned daemon to accept connections.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for a stopped daemon's endpoint to stop answering.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
                ServerMessage::Output { .. }
                | ServerMessage::TestEvent(_)
                | ServerMessage::TapFrame(_) => {}
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
                // `run` produces no test events or tap frames; ignore defensively.
                ServerMessage::TestEvent(_) | ServerMessage::TapFrame(_) => {}
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

    /// Runs `example`/`forall` contracts under daemon supervision, printing live
    /// per-contract lines (human mode) as they stream in, then the daemon's
    /// rendered report, and returns its exit code.
    pub fn stream_test(
        &mut self,
        request: TestRequest,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<i32, DaemonError> {
        let human = matches!(request.opts.format, OutputFormat::Human);
        self.send(&Request::Test(request))?;
        loop {
            match self.next_message()? {
                ServerMessage::TestEvent(event) => {
                    if human {
                        let _ = out.write_all(render_test_event_line(&event).as_bytes());
                        let _ = out.flush();
                    }
                }
                // `test` contracts have no capabilities, so no `$/output` is
                // expected; ignore output and tap frames defensively.
                ServerMessage::Output { .. } | ServerMessage::TapFrame(_) => {}
                ServerMessage::Result(Response::Test(rendered)) => {
                    let _ = out.write_all(rendered.stdout.as_bytes());
                    let _ = err.write_all(rendered.stderr.as_bytes());
                    return Ok(rendered.exit);
                }
                ServerMessage::Result(Response::Error(message)) => {
                    return Err(DaemonError::Protocol(message));
                }
                ServerMessage::Result(other) => {
                    return Err(DaemonError::Protocol(format!("unexpected response: {other:?}")));
                }
            }
        }
    }

    /// Subscribes to the daemon's traffic and prints a JSON decode of every
    /// frame on other connections to `out`, one per line, until the connection
    /// closes. A readiness notice is written to `status` once the daemon has
    /// acknowledged the subscription (so a caller can know observation is live).
    ///
    /// Returns `Ok(())` on a clean end of stream (the daemon shut down or the
    /// connection was closed), so an interrupted tap is not reported as an error.
    pub fn tap(&mut self, out: &mut dyn Write, status: &mut dyn Write) -> Result<(), DaemonError> {
        self.send(&Request::Tap)?;
        // The first frame is the subscription acknowledgement; once it arrives,
        // the daemon has registered this tap and every later frame is observed.
        match self.next_message()? {
            ServerMessage::Result(Response::Ok) => {}
            ServerMessage::Result(Response::Error(message)) => {
                return Err(DaemonError::Protocol(message));
            }
            other => {
                return Err(DaemonError::Protocol(format!("unexpected tap response: {other:?}")));
            }
        }
        let _ = writeln!(status, "tapping daemon traffic for this workspace (Ctrl-C to stop)");
        let _ = status.flush();

        loop {
            match self.next_message() {
                Ok(ServerMessage::TapFrame(frame)) => {
                    let _ = writeln!(out, "{}", render_tap(&frame));
                    let _ = out.flush();
                }
                // No other server message is expected on a tap connection; ignore.
                Ok(_) => {}
                // A clean close (the daemon shut down, or we were interrupted) ends
                // the tap without an error.
                Err(error) if is_disconnect(&error) => return Ok(()),
                Err(error) => return Err(error.into()),
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

/// Blocks until no daemon answers at `root`'s endpoint — the just-stopped daemon
/// has unlinked its socket and exited — so a following spawn binds a genuinely
/// fresh daemon.
///
/// A daemon acknowledges [`Request::Shutdown`] *before* it unlinks its socket and
/// exits, so a bare `shutdown` returns while the old process is still listening;
/// probing here makes the stop synchronous. Best-effort: returns after
/// [`SHUTDOWN_TIMEOUT`] even if something still answers.
pub fn wait_until_unreachable(root: &Utf8Path) {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while transport::connect(root).is_ok() {
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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

/// Whether an I/O error means the peer closed the connection cleanly (the daemon
/// shut down, or the stream reached EOF), as opposed to a real protocol failure.
fn is_disconnect(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
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
    // On Windows, stop the daemon from inheriting (and holding open) the client's
    // own stdio handles. The daemon's own stdio is set to NUL above, but a plain
    // `CreateProcess` inherits *every* inheritable handle in the client — including
    // its stdout/stderr pipe write ends when the client's output is captured — so
    // the daemon would keep those pipes open for its whole life and a piped client
    // would block until the daemon's idle timeout instead of returning promptly.
    #[cfg(windows)]
    prevent_handle_inheritance();
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

/// Clears the inheritable flag on this process's standard handles so a later
/// `CreateProcess` (the detached daemon spawn) does not pass them to the child.
///
/// The stable `std::process::Command` always spawns with `bInheritHandles = TRUE`
/// and no handle-list restriction, so without this the daemon inherits the
/// client's stdio pipes. There is no safe std API to control per-handle
/// inheritance, so this calls `SetHandleInformation` directly; every step is
/// best-effort (a missing or already-non-inheritable handle is fine).
#[cfg(windows)]
#[allow(unsafe_code)]
fn prevent_handle_inheritance() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `id` is a documented standard-handle selector; `GetStdHandle`
        // returns a borrowed OS handle (or a null/invalid sentinel) and reads no
        // memory we own.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `handle` is a live standard handle owned by this process;
        // `SetHandleInformation` only flips its inherit flag. The result is
        // ignored because clearing an already-clear flag (or a non-settable
        // handle) is harmless here.
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no daemon at the endpoint, the probe must return promptly rather than
    /// spin until the timeout — a connect to an unbound endpoint refuses at once.
    #[test]
    fn wait_until_unreachable_returns_when_no_daemon() {
        let root = Utf8Path::new("/nonexistent/fai-wait-no-daemon-probe");
        let started = Instant::now();
        wait_until_unreachable(root);
        assert!(
            started.elapsed() < SHUTDOWN_TIMEOUT,
            "probe should return well before the timeout when nothing is listening"
        );
    }
}
