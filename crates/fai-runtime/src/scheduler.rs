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
//! (`Box<dyn FnOnce() -> Value>`). The `fai_*` C-ABI entry points that wrap a Fai
//! thunk — and the reference-counted `Task`/`Channel` handle representation they
//! hand to Fai code — are added with the `Concurrency` capability that calls them;
//! until then this scheduler API is reached only from the tests below.
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::VecDeque;
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
        CoroutineResult::Return(()) => *guard = None,
    }
}

/// Builds a task whose body runs `body` and stores the result via `on_done`.
fn make_task(body: Box<dyn FnOnce() -> Value + Send>, on_done: Box<dyn FnOnce(Value) + Send>) -> Arc<Task> {
    let coro = Coroutine::new(move |yielder: &Yielder<(), Suspend>, ()| {
        CURRENT_YIELDER.with(|c| c.set(std::ptr::from_ref(yielder)));
        let result = body();
        on_done(result);
    });
    Arc::new(Task { coro: Mutex::new(Some(SendCoro(coro))) })
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
    /// Signals an OS thread blocked in [`block_on`] (the root task); green-task
    /// awaiters park instead and are listed in `awaiters`.
    blocker: Condvar,
}

/// Completes `handle` with `result`, waking every awaiting task and any blocked
/// OS thread.
fn complete(handle: &Arc<Handle>, result: Value) {
    let awaiters = {
        let mut st = handle.state.lock().expect("handle lock");
        st.done = true;
        st.result = result;
        handle.blocker.notify_all();
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
        blocker: Condvar::new(),
    });
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

/// Runs `body` as the program's root task and blocks the calling OS thread until it
/// finishes, returning its result. Starts the scheduler on first use.
pub fn block_on(body: Box<dyn FnOnce() -> Value + Send>) -> Value {
    let handle = spawn(body);
    let mut st = handle.state.lock().expect("handle lock");
    while !st.done {
        st = handle.blocker.wait(st).expect("handle wait");
    }
    fai_dup(st.result)
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
}
