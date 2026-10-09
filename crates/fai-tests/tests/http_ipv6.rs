//! URL authority brackets stay on the wire but are removed for socket/TLS hosts.

use fai_db::Db;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

fn serve(mut stream: impl Read + Write, port: u16) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.contains(&format!("Host: [::1]:{port}\r\n")), "{head}");
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
    stream.flush().unwrap();
}

#[track_caller]
fn check(tls: bool, pooled: bool) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = match std::net::TcpListener::bind("[::1]:0") {
        Ok(listener) => listener,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("IPv6 bind: {error}"),
    };
    let port = listener.local_addr().unwrap().port();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["::1".into()]).unwrap();
    let pem = cert.cert.pem();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )
        .unwrap();
    let peer = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        // This startup wait includes cold JIT compilation of the client.
        let deadline = Instant::now() + Duration::from_secs(180);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "IPv6 client did not connect");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("IPv6 accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        if tls {
            serve(
                rustls::StreamOwned::new(
                    rustls::ServerConnection::new(Arc::new(config)).unwrap(),
                    stream,
                ),
                port,
            );
        } else {
            serve(stream, port);
        }
    });
    let url = format!("{}://[::1]:{port}/", if tls { "https" } else { "http" });
    let request = if pooled {
        format!("Http.withClient r (fun client -> readResponse (Http.getOn r client {url:?}))")
    } else if tls {
        format!("readResponse (Http.getWith r (Some (Bytes.fromString {pem:?})) {url:?})")
    } else {
        format!("readResponse (Http.get r {url:?})")
    };
    let effects = if pooled { "Concurrency, Console, Net, Tls" } else { "Console, Net, Tls" };
    let source = format!(
        "module Main\nreadResponse : Result (Http.Response 'e) String -> String / 'e\nlet readResponse result =\n  match result with\n  | Err e -> e\n  | Ok response -> Result.withDefault \"body error\" (Http.bodyText response.body)\npublic main : Runtime -> Unit / {{ {effects} }}\nlet main r = r.console.writeLine ({request})\n"
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
    peer.join().unwrap();
}

#[test]
fn ipv6_http_connects_and_preserves_host_header() {
    check(false, false);
}
#[test]
fn ipv6_pool_uses_the_unbracketed_socket_address() {
    check(false, true);
}
#[test]
fn ipv6_https_validates_the_ip_certificate_name() {
    check(true, false);
}
