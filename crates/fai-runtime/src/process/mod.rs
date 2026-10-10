//! Scoped process supervision with bounded, cancellable, full-duplex pipe I/O.

#[cfg(unix)]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
mod wire;
pub use wire::*;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Spec {
    executable: String,
    arguments: Vec<String>,
    directory: String,
    inherit: bool,
    edits: Vec<(String, Option<String>)>,
}

impl Spec {
    fn environment(&self) -> Vec<(OsString, OsString)> {
        let mut values: Vec<_> =
            if self.inherit { std::env::vars_os().collect() } else { Vec::new() };
        for (key, value) in &self.edits {
            values.retain(|(name, _)| !env_equal(name, std::ffi::OsStr::new(key)));
            if let Some(value) = value {
                values.push((key.into(), value.into()));
            }
        }
        values.sort_by(|a, b| a.0.cmp(&b.0));
        values
    }
    fn command(&self) -> io::Result<std::process::Command> {
        let cwd = if self.directory.is_empty() {
            std::env::current_dir()?
        } else {
            std::env::current_dir()?.join(&self.directory)
        };
        let env = self.environment();
        let executable = Path::new(&self.executable);
        let executable = if executable.is_absolute() {
            executable.to_path_buf()
        } else if executable.components().count() > 1 {
            cwd.join(executable)
        } else {
            let path = env
                .iter()
                .find(|(key, _)| env_equal(key, std::ffi::OsStr::new("PATH")))
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            std::env::split_paths(&path)
                .find_map(|dir| {
                    let path = cwd.join(dir).join(executable);
                    executable_file(path)
                })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "executable not found in effective PATH",
                    )
                })?
        };
        #[cfg(windows)]
        if executable
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch files require an explicit command interpreter",
            ));
        }
        let mut command = std::process::Command::new(executable);
        command.args(&self.arguments).current_dir(cwd).env_clear().envs(env);
        Ok(command)
    }
    fn validate(&self) -> bool {
        !self.executable.is_empty()
            && !self.executable.contains('\0')
            && !self.directory.contains('\0')
            && self.arguments.iter().all(|s| !s.contains('\0'))
            && self.edits.iter().all(|(k, v)| {
                !k.is_empty()
                    && !k.contains(['\0', '='])
                    && v.as_ref().is_none_or(|v| !v.contains('\0'))
            })
    }
}

fn executable_file(path: PathBuf) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .ok()
            .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .map(|_| path)
    }
    #[cfg(windows)]
    {
        if path.is_file() {
            Some(path)
        } else {
            let executable = path.with_extension("exe");
            executable.is_file().then_some(executable)
        }
    }
}

fn env_equal(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    #[cfg(unix)]
    {
        a == b
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let a: Vec<_> = a.encode_wide().collect();
        let b: Vec<_> = b.encode_wide().collect();
        // SAFETY: both UTF-16 buffers remain valid for their explicit lengths.
        unsafe {
            windows_sys::Win32::Globalization::CompareStringOrdinal(
                a.as_ptr(),
                a.len() as i32,
                b.as_ptr(),
                b.len() as i32,
                1,
            ) == 2
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Exit {
    Code(i64),
    #[cfg_attr(windows, allow(dead_code))]
    Signal(i64, bool),
}

#[derive(Clone, Debug)]
struct Error {
    kind: i64,
    code: i64,
    stage: i64,
    message: String,
}

fn error(kind: i64, stage: usize, message: impl Into<String>) -> Error {
    Error { kind, code: 0, stage: stage as i64, message: message.into() }
}
fn os_error(kind: i64, stage: usize, err: io::Error) -> Error {
    Error {
        kind,
        code: i64::from(err.raw_os_error().unwrap_or(0)),
        stage: stage as i64,
        message: err.to_string(),
    }
}

#[derive(Clone)]
struct Options {
    buffer: usize,
    chunk: usize,
    timeout: Option<Duration>,
    drain: Option<Duration>,
    grace: Duration,
    poll: Duration,
}

#[derive(Debug)]
struct Output {
    stage: usize,
    stderr: bool,
    bytes: Vec<u8>,
}
struct Input {
    bytes: Vec<u8>,
    offset: usize,
}
struct State {
    ready: bool,
    closed: bool,
    stop: bool,
    force: bool,
    done: bool,
    input_closed: bool,
    input: Option<Input>,
    completed_writes: u64,
    output: VecDeque<Output>,
    buffered: usize,
    failure: Option<Error>,
    cleanup: Vec<Error>,
    exits: Vec<Option<Exit>>,
    waiters: Vec<(Arc<()>, crate::scheduler::Parked)>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    wake: mio::Waker,
}
struct Session {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    chunk: usize,
    reader: Mutex<()>,
    writer: Mutex<()>,
    started: Instant,
    timeout: Option<Duration>,
}

static ACTIVE: OnceLock<Mutex<Vec<Weak<Session>>>> = OnceLock::new();

/// Stop native child groups before a reported runtime fault exits the parent.
pub(crate) fn restore_on_fault() {
    let sessions: Vec<_> = ACTIVE
        .get()
        .and_then(|list| list.try_lock().ok())
        .map(|list| list.iter().filter_map(Weak::upgrade).collect())
        .unwrap_or_default();
    for session in sessions {
        lock(&session.shared.state).force = true;
        session.close();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn notify(&self, state: &mut State) {
        self.changed.notify_all();
        for (_, task) in state.waiters.drain(..) {
            crate::scheduler::unpark(task);
        }
    }
    fn wait<'a>(&'a self, mut state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        if crate::scheduler::in_task() {
            let id = Arc::new(());
            state.waiters.push((id.clone(), crate::scheduler::current_parked()));
            drop(state);
            crate::scheduler::park();
            let mut state = lock(&self.state);
            state.waiters.retain(|(other, _)| !Arc::ptr_eq(&id, other));
            state
        } else {
            self.changed.wait(state).unwrap_or_else(|e| e.into_inner())
        }
    }
}

impl Session {
    fn start(specs: Vec<Spec>, options: Options) -> Result<Arc<Self>, Error> {
        if crate::scheduler::is_cancelled() {
            return Err(error(5, 0, "process startup cancelled"));
        }
        if specs.is_empty() || specs.len() > 1024 || specs.iter().any(|s| !s.validate()) {
            return Err(error(1, 0, "invalid command or pipeline"));
        }
        let poll = mio::Poll::new().map_err(|e| os_error(2, 0, e))?;
        let wake =
            mio::Waker::new(poll.registry(), mio::Token(0)).map_err(|e| os_error(2, 0, e))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                ready: false,
                closed: false,
                stop: false,
                force: false,
                done: false,
                input_closed: false,
                input: None,
                completed_writes: 0,
                output: VecDeque::new(),
                buffered: 0,
                failure: None,
                cleanup: Vec::new(),
                exits: vec![None; specs.len()],
                waiters: Vec::new(),
            }),
            changed: Condvar::new(),
            wake,
        });
        let worker = shared.clone();
        let chunk = options.chunk;
        let timeout = options.timeout;
        let started = Instant::now();
        let thread = std::thread::Builder::new()
            .name("fai-process".into())
            .spawn(move || supervise(worker, poll, specs, options))
            .map_err(|e| os_error(2, 0, e))?;
        let session = Arc::new(Self {
            shared,
            thread: Mutex::new(Some(thread)),
            chunk,
            reader: Mutex::new(()),
            writer: Mutex::new(()),
            started,
            timeout,
        });
        {
            let mut active = lock(ACTIVE.get_or_init(Mutex::default));
            active.retain(|session| session.strong_count() > 0);
            active.push(Arc::downgrade(&session));
        }
        let mut state = lock(&session.shared.state);
        while !state.ready && !state.done {
            if crate::scheduler::is_cancelled() {
                state.stop = true;
                state.force = true;
                let _ = session.shared.wake.wake();
                drop(state);
                session.close();
                return Err(error(5, 0, "process startup cancelled"));
            }
            state = session.shared.wait(state);
        }
        let failure = state.failure.clone();
        drop(state);
        if let Some(failure) = failure {
            session.close();
            Err(failure)
        } else {
            Ok(session)
        }
    }
    fn next(&self) -> Result<Option<Output>, Error> {
        let _reader =
            self.reader.try_lock().map_err(|_| error(9, 0, "another output read is active"))?;
        let mut state = lock(&self.shared.state);
        loop {
            self.check_deadline(&mut state);
            if state.closed {
                return Err(error(8, 0, "process session is closed"));
            }
            if crate::scheduler::is_cancelled() {
                return Err(error(5, 0, "process read cancelled"));
            }
            if let Some(output) = state.output.pop_front() {
                state.buffered -= output.bytes.len();
                let _ = self.shared.wake.wake();
                return Ok(Some(output));
            }
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
            if state.done {
                return Ok(None);
            }
            state = self.shared.wait(state);
        }
    }
    fn write(&self, bytes: &[u8]) -> Result<usize, Error> {
        let _writer =
            self.writer.try_lock().map_err(|_| error(9, 0, "another input write is active"))?;
        let mut state = lock(&self.shared.state);
        if state.input.is_some() {
            return Err(error(9, 0, "another input write is active"));
        }
        if state.closed || state.input_closed || state.done {
            return Err(error(8, 0, "process input is closed"));
        }
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        if crate::scheduler::is_cancelled() {
            return Err(error(5, 0, "process write cancelled"));
        }
        let n = bytes.len().min(self.chunk);
        if n == 0 {
            return Ok(0);
        }
        state.input = Some(Input { bytes: bytes[..n].to_vec(), offset: 0 });
        let completed = state.completed_writes;
        let _ = self.shared.wake.wake();
        while state.input.is_some() && !state.done && !state.closed {
            if crate::scheduler::is_cancelled() {
                state.input_closed = true;
                state.input.take();
                let _ = self.shared.wake.wake();
                return Err(error(5, 0, "process write cancelled"));
            }
            state = self.shared.wait(state);
        }
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        if state.closed {
            return Err(error(8, 0, "process session is closed"));
        }
        if state.completed_writes == completed {
            return Err(error(4, 0, "process input closed before the write completed"));
        }
        Ok(n)
    }
    fn end_input(&self) -> Result<(), Error> {
        let mut state = lock(&self.shared.state);
        if state.closed {
            return Err(error(8, 0, "process session is closed"));
        }
        state.input_closed = true;
        let _ = self.shared.wake.wake();
        Ok(())
    }
    fn stop(&self) {
        let mut state = lock(&self.shared.state);
        state.stop = true;
        state.input_closed = true;
        let _ = self.shared.wake.wake();
        self.shared.notify(&mut state);
    }
    fn close(&self) {
        crate::scheduler::cleanup(|| self.close_inner());
    }
    fn close_inner(&self) {
        self.check_deadline(&mut lock(&self.shared.state));
        self.stop();
        let mut state = lock(&self.shared.state);
        state.closed = true;
        state.output.clear();
        state.buffered = 0;
        self.shared.notify(&mut state);
        while !state.done {
            // Closing masks caller cancellation: the native supervisor has its own
            // bounded grace and continues until every direct child is reaped.
            state = self.shared.wait(state);
        }
        drop(state);
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
    fn check_deadline(&self, state: &mut State) {
        if !state.stop && self.timeout.is_some_and(|duration| self.started.elapsed() >= duration) {
            state.failure.get_or_insert_with(|| error(6, 0, "process deadline exceeded"));
            state.stop = true;
            let _ = self.shared.wake.wake();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        lock(&self.shared.state).force = true;
        self.close();
    }
}

struct Reader {
    pipe: platform::Pipe,
    stage: usize,
    stderr: bool,
}

fn setup(
    specs: &[Spec],
    poll: &mio::Poll,
    children: &mut Vec<(usize, platform::Child)>,
    shared: &Shared,
) -> Result<(platform::Pipe, Vec<Reader>), Error> {
    let (mut input, child_input) = platform::endpoint(false).map_err(|e| os_error(2, 0, e))?;
    input.register(poll.registry(), 1, false).map_err(|e| os_error(2, 0, e))?;
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    inputs.push(Some(child_input));
    for stage in 0..specs.len() - 1 {
        let (read, write) = platform::pipe().map_err(|e| os_error(2, stage, e))?;
        outputs.push(Some(write));
        inputs.push(Some(read));
    }
    let (mut output, child_output) =
        platform::endpoint(true).map_err(|e| os_error(2, specs.len() - 1, e))?;
    output.register(poll.registry(), 2, true).map_err(|e| os_error(2, 0, e))?;
    outputs.push(Some(child_output));
    let mut readers = vec![Reader { pipe: output, stage: specs.len() - 1, stderr: false }];
    for stage in (0..specs.len()).rev() {
        if lock(&shared.state).stop {
            return Err(error(5, stage, "process startup cancelled"));
        }
        let (mut stderr, child_error) =
            platform::endpoint(true).map_err(|e| os_error(2, stage, e))?;
        stderr.register(poll.registry(), stage + 3, true).map_err(|e| os_error(2, stage, e))?;
        let child = platform::spawn(
            &specs[stage],
            inputs[stage].take().expect("pipeline input"),
            outputs[stage].take().expect("pipeline output"),
            child_error,
        )
        .map_err(|e| os_error(2, stage, e))?;
        children.push((stage, child));
        readers.push(Reader { pipe: stderr, stage, stderr: true });
    }
    Ok((input, readers))
}

fn supervise(shared: Arc<Shared>, mut poll: mio::Poll, specs: Vec<Spec>, options: Options) {
    let started = Instant::now();
    let mut children = Vec::new();
    let setup = setup(&specs, &poll, &mut children, &shared);
    let (mut input, mut readers) = match setup {
        Ok((input, readers)) => (Some(input), readers),
        Err(e) => {
            let mut state = lock(&shared.state);
            state.failure = Some(e);
            state.force = true;
            state.stop = true;
            (None, Vec::new())
        }
    };
    let mut events = mio::Events::with_capacity(specs.len() + 4);
    let mut stopping = None;
    let mut exited = None;
    let mut killed = false;
    let mut cursor = 0;
    {
        let mut state = lock(&shared.state);
        state.ready = true;
        shared.notify(&mut state);
    }
    loop {
        let mut progressed = false;
        let mut state = lock(&shared.state);
        if !state.stop
            && options.timeout.is_some_and(|t| started.elapsed() >= t)
            && state.failure.is_none()
        {
            state.failure = Some(error(6, 0, "process deadline exceeded"));
            state.stop = true;
        }
        if state.stop || state.failure.is_some() {
            stopping.get_or_insert_with(Instant::now);
            input.take();
            state.input.take();
        }
        if stopping.is_some_and(|t| t.elapsed() >= options.grace) || state.force {
            if !killed {
                for (stage, child) in &children {
                    if let Err(e) = child.kill() {
                        state.cleanup.push(os_error(10, *stage, e));
                    }
                }
                killed = true;
            }
            readers.clear();
        }
        for (stage, child) in &mut children {
            if state.exits[*stage].is_none() {
                match child.status() {
                    Ok(status) => state.exits[*stage] = status,
                    Err(e) => {
                        state.failure.get_or_insert_with(|| os_error(3, *stage, e));
                        state.stop = true;
                    }
                }
            }
        }
        let all_exited = children.iter().all(|(stage, _)| state.exits[*stage].is_some());
        if all_exited {
            exited.get_or_insert_with(Instant::now);
            input.take();
            state.input.take();
            if !readers.is_empty()
                && options.drain.is_some_and(|d| exited.is_some_and(|t| t.elapsed() >= d))
            {
                state.failure.get_or_insert_with(|| {
                    error(7, 0, "output pipes remained open after process exit")
                });
                state.force = true;
            }
        }
        if let Some(pipe) = &mut input {
            if let Some(pending) = &mut state.input {
                let progress = if pending.offset < pending.bytes.len() {
                    pipe.write(&pending.bytes[pending.offset..]).and_then(|n| {
                        pending.offset += n;
                        progressed = n > 0;
                        if pending.offset == pending.bytes.len() {
                            pipe.drained()
                        } else {
                            Ok(false)
                        }
                    })
                } else {
                    pipe.drained()
                };
                match progress {
                    Ok(true) => {
                        state.input.take();
                        state.completed_writes = state.completed_writes.wrapping_add(1);
                        progressed = true;
                    }
                    Ok(false) => {}
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        state.failure.get_or_insert_with(|| os_error(4, 0, e));
                        state.stop = true;
                        state.input.take();
                    }
                }
            }
            if state.input_closed && state.input.is_none() {
                input.take();
            }
        }
        if !readers.is_empty() {
            cursor %= readers.len();
            let mut budget = options.buffer.saturating_sub(state.buffered);
            if state.closed || state.stop {
                budget = options.chunk;
            }
            if budget > 0 {
                let reader = &mut readers[cursor];
                let mut bytes = vec![0; options.chunk.min(budget)];
                match reader.pipe.read(&mut bytes) {
                    Ok(0) => {
                        readers.remove(cursor);
                        progressed = true;
                    }
                    Ok(n) => {
                        progressed = true;
                        bytes.truncate(n);
                        if !state.closed && !state.stop {
                            state.buffered += n;
                            state.output.push_back(Output {
                                stage: reader.stage,
                                stderr: reader.stderr,
                                bytes,
                            });
                        }
                        cursor += 1;
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted =>
                    {
                        cursor += 1;
                    }
                    Err(e) => {
                        state.failure.get_or_insert_with(|| os_error(3, reader.stage, e));
                        state.stop = true;
                        readers.remove(cursor);
                    }
                }
            }
        }
        shared.notify(&mut state);
        if all_exited && readers.is_empty() {
            break;
        }
        // Readiness is a hint: retry all endpoints after a bounded wait, including
        // after backpressure pauses an edge-triggered source. Exit observation
        // shares this timer and never occupies the general blocking pool.
        drop(state);
        let delay = if progressed { Duration::ZERO } else { options.poll };
        if let Err(e) = poll.poll(&mut events, Some(delay))
            && e.kind() != io::ErrorKind::Interrupted
        {
            let mut state = lock(&shared.state);
            state.failure.get_or_insert_with(|| os_error(3, 0, e));
            state.force = true;
        }
    }
    input.take();
    readers.clear();
    let mut cleanup = Vec::new();
    for (stage, child) in &mut children {
        if !killed && let Err(e) = child.kill() {
            cleanup.push(os_error(10, *stage, e));
        }
        if let Err(e) = child.reap() {
            cleanup.push(os_error(10, *stage, e));
        }
    }
    let mut state = lock(&shared.state);
    state.cleanup.extend(cleanup);
    state.done = true;
    state.input.take();
    shared.notify(&mut state);
}

#[cfg(test)]
mod tests;
