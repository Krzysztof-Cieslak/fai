//! Scoped HTTP response lifetime and connection reuse over real loopback sockets.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;

use fai_db::Db;

static SERIAL: Mutex<()> = Mutex::new(());

fn head(socket: &mut TcpStream) {
    socket.set_read_timeout(Some(std::time::Duration::from_secs(15))).unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
}

fn run(port: u16, body: &str) -> String {
    let source = format!(
        r#"module Scoped
fetch : Runtime -> Http.Client -> (Http.Response {{ Concurrency, Net, Tls }} -> 'a / 'e) -> Result 'a String / {{ Concurrency, Net, Tls | 'e }}
let fetch r client consume =
  match Url.parse "http://127.0.0.1:{port}/" with
  | Err e -> Err e
  | Ok url -> Http.withResponseOn r client {{ method = Http.GET, url = url, headers = Headers.empty, body = Http.emptyBody }} consume
read : Http.Response {{ Concurrency, Net, Tls }} -> Result String String / {{ Concurrency, Net, Tls }}
let read response = Http.bodyText response.body
session : Runtime -> Http.Client -> String / {{ Concurrency, Net, Tls }}
{body}
public main : Runtime -> Unit / {{ Console, Concurrency, Net, Tls }}
let main r = r.console.writeLine (Http.withClientWith r None (session r))
"#
    );
    run_source(source)
}

fn run_source(source: String) -> String {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Scoped.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{output}\n{:?}", result.diagnostics);
    output
}

#[test]
fn completed_scoped_body_returns_connection_to_pool() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        head(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n0\r\nX-End: yes\r\n\r\n").unwrap();
        head(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ntwo").unwrap();
    });
    let output = run(
        port,
        r#"
let session r client =
  let a = fetch r client read
  let b = fetch r client read
  if a = Ok (Ok "one") && b = Ok (Ok "two") then "ok" else "bad"
"#,
    );
    server.join().unwrap();
    assert_eq!(output, "ok\n");
}

#[test]
fn unread_scoped_body_closes_before_next_request() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        head(&mut first);
        first.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").unwrap();
        assert_eq!(first.read(&mut [0]).unwrap(), 0);
        let (mut second, _) = listener.accept().unwrap();
        head(&mut second);
        second.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
    });
    let output = run(
        port,
        r#"
let session r client =
  let a = fetch r client (fun response -> response.status)
  let b = fetch r client read
  if a = Ok 200 && b = Ok (Ok "ok") then "ok" else "bad"
"#,
    );
    server.join().unwrap();
    assert_eq!(output, "ok\n");
}

#[test]
fn escaped_scoped_body_cannot_read_after_callback() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        head(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").unwrap();
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    });
    let output = run(
        port,
        r#"
let session r client =
  match fetch r client (fun response -> response.body) with
  | Err e -> e
  | Ok body -> if Http.bodyText body = Err "response scope has ended" then "ok" else "bad"
"#,
    );
    server.join().unwrap();
    assert_eq!(output, "ok\n");
}

#[test]
fn cancellation_closes_an_unfinished_scoped_body() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        head(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").unwrap();
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    });
    let output = run(
        port,
        r#"
consumeReady : Runtime -> Channel Unit -> Http.Response { Concurrency, Net, Tls } -> Result String String / { Concurrency, Net, Tls }
let consumeReady r ready response =
  let signalled = r.concurrency.send ready ()
  Http.bodyText response.body
inScope : Runtime -> Http.Client -> Nursery -> String / { Concurrency, Net, Tls }
let inScope r client nursery =
  let ready = r.concurrency.channel 1
  let worker = r.concurrency.spawn nursery (fun _ -> fetch r client (consumeReady r ready))
  let started = r.concurrency.recv ready
  let cancelled = r.concurrency.cancel worker
  let joined = r.concurrency.await worker
  "ok"
let session r client = r.concurrency.scope (inScope r client)
"#,
    );
    server.join().unwrap();
    assert_eq!(output, "ok\n");
}

#[test]
fn pool_trusts_its_configured_extra_roots() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let pem = cert.cert.pem().replace('\n', "\\n");
    let key = cert.key_pair.serialize_pem().replace('\n', "\\n");
    let output = run_source(format!(
        r#"module Scoped
let handler request = Ok (Http.textResponse 200 "secure")
fetch : Runtime -> Int -> Http.Client -> String / {{ Concurrency, Net, Tls }}
let fetch r port client =
  match Url.parse ("https://127.0.0.1:" ++ Int.toString port ++ "/") with
  | Err e -> e
  | Ok url ->
    let request = {{ method = Http.GET, url = url, headers = Headers.empty, body = Http.emptyBody }}
    match Http.withResponseOn r client request (fun response -> Http.bodyText response.body) with
    | Ok (Ok text) -> text
    | _ -> "failed"
serve : Runtime -> Listener -> Nursery -> Unit / {{ Console, Concurrency, Net, Tls }}
let serve r listener nursery =
  let server = r.concurrency.spawn nursery (fun _ -> Http.serveListenerTls r listener (Bytes.fromString "{pem}") (Bytes.fromString "{key}") handler)
  let text = Http.withClientWith r (Some (Bytes.fromString "{pem}")) (fetch r (r.net.localPort listener))
  let cancelled = r.concurrency.cancel server
  r.console.writeLine text
public main : Runtime -> Unit / {{ Console, Concurrency, Net, Tls }}
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (serve r listener)
"#
    ));
    assert_eq!(output, "secure\n");
}
