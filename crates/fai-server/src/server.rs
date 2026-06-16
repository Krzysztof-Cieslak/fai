//! The daemon: one warm [`Session`] serving framed requests over the endpoint.
//!
//! Connections are handled on per-connection threads. Read commands run
//! concurrently: the session lock is held only briefly — to sync inputs to disk
//! and clone a read snapshot — and the command itself runs off-lock on that
//! snapshot ([`with_fresh_snapshot`]). Snapshots are independent database handles
//! that share salsa's storage and memoization, so distinct requests execute in
//! parallel; salsa cancels any outstanding snapshot when an input is mutated, so
//! an in-flight read that a concurrent edit cancels is retried on the new
//! revision (cancel-and-retry). `run`/`test` supervision is intentionally
//! off-lock, and their long workers run after the snapshot is dropped so an edit
//! is never blocked behind them. The daemon shuts down on an explicit `Shutdown`
//! request or after an idle period, unlinking its socket on the way out.

use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use fai_driver::{
    ContractEvent, DirtyFile, EXIT_FAILURES, EXIT_INTERNAL, EXIT_OK, OutputFormat, Rendered,
    Session, TestConfig, TestPlan, WireBundle, assemble_outcome, build_test_plan,
    catch_cancellation, run_command, run_test_workers,
};
use interprocess::local_socket::Stream;
use wait_timeout::ChildExt;

use crate::protocol::{
    CommandRequest, InitResult, OutputStream, PROTOCOL_VERSION, Request, Response, RunRequest,
    ServerMessage, StatusInfo, TapDirection, TapFrame, TestRequest, frame_to_json, read_frame,
    write_frame,
};
use crate::tap::TapRegistry;
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

/// Default cap on the warm database's in-memory native-object cache (number of
/// `object_code` blobs; 0 = unbounded). Overridable via `FAI_DAEMON_OBJECT_CACHE`.
const DEFAULT_OBJECT_CACHE: usize = 1024;

/// The configured native-object cache cap (see [`DEFAULT_OBJECT_CACHE`]).
fn object_cache_capacity() -> usize {
    std::env::var("FAI_DAEMON_OBJECT_CACHE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_OBJECT_CACHE)
}

/// Shared daemon state.
struct Daemon {
    /// The warm workspace session. The mutex is held only briefly — to sync
    /// inputs to disk and clone a read snapshot — and the command itself runs
    /// off-lock on that snapshot, so distinct read requests are served
    /// concurrently. (A `RwLock` is not an option: `Session` owns a salsa
    /// database, which is `Send` but not `Sync`, so it cannot be shared as `&T`
    /// across threads; concurrency comes from per-request cloned handles, not
    /// from shared borrows.)
    session: Mutex<Session>,
    start: Instant,
    /// Epoch-ish activity clock: milliseconds since `start` of the last request.
    last_activity_ms: AtomicU64,
    socket_path: Option<PathBuf>,
    idle: Duration,
    /// Latency profiling for served `Command` requests (the compile path:
    /// check/query/fmt/build): how many, their total processing time, and the
    /// slowest single one. `run` is excluded (it is dominated by the user
    /// program's own execution in the worker, not daemon work).
    commands: AtomicU64,
    command_micros_total: AtomicU64,
    command_micros_max: AtomicU64,
    /// Read commands executing off-lock right now, and the peak ever observed.
    /// The peak is reported in `daemon status` as evidence that reads run
    /// concurrently (a peak > 1 means two requests overlapped).
    in_flight: AtomicU64,
    max_in_flight: AtomicU64,
    /// Live `tap` subscribers and the next connection id to hand out.
    taps: TapRegistry,
    conn_seq: AtomicU64,
}

impl Daemon {
    /// Records that a `Command` was processed in `elapsed`.
    fn record_command(&self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.commands.fetch_add(1, Ordering::Relaxed);
        self.command_micros_total.fetch_add(micros, Ordering::Relaxed);
        self.command_micros_max.fetch_max(micros, Ordering::Relaxed);
    }

    /// Runs an off-lock read `f`, accounting it in the concurrency gauge and
    /// catching a salsa cancellation (returns `None` so the caller retries on a
    /// fresh snapshot). The gauge is balanced even if `f` propagates a non-cancel
    /// panic (the guard decrements on unwind).
    fn run_read<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let current = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_in_flight.fetch_max(current, Ordering::Relaxed);
        let _guard = InFlightGuard(&self.in_flight);
        test_hold();
        catch_cancellation(AssertUnwindSafe(f))
    }
}

/// Decrements the in-flight gauge when an off-lock read finishes or unwinds.
struct InFlightGuard<'a>(&'a AtomicU64);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A test-only artificial delay (`FAI_DAEMON_TEST_HOLD_MS`) inserted into the
/// off-lock read region so a test can deterministically force concurrent reads
/// to overlap (and observe `max_in_flight` > 1). Unset/zero in production.
fn test_hold() {
    if let Some(ms) = std::env::var("FAI_DAEMON_TEST_HOLD_MS").ok().and_then(|v| v.parse().ok())
        && ms > 0
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Locks the session (for a sync or a snapshot), recovering from a poisoned lock.
/// Held only briefly: the actual command runs off-lock on the cloned snapshot.
fn lock_session(daemon: &Daemon) -> MutexGuard<'_, Session> {
    daemon.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Syncs the workspace to disk once (exclusive), then runs the read `f` on a
/// consistent off-lock snapshot, retrying if a concurrent edit cancels it.
///
/// A no-op sync does not bump the salsa revision, so it never cancels concurrent
/// reads; only a real input change does. Retries deliberately do **not** re-sync:
/// the edit that cancelled us is already in the shared storage, and skipping the
/// re-scan keeps a side-effecting command (`fmt`/`build` writing to disk) from
/// observing its own writes on a retry.
fn with_fresh_snapshot<T>(
    daemon: &Daemon,
    dirty: &[DirtyFile],
    f: impl Fn(&Session) -> T,
) -> Result<T, Rendered> {
    {
        let mut session = lock_session(daemon);
        if let Err(error) = session.sync_from_disk() {
            return Err(sync_error(&error.to_string()));
        }
        if !dirty.is_empty()
            && let Err(error) = session.apply_dirty(dirty)
        {
            return Err(sync_error(&error.to_string()));
        }
    }
    Ok(with_snapshot(daemon, f))
}

/// Runs the read `f` on an off-lock snapshot taken under a read lock (no sync),
/// retrying if a concurrent edit cancels it. For follow-up reads (e.g. rendering
/// a `test` report) that must reflect the current revision without re-scanning.
fn with_snapshot<T>(daemon: &Daemon, f: impl Fn(&Session) -> T) -> T {
    loop {
        let snapshot = lock_session(daemon).snapshot();
        if let Some(value) = daemon.run_read(|| f(&snapshot)) {
            return value;
        }
        // Cancelled by a concurrent edit; the snapshot drops here, then retry.
    }
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

    let mut session =
        Session::open(root.clone()).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Bound the warm database's native-object cache so the large, on-disk-backed
    // object blobs do not accumulate over a long-lived daemon (0 = unbounded).
    session.set_object_cache_capacity(object_cache_capacity());

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
        commands: AtomicU64::new(0),
        command_micros_total: AtomicU64::new(0),
        command_micros_max: AtomicU64::new(0),
        in_flight: AtomicU64::new(0),
        max_in_flight: AtomicU64::new(0),
        taps: TapRegistry::default(),
        conn_seq: AtomicU64::new(0),
    });

    spawn_idle_watchdog(Arc::clone(&daemon));

    loop {
        match transport::accept(&listener) {
            Ok(stream) => {
                let daemon = Arc::clone(&daemon);
                let id = daemon.conn_seq.fetch_add(1, Ordering::Relaxed);
                std::thread::spawn(move || handle_connection(stream, &daemon, id));
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

/// One connection: the framed stream plus the context needed to mirror every
/// frame to `tap` subscribers. All reads and writes on a served connection go
/// through [`Conn::read`]/[`Conn::send`], so the tap feed sees the complete
/// traffic without each call site remembering to broadcast.
struct Conn<'a> {
    stream: Stream,
    daemon: &'a Daemon,
    /// This connection's id, stamped onto every tapped frame.
    id: u64,
}

impl<'a> Conn<'a> {
    fn new(stream: Stream, daemon: &'a Daemon, id: u64) -> Self {
        Self { stream, daemon, id }
    }

    /// Reads one request, mirroring it to any tap subscribers as inbound.
    fn read(&mut self) -> std::io::Result<Request> {
        let request: Request = read_frame(&mut self.stream)?;
        self.broadcast(TapDirection::Inbound, &request);
        Ok(request)
    }

    /// Writes one server message, mirroring it to any tap subscribers as
    /// outbound (before the write, so a tap sees it even if the client has gone).
    fn send(&mut self, message: &ServerMessage) -> std::io::Result<()> {
        self.broadcast(TapDirection::Outbound, message);
        write_frame(&mut self.stream, message)
    }

    /// Offers a frame to tap subscribers. Decoding to JSON is skipped entirely
    /// when no tap is attached (the common case), so the served path pays only a
    /// relaxed atomic load.
    fn broadcast<T: serde::Serialize>(&self, direction: TapDirection, message: &T) {
        if self.daemon.taps.is_empty() {
            return;
        }
        let frame = TapFrame { conn: self.id, direction, json: frame_to_json(message) };
        self.daemon.taps.broadcast(&frame);
    }
}

/// Serves requests on one connection until it closes (or a shutdown is
/// requested, which exits the process).
fn handle_connection(stream: Stream, daemon: &Daemon, id: u64) {
    let mut conn = Conn::new(stream, daemon, id);
    loop {
        let request = match conn.read() {
            Ok(request) => request,
            // EOF or a malformed frame ends the connection.
            Err(_) => return,
        };
        daemon.touch();

        // `run` streams `$/output` frames before its terminal result.
        if let Request::Run(request) = request {
            if handle_run(&mut conn, &request).is_err() {
                return;
            }
            continue;
        }

        // `test` streams `$/testEvent` frames before its terminal result.
        if let Request::Test(request) = request {
            if handle_test(&mut conn, &request).is_err() {
                return;
            }
            continue;
        }

        // `tap` turns this connection into a passive subscriber and never returns
        // to the request loop.
        if matches!(request, Request::Tap) {
            subscribe_and_stream(&mut conn);
            return;
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
            Request::Command(command) => {
                let started = Instant::now();
                let rendered = run(daemon, command);
                daemon.record_command(started.elapsed());
                Response::Command(rendered)
            }
            Request::Status => Response::Status(StatusInfo {
                pid: std::process::id(),
                compiler_version: VERSION.to_owned(),
                protocol_version: PROTOCOL_VERSION,
                uptime_secs: daemon.start.elapsed().as_secs(),
                commands_served: daemon.commands.load(Ordering::Relaxed),
                command_micros_total: daemon.command_micros_total.load(Ordering::Relaxed),
                command_micros_max: daemon.command_micros_max.load(Ordering::Relaxed),
                max_concurrency: daemon.max_in_flight.load(Ordering::Relaxed),
            }),
            Request::Run(_) | Request::Test(_) | Request::Tap => {
                unreachable!("handled above")
            }
            Request::Shutdown => {
                let _ = conn.send(&ServerMessage::Result(Response::Ok));
                shutdown(daemon);
            }
            Request::Exit => return,
        };

        if conn.send(&ServerMessage::Result(response)).is_err() {
            return;
        }
    }
}

/// Subscribes this connection to the tap feed and streams decoded frames until
/// the client disconnects.
///
/// The subscription is registered, then acknowledged with [`Response::Ok`]
/// *before* streaming begins, so a client that waits for the ack is guaranteed
/// to observe every frame produced after it — there is no window where traffic
/// slips past a not-yet-registered tap. Tap frames are written directly (not via
/// [`Conn::send`]) so the feed never echoes itself.
///
/// An idle tap whose client has vanished is reaped on the next broadcast (its
/// send fails) or when the daemon shuts down.
fn subscribe_and_stream(conn: &mut Conn) {
    let frames = conn.daemon.taps.subscribe();
    if write_frame(&mut conn.stream, &ServerMessage::Result(Response::Ok)).is_err() {
        return;
    }
    for frame in frames {
        if write_frame(&mut conn.stream, &ServerMessage::TapFrame(frame)).is_err() {
            return;
        }
    }
}

/// Syncs the workspace, applies any dirty-set, and runs a command off-lock on a
/// read snapshot (so concurrent commands run in parallel), retrying if a
/// concurrent edit cancels it.
fn run(daemon: &Daemon, command: CommandRequest) -> Rendered {
    let CommandRequest { spec, opts, dirty } = command;
    with_fresh_snapshot(daemon, &dirty, |snapshot| run_command(snapshot, &spec, opts))
        .unwrap_or_else(|rendered| rendered)
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
fn handle_run(conn: &mut Conn, request: &RunRequest) -> std::io::Result<()> {
    let bundle = match prepare_run(conn.daemon, request) {
        Prepared::Bundle(bundle) => bundle,
        Prepared::Failed(message) => {
            if !message.is_empty() {
                conn.send(&ServerMessage::Output {
                    stream: OutputStream::Stderr,
                    chunk: message.into_bytes(),
                })?;
            }
            return conn.send(&ServerMessage::Result(Response::RunExit(COMPILE_ERROR_EXIT)));
        }
    };

    let bundle_path = match write_bundle(&bundle) {
        Ok(path) => path,
        Err(message) => {
            conn.send(&ServerMessage::Output {
                stream: OutputStream::Stderr,
                chunk: format!("error: {message}\n").into_bytes(),
            })?;
            return conn
                .send(&ServerMessage::Result(Response::RunExit(fai_driver::EXIT_WORKSPACE)));
        }
    };

    let exit = supervise(conn, &bundle_path)?;
    let _ = std::fs::remove_file(&bundle_path);
    conn.send(&ServerMessage::Result(Response::RunExit(exit)))
}

/// The result of preparing a run: a ready bundle, or rendered failure text.
enum Prepared {
    Bundle(WireBundle),
    Failed(String),
}

/// Builds the run bundle on a warm off-lock snapshot (front end), rendering any
/// diagnostics server-side. The snapshot is dropped before the worker runs, so a
/// concurrent edit is never blocked behind the supervised program.
fn prepare_run(daemon: &Daemon, request: &RunRequest) -> Prepared {
    let prepared = with_fresh_snapshot(daemon, &request.dirty, |snapshot| {
        let files = snapshot.select_files(Some(Utf8Path::new(&request.path)));
        let Some(entry) = files.first().copied() else {
            return Prepared::Failed(format!(
                "error: no such file in workspace: {}\n",
                request.path
            ));
        };
        // Native dependencies for user `foreign` functions (from `fai.toml`).
        let native = match fai_driver::read_native_manifest(snapshot.root()) {
            Ok(deps) => deps,
            Err(message) => return Prepared::Failed(format!("error: {message}\n")),
        };
        let result = fai_driver::build_run_bundle_with_deps(snapshot.db(), entry, &native);
        match result.bundle {
            Some(bundle) => Prepared::Bundle(bundle),
            None => {
                let resolver = snapshot.resolver();
                Prepared::Failed(fai_driver::render_diagnostics(&result.diagnostics, &resolver))
            }
        }
    });
    // A sync failure renders as plain `error: …` text (matching the prior path).
    prepared.unwrap_or_else(|rendered| Prepared::Failed(rendered.stderr))
}

/// Handles a `test` request: build the plan warm (on a snapshot), then supervise
/// the isolated worker(s) off-lock, streaming each contract's result as a
/// `$/testEvent`, and finally render the report (on a fresh snapshot) as the
/// terminal `Test` result. The worker execution — the long part — runs off-lock
/// (and after the snapshot is dropped) so the daemon stays responsive and a
/// concurrent edit is never blocked; a crashing contract is a separate process,
/// so the daemon always survives.
fn handle_test(conn: &mut Conn, request: &TestRequest) -> std::io::Result<()> {
    let plan = match prepare_test(conn.daemon, request) {
        Ok(plan) => plan,
        Err(rendered) => {
            return conn.send(&ServerMessage::Result(Response::Test(rendered)));
        }
    };

    let results = if plan.blocked || plan.bundle.contracts.is_empty() {
        Vec::new()
    } else {
        let mut send_err: std::io::Result<()> = Ok(());
        let results = {
            let mut on_event = |event: &ContractEvent| {
                if send_err.is_ok() {
                    send_err = conn.send(&ServerMessage::TestEvent(event.clone()));
                }
            };
            run_test_workers(&plan, &mut on_event)
        };
        send_err?;
        results
    };

    let rendered = render_test(conn.daemon, request, &plan, &results);
    conn.send(&ServerMessage::Result(Response::Test(rendered)))
}

/// Builds the test plan on a warm off-lock snapshot (front end). The snapshot is
/// dropped before the worker(s) run, so a concurrent edit is never blocked behind
/// the (potentially long) test execution.
fn prepare_test(daemon: &Daemon, request: &TestRequest) -> Result<TestPlan, Rendered> {
    with_fresh_snapshot(daemon, &request.dirty, |snapshot| {
        let files = snapshot.select_files(request.path.as_deref().map(Utf8Path::new));
        let defaults = TestConfig::default();
        let config = TestConfig {
            seed: request.seed.unwrap_or(defaults.seed),
            trials: request.count.unwrap_or(defaults.trials),
            max_size: request.max_size.unwrap_or(defaults.max_size),
        };
        build_test_plan(snapshot.db(), &files, request.r#match.as_deref(), config)
    })
}

/// Renders the assembled outcome to the terminal `Rendered`, resolving spans on a
/// fresh off-lock snapshot (no re-sync), using the same code path as the
/// in-process CLI so warm output is byte-identical to `--no-daemon`.
fn render_test(
    daemon: &Daemon,
    request: &TestRequest,
    plan: &TestPlan,
    results: &[fai_driver::ContractResult],
) -> Rendered {
    let outcome = assemble_outcome(plan, results);
    let exit = if outcome.ok { EXIT_OK } else { EXIT_FAILURES };
    with_snapshot(daemon, |snapshot| {
        let resolver = snapshot.resolver();
        match request.opts.format {
            OutputFormat::Json => {
                match serde_json::to_string_pretty(&outcome.to_output(&resolver)) {
                    Ok(json) => {
                        Rendered { stdout: format!("{json}\n"), stderr: String::new(), exit }
                    }
                    Err(error) => Rendered {
                        stdout: String::new(),
                        stderr: format!("internal error: failed to serialize output: {error}\n"),
                        exit: EXIT_INTERNAL,
                    },
                }
            }
            OutputFormat::Human => Rendered {
                stdout: outcome.render_human(&resolver, request.opts.color),
                stderr: String::new(),
                exit,
            },
        }
    })
}

/// Spawns and supervises the worker, streaming its stdout/stderr as `$/output`
/// and enforcing the wall-clock timeout. Returns the program's exit code.
fn supervise(conn: &mut Conn, bundle_path: &Path) -> std::io::Result<i32> {
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
        conn.send(&ServerMessage::Output { stream: which, chunk })?;
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
