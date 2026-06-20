//! The network I/O reactor: readiness-driven non-blocking sockets over [`mio`].
//!
//! A single **reactor thread** runs `mio::Poll`, translating OS readiness events
//! (epoll/kqueue/IOCP) into task wakeups. Worker threads register their own
//! sockets (the `Registry` is `Send + Sync`) and perform the actual read/write
//! syscalls themselves; the reactor only says "this socket is now readable /
//! writable → wake whoever waits on it". This **readiness** model (rather than a
//! completion/callback loop) is the natural fit for the M:N work-stealing
//! scheduler: the worker that owns a task does its I/O, and only readiness
//! wakeups cross to the reactor thread, so no socket data or operation is
//! marshalled between threads.
//!
//! A socket operation runs as a retry loop: attempt the syscall; on `WouldBlock`,
//! [`wait_readable`]/[`wait_writable`] parks the task until the reactor signals
//! readiness, then the loop retries. A per-direction readiness latch closes the
//! lost-wake race — the reactor records readiness even when no task is waiting
//! yet, so a readiness edge that arrives between the failed syscall and the park
//! is observed (the task retries instead of parking forever).
//!
//! The reactor starts lazily on the first socket registration, so a program that
//! does no networking never spawns it.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use mio::event::Source;
use mio::{Events, Interest, Poll, Registry, Token, Waker};

use crate::scheduler::{self, Parked};

/// The reserved poll token for the reactor's own [`Waker`], used to break an
/// in-progress `poll` when a newly registered timer's deadline is sooner than the
/// current poll timeout (so it is not missed). Socket tokens start at 1.
const WAKER_TOKEN: Token = Token(0);

/// One direction's (read or write) readiness state for a registered socket.
#[derive(Default)]
struct Readiness {
    /// A readiness edge has arrived since it was last consumed. Set by the reactor,
    /// cleared by the waiter; lets a wake that races ahead of the park be observed.
    ready: bool,
    /// The task parked waiting for this direction, if any.
    waiter: Option<Parked>,
}

/// The reactor-side state of one registered socket, shared between the worker
/// performing I/O and the reactor thread that wakes it.
pub struct IoSource {
    token: usize,
    read: Mutex<Readiness>,
    write: Mutex<Readiness>,
}

/// One pending timer: a parked task to wake at `deadline`. `seq` breaks ties and
/// identifies the entry so a cancelled sleep can remove it before it fires.
struct TimerEntry {
    deadline: Instant,
    seq: u64,
    waiter: Parked,
}

// Ordered by `(deadline, seq)` so the binary heap (wrapped in `Reverse`) yields the
// earliest deadline first. Only the schedulable key participates; `waiter` is data.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline).then(self.seq.cmp(&other.seq))
    }
}

/// The reactor's pending timers: a min-heap (by deadline) plus a sequence counter
/// for stable, identifiable entries.
struct Timers {
    heap: BinaryHeap<Reverse<TimerEntry>>,
    next_seq: u64,
}

/// The global reactor: a `mio::Poll` on its own thread plus a cloned `Registry`
/// (used by workers to register their sockets), a `Waker` (to interrupt a poll when
/// a sooner timer arrives), the token→source table, and the pending timers.
struct Reactor {
    registry: Registry,
    waker: Waker,
    sources: Mutex<HashMap<usize, Arc<IoSource>>>,
    next_token: AtomicUsize,
    timers: Mutex<Timers>,
}

static REACTOR: OnceLock<Reactor> = OnceLock::new();

/// Returns the global reactor, starting its thread on first use (the first socket
/// registration *or* the first timer, so a program that only sleeps still gets it).
fn reactor() -> &'static Reactor {
    REACTOR.get_or_init(|| {
        let poll = Poll::new().expect("create the network reactor poll");
        let registry = poll.registry().try_clone().expect("clone the reactor registry");
        let waker = Waker::new(poll.registry(), WAKER_TOKEN).expect("create the reactor waker");
        std::thread::Builder::new()
            .name("fai-reactor".to_owned())
            .spawn(move || reactor_loop(poll))
            .expect("spawn the network reactor thread");
        Reactor {
            registry,
            waker,
            sources: Mutex::new(HashMap::new()),
            next_token: AtomicUsize::new(1),
            timers: Mutex::new(Timers { heap: BinaryHeap::new(), next_seq: 0 }),
        }
    })
}

/// The duration until the earliest pending timer's deadline (zero if one is already
/// due), or `None` if there are no timers — the poll timeout for the next iteration.
fn next_timeout() -> Option<Duration> {
    let timers = reactor().timers.lock().expect("reactor timers");
    timers.heap.peek().map(|Reverse(e)| e.deadline.saturating_duration_since(Instant::now()))
}

/// Wakes every timer whose deadline has passed, removing it from the heap.
fn fire_expired_timers() {
    let now = Instant::now();
    let mut due = Vec::new();
    {
        let mut timers = reactor().timers.lock().expect("reactor timers");
        while let Some(Reverse(entry)) = timers.heap.peek() {
            if entry.deadline <= now {
                let Reverse(entry) = timers.heap.pop().expect("peeked entry");
                due.push(entry.waiter);
            } else {
                break;
            }
        }
    }
    for waiter in due {
        scheduler::unpark(waiter);
    }
}

/// Removes the pending timer with `seq`, if it has not already fired. Used to clean
/// up after a wake that was not this timer (a cancellation, later) so the stale
/// entry cannot fire against a task that has moved on.
fn remove_timer(seq: u64) {
    let mut timers = reactor().timers.lock().expect("reactor timers");
    timers.heap.retain(|Reverse(e)| e.seq != seq);
}

/// Parks the current task until `deadline`. Must be called inside a task. Registers
/// a timer, wakes the reactor (so a sooner deadline preempts an in-progress poll),
/// and parks; the reactor fires the timer at the deadline. A wake from any other
/// source (e.g. cancellation, later) also returns — the caller re-checks its own
/// condition — and the still-pending timer entry is removed so it cannot fire late.
pub fn sleep_until(deadline: Instant) {
    if Instant::now() >= deadline {
        return;
    }
    let r = reactor();
    let seq = {
        let mut timers = r.timers.lock().expect("reactor timers");
        let seq = timers.next_seq;
        timers.next_seq += 1;
        timers.heap.push(Reverse(TimerEntry {
            deadline,
            seq,
            waiter: scheduler::current_parked(),
        }));
        seq
    };
    let _ = r.waker.wake();
    scheduler::park();
    remove_timer(seq);
}

/// The reactor thread: poll for readiness (bounded by the nearest timer deadline)
/// forever, firing due timers and waking the task waiting on each ready direction of
/// each signalled socket.
fn reactor_loop(mut poll: Poll) -> ! {
    let mut events = Events::with_capacity(256);
    loop {
        // Poll until a socket is ready or the nearest timer is due (whichever first);
        // a sooner timer registered mid-poll wakes us early via the reactor's `Waker`.
        if let Err(e) = poll.poll(&mut events, next_timeout())
            && e.kind() != io::ErrorKind::Interrupted
        {
            // Not actionable here; loop to fire any due timers and poll again.
        }
        fire_expired_timers();
        for event in events.iter() {
            let token = event.token().0;
            if token == WAKER_TOKEN.0 {
                // A wake to recompute the poll timeout (a new/sooner timer); no source.
                continue;
            }
            let source = reactor().sources.lock().expect("reactor sources").get(&token).cloned();
            let Some(source) = source else { continue };
            if event.is_readable() {
                wake_direction(&source.read);
            }
            if event.is_writable() {
                wake_direction(&source.write);
            }
        }
    }
}

/// Latches a direction ready and wakes its parked task, if any.
fn wake_direction(dir: &Mutex<Readiness>) {
    let waiter = {
        let mut r = dir.lock().expect("reactor readiness");
        r.ready = true;
        r.waiter.take()
    };
    if let Some(waiter) = waiter {
        scheduler::unpark(waiter);
    }
}

/// Registers `source` with the reactor for readable+writable readiness, returning
/// its [`IoSource`]. The reactor starts on the first call.
pub fn register<S: Source>(source: &mut S) -> io::Result<Arc<IoSource>> {
    let r = reactor();
    let token = r.next_token.fetch_add(1, Ordering::Relaxed);
    r.registry.register(source, Token(token), Interest::READABLE | Interest::WRITABLE)?;
    let io = Arc::new(IoSource {
        token,
        read: Mutex::new(Readiness::default()),
        write: Mutex::new(Readiness::default()),
    });
    r.sources.lock().expect("reactor sources").insert(token, Arc::clone(&io));
    Ok(io)
}

/// Deregisters a source (on socket close): drop the poll registration and forget
/// its [`IoSource`]. Best-effort — a deregister error cannot be acted on.
pub fn deregister<S: Source>(io: &Arc<IoSource>, source: &mut S) {
    let r = reactor();
    let _ = r.registry.deregister(source);
    r.sources.lock().expect("reactor sources").remove(&io.token);
}

/// Parks the current task until `io` is readable. Must be called inside a task,
/// and only **after** attempting the operation and observing it would block —
/// never as a bare wait. An edge-triggered poll need not report readiness that
/// predates registration, so a wait that has not first confirmed the operation
/// would block (e.g. by a `WouldBlock` or a `NotConnected` probe) can park forever.
pub fn wait_readable(io: &Arc<IoSource>) {
    wait(&io.read);
}

/// Parks the current task until `io` is writable (e.g. a connect completes or send
/// buffer space frees). Must be called inside a task, and only after the operation
/// has been attempted and would block (see [`wait_readable`]).
pub fn wait_writable(io: &Arc<IoSource>) {
    wait(&io.write);
}

/// The shared wait for one direction: consume a pending readiness edge if one
/// already arrived, else register as the waiter and park until the reactor wakes
/// us. The waiter's lock is released before parking, so a racing wake is not lost.
fn wait(dir: &Mutex<Readiness>) {
    {
        let mut r = dir.lock().expect("reactor readiness");
        if r.ready {
            // A readiness edge already arrived (possibly between the caller's
            // failed syscall and now): consume it and let the caller retry.
            r.ready = false;
            return;
        }
        r.waiter = Some(scheduler::current_parked());
    }
    scheduler::park();
    // Woken by the reactor, which set `ready` and took our waiter. Consume the
    // readiness so the next `WouldBlock` parks again.
    dir.lock().expect("reactor readiness").ready = false;
}

// ---------------------------------------------------------------------------
// TCP sockets: the `Net` capability's runtime operations.
// ---------------------------------------------------------------------------
//
// A `Listener`/`Connection` Fai value is a reference-counted heap cell
// (`KIND_NET`) whose slot owns a raw `Arc<NetObject>` (the mio socket plus its
// reactor registration); the free path drops that `Arc`, whose `Drop`
// deregisters the socket and closes its fd. Each operation runs on the calling
// task (a program that uses `Net` runs on the scheduler), performing the syscall
// itself and parking on the reactor when it would block — so I/O runs in parallel
// across workers and only readiness wakeups cross to the reactor thread.

use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, ToSocketAddrs};

use mio::net::{TcpListener, TcpStream, UdpSocket};

use crate::Value;

/// The reactor-side state of a network socket, owned by its Fai handle cell.
enum NetObject {
    Listener { sock: Mutex<TcpListener>, src: Arc<IoSource> },
    Conn { sock: Mutex<TcpStream>, src: Arc<IoSource> },
    Udp { sock: Mutex<UdpSocket>, src: Arc<IoSource> },
}

impl Drop for NetObject {
    fn drop(&mut self) {
        // Deregister from the reactor (remove the sources-map entry and the poll
        // registration) before the mio socket's own `Drop` closes the fd.
        match self {
            NetObject::Listener { sock, src } => {
                if let Ok(s) = sock.get_mut() {
                    deregister(src, s);
                }
            }
            NetObject::Conn { sock, src } => {
                if let Ok(s) = sock.get_mut() {
                    deregister(src, s);
                }
            }
            NetObject::Udp { sock, src } => {
                if let Ok(s) = sock.get_mut() {
                    deregister(src, s);
                }
            }
        }
    }
}

/// Resolves `host:port` to a socket address. An IP literal is parsed inline (no
/// blocking); a hostname is resolved on the blocking pool (DNS may block), parking
/// the calling task. Must be called inside a task for the hostname path.
fn resolve_addr(host: String, port: u16) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    crate::scheduler::run_blocking(Box::new(move || {
        (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| e.to_string())
            .and_then(|mut addrs| addrs.next().ok_or_else(|| "no address for host".to_owned()))
    }))
}

/// Wraps socket state as a Fai `KIND_NET` value owning the `Arc`.
fn net_handle_value(obj: NetObject) -> Value {
    let raw = Arc::into_raw(Arc::new(obj)) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_NET_DESC));
    // SAFETY: `p` has room for the header and one slot.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Releases the `Arc<NetObject>` a dead socket cell owned (called by `free_obj`).
pub(crate) fn drop_net_handle(raw: i64) {
    // SAFETY: `raw` came from `Arc::into_raw` in `net_handle_value`.
    drop(unsafe { Arc::from_raw(raw as usize as *const NetObject) });
}

/// Borrows the `Arc<NetObject>` from a socket value without consuming a reference
/// (the reference stays owned by the cell, released when the cell is dropped).
fn net_of(v: Value) -> ManuallyDrop<Arc<NetObject>> {
    // SAFETY: `v` is a live `KIND_NET` cell whose slot holds the `Arc` pointer.
    let raw = unsafe { crate::read_i64(crate::as_obj(v), crate::HANDLE_PTR_OFFSET) };
    ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const NetObject) })
}

/// Builds `Ok v` (`Result _ _` is `| Ok 'ok | Err 'err`, so `Ok` is tag 0).
fn ok_result(v: Value) -> Value {
    // SAFETY: one owned field moves into the `Ok` cell.
    unsafe { crate::fai_make_data(0, 1, [v].as_ptr()) }
}

/// Builds `Err <message>` (tag 1) from an error string.
fn err_result(msg: &str) -> Value {
    let s = crate::make_string(msg.as_bytes());
    // SAFETY: one owned field moves into the `Err` cell.
    unsafe { crate::fai_make_data(1, 1, [s].as_ptr()) }
}

/// `Net.listen`: bind and listen on `0.0.0.0:port` (port `0` picks a free one).
/// Returns `Result Listener String`. The port is an immediate `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_listen(port: Value) -> Value {
    let p = crate::unbox_int(port) as u16;
    crate::fai_drop(port);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, p));
    match TcpListener::bind(addr) {
        Ok(mut sock) => match register(&mut sock) {
            Ok(src) => {
                ok_result(net_handle_value(NetObject::Listener { sock: Mutex::new(sock), src }))
            }
            Err(e) => err_result(&e.to_string()),
        },
        Err(e) => err_result(&e.to_string()),
    }
}

/// `Net.localPort`: the port a listener is bound to (`0` if unavailable). Consumes
/// the listener reference.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_local_port(listener: Value) -> Value {
    let port = {
        let obj = net_of(listener);
        match &**obj {
            NetObject::Listener { sock, .. } => {
                sock.lock().expect("listener lock").local_addr().map_or(0, |a| a.port())
            }
            NetObject::Conn { .. } | NetObject::Udp { .. } => 0,
        }
    };
    crate::fai_drop(listener);
    crate::fai_box_int(i64::from(port))
}

/// `Net.accept`: accept the next connection (parking until one arrives). Returns
/// `Result Connection String`. Consumes the listener reference (callers re-use the
/// listener through the ordinary reference-count duplication).
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_accept(listener: Value) -> Value {
    let result = {
        let obj = net_of(listener);
        match &**obj {
            NetObject::Listener { sock, src } => accept_loop(sock, src),
            NetObject::Conn { .. } | NetObject::Udp { .. } => {
                Err("accept: handle is not a listener".to_owned())
            }
        }
    };
    crate::fai_drop(listener);
    match result {
        Ok(conn) => ok_result(net_handle_value(conn)),
        Err(e) => err_result(&e),
    }
}

fn accept_loop(sock: &Mutex<TcpListener>, src: &Arc<IoSource>) -> Result<NetObject, String> {
    loop {
        if scheduler::is_cancelled() {
            return Err(scheduler::CANCELLED_MESSAGE.to_owned());
        }
        let accepted = sock.lock().expect("listener lock").accept();
        match accepted {
            Ok((mut stream, _)) => {
                let csrc = register(&mut stream).map_err(|e| e.to_string())?;
                return Ok(NetObject::Conn { sock: Mutex::new(stream), src: csrc });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_readable(src),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `Net.connect`: connect to `host:port` (resolving a hostname on the blocking
/// pool) without blocking a worker. Returns `Result Connection String`. Consumes
/// `host`; `port` is an immediate `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_connect(host: Value, port: Value) -> Value {
    // SAFETY: `host` is a boxed `String`.
    let h = unsafe { crate::string_str(host) }.to_owned();
    let p = crate::unbox_int(port) as u16;
    crate::fai_drop(host);
    crate::fai_drop(port);
    let addr = match resolve_addr(h, p) {
        Ok(a) => a,
        Err(e) => return err_result(&e),
    };
    match TcpStream::connect(addr) {
        Ok(mut stream) => {
            let src = match register(&mut stream) {
                Ok(s) => s,
                Err(e) => return err_result(&e.to_string()),
            };
            // A non-blocking connect completes asynchronously, signaled by the
            // socket becoming writable. *Check completion directly* each iteration
            // rather than only waiting for the writable edge: the connect can finish
            // (or fail) before — or in the same instant as — registration, and an
            // edge-triggered poll (epoll) need not re-report readiness that predates
            // the registration, so a bare wait could park forever. `take_error`
            // surfaces a failed connect (e.g. refused); `peer_addr` succeeds once
            // connected and is `NotConnected` while still in progress.
            loop {
                if scheduler::is_cancelled() {
                    return err_result(scheduler::CANCELLED_MESSAGE);
                }
                match stream.take_error() {
                    Ok(None) => {}
                    Ok(Some(e)) => return err_result(&e.to_string()),
                    Err(e) => return err_result(&e.to_string()),
                }
                match stream.peer_addr() {
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::NotConnected => wait_writable(&src),
                    Err(e) => return err_result(&e.to_string()),
                }
            }
            ok_result(net_handle_value(NetObject::Conn { sock: Mutex::new(stream), src }))
        }
        Err(e) => err_result(&e.to_string()),
    }
}

/// `Net.send`: write all of `bytes` to a connection (parking while the send buffer
/// is full). Returns `Result Unit String`. Consumes both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_send(conn: Value, bytes: Value) -> Value {
    let result = {
        let obj = net_of(conn);
        match &**obj {
            NetObject::Conn { sock, src } => {
                // SAFETY: `bytes` is a boxed `Bytes`; the slice is valid until the
                // owned `bytes` reference is dropped below.
                let data = unsafe { crate::bytes_bytes(bytes) };
                send_loop(sock, src, data)
            }
            NetObject::Listener { .. } | NetObject::Udp { .. } => {
                Err("send: handle is not a connection".to_owned())
            }
        }
    };
    crate::fai_drop(conn);
    crate::fai_drop(bytes);
    match result {
        Ok(()) => ok_result(crate::FAI_UNIT),
        Err(e) => err_result(&e),
    }
}

fn send_loop(sock: &Mutex<TcpStream>, src: &Arc<IoSource>, data: &[u8]) -> Result<(), String> {
    let mut written = 0;
    while written < data.len() {
        if scheduler::is_cancelled() {
            return Err(scheduler::CANCELLED_MESSAGE.to_owned());
        }
        let res = sock.lock().expect("connection lock").write(&data[written..]);
        match res {
            Ok(0) => return Err("send: connection closed".to_owned()),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_writable(src),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// `Net.recv`: read up to `max` bytes from a connection (parking until data is
/// available). Returns `Result Bytes String`; an empty buffer signals end of
/// stream (the peer closed). Consumes the connection reference; `max` is an `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_recv(conn: Value, max: Value) -> Value {
    let cap = crate::unbox_int(max).max(0) as usize;
    let result = {
        let obj = net_of(conn);
        match &**obj {
            NetObject::Conn { sock, src } => recv_loop(sock, src, cap),
            NetObject::Listener { .. } | NetObject::Udp { .. } => {
                Err("recv: handle is not a connection".to_owned())
            }
        }
    };
    crate::fai_drop(conn);
    crate::fai_drop(max);
    match result {
        Ok(buf) => ok_result(crate::make_bytes(&buf)),
        Err(e) => err_result(&e),
    }
}

fn recv_loop(sock: &Mutex<TcpStream>, src: &Arc<IoSource>, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; cap];
    loop {
        if scheduler::is_cancelled() {
            return Err(scheduler::CANCELLED_MESSAGE.to_owned());
        }
        let res = sock.lock().expect("connection lock").read(&mut buf);
        match res {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_readable(src),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `Net.close`: shut down a connection (a no-op extra on a listener) and release
/// this reference. The socket's fd closes once the last reference is dropped (its
/// `Drop` deregisters from the reactor). Returns `Unit`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_close(handle: Value) -> Value {
    {
        let obj = net_of(handle);
        if let NetObject::Conn { sock, .. } = &**obj
            && let Ok(s) = sock.lock()
        {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
    crate::fai_drop(handle);
    crate::FAI_UNIT
}

// ---------------------------------------------------------------------------
// UDP sockets.
// ---------------------------------------------------------------------------
//
// A `UdpSocket` Fai value is a `KIND_NET` handle like the TCP ones. UDP is
// connectionless: `udpSend` addresses each datagram (`host`/`port`) and `udpRecv`
// reports the sender, so there is no accept/connect. Datagrams are whole messages:
// `udpRecv` returns one datagram (truncated to the buffer) per call.

/// `Net.udpBind`: bind a UDP socket to `0.0.0.0:port` (port `0` picks a free one).
/// Returns `Result UdpSocket String`. The port is an immediate `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_udp_bind(port: Value) -> Value {
    let p = crate::unbox_int(port) as u16;
    crate::fai_drop(port);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, p));
    match UdpSocket::bind(addr) {
        Ok(mut sock) => match register(&mut sock) {
            Ok(src) => ok_result(net_handle_value(NetObject::Udp { sock: Mutex::new(sock), src })),
            Err(e) => err_result(&e.to_string()),
        },
        Err(e) => err_result(&e.to_string()),
    }
}

/// `Net.udpLocalPort`: the port a UDP socket is bound to (`0` if unavailable).
/// Consumes the socket reference.
#[unsafe(no_mangle)]
pub extern "C" fn fai_udp_local_port(sock: Value) -> Value {
    let port = {
        let obj = net_of(sock);
        match &**obj {
            NetObject::Udp { sock, .. } => {
                sock.lock().expect("udp lock").local_addr().map_or(0, |a| a.port())
            }
            NetObject::Listener { .. } | NetObject::Conn { .. } => 0,
        }
    };
    crate::fai_drop(sock);
    crate::fai_box_int(i64::from(port))
}

/// `Net.udpSend`: send `bytes` as one datagram to `host:port` (resolving a hostname
/// on the blocking pool). Returns `Result Unit String`. Consumes `sock` and
/// `bytes`; `host` is a boxed `String` and `port` an immediate `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_udp_send(sock: Value, host: Value, port: Value, bytes: Value) -> Value {
    // SAFETY: `host` is a boxed `String`.
    let h = unsafe { crate::string_str(host) }.to_owned();
    let p = crate::unbox_int(port) as u16;
    crate::fai_drop(host);
    crate::fai_drop(port);
    let dest = match resolve_addr(h, p) {
        Ok(a) => a,
        Err(e) => {
            crate::fai_drop(sock);
            crate::fai_drop(bytes);
            return err_result(&e);
        }
    };
    let result = {
        let obj = net_of(sock);
        match &**obj {
            NetObject::Udp { sock, src } => {
                // SAFETY: `bytes` is a boxed `Bytes`, valid until dropped below.
                let data = unsafe { crate::bytes_bytes(bytes) };
                udp_send_loop(sock, src, data, dest)
            }
            NetObject::Listener { .. } | NetObject::Conn { .. } => {
                Err("udpSend: handle is not a UDP socket".to_owned())
            }
        }
    };
    crate::fai_drop(sock);
    crate::fai_drop(bytes);
    match result {
        Ok(()) => ok_result(crate::FAI_UNIT),
        Err(e) => err_result(&e),
    }
}

fn udp_send_loop(
    sock: &Mutex<UdpSocket>,
    src: &Arc<IoSource>,
    data: &[u8],
    dest: SocketAddr,
) -> Result<(), String> {
    loop {
        if scheduler::is_cancelled() {
            return Err(scheduler::CANCELLED_MESSAGE.to_owned());
        }
        let res = sock.lock().expect("udp lock").send_to(data, dest);
        match res {
            // A datagram is sent whole; a short count should not happen for UDP.
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_writable(src),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `Net.udpRecv`: receive one datagram (up to `max` bytes; a longer datagram is
/// truncated), parking until one arrives. Returns `Result (Bytes * String * Int)
/// String` — the payload and the sender's host and port. Consumes the socket
/// reference; `max` is an `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_udp_recv(sock: Value, max: Value) -> Value {
    let cap = crate::unbox_int(max).max(0) as usize;
    let result = {
        let obj = net_of(sock);
        match &**obj {
            NetObject::Udp { sock, src } => udp_recv_loop(sock, src, cap),
            NetObject::Listener { .. } | NetObject::Conn { .. } => {
                Err("udpRecv: handle is not a UDP socket".to_owned())
            }
        }
    };
    crate::fai_drop(sock);
    crate::fai_drop(max);
    match result {
        Ok((buf, from)) => {
            let data = crate::make_bytes(&buf);
            let host = crate::make_string(from.ip().to_string().as_bytes());
            let port = crate::fai_box_int(i64::from(from.port()));
            // A 3-tuple `(Bytes, String, Int)` is a tag-0 data value with 3 fields.
            // SAFETY: three owned fields move into the tuple.
            let tuple = unsafe { crate::fai_make_data(0, 3, [data, host, port].as_ptr()) };
            ok_result(tuple)
        }
        Err(e) => err_result(&e),
    }
}

fn udp_recv_loop(
    sock: &Mutex<UdpSocket>,
    src: &Arc<IoSource>,
    cap: usize,
) -> Result<(Vec<u8>, SocketAddr), String> {
    let mut buf = vec![0u8; cap];
    loop {
        if scheduler::is_cancelled() {
            return Err(scheduler::CANCELLED_MESSAGE.to_owned());
        }
        let res = sock.lock().expect("udp lock").recv_from(&mut buf);
        match res {
            Ok((n, from)) => {
                buf.truncate(n);
                return Ok((buf, from));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_readable(src),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// `Net.udpClose`: release this UDP socket reference (the fd closes once the last
/// reference is dropped). Returns `Unit`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_udp_close(sock: Value) -> Value {
    crate::fai_drop(sock);
    crate::FAI_UNIT
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use mio::net::TcpStream;

    use super::*;
    use crate::scheduler::block_on;

    fn imm(n: i64) -> crate::Value {
        (n << 1) | 1
    }
    fn of_imm(v: crate::Value) -> i64 {
        v >> 1
    }

    #[test]
    fn reactor_wakes_a_task_on_connection_and_readability() {
        // Exercises the whole reactor path end to end against a plain blocking std
        // peer: a non-blocking connect completes asynchronously (the socket becomes
        // *writable* once connected), and a byte the peer sends later arrives
        // asynchronously (the socket becomes *readable*). The task parks on each and
        // the reactor (poll thread + readiness latch) wakes it — covering both
        // directions of [`wait_writable`]/[`wait_readable`].
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Send a byte a little later, so the client must park on readability.
            std::thread::sleep(std::time::Duration::from_millis(20));
            stream.write_all(b"x").expect("write");
            std::thread::sleep(std::time::Duration::from_millis(20));
            drop(stream);
        });

        let r = block_on(Box::new(move || {
            let mut client = TcpStream::connect(addr).expect("connect");
            let src = register(&mut client).expect("register");
            // Wait for the connection to complete, checking directly each iteration
            // (an already-completed connect's readiness can predate registration and
            // not be re-reported by an edge-triggered poll, so a bare wait could hang).
            loop {
                match client.peer_addr() {
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::NotConnected => wait_writable(&src),
                    Err(e) => panic!("connect: {e}"),
                }
            }
            // Read the peer's byte, parking on readability until it arrives.
            let mut buf = [0u8; 1];
            let n = loop {
                match client.read(&mut buf) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_readable(&src),
                    Err(e) => panic!("read: {e}"),
                }
            };
            deregister(&src, &mut client);
            imm(i64::from(n == 1 && buf[0] == b'x'))
        }));

        assert_eq!(of_imm(r), 1, "the task connected, then woke to read the byte");
        peer.join().expect("peer thread");
    }

    /// Unwraps a `Result` value built by the net ops: `Ok` (tag 0) yields the
    /// duplicated payload; `Err` (tag 1) panics with the error string.
    ///
    /// # Safety
    /// `result` is a boxed `Result` cell (an `Ok`/`Err` data value).
    unsafe fn unwrap_ok(result: Value) -> Value {
        // SAFETY: a `Result` is a boxed data cell with its tag and one field inline.
        unsafe {
            let p = crate::as_obj(result);
            let tag = crate::read_u64(p, crate::DATA_TAG_OFFSET);
            let field = crate::read_i64(p, crate::DATA_FIELDS_OFFSET);
            if tag != 0 {
                let msg = String::from_utf8_lossy(crate::string_bytes(field)).into_owned();
                panic!("expected Ok, got Err: {msg}");
            }
            let v = crate::fai_dup(field);
            crate::fai_drop(result);
            v
        }
    }

    #[test]
    fn tcp_loopback_echo_through_the_net_ops() {
        // A full TCP round trip through the runtime `fai_net_*` operations: a server
        // task accepts one connection, reads, and echoes; the client connects,
        // sends, and reads the echo back — all on the scheduler, parking on the
        // reactor at each would-block. Drives accept/connect/send/recv/close and the
        // `Bytes` payloads end to end.
        let result = block_on(Box::new(|| {
            // SAFETY: each `fai_net_*` returns a `Result`; `unwrap_ok` extracts `Ok`.
            unsafe {
                let listener = unwrap_ok(fai_net_listen(imm(0)));
                let port = of_imm(fai_net_local_port(crate::fai_dup(listener)));

                let server = crate::scheduler::spawn(Box::new(move || {
                    let conn = unwrap_ok(fai_net_accept(listener));
                    let data = unwrap_ok(fai_net_recv(crate::fai_dup(conn), imm(64)));
                    let _ = unwrap_ok(fai_net_send(crate::fai_dup(conn), data));
                    fai_net_close(conn);
                    imm(0)
                }));

                let host = crate::make_string(b"127.0.0.1");
                let conn = unwrap_ok(fai_net_connect(host, imm(port)));
                let sent = crate::make_bytes(b"ping");
                let _ = unwrap_ok(fai_net_send(crate::fai_dup(conn), sent));
                let echo = unwrap_ok(fai_net_recv(crate::fai_dup(conn), imm(64)));
                let ok = crate::bytes_bytes(echo) == b"ping";
                crate::fai_drop(echo);
                fai_net_close(conn);
                crate::scheduler::await_handle(&server);
                imm(i64::from(ok))
            }
        }));
        assert_eq!(of_imm(result), 1, "the client received the echoed bytes");
    }

    /// Reads a `(Bytes, String, Int)` datagram tuple (`udpRecv`'s payload), dupping
    /// each field out and dropping the tuple.
    ///
    /// # Safety
    /// `t` is a boxed tag-0 data value with three fields.
    unsafe fn read_datagram(t: Value) -> (Value, Value, Value) {
        // SAFETY: `t` is a boxed 3-tuple; its fields are inline.
        unsafe {
            let p = crate::as_obj(t);
            let data = crate::fai_dup(crate::read_i64(p, crate::DATA_FIELDS_OFFSET));
            let host = crate::fai_dup(crate::read_i64(p, crate::DATA_FIELDS_OFFSET + 8));
            let port = crate::fai_dup(crate::read_i64(p, crate::DATA_FIELDS_OFFSET + 16));
            crate::fai_drop(t);
            (data, host, port)
        }
    }

    #[test]
    fn udp_loopback_roundtrip_through_the_net_ops() {
        // A UDP round trip through the runtime `fai_udp_*` operations: a client
        // socket sends a datagram to a server socket, which receives it (with the
        // sender's address) and echoes it back to that address; the client then
        // receives the echo. Datagrams are buffered by the kernel, so this runs in
        // one task — each `udpRecv` parks on the reactor until its datagram arrives.
        let result = block_on(Box::new(|| {
            // SAFETY: each `fai_udp_*` returns a `Result`; `unwrap_ok` extracts `Ok`.
            unsafe {
                let server = unwrap_ok(fai_udp_bind(imm(0)));
                let server_port = of_imm(fai_udp_local_port(crate::fai_dup(server)));
                let client = unwrap_ok(fai_udp_bind(imm(0)));

                let host = crate::make_string(b"127.0.0.1");
                let payload = crate::make_bytes(b"ping");
                let _ = unwrap_ok(fai_udp_send(
                    crate::fai_dup(client),
                    host,
                    imm(server_port),
                    payload,
                ));

                // The server receives the datagram and its sender, then echoes back.
                let (data, from_host, from_port) =
                    read_datagram(unwrap_ok(fai_udp_recv(crate::fai_dup(server), imm(64))));
                let _ = unwrap_ok(fai_udp_send(crate::fai_dup(server), from_host, from_port, data));

                // The client receives the echo.
                let (echo, echo_host, echo_port) =
                    read_datagram(unwrap_ok(fai_udp_recv(crate::fai_dup(client), imm(64))));
                let ok = crate::bytes_bytes(echo) == b"ping";
                crate::fai_drop(echo);
                crate::fai_drop(echo_host);
                crate::fai_drop(echo_port);

                fai_udp_close(server);
                fai_udp_close(client);
                imm(i64::from(ok))
            }
        }));
        assert_eq!(of_imm(result), 1, "the client received the echoed datagram");
    }

    #[test]
    fn sleep_until_wakes_after_its_deadline() {
        // A task sleeps on the reactor timer and resumes only once the deadline has
        // passed (parking, not busy-waiting): the reactor fires the timer and unparks.
        let start = Instant::now();
        let r = block_on(Box::new(|| {
            sleep_until(Instant::now() + Duration::from_millis(40));
            imm(1)
        }));
        assert_eq!(of_imm(r), 1);
        assert!(
            start.elapsed() >= Duration::from_millis(35),
            "slept ~40ms, got {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn sleep_with_a_past_deadline_returns_immediately() {
        // A deadline already in the past does not register a timer or park.
        let r = block_on(Box::new(|| {
            sleep_until(Instant::now() - Duration::from_millis(10));
            imm(7)
        }));
        assert_eq!(of_imm(r), 7);
    }

    #[test]
    fn many_concurrent_sleeps_multiplex_on_the_timer() {
        // Far more sleeping tasks than workers all park on the reactor timer at once
        // and fire around the same time, so the whole batch finishes in ~one sleep
        // duration rather than serializing — proving sleep parks (frees its worker)
        // rather than blocking one.
        let start = Instant::now();
        let r = block_on(Box::new(|| {
            let handles: Vec<_> = (0..300)
                .map(|_| {
                    crate::scheduler::spawn(Box::new(|| {
                        sleep_until(Instant::now() + Duration::from_millis(40));
                        imm(1)
                    }))
                })
                .collect();
            let mut sum = 0;
            for h in &handles {
                sum += of_imm(crate::scheduler::await_handle(h));
            }
            imm(sum)
        }));
        assert_eq!(of_imm(r), 300, "every sleeping task completed");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "sleeps multiplexed, took {:?}",
            start.elapsed()
        );
    }
}
