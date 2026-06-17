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

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use mio::event::Source;
use mio::{Events, Interest, Poll, Registry, Token};

use crate::scheduler::{self, Parked};

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

/// The global reactor: a `mio::Poll` on its own thread plus a cloned `Registry`
/// (used by workers to register their sockets) and the token→source table.
struct Reactor {
    registry: Registry,
    sources: Mutex<HashMap<usize, Arc<IoSource>>>,
    next_token: AtomicUsize,
}

static REACTOR: OnceLock<Reactor> = OnceLock::new();

/// Returns the global reactor, starting its thread on first use.
fn reactor() -> &'static Reactor {
    REACTOR.get_or_init(|| {
        let poll = Poll::new().expect("create the network reactor poll");
        let registry = poll.registry().try_clone().expect("clone the reactor registry");
        std::thread::Builder::new()
            .name("fai-reactor".to_owned())
            .spawn(move || reactor_loop(poll))
            .expect("spawn the network reactor thread");
        Reactor { registry, sources: Mutex::new(HashMap::new()), next_token: AtomicUsize::new(1) }
    })
}

/// The reactor thread: poll for readiness forever, waking the task waiting on each
/// ready direction of each signalled socket.
fn reactor_loop(mut poll: Poll) -> ! {
    let mut events = Events::with_capacity(256);
    loop {
        if let Err(e) = poll.poll(&mut events, None) {
            // A signal can interrupt the poll; just poll again. Any other error is
            // not actionable here, so retry rather than abort the whole program.
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            continue;
        }
        for event in events.iter() {
            let token = event.token().0;
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
/// after a syscall on the socket returned `WouldBlock`.
pub fn wait_readable(io: &Arc<IoSource>) {
    wait(&io.read);
}

/// Parks the current task until `io` is writable (e.g. a connect completes or send
/// buffer space frees). Must be called inside a task.
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
use std::net::{Ipv4Addr, Shutdown, SocketAddr, ToSocketAddrs};

use mio::net::{TcpListener, TcpStream};

use crate::Value;

/// The reactor-side state of a network socket, owned by its Fai handle cell.
enum NetObject {
    Listener { sock: Mutex<TcpListener>, src: Arc<IoSource> },
    Conn { sock: Mutex<TcpStream>, src: Arc<IoSource> },
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
        }
    }
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
            NetObject::Conn { .. } => 0,
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
            NetObject::Conn { .. } => Err("accept: handle is not a listener".to_owned()),
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

/// `Net.connect`: connect to `host:port`, resolving `host` on the blocking pool and
/// then connecting without blocking a worker. Returns `Result Connection String`.
/// Consumes `host`; `port` is an immediate `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_net_connect(host: Value, port: Value) -> Value {
    // SAFETY: `host` is a boxed `String`.
    let h = unsafe { crate::string_str(host) }.to_owned();
    let p = crate::unbox_int(port) as u16;
    crate::fai_drop(host);
    crate::fai_drop(port);
    // Resolve on the blocking pool (a DNS lookup may block); the task parks.
    let resolved: Result<SocketAddr, String> =
        crate::scheduler::run_blocking(Box::new(move || {
            (h.as_str(), p)
                .to_socket_addrs()
                .map_err(|e| e.to_string())
                .and_then(|mut addrs| addrs.next().ok_or_else(|| "no address for host".to_owned()))
        }));
    let addr = match resolved {
        Ok(a) => a,
        Err(e) => return err_result(&e),
    };
    match TcpStream::connect(addr) {
        Ok(mut stream) => {
            let src = match register(&mut stream) {
                Ok(s) => s,
                Err(e) => return err_result(&e.to_string()),
            };
            // A non-blocking connect completes asynchronously: it is writable once
            // done; then `take_error` reports a failed connect (e.g. refused).
            wait_writable(&src);
            match stream.take_error() {
                Ok(None) => {}
                Ok(Some(e)) => return err_result(&e.to_string()),
                Err(e) => return err_result(&e.to_string()),
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
            NetObject::Listener { .. } => Err("send: handle is not a connection".to_owned()),
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
            NetObject::Listener { .. } => Err("recv: handle is not a connection".to_owned()),
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
            // Park until the connection completes (writable).
            wait_writable(&src);
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
}
