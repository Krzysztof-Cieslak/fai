//! Broken vectored sends return errors without changing Unix signal handling.

use std::net::{Shutdown, TcpListener, TcpStream as StdTcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;

static SIGNALS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn received_signal(_: libc::c_int) {
    SIGNALS.fetch_add(1, Ordering::SeqCst);
}

#[track_caller]
fn run_child(mode: &str) {
    use wait_timeout::ChildExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "reactor::send_signal_tests::signal_worker", "--nocapture"])
        .env("FAI_SEND_SIGNAL_CASE", mode)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let status = child.wait_timeout(Duration::from_secs(20)).unwrap();
    if status.is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(status.is_some() && output.status.success(), "{mode}: {output:?}");
}

#[test]
fn broken_vectored_send_survives_default_sigpipe() {
    run_child("default");
}

#[test]
fn broken_vectored_send_preserves_custom_sigpipe_handler() {
    run_child("handler");
}

#[test]
fn signal_worker() {
    let Ok(mode) = std::env::var("FAI_SEND_SIGNAL_CASE") else { return };
    let handler = if mode == "handler" {
        received_signal as *const () as libc::sighandler_t
    } else {
        libc::SIG_DFL
    };
    // SAFETY: this isolated subprocess owns its signal disposition. The custom
    // handler only increments a lock-free atomic and no test runs alongside it.
    assert_ne!(unsafe { libc::signal(libc::SIGPIPE, handler) }, libc::SIG_ERR);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let sender = StdTcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    // A local write shutdown deterministically produces EPIPE, without racing
    // remote FIN/RST delivery. The ordinary vectored write would raise SIGPIPE.
    sender.shutdown(Shutdown::Write).unwrap();
    sender.set_nonblocking(true).unwrap();
    let mut sender = mio::net::TcpStream::from_std(sender);
    let source = register(&mut sender).unwrap();
    let before = (crate::live_count(), crate::live_bytes());
    let connection = net_handle_value(NetObject::Conn { sock: Mutex::new(sender), src: source });
    let chunks = crate::cons_list(&[crate::make_bytes(b"header"), crate::make_bytes(b"body")]);
    let result = fai_net_send_many(connection, chunks);
    assert_eq!(crate::read_int(crate::fai_data_tag(result)), 1);
    let message = crate::fai_data_field(result, 0);
    assert!(!crate::read_string(message).is_empty());
    crate::fai_drop(message);
    crate::fai_drop(result);
    assert_eq!((crate::live_count(), crate::live_bytes()), before);
    assert_eq!(SIGNALS.load(Ordering::SeqCst), 0);
    // SAFETY: only this process controls SIGPIPE. Reinstalling the same handler
    // also checks that the send operation did not alter the global disposition.
    assert_eq!(unsafe { libc::signal(libc::SIGPIPE, handler) }, handler);
}
