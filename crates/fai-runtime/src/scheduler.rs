//! The M:N green-thread scheduler that runs a Fai program's concurrent tasks.
//!
//! A **task** is a stackful coroutine ([`corosensei`]) whose body runs arbitrary
//! compiled Fai code. A fixed pool of **worker** OS threads runs tasks, pulling
//! from lock-free Chase-Lev work-stealing deques ([`crossbeam_deque`]) so a task
//! may migrate between workers. Suspension points (awaiting a task, or a blocked
//! channel send/recv) **park** the task — its worker is freed to run another —
//! and a later completion/space **wakes** it by re-queueing it; so many tasks
//! multiplex onto few threads (the M:N shape) and `await` never blocks a worker.
//!
//! Ownership & safety. A task is an `Arc<Task>` whose coroutine sits behind a
//! `Mutex<Option<…>>`; a worker holds that lock for the *duration* of a resume,
//! so a task can never be resumed on two workers at once and a wake that arrives
//! mid-run simply blocks until the running worker releases the lock. A parked task
//! is referenced only by the one synchronization object it waits on (a task handle
//! or a channel); waking moves it back into the run queue. The coroutine's stack
//! holds only `Send` data (Fai values are plain words; the runtime is thread-safe),
//! so the `!Send` coroutine is wrapped in a `Send` newtype with that justification.
//!
//! This module is self-contained and unit-tested against a Rust task body
//! (`Box<dyn FnOnce() -> Value>`). The `fai_*` C-ABI entry points wrap a Fai thunk
//! and expose the reference-counted `Task`/`Channel` handle values to Fai code; the
//! `Concurrency` capability's std module binds to them.

use std::cell::Cell;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use corosensei::{Coroutine, CoroutineResult, Yielder};
use crossbeam_deque::{Injector, Steal, Stealer, Worker as Deque};

use crate::{Value, fai_dup};

/// Why a task yielded control back to its worker.
enum Suspend {
    /// Parked on a synchronization object; do not re-queue until woken.
    Park,
}

/// The coroutine type backing a task: resumed with `()`, yields a [`Suspend`]
/// reason, and returns `()` once the body has completed (the body writes its Fai
/// result into the task's handle before returning).
type TaskCoro = Coroutine<(), Suspend, ()>;

/// A `Send` wrapper over the `!Send` coroutine.
///
/// SAFETY: a task's coroutine stack holds only `Send` data — Fai values are plain
/// 64-bit words and every runtime entry point a task calls is thread-safe — so the
/// coroutine may be resumed on a different worker after parking (work-stealing
/// migration). Exclusive access is enforced by the task's `Mutex`, so the stack is
/// never touched by two threads at once.
struct SendCoro(TaskCoro);
unsafe impl Send for SendCoro {}

/// A green task. The coroutine is `None` once the body has returned, so a spurious
/// wake of a finished task is a no-op.
struct Task {
    coro: Mutex<Option<SendCoro>>,
}

thread_local! {
    /// The currently running task on this worker, set by [`run_task`] before each
    /// resume so a suspension point can re-queue *itself* when woken.
    static CURRENT_TASK: Cell<*const Task> = const { Cell::new(std::ptr::null()) };
    /// The current coroutine's yielder. The worker cannot know it (it lives on the
    /// coroutine's stack), so the coroutine sets it on entry and every suspension
    /// re-establishes it after resuming — which may be on a different worker.
    static CURRENT_YIELDER: Cell<*const Yielder<(), Suspend>> =
        const { Cell::new(std::ptr::null()) };
}

/// Suspends the current task with `reason`, re-establishing the per-worker yielder
/// pointer after it resumes (possibly on a different worker than it parked on).
fn suspend_current(reason: Suspend) {
    let yielder = CURRENT_YIELDER.with(Cell::get);
    debug_assert!(!yielder.is_null(), "suspended outside a task");
    // SAFETY: `yielder` points at the running coroutine's yielder, valid for the
    // coroutine's whole life and stable across resumes (it lives on the coroutine
    // stack; `resume` refreshes its parent link).
    unsafe { (*yielder).suspend(reason) };
    // Resumed: this may be a different worker, whose TLS yielder is stale.
    CURRENT_YIELDER.with(|c| c.set(yielder));
}

/// Whether the calling OS thread is currently running a task (a scheduler
/// worker, mid-resume). A host operation uses this to decide whether it may park
/// the caller (offloading blocking work) or must run inline — a program that
/// never uses concurrency calls host operations outside any task, with no
/// scheduler to park on.
pub fn in_task() -> bool {
    !CURRENT_TASK.with(Cell::get).is_null()
}

/// An opaque handle to a parked task, held by whatever it is waiting on (e.g. the
/// network reactor) so it can be re-queued when ready. Created by
/// [`current_parked`] inside a task and consumed by [`unpark`] from any thread.
pub struct Parked(Arc<Task>);

/// Captures the currently running task so an external waker (the reactor) can
/// re-queue it. Must be called inside a task (see [`in_task`]). Pair it with a
/// later [`park`] — register the [`Parked`] with the waiter, release the waiter's
/// lock, then `park`; a wake that races ahead is safe (the task is queued once and
/// resumes after it yields).
pub fn current_parked() -> Parked {
    Parked(current_task())
}

/// Suspends the current task until it is [`unpark`]ed. Must be called inside a
/// task, after registering its [`Parked`] handle with the object it waits on.
pub fn park() {
    suspend_current(Suspend::Park);
}

/// Re-queues a parked task (from any thread). Idempotent against a racing
/// self-park: the task runs once when next scheduled.
pub fn unpark(parked: Parked) {
    schedule(parked.0);
}

/// The currently running task (for a suspension point to re-queue itself).
fn current_task() -> Arc<Task> {
    let p = CURRENT_TASK.with(Cell::get);
    debug_assert!(!p.is_null(), "no current task");
    // SAFETY: `p` was set from an `Arc<Task>` that the worker keeps alive across the
    // resume; cloning the `Arc` takes a fresh owned reference.
    unsafe {
        Arc::increment_strong_count(p);
        Arc::from_raw(p)
    }
}

// ---------------------------------------------------------------------------
// The global scheduler.
// ---------------------------------------------------------------------------

struct Scheduler {
    injector: Injector<Arc<Task>>,
    stealers: Vec<Stealer<Arc<Task>>>,
    /// Idle-worker gate: workers wait here when they find no work, and [`schedule`]
    /// signals it. A short wait timeout backstops any missed signal.
    idle: Mutex<()>,
    signal: Condvar,
    shutdown: AtomicBool,
    /// The number of live tasks (created but not yet fully finished, including all
    /// of a finished task's cleanup). [`block_on`] waits for this to reach zero so
    /// the heap is quiescent before its caller (a program's exit-time leak check)
    /// observes it — a task's final drops run on its worker *after* the awaiter is
    /// woken, so waiting only for the root's completion would race that cleanup.
    active: Mutex<usize>,
    quiescent: Condvar,
}

static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
static WORKERS_STARTED: std::sync::Once = std::sync::Once::new();
/// The per-worker deques, created with the scheduler (to take their stealers) and
/// handed to the worker threads when they start (a `Deque` is `!Sync`, so it must
/// be moved into its owning thread).
static PENDING_DEQUES: Mutex<Option<Vec<Deque<Arc<Task>>>>> = Mutex::new(None);

/// The number of worker threads, from `FAI_WORKERS` or the host parallelism.
fn worker_count() -> usize {
    std::env::var("FAI_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Builds the scheduler (queues + stealers), stashing the worker deques for the
/// worker threads to claim when they start.
fn build_scheduler() -> Scheduler {
    let n = worker_count();
    let deques: Vec<Deque<Arc<Task>>> = (0..n).map(|_| Deque::new_fifo()).collect();
    let stealers = deques.iter().map(Deque::stealer).collect();
    *PENDING_DEQUES.lock().expect("pending deques") = Some(deques);
    Scheduler {
        injector: Injector::new(),
        stealers,
        idle: Mutex::new(()),
        signal: Condvar::new(),
        shutdown: AtomicBool::new(false),
        active: Mutex::new(0),
        quiescent: Condvar::new(),
    }
}

/// Lazily starts the worker pool and returns the global scheduler. Idempotent: the
/// first caller (the program's first use of concurrency) builds it and spawns the
/// workers, which read the now-installed scheduler back through [`scheduler`].
fn scheduler() -> &'static Scheduler {
    let sched = SCHEDULER.get_or_init(build_scheduler);
    WORKERS_STARTED.call_once(|| {
        let deques = PENDING_DEQUES.lock().expect("pending deques").take().expect("deques present");
        for (id, deque) in deques.into_iter().enumerate() {
            std::thread::Builder::new()
                .name(format!("fai-worker-{id}"))
                .spawn(move || worker_loop(deque))
                .expect("spawn a scheduler worker");
        }
    });
    sched
}

/// Makes a task runnable: push it to the global queue and wake an idle worker.
fn schedule(task: Arc<Task>) {
    let sched = scheduler();
    sched.injector.push(task);
    let _guard = sched.idle.lock().expect("idle lock");
    sched.signal.notify_one();
}

/// A worker thread: run tasks until shutdown, stealing when its own queue is empty.
fn worker_loop(local: Deque<Arc<Task>>) {
    let sched = scheduler();
    loop {
        if sched.shutdown.load(Ordering::Acquire) {
            return;
        }
        match find_task(sched, &local) {
            Some(task) => run_task(&task),
            None => {
                // No work: wait briefly for a signal, then re-check (the timeout
                // backstops any missed notification, keeping the loop robust).
                let guard = sched.idle.lock().expect("idle lock");
                if sched.injector.is_empty() && !sched.shutdown.load(Ordering::Acquire) {
                    let _ = sched.signal.wait_timeout(guard, Duration::from_millis(1));
                }
            }
        }
    }
}

/// Finds a runnable task: the worker's own queue first, then the global injector,
/// then stealing from a sibling worker.
fn find_task(sched: &Scheduler, local: &Deque<Arc<Task>>) -> Option<Arc<Task>> {
    if let Some(t) = local.pop() {
        return Some(t);
    }
    loop {
        match sched.injector.steal_batch_and_pop(local) {
            Steal::Success(t) => return Some(t),
            Steal::Retry => continue,
            Steal::Empty => break,
        }
    }
    for stealer in &sched.stealers {
        loop {
            match stealer.steal() {
                Steal::Success(t) => return Some(t),
                Steal::Retry => continue,
                Steal::Empty => break,
            }
        }
    }
    None
}

/// Resumes a task once, holding its coroutine lock for the whole resume so it can
/// never run on two workers at once. A completed task drops its coroutine.
fn run_task(task: &Arc<Task>) {
    let mut guard = task.coro.lock().expect("task coro lock");
    let Some(coro) = guard.as_mut() else {
        return; // already finished (a spurious wake)
    };
    let prev = CURRENT_TASK.with(|c| c.replace(Arc::as_ptr(task)));
    let result = coro.0.resume(());
    CURRENT_TASK.with(|c| c.set(prev));
    match result {
        CoroutineResult::Yield(Suspend::Park) => {
            // Parked: the object it waits on holds an `Arc` and will re-queue it.
            // Keep the coroutine so the next resume continues it.
        }
        CoroutineResult::Return(()) => {
            // Drop the coroutine (and its stack) now, while still on this worker, so
            // all of the task's final cleanup happens before it is counted finished.
            *guard = None;
            drop(guard);
            let sched = scheduler();
            let mut active = sched.active.lock().expect("active lock");
            *active -= 1;
            if *active == 0 {
                sched.quiescent.notify_all();
            }
        }
    }
}

/// Builds a task whose body runs `body` and stores the result via `on_done`.
fn make_task(
    body: Box<dyn FnOnce() -> Value + Send>,
    on_done: Box<dyn FnOnce(Value) + Send>,
) -> Arc<Task> {
    let coro = Coroutine::new(move |yielder: &Yielder<(), Suspend>, ()| {
        CURRENT_YIELDER.with(|c| c.set(std::ptr::from_ref(yielder)));
        let result = body();
        on_done(result);
    });
    Arc::new(Task { coro: Mutex::new(Some(SendCoro(coro))) })
}

// ---------------------------------------------------------------------------
// The blocking-work pool (off-worker I/O).
// ---------------------------------------------------------------------------
//
// A host operation that blocks the OS thread — file I/O, a DNS lookup — must not
// run on a scheduler worker, or it would stall every task multiplexed onto that
// worker. Instead it runs on a separate pool of OS threads while its task parks,
// and the pool wakes the task when the work finishes. The pool grows lazily (a
// new thread only when every existing one is busy) up to a cap, so a program that
// does no blocking work spawns none.

/// A unit of blocking work: it performs the OS call and wakes its parked task.
type BlockingJob = Box<dyn FnOnce() + Send>;

struct BlockingPool {
    inner: Mutex<BlockingInner>,
    signal: Condvar,
}

struct BlockingInner {
    queue: VecDeque<BlockingJob>,
    /// Threads currently blocked waiting for a job (available to take new work).
    idle: usize,
    /// Threads spawned so far (idle or running).
    threads: usize,
}

static BLOCKING_POOL: OnceLock<BlockingPool> = OnceLock::new();

/// The maximum number of blocking-pool threads. Generous because these threads
/// spend their time blocked in OS calls (not consuming CPU); overridable with
/// `FAI_BLOCKING_THREADS`.
fn max_blocking_threads() -> usize {
    std::env::var("FAI_BLOCKING_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(512)
}

/// Submits a job to the blocking pool, spawning a fresh thread only when no idle
/// thread is available and the cap has not been reached (otherwise an existing
/// thread will pick the job up).
fn submit_blocking(job: BlockingJob) {
    let pool = BLOCKING_POOL.get_or_init(|| BlockingPool {
        inner: Mutex::new(BlockingInner { queue: VecDeque::new(), idle: 0, threads: 0 }),
        signal: Condvar::new(),
    });
    let mut inner = pool.inner.lock().expect("blocking pool lock");
    inner.queue.push_back(job);
    if inner.idle == 0 && inner.threads < max_blocking_threads() {
        inner.threads += 1;
        drop(inner);
        std::thread::Builder::new()
            .name("fai-blocking".to_owned())
            .spawn(|| blocking_worker_loop(pool))
            .expect("spawn a blocking-pool thread");
    } else {
        pool.signal.notify_one();
    }
}

/// A blocking-pool thread: take jobs and run them, waiting (counted idle) when the
/// queue is empty.
fn blocking_worker_loop(pool: &'static BlockingPool) {
    loop {
        let job = {
            let mut inner = pool.inner.lock().expect("blocking pool lock");
            loop {
                if let Some(job) = inner.queue.pop_front() {
                    break job;
                }
                inner.idle += 1;
                inner = pool.signal.wait(inner).expect("blocking pool wait");
                inner.idle -= 1;
            }
        };
        job();
    }
}

/// Runs blocking work `f` on the blocking pool, parking the current task until it
/// finishes, then returns its result. Must be called inside a task (see
/// [`in_task`]); the result is a plain Rust value built off-worker, so no Fai heap
/// allocation happens on the pool thread (the caller turns it into Fai values back
/// on its worker).
pub fn run_blocking<T: Send + 'static>(f: Box<dyn FnOnce() -> T + Send>) -> T {
    debug_assert!(in_task(), "run_blocking outside a task");
    let result: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let task = current_task();
    let slot = Arc::clone(&result);
    submit_blocking(Box::new(move || {
        let r = f();
        *slot.lock().expect("blocking result") = Some(r);
        // Wake the parked task. This may race ahead of the `suspend_current`
        // below; that is safe — the task is queued once and resumes after it
        // yields (its coroutine lock serializes the resume against this worker).
        schedule(task);
    }));
    suspend_current(Suspend::Park);
    result.lock().expect("blocking result").take().expect("blocking result set")
}

// ---------------------------------------------------------------------------
// Task handles (spawn / await).
// ---------------------------------------------------------------------------

struct HandleState {
    done: bool,
    result: Value,
    awaiters: Vec<Arc<Task>>,
}

/// A handle to a spawned task's eventual result (`Task 'a` to Fai code).
pub struct Handle {
    state: Mutex<HandleState>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // The completed result is owned by the handle (awaiters duplicate it out),
        // so release it when the last reference to the handle goes away.
        let st = self.state.get_mut().expect("handle state");
        if st.done {
            crate::fai_drop(st.result);
        }
    }
}

/// Completes `handle` with `result`, waking every awaiting task.
fn complete(handle: &Arc<Handle>, result: Value) {
    let awaiters = {
        let mut st = handle.state.lock().expect("handle lock");
        st.done = true;
        st.result = result;
        std::mem::take(&mut st.awaiters)
    };
    for task in awaiters {
        schedule(task);
    }
}

/// Spawns `body` as a task, returning its handle. The scheduler starts lazily on
/// the first spawn.
pub fn spawn(body: Box<dyn FnOnce() -> Value + Send>) -> Arc<Handle> {
    let handle = Arc::new(Handle {
        state: Mutex::new(HandleState { done: false, result: 0, awaiters: Vec::new() }),
    });
    // Count the task live before it can run (a worker must not finish and
    // decrement before this increment).
    *scheduler().active.lock().expect("active lock") += 1;
    let done_handle = Arc::clone(&handle);
    let task = make_task(body, Box::new(move |result| complete(&done_handle, result)));
    schedule(task);
    handle
}

/// Awaits a task's result from *within another task* (parks the caller until the
/// awaited task completes). The result is duplicated out, so the handle may be
/// awaited again. Must be called on a worker (inside a task).
pub fn await_handle(handle: &Arc<Handle>) -> Value {
    {
        let mut st = handle.state.lock().expect("handle lock");
        if st.done {
            return fai_dup(st.result);
        }
        // Register before parking (still under the lock), so a completion that
        // races us cannot be missed.
        st.awaiters.push(current_task());
    }
    suspend_current(Suspend::Park);
    let st = handle.state.lock().expect("handle lock");
    debug_assert!(st.done, "woken before completion");
    fai_dup(st.result)
}

/// Waits (from within a task) for a task to complete, *without* taking its result
/// — used to join a nursery's children at scope end. Parks the caller until the
/// awaited task is done.
fn join_handle(handle: &Arc<Handle>) {
    {
        let mut st = handle.state.lock().expect("handle lock");
        if st.done {
            return;
        }
        st.awaiters.push(current_task());
    }
    suspend_current(Suspend::Park);
}

/// Runs `body` as the program's root task and blocks the calling OS thread until
/// the scheduler is quiescent (every task created during the run has fully
/// finished, including its cleanup), then returns the root's result. Starts the
/// scheduler on first use. Waiting for quiescence — not merely the root's
/// completion — means the caller's exit-time leak check sees a settled heap.
pub fn block_on(body: Box<dyn FnOnce() -> Value + Send>) -> Value {
    let handle = spawn(body);
    let sched = scheduler();
    {
        let mut active = sched.active.lock().expect("active lock");
        while *active != 0 {
            active = sched.quiescent.wait(active).expect("quiescent wait");
        }
    }
    // The root has finished (quiescence implies it), so its result is set; the
    // handle is still alive through `handle`, so the result is not yet released.
    let st = handle.state.lock().expect("handle lock");
    debug_assert!(st.done, "root not done at quiescence");
    fai_dup(st.result)
}

// ---------------------------------------------------------------------------
// Nurseries (structured concurrency: spawned tasks join before the scope ends).
// ---------------------------------------------------------------------------

/// A structured-concurrency scope (`Nursery` to Fai code): the tasks spawned into
/// it, joined before the scope returns.
pub struct Nursery {
    children: Mutex<Vec<Arc<Handle>>>,
}

/// Opens a structured scope: runs `body` (given the nursery), then joins every task
/// spawned into the nursery before returning `body`'s result. A task not explicitly
/// awaited is still waited for here, so no task outlives the scope. Runs inside a
/// task (the joins park it).
pub fn scope(body: Box<dyn FnOnce(Arc<Nursery>) -> Value>) -> Value {
    let nursery = Arc::new(Nursery { children: Mutex::new(Vec::new()) });
    let result = body(Arc::clone(&nursery));
    // Join every spawned child. Re-read the list each step: a joined child may
    // itself have spawned more before finishing.
    let mut joined = 0;
    loop {
        let next = {
            let children = nursery.children.lock().expect("nursery lock");
            children.get(joined).cloned()
        };
        match next {
            Some(child) => {
                join_handle(&child);
                joined += 1;
            }
            None => break,
        }
    }
    result
}

/// Spawns `body` into `nursery` (registering it for the scope's join) and returns
/// its handle.
pub fn spawn_in(nursery: &Arc<Nursery>, body: Box<dyn FnOnce() -> Value + Send>) -> Arc<Handle> {
    let handle = spawn(body);
    nursery.children.lock().expect("nursery lock").push(Arc::clone(&handle));
    handle
}

// ---------------------------------------------------------------------------
// Channels (bounded MPMC, parking on full/empty, with close).
// ---------------------------------------------------------------------------

struct ChanState {
    buf: VecDeque<Value>,
    closed: bool,
    recv_waiters: Vec<Arc<Task>>,
    send_waiters: Vec<Arc<Task>>,
}

/// A bounded multi-producer/multi-consumer channel (`Channel 'a` to Fai code).
pub struct Chan {
    state: Mutex<ChanState>,
    capacity: usize,
}

impl Drop for Chan {
    fn drop(&mut self) {
        // Release any values still buffered when the last reference to the channel
        // is dropped (the channel owns them until received).
        let st = self.state.get_mut().expect("chan state");
        while let Some(v) = st.buf.pop_front() {
            crate::fai_drop(v);
        }
    }
}

/// Creates a channel with the given capacity (at least 1).
pub fn channel(capacity: usize) -> Arc<Chan> {
    Arc::new(Chan {
        state: Mutex::new(ChanState {
            buf: VecDeque::new(),
            closed: false,
            recv_waiters: Vec::new(),
            send_waiters: Vec::new(),
        }),
        capacity: capacity.max(1),
    })
}

/// Sends a value, parking the caller while the channel is full. Ownership of `v`
/// transfers into the channel. Must be called inside a task.
pub fn chan_send(chan: &Arc<Chan>, v: Value) {
    loop {
        let mut st = chan.state.lock().expect("chan lock");
        if st.buf.len() < chan.capacity {
            st.buf.push_back(v);
            let rx = st.recv_waiters.pop();
            drop(st);
            // Wake one blocked receiver (outside the lock).
            if let Some(rx) = rx {
                schedule(rx);
            }
            return;
        }
        // Full: register as a send-waiter and park; retry after a receiver frees space.
        st.send_waiters.push(current_task());
        drop(st);
        suspend_current(Suspend::Park);
    }
}

/// Receives a value, parking the caller while the channel is empty. Returns `None`
/// once the channel is closed and drained. Must be called inside a task.
pub fn chan_recv(chan: &Arc<Chan>) -> Option<Value> {
    loop {
        let mut st = chan.state.lock().expect("chan lock");
        if let Some(v) = st.buf.pop_front() {
            let sx = st.send_waiters.pop();
            drop(st);
            // Freed a slot: wake one blocked sender.
            if let Some(sx) = sx {
                schedule(sx);
            }
            return Some(v);
        }
        if st.closed {
            return None;
        }
        // Empty and open: register as a receive-waiter and park.
        st.recv_waiters.push(current_task());
        drop(st);
        suspend_current(Suspend::Park);
    }
}

/// Closes a channel: no more sends, and receivers drain then get `None`. Wakes all
/// blocked receivers so they observe the close.
pub fn chan_close(chan: &Arc<Chan>) {
    let waiters = {
        let mut st = chan.state.lock().expect("chan lock");
        st.closed = true;
        std::mem::take(&mut st.recv_waiters)
    };
    for rx in waiters {
        schedule(rx);
    }
}

// ---------------------------------------------------------------------------
// C-ABI entry points and the Fai `Task`/`Channel` handle representation.
// ---------------------------------------------------------------------------
//
// A handle is a Fai heap cell (KIND_TASK / KIND_CHANNEL) whose single slot stores
// a raw `Arc` pointer to scheduler state. The cell is reference-counted like any
// Fai value; when it dies, `free_obj` calls `drop_task_handle`/`drop_channel_handle`
// to release that `Arc`. Operations follow the runtime's uniform consume
// convention: each consumes its handle/value operands.

/// Wraps a task handle as a Fai `Task` value (a `KIND_TASK` cell owning the `Arc`).
fn task_handle_value(h: Arc<Handle>) -> Value {
    let raw = Arc::into_raw(h) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_TASK_DESC));
    // SAFETY: `p` has room for the header and one slot.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Wraps a channel as a Fai `Channel` value (a `KIND_CHANNEL` cell owning the `Arc`).
fn channel_handle_value(c: Arc<Chan>) -> Value {
    let raw = Arc::into_raw(c) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_CHANNEL_DESC));
    // SAFETY: `p` has room for the header and one slot.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Releases the `Arc<Handle>` a dead task cell owned (called by `free_obj`).
pub(crate) fn drop_task_handle(raw: i64) {
    // SAFETY: `raw` came from `Arc::into_raw` in `task_handle_value`.
    drop(unsafe { Arc::from_raw(raw as usize as *const Handle) });
}

/// Wraps a nursery as a Fai `Nursery` value (a `KIND_NURSERY` cell owning the `Arc`).
fn nursery_handle_value(n: Arc<Nursery>) -> Value {
    let raw = Arc::into_raw(n) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_NURSERY_DESC));
    // SAFETY: `p` has room for the header and one slot.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Releases the `Arc<Chan>` a dead channel cell owned (called by `free_obj`).
pub(crate) fn drop_channel_handle(raw: i64) {
    // SAFETY: `raw` came from `Arc::into_raw` in `channel_handle_value`.
    drop(unsafe { Arc::from_raw(raw as usize as *const Chan) });
}

/// Releases the `Arc<Nursery>` a dead nursery cell owned (called by `free_obj`).
pub(crate) fn drop_nursery_handle(raw: i64) {
    // SAFETY: `raw` came from `Arc::into_raw` in `nursery_handle_value`.
    drop(unsafe { Arc::from_raw(raw as usize as *const Nursery) });
}

/// Borrows the `Arc<Nursery>` from a `Nursery` value without consuming a reference.
fn nursery_of(nursery: Value) -> ManuallyDrop<Arc<Nursery>> {
    // SAFETY: `nursery` is a live `KIND_NURSERY` cell whose slot holds the `Arc`.
    let raw = unsafe { crate::read_i64(crate::as_obj(nursery), crate::HANDLE_PTR_OFFSET) };
    ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const Nursery) })
}

/// Borrows the `Arc<Handle>` from a `Task` value without consuming a reference (the
/// reference stays owned by the cell, released when the cell is dropped).
fn handle_of(task: Value) -> ManuallyDrop<Arc<Handle>> {
    // SAFETY: `task` is a live `KIND_TASK` cell whose slot holds the `Arc` pointer.
    let raw = unsafe { crate::read_i64(crate::as_obj(task), crate::HANDLE_PTR_OFFSET) };
    ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const Handle) })
}

/// Borrows the `Arc<Chan>` from a `Channel` value without consuming a reference.
fn chan_of(chan: Value) -> ManuallyDrop<Arc<Chan>> {
    // SAFETY: `chan` is a live `KIND_CHANNEL` cell whose slot holds the `Arc` pointer.
    let raw = unsafe { crate::read_i64(crate::as_obj(chan), crate::HANDLE_PTR_OFFSET) };
    ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const Chan) })
}

/// Opens a structured scope: builds a nursery, applies `body` to it, then joins
/// every task spawned into the nursery before returning `body`'s result. Consumes
/// `body`. Runs inside a task (the joins park it). The nursery value handed to
/// `body` is borrowed — it lives only for the scope.
#[unsafe(no_mangle)]
pub extern "C" fn fai_scope(body: Value) -> Value {
    scope(Box::new(move |nursery| {
        // Hand `body` ownership of one nursery reference (the uniform consume
        // convention: the body drops it at its last use). `scope` keeps its own
        // `Arc<Nursery>` for the join, independent of this Fai cell.
        let nursery_value = nursery_handle_value(nursery);
        let args = [nursery_value];
        // SAFETY: `body` is an arity-1 closure taking the nursery; it consumes it.
        unsafe { crate::fai_apply_n(body, 1, args.as_ptr()) }
    }))
}

/// Spawns `thunk` (a `Unit -> 'a` closure) into `nursery`, returning its `Task`
/// handle. The thunk's captured graph is marked shared (it runs on another worker)
/// and so is the result (it returns to the awaiter's worker). Consumes `nursery`
/// (a borrowed reference passed per call) and `thunk`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_spawn(nursery: Value, thunk: Value) -> Value {
    crate::fai_mark_shared(thunk);
    let handle = {
        let n = nursery_of(nursery);
        spawn_in(
            &n,
            Box::new(move || {
                let args = [crate::FAI_UNIT];
                // SAFETY: `thunk` is an arity-1 closure; one owned `Unit` argument.
                let result = unsafe { crate::fai_apply_n(thunk, 1, args.as_ptr()) };
                crate::fai_mark_shared(result)
            }),
        )
    };
    crate::fai_drop(nursery);
    task_handle_value(handle)
}

/// Awaits a task's result, parking the caller until it completes. The result is
/// duplicated out (so another reference may await it again). Consumes the handle.
#[unsafe(no_mangle)]
pub extern "C" fn fai_await(task: Value) -> Value {
    let result = await_handle(&handle_of(task));
    crate::fai_drop(task);
    result
}

/// Creates a bounded channel of the given capacity (a Fai `Int`); returns a
/// `Channel` handle.
#[unsafe(no_mangle)]
pub extern "C" fn fai_channel(capacity: Value) -> Value {
    channel_handle_value(channel((capacity >> 1) as usize))
}

/// Sends a value on a channel, parking while full. The value's graph is marked
/// shared (it crosses to the receiver). Consumes both operands; returns `Unit`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_send(chan: Value, v: Value) -> Value {
    crate::fai_mark_shared(v);
    chan_send(&chan_of(chan), v);
    crate::fai_drop(chan);
    crate::FAI_UNIT
}

/// Receives a value, parking while empty. Returns a standard `Option`: `Some v`
/// (tag 1) or `None` (tag 0, an immediate) once the channel is closed and drained.
/// Consumes the `Channel` handle.
#[unsafe(no_mangle)]
pub extern "C" fn fai_recv(chan: Value) -> Value {
    let received = chan_recv(&chan_of(chan));
    crate::fai_drop(chan);
    match received {
        // SAFETY: one owned field moves into the `Some` cell.
        Some(v) => unsafe { crate::fai_make_data(1, 1, [v].as_ptr()) },
        None => 1,
    }
}

/// Closes a channel (receivers drain, then get `None`). Consumes the handle.
#[unsafe(no_mangle)]
pub extern "C" fn fai_close(chan: Value) -> Value {
    chan_close(&chan_of(chan));
    crate::fai_drop(chan);
    crate::FAI_UNIT
}

/// Runs `main_thunk` (`Unit -> a`) as the program's root task on the scheduler,
/// blocking until it finishes. The entry trampoline uses this for a program that
/// uses concurrency.
#[unsafe(no_mangle)]
pub extern "C" fn fai_block_on(main_thunk: Value) -> Value {
    block_on(Box::new(move || {
        let args = [crate::FAI_UNIT];
        // SAFETY: arity-1 root closure; one owned `Unit` argument.
        unsafe { crate::fai_apply_n(main_thunk, 1, args.as_ptr()) }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The scheduler is process-global and starts on first use; these tests share
    // one pool. Bodies return plain immediate `Value`s (`n << 1 | 1`).
    fn imm(n: i64) -> Value {
        (n << 1) | 1
    }
    fn of_imm(v: Value) -> i64 {
        v >> 1
    }

    #[test]
    fn block_on_returns_a_simple_result() {
        let r = block_on(Box::new(|| imm(42)));
        assert_eq!(of_imm(r), 42);
    }

    #[test]
    fn spawn_and_await_one_child() {
        let r = block_on(Box::new(|| {
            let h = spawn(Box::new(|| imm(7)));
            let v = await_handle(&h);
            imm(of_imm(v) + 1)
        }));
        assert_eq!(of_imm(r), 8);
    }

    #[test]
    fn fan_out_fan_in_sum() {
        let r = block_on(Box::new(|| {
            let handles: Vec<_> = (0..64).map(|i| spawn(Box::new(move || imm(i)))).collect();
            let mut sum = 0;
            for h in &handles {
                sum += of_imm(await_handle(h));
            }
            imm(sum)
        }));
        assert_eq!(of_imm(r), (0..64).sum::<i64>());
    }

    #[test]
    fn nested_spawns() {
        let r = block_on(Box::new(|| {
            let outer = spawn(Box::new(|| {
                let inner = spawn(Box::new(|| imm(10)));
                imm(of_imm(await_handle(&inner)) * 2)
            }));
            await_handle(&outer)
        }));
        assert_eq!(of_imm(r), 20);
    }

    #[test]
    fn channel_producer_consumer() {
        let r = block_on(Box::new(|| {
            let ch = channel(4);
            let producer = {
                let ch = Arc::clone(&ch);
                spawn(Box::new(move || {
                    for i in 0..100 {
                        chan_send(&ch, imm(i));
                    }
                    chan_close(&ch);
                    imm(0)
                }))
            };
            let mut total = 0;
            while let Some(v) = chan_recv(&ch) {
                total += of_imm(v);
            }
            await_handle(&producer);
            imm(total)
        }));
        assert_eq!(of_imm(r), (0..100).sum::<i64>());
    }

    #[test]
    fn scope_joins_unawaited_children() {
        use std::sync::atomic::{AtomicI64, Ordering};
        static RAN: AtomicI64 = AtomicI64::new(0);
        RAN.store(0, Ordering::SeqCst);
        let r = block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                for _ in 0..32 {
                    // Spawned but never explicitly awaited: the scope must join them.
                    let _ = spawn_in(
                        &nursery,
                        Box::new(|| {
                            RAN.fetch_add(1, Ordering::SeqCst);
                            imm(0)
                        }),
                    );
                }
                imm(7)
            }))
        }));
        assert_eq!(of_imm(r), 7, "the scope returns its body's result");
        assert_eq!(RAN.load(Ordering::SeqCst), 32, "every spawned child ran before scope returned");
    }

    #[test]
    fn many_tasks_fan_out() {
        // Far more tasks than workers: they multiplex onto the pool (the M:N point).
        const N: i64 = 1000;
        let r = block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                let handles: Vec<_> =
                    (0..N).map(|i| spawn_in(&nursery, Box::new(move || imm(i)))).collect();
                let mut sum = 0;
                for h in &handles {
                    sum += of_imm(await_handle(h));
                }
                imm(sum)
            }))
        }));
        assert_eq!(of_imm(r), (0..N).sum::<i64>());
    }

    #[test]
    fn await_the_same_handle_twice_is_memoized() {
        let r = block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                let h = spawn_in(&nursery, Box::new(|| imm(21)));
                let a = of_imm(await_handle(&h));
                let b = of_imm(await_handle(&h));
                imm(a + b)
            }))
        }));
        assert_eq!(of_imm(r), 42, "a second await returns the same (memoized) result");
    }

    #[test]
    fn empty_scope_returns_its_body() {
        // A scope that spawns nothing is just its body's value.
        let r = block_on(Box::new(|| scope(Box::new(|_nursery| imm(5)))));
        assert_eq!(of_imm(r), 5);
    }

    #[test]
    fn deeply_nested_scopes() {
        // Each level opens a scope and spawns a task that recurses one level deeper,
        // so scopes nest across tasks.
        fn nest(depth: i64) -> Value {
            if depth == 0 {
                return imm(0);
            }
            scope(Box::new(move |nursery| {
                let h = spawn_in(&nursery, Box::new(move || imm(1 + of_imm(nest(depth - 1)))));
                await_handle(&h)
            }))
        }
        let r = block_on(Box::new(|| nest(24)));
        assert_eq!(of_imm(r), 24);
    }

    #[test]
    fn channel_capacity_one_serializes_with_backpressure() {
        // Capacity 1: the producer blocks after each send until the consumer drains
        // a slot, so the two interleave one item at a time.
        let r = block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                let ch = channel(1);
                {
                    let ch = Arc::clone(&ch);
                    let _ = spawn_in(
                        &nursery,
                        Box::new(move || {
                            for i in 1..=50 {
                                chan_send(&ch, imm(i));
                            }
                            chan_close(&ch);
                            imm(0)
                        }),
                    );
                }
                let mut total = 0;
                while let Some(v) = chan_recv(&ch) {
                    total += of_imm(v);
                }
                imm(total)
            }))
        }));
        assert_eq!(of_imm(r), (1..=50).sum::<i64>());
    }

    #[test]
    fn channel_recv_receives_a_value_sent_concurrently() {
        // The consumer may recv before the producer sends (parking on the empty
        // channel) or after; either way it receives the value.
        let r = block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                let ch = channel(4);
                {
                    let ch = Arc::clone(&ch);
                    let _ = spawn_in(
                        &nursery,
                        Box::new(move || {
                            chan_send(&ch, imm(99));
                            chan_close(&ch);
                            imm(0)
                        }),
                    );
                }
                match chan_recv(&ch) {
                    Some(v) => imm(of_imm(v)),
                    None => imm(-1),
                }
            }))
        }));
        assert_eq!(of_imm(r), 99);
    }

    #[test]
    fn channel_multiple_producers() {
        // Several producers feed one consumer over a bounded channel; the consumer
        // drains the exact total count (no close needed).
        const PRODUCERS: i64 = 5;
        const PER: i64 = 20;
        let r = block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                let ch = channel(4);
                for _ in 0..PRODUCERS {
                    let ch = Arc::clone(&ch);
                    let _ = spawn_in(
                        &nursery,
                        Box::new(move || {
                            for i in 1..=PER {
                                chan_send(&ch, imm(i));
                            }
                            imm(0)
                        }),
                    );
                }
                let mut total = 0;
                for _ in 0..(PRODUCERS * PER) {
                    if let Some(v) = chan_recv(&ch) {
                        total += of_imm(v);
                    }
                }
                imm(total)
            }))
        }));
        assert_eq!(of_imm(r), PRODUCERS * (1..=PER).sum::<i64>());
    }

    #[test]
    fn channel_close_wakes_multiple_consumers() {
        // Two consumers drain until the channel is closed and empty; `close` wakes
        // any parked on an empty channel so they observe the end.
        use std::sync::atomic::{AtomicI64, Ordering};
        static TOTAL: AtomicI64 = AtomicI64::new(0);
        TOTAL.store(0, Ordering::SeqCst);
        block_on(Box::new(|| {
            scope(Box::new(|nursery| {
                let ch = channel(4);
                for _ in 0..2 {
                    let ch = Arc::clone(&ch);
                    let _ = spawn_in(
                        &nursery,
                        Box::new(move || {
                            while let Some(v) = chan_recv(&ch) {
                                TOTAL.fetch_add(of_imm(v), Ordering::SeqCst);
                            }
                            imm(0)
                        }),
                    );
                }
                for i in 1..=100 {
                    chan_send(&ch, imm(i));
                }
                chan_close(&ch);
                imm(0)
            }))
        }));
        assert_eq!(TOTAL.load(Ordering::SeqCst), (1..=100).sum::<i64>());
    }

    #[test]
    fn run_blocking_returns_its_result() {
        // A task offloads blocking work and gets its result back after parking.
        let r = block_on(Box::new(|| {
            let v: i64 = run_blocking(Box::new(|| 6 * 7));
            imm(v)
        }));
        assert_eq!(of_imm(r), 42);
    }

    #[test]
    fn blocking_work_does_not_consume_a_worker() {
        // More simultaneously-blocking tasks than workers: each parks its task and
        // runs on the blocking pool. A shared barrier releases only once *every*
        // task is blocked at it at the same time, so this completes only because a
        // blocking op does not occupy its worker — were blocking to run on the
        // worker, at most `worker_count()` tasks could block at once and the
        // barrier (needing them all) would deadlock.
        use std::sync::Barrier;
        let n = worker_count() + 4;
        let barrier = Arc::new(Barrier::new(n));
        let r = block_on(Box::new(move || {
            scope(Box::new(move |nursery| {
                let handles: Vec<_> = (0..n)
                    .map(|_| {
                        let b = Arc::clone(&barrier);
                        spawn_in(
                            &nursery,
                            Box::new(move || {
                                let v: i64 = run_blocking(Box::new(move || {
                                    b.wait();
                                    1
                                }));
                                imm(v)
                            }),
                        )
                    })
                    .collect();
                let mut sum = 0;
                for h in &handles {
                    sum += of_imm(await_handle(h));
                }
                imm(sum)
            }))
        }));
        assert_eq!(of_imm(r), (worker_count() + 4) as i64);
    }
}
