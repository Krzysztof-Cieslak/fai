//! The daemon: one warm [`Session`] serving framed requests over the endpoint.
//!
//! Connections are handled on per-connection threads, but all database access is
//! serialized through a single mutex (true serialization; concurrent reads and
//! cancellation are future work). `run` supervision is intentionally off-lock.
//! The daemon shuts down on an explicit `Shutdown` request or after an idle
//! period, unlinking its socket on the way out.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use fai_driver::{Rendered, Session, run_command};
use interprocess::local_socket::Stream;

use crate::protocol::{
    CommandRequest, InitResult, PROTOCOL_VERSION, Request, Response, ServerMessage, StatusInfo,
    read_frame, write_frame,
};
use crate::transport::{self, BindError};

/// The compiler version this daemon serves.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default idle lifetime before the daemon shuts itself down.
const DEFAULT_IDLE_SECS: u64 = 600;

/// Shared daemon state.
struct Daemon {
    session: Mutex<Session>,
    start: Instant,
    /// Epoch-ish activity clock: milliseconds since `start` of the last request.
    last_activity_ms: AtomicU64,
    socket_path: Option<PathBuf>,
    idle: Duration,
}

impl Daemon {
    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_activity_ms.store(ms, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        self.start.elapsed().saturating_sub(Duration::from_millis(last))
    }
}

/// Runs the daemon for `root`. Returns when the listener cannot be created or the
/// workspace cannot be opened; otherwise it serves until shutdown (which exits the
/// process). A lost spawn race (another daemon already bound) returns `Ok(())`.
pub fn serve(root: Utf8PathBuf) -> std::io::Result<()> {
    detach_from_terminal();

    let listener = match transport::bind(&root) {
        Ok(listener) => listener,
        Err(BindError::AlreadyRunning) => return Ok(()),
        Err(BindError::Io(error)) => return Err(error),
    };

    let session = Session::open(root.clone()).map_err(|e| std::io::Error::other(e.to_string()))?;

    let idle = Duration::from_secs(
        std::env::var("FAI_DAEMON_IDLE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_IDLE_SECS),
    );

    let daemon = Arc::new(Daemon {
        session: Mutex::new(session),
        start: Instant::now(),
        last_activity_ms: AtomicU64::new(0),
        socket_path: transport::socket_path(&root),
        idle,
    });

    spawn_idle_watchdog(Arc::clone(&daemon));

    loop {
        match transport::accept(&listener) {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                std::thread::spawn(move || handle_connection(stream, &daemon));
            }
            // A failed accept is transient; keep serving.
            Err(_) => continue,
        }
    }
}

/// Detaches from the controlling terminal so a terminal hangup can't kill the
/// daemon (Unix). A no-op elsewhere; the client also spawns us detached.
fn detach_from_terminal() {
    #[cfg(unix)]
    {
        // Already-a-session-leader is fine; ignore the error.
        let _ = nix::unistd::setsid();
    }
}

/// Periodically shuts the daemon down once it has been idle past its limit.
fn spawn_idle_watchdog(daemon: Arc<Daemon>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if daemon.idle_for() >= daemon.idle {
                shutdown(&daemon);
            }
        }
    });
}

/// Unlinks the socket and exits the process.
fn shutdown(daemon: &Daemon) -> ! {
    if let Some(path) = &daemon.socket_path {
        let _ = std::fs::remove_file(path);
    }
    std::process::exit(0);
}

/// Serves requests on one connection until it closes (or a shutdown is
/// requested, which exits the process).
fn handle_connection(mut stream: Stream, daemon: &Daemon) {
    loop {
        let request: Request = match read_frame(&mut stream) {
            Ok(request) => request,
            // EOF or a malformed frame ends the connection.
            Err(_) => return,
        };
        daemon.touch();

        let response = match request {
            Request::Initialize(params) => {
                if params.protocol_version == PROTOCOL_VERSION && params.compiler_version == VERSION
                {
                    Response::Initialized(InitResult {
                        protocol_version: PROTOCOL_VERSION,
                        compiler_version: VERSION.to_owned(),
                    })
                } else {
                    Response::Error(format!(
                        "version mismatch: daemon {VERSION}/{PROTOCOL_VERSION}, client {}/{}",
                        params.compiler_version, params.protocol_version
                    ))
                }
            }
            Request::Command(command) => Response::Command(run(daemon, command)),
            Request::Status => Response::Status(StatusInfo {
                pid: std::process::id(),
                compiler_version: VERSION.to_owned(),
                protocol_version: PROTOCOL_VERSION,
                uptime_secs: daemon.start.elapsed().as_secs(),
            }),
            Request::Run(_) => {
                Response::Error("`run` is not served by the daemon in this build".to_owned())
            }
            Request::Shutdown => {
                let _ = write_frame(&mut stream, &ServerMessage::Result(Response::Ok));
                shutdown(daemon);
            }
            Request::Exit => return,
        };

        if write_frame(&mut stream, &ServerMessage::Result(response)).is_err() {
            return;
        }
    }
}

/// Syncs the workspace, applies any dirty-set, and runs a command under the lock.
fn run(daemon: &Daemon, command: CommandRequest) -> Rendered {
    let CommandRequest { spec, opts, dirty } = command;
    let mut session = match daemon.session.lock() {
        Ok(session) => session,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Err(error) = session.sync_from_disk() {
        return sync_error(&error.to_string());
    }
    if !dirty.is_empty()
        && let Err(error) = session.apply_dirty(&dirty)
    {
        return sync_error(&error.to_string());
    }
    run_command(&session, &spec, opts)
}

/// A rendered workspace/IO error (plain text on stderr, exit 3).
fn sync_error(message: &str) -> Rendered {
    Rendered {
        stdout: String::new(),
        stderr: format!("error: {message}\n"),
        exit: fai_driver::EXIT_WORKSPACE,
    }
}
