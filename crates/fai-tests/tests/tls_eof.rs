//! TLS close-delimited HTTP bodies distinguish close-notify from transport truncation.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fai_db::Db;

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn response_eof(clean: bool) -> String {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let pem = cert.cert.pem();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (done, finished) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "TLS client did not connect");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("TLS accept: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        socket.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\npayload").unwrap();
        stream.flush().unwrap();
        if clean {
            stream.conn.send_close_notify();
            stream.flush().unwrap();
            // TCP stays open until the Fai caller has finished reading the TLS
            // body, proving it did not wait for an unrelated transport EOF.
            finished
                .recv_timeout(Duration::from_secs(15))
                .expect("client completed at close-notify");
        }
    });
    let source = format!(
        r#"module Main
public main : Runtime -> Unit / {{ Console, Net, Tls }}
let main r =
  match Http.getWith r (Some (Bytes.fromString {pem:?})) "https://127.0.0.1:{port}/" with
  | Err e -> r.console.writeLine ("connection error: " ++ e)
  | Ok response ->
    match Http.bodyText response.body with
    | Err e -> r.console.writeLine "truncated"
    | Ok text -> r.console.writeLine text
"#
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    let file = db.source_file(id).unwrap();
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let output = fai_runtime::capture_take();
    let _ = done.send(());
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    peer.join().unwrap();
    output
}

#[test]
fn close_notify_completes_a_body_without_waiting_for_tcp_eof() {
    assert_eq!(response_eof(true), "payload\n");
}

#[test]
fn raw_tcp_eof_does_not_authenticate_a_close_delimited_body() {
    assert_eq!(response_eof(false), "truncated\n");
}
