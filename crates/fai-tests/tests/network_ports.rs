//! Native port validation precedes DNS, connection, and datagram side effects.

use fai_db::Db;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn check(body: &str) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source = format!(
        "module Main\nlet invalid result =\n  match result with\n  | Err message -> message = \"port must be in 0..65535\"\n  | Ok value -> false\ncheck : Runtime -> Bool / {{ Net }}\nlet check r =\n{body}\npublic main : Runtime -> Unit / {{ Console, Net }}\nlet main r = r.console.writeLine (if check r then \"ok\" else \"failed\")\n"
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

#[test]
fn invalid_listen_does_not_request_an_ephemeral_port() {
    check("invalid (r.net.listen 65536)");
}

#[test]
fn invalid_udp_bind_rejects_a_boxed_port() {
    check("invalid (r.net.udpBind 9223372036854775807)");
}

#[test]
fn invalid_connect_never_reaches_the_wrapped_port() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = i64::from(listener.local_addr().unwrap().port()) + 65536;
    check(&format!("invalid (r.net.connect \"127.0.0.1\" {port})"));
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn invalid_udp_send_releases_payload_without_sending() {
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = i64::from(receiver.local_addr().unwrap().port()) + 65536;
    check(&format!(
        "match r.net.udpBind 0 with\n| Err e -> false\n| Ok socket -> invalid (r.net.udpSend socket \"127.0.0.1\" {port} (Bytes.fromString \"not sent\"))"
    ));
    receiver.set_nonblocking(true).unwrap();
    assert_eq!(receiver.recv(&mut [0; 16]).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
