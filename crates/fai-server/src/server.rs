//! The daemon: one warm [`Session`] serving framed requests over the endpoint.
//!
//! Connections are handled on per-connection threads, but all database access is
//! serialized through a single mutex (true serialization; concurrent reads and
//! cancellation are future work). `run` supervision is intentionally off-lock.
//! The daemon shuts down on an explicit `Shutdown` request or after an idle
//! period, unlinking its socket on the way out.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use fai_driver::{Rendered, Session, WireBundle, run_command};
use interprocess::local_socket::Stream;
use wait_timeout::ChildExt;

use crate::protocol::{
    CommandRequest, InitResult, OutputStream, PROTOCOL_VERSION, Request, Response, RunRequest,
    ServerMessage, StatusInfo, read_frame, write_frame,
};
use crate::transport::{self, BindError};

/// Exit code when a supervised run exceeds its time limit.
const TIMEOUT_EXIT: i32 = 124;
/// Exit code when a supervised run terminates abnormally (e.g. a signal).
const CRASH_EXIT: i32 = 134;
/// Exit code when a program fails to compile.
const COMPILE_ERROR_EXIT: i32 = 4;
/// Default wall-clock limit for a supervised run.
const DEFAULT_RUN_TIMEOUT_MS: u64 = 300_000;

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

        // `run` streams `$/output` frames before its terminal result.
        if let Request::Run(request) = request {
            if handle_run(&mut stream, daemon, &request).is_err() {
                return;
            }
            continue;
        }

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
            Request::Run(_) => unreachable!("handled above"),
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

/// Handles a `run` request: build the bundle warm, then supervise an isolated
/// worker, streaming its output and enforcing a timeout. Writes its own
/// `$/output` frames and terminal `RunExit`.
fn handle_run(stream: &mut Stream, daemon: &Daemon, request: &RunRequest) -> std::io::Result<()> {
    let bundle = match prepare_run(daemon, request) {
        Prepared::Bundle(bundle) => bundle,
        Prepared::Failed(message) => {
            if !message.is_empty() {
                write_frame(
                    stream,
                    &ServerMessage::Output {
                        stream: OutputStream::Stderr,
                        chunk: message.into_bytes(),
                    },
                )?;
            }
            return write_frame(
                stream,
                &ServerMessage::Result(Response::RunExit(COMPILE_ERROR_EXIT)),
            );
        }
    };

    let bundle_path = match write_bundle(&bundle) {
        Ok(path) => path,
        Err(message) => {
            write_frame(
                stream,
                &ServerMessage::Output {
                    stream: OutputStream::Stderr,
                    chunk: format!("error: {message}\n").into_bytes(),
                },
            )?;
            return write_frame(
                stream,
                &ServerMessage::Result(Response::RunExit(fai_driver::EXIT_WORKSPACE)),
            );
        }
    };

    let exit = supervise(stream, &bundle_path)?;
    let _ = std::fs::remove_file(&bundle_path);
    write_frame(stream, &ServerMessage::Result(Response::RunExit(exit)))
}

/// The result of preparing a run: a ready bundle, or rendered failure text.
enum Prepared {
    Bundle(WireBundle),
    Failed(String),
}

/// Builds the run bundle under the session lock (warm front end), rendering any
/// diagnostics server-side. The lock is released before the worker runs.
fn prepare_run(daemon: &Daemon, request: &RunRequest) -> Prepared {
    let mut session = match daemon.session.lock() {
        Ok(session) => session,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Err(error) = session.sync_from_disk() {
        return Prepared::Failed(format!("error: {error}\n"));
    }
    if !request.dirty.is_empty()
        && let Err(error) = session.apply_dirty(&request.dirty)
    {
        return Prepared::Failed(format!("error: {error}\n"));
    }
    let files = session.select_files(Some(Utf8Path::new(&request.path)));
    let Some(entry) = files.first().copied() else {
        return Prepared::Failed(format!("error: no such file in workspace: {}\n", request.path));
    };
    let result = fai_driver::build_run_bundle(session.db(), entry);
    match result.bundle {
        Some(bundle) => Prepared::Bundle(bundle),
        None => {
            let resolver = session.resolver();
            Prepared::Failed(fai_driver::render_diagnostics(&result.diagnostics, &resolver))
        }
    }
}

/// Spawns and supervises the worker, streaming its stdout/stderr as `$/output`
/// and enforcing the wall-clock timeout. Returns the program's exit code.
fn supervise(stream: &mut Stream, bundle_path: &Path) -> std::io::Result<i32> {
    let timeout = run_timeout();
    let cpu_secs = timeout.as_secs().max(1);

    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg("__run-worker")
        .arg(bundle_path)
        .env("FAI_RUN_CPU_SECS", cpu_secs.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let child_stdout = child.stdout.take().expect("piped stdout");
    let child_stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<(OutputStream, Vec<u8>)>();
    let reader_out = spawn_reader(child_stdout, OutputStream::Stdout, tx.clone());
    let reader_err = spawn_reader(child_stderr, OutputStream::Stderr, tx);

    // Enforce the timeout off the streaming path so a silent hang is still reaped.
    let (code_tx, code_rx) = mpsc::channel::<i32>();
    std::thread::spawn(move || {
        let code = match child.wait_timeout(timeout) {
            Ok(Some(status)) => status.code().unwrap_or(CRASH_EXIT),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                TIMEOUT_EXIT
            }
            Err(_) => CRASH_EXIT,
        };
        let _ = code_tx.send(code);
    });

    // Forward output until both pipes reach EOF (the child has exited or was
    // killed). A write failure means the client disconnected.
    for (which, chunk) in rx {
        write_frame(stream, &ServerMessage::Output { stream: which, chunk })?;
    }
    let _ = reader_out.join();
    let _ = reader_err.join();
    Ok(code_rx.recv().unwrap_or(CRASH_EXIT))
}

/// Reads `reader` to EOF, forwarding chunks tagged with `which`.
fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    which: OutputStream,
    tx: Sender<(OutputStream, Vec<u8>)>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((which, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// The supervised-run wall-clock limit (`FAI_RUN_TIMEOUT_MS`, default 300s).
fn run_timeout() -> Duration {
    let ms = std::env::var("FAI_RUN_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_RUN_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// Serializes a run bundle to a unique temp file (JSON), returning its path.
fn write_bundle(bundle: &WireBundle) -> Result<PathBuf, String> {
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
