//! Persistent server connections, ordered requests, and response lifetime.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fai_db::Db;

static SERIAL: Mutex<()> = Mutex::new(());

const HANDLER: &str = r#"
handle : Runtime -> Http.Request {} -> Result (Http.Response {}) String
let handle r request =
  let path = Url.path request.url
  let response =
    if path = "/chunked" then
      Http.chunkedResponse 200 Headers.empty (Stream.fromList [Bytes.fromString "one", Bytes.fromString "two"])
    else if path = "/head" then
      Http.response 200 (Stream.failed "body must not run")
    else if path = "/204" then
      Http.response 204 (Stream.failed "body must not run")
    else if path = "/304" then
      Http.response 304 (Stream.failed "body must not run")
    else
      Http.textResponse 200 (path ++ ":" ++ Result.withDefault "bad body" (Http.bodyText request.body))
  if path = "/close" then
    Ok { response with headers = Headers.fromList [("Connection", "keep-alive, ClOsE")] }
  else if path = "/error" then
    Err "handler failure"
  else
    Ok response
"#;

#[track_caller]
fn server(
    handler: &str,
    managed: bool,
    peer: impl FnOnce(&mut BufReader<TcpStream>) + Send + 'static,
) {
    server_with(handler, "  let app = handle r", managed, peer);
}

#[track_caller]
fn server_with(
    handler: &str,
    setup: &str,
    managed: bool,
    peer: impl FnOnce(&mut BufReader<TcpStream>) + Send + 'static,
) {
    server_config(handler, setup, managed, None, false, peer);
}

#[track_caller]
fn server_config(
    handler: &str,
    setup: &str,
    managed: bool,
    tls: Option<(String, String)>,
    aot: bool,
    peer: impl FnOnce(&mut BufReader<TcpStream>) + Send + 'static,
) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let coordinator = TcpListener::bind("127.0.0.1:0").unwrap();
    coordinator.set_nonblocking(true).unwrap();
    let port = coordinator.local_addr().unwrap().port();
    let client = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut control = loop {
            match coordinator.accept() {
                Ok((socket, _)) => {
                    // Accepted sockets may inherit the listener's nonblocking mode.
                    socket.set_nonblocking(false).unwrap();
                    break BufReader::new(socket);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "server did not announce its port");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("coordinator: {error}"),
            }
        };
        control.get_mut().set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let mut line = String::new();
        control.read_line(&mut line).unwrap();
        let server_port: u16 = line.trim().parse().unwrap();
        let socket = TcpStream::connect(("127.0.0.1", server_port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        socket.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
        socket.set_nodelay(true).unwrap();
        let mut socket = BufReader::new(socket);
        peer(&mut socket);
        // Keep the HTTP socket open until cancellation has joined the server.
        control.get_mut().write_all(b"done").unwrap();
        let mut finished = Vec::new();
        control.read_to_end(&mut finished).unwrap();
    });
    let serve = if managed { "serveManagedListener" } else { "serveListener" };
    let (serve, certificates) = tls.map_or_else(
        || (serve.to_owned(), String::new()),
        |(cert, key)| {
            (
                format!("{serve}Tls"),
                format!("(Bytes.fromString {cert:?}) (Bytes.fromString {key:?})"),
            )
        },
    );
    let source = format!(
        r#"module Persistent
{handler}
session : Runtime -> Connection -> Listener -> Nursery -> Unit / {{ Concurrency, Net, Tls }}
let session r control listener nursery =
{setup}
  let task = r.concurrency.spawn nursery (fun _ -> Http.{serve} r listener {certificates} app)
  let announced = r.net.send control (Bytes.fromString (Int.toString (r.net.localPort listener) ++ "\n"))
  let finished = r.net.recv control 16
  let stopped = r.concurrency.cancel task
  ()
public main : Runtime -> Unit / {{ Console, Concurrency, Net, Tls }}
let main r =
  match r.net.connect "127.0.0.1" {port} with
  | Err e -> r.console.writeLine e
  | Ok control ->
    match r.net.listen 0 with
    | Err e -> r.console.writeLine e
    | Ok listener ->
      let finished = r.concurrency.scope (session r control listener)
      r.net.close control
"#
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Persistent.fai".into(), source);
    if aot {
        use wait_timeout::ChildExt;

        let temp = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(temp.path().join("persistent")).unwrap();
        let built = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
        assert!(built.ok, "{:?}", built.diagnostics);
        let mut child = std::process::Command::new(built.artifact.unwrap())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let status = child.wait_timeout(Duration::from_secs(60)).unwrap();
        if status.is_none() {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(status.is_some(), "native server timed out: {output:?}");
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    } else {
        fai_runtime::capture_start();
        let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
        let output = fai_runtime::capture_take();
        assert_eq!(result.exit_code, 0, "{output}\n{:?}", result.diagnostics);
        assert!(output.is_empty(), "{output}");
    }
    client.join().unwrap();
}

#[track_caller]
fn send<T: Read + Write>(socket: &mut BufReader<T>, bytes: &[u8]) {
    socket.get_mut().write_all(bytes).unwrap();
}

#[derive(Debug)]
struct Response {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

#[track_caller]
fn response<T: Read>(socket: &mut BufReader<T>, bodyless: bool) -> Response {
    let mut status = String::new();
    assert!(socket.read_line(&mut status).unwrap() > 0, "connection closed before response");
    let status = status.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        assert!(socket.read_line(&mut line).unwrap() > 0, "truncated headers");
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line.to_ascii_lowercase());
    }
    let mut body = Vec::new();
    if !bodyless {
        if headers.contains("transfer-encoding: chunked") {
            loop {
                let mut size = String::new();
                socket.read_line(&mut size).unwrap();
                let size = usize::from_str_radix(size.trim(), 16).unwrap();
                if size == 0 {
                    let mut end = String::new();
                    socket.read_line(&mut end).unwrap();
                    assert_eq!(end, "\r\n");
                    break;
                }
                let start = body.len();
                body.resize(start + size, 0);
                socket.read_exact(&mut body[start..]).unwrap();
                let mut crlf = [0; 2];
                socket.read_exact(&mut crlf).unwrap();
                assert_eq!(&crlf, b"\r\n");
            }
        } else {
            let length = headers.lines().find_map(|line| line.strip_prefix("content-length: "));
            body.resize(length.expect("response must be framed").parse().unwrap(), 0);
            socket.read_exact(&mut body).unwrap();
        }
    }
    Response { status, headers, body }
}

#[track_caller]
fn closed<T: Read>(socket: &mut BufReader<T>) {
    match socket.read(&mut [0]) {
        Ok(0) => (),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        result => panic!("connection must close: {result:?}"),
    }
}

#[test]
fn sequential_requests_reuse_one_connection() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let first = response(socket, false);
        assert_eq!(first.body, b"/one:");
        assert!(!first.headers.contains("connection: close"));
        send(socket, b"GET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn pipelined_requests_preserve_order_and_buffered_bytes() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn content_length_body_retains_the_following_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"POST /one HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabcGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:abc");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn chunked_body_and_trailers_retain_the_following_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"POST /one HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n2\r\nbc\r\n0\r\nX-End: yes\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:abc");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn empty_content_length_body_preserves_buffered_requests() {
    server(HANDLER, false, |socket| {
        send(socket, b"POST /one HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn unicode_body_lengths_use_bytes_before_the_next_request() {
    server(HANDLER, false, |socket| {
        send(socket, "POST /one HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\n\r\né😀GET /two HTTP/1.1\r\nHost: localhost\r\n\r\n".as_bytes());
        assert_eq!(response(socket, false).body, "/one:é😀".as_bytes());
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn request_close_token_stops_before_a_pipelined_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nconnection: Upgrade, ClOsE\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let first = response(socket, false);
        assert_eq!(first.body, b"/one:");
        assert!(first.headers.contains("connection: close"));
        closed(socket);
    });
}

#[test]
fn response_close_token_stops_before_a_pipelined_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /close HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/close:");
        closed(socket);
    });
}

#[test]
fn http_10_remains_close_by_default() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.0\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        closed(socket);
    });
}

#[test]
fn chunked_response_finishes_before_the_next_response() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /chunked HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"onetwo");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn head_response_does_not_force_its_body_or_consume_next_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"HEAD /head HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, true).status, 200);
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn no_content_response_preserves_the_next_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /204 HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, true).status, 204);
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn not_modified_response_preserves_the_next_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /304 HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, true).status, 304);
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn handler_error_is_framed_and_allows_the_next_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /error HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).status, 500);
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn malformed_second_request_closes_without_an_extra_response() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nPOST /bad HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        closed(socket);
    });
}

#[test]
fn cancellation_joins_idle_keep_alive_connections() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        // The helper keeps this socket alive while it cancels and joins serving.
    });
}

#[test]
fn cancellation_joins_partially_received_requests() {
    server(HANDLER, false, |socket| {
        send(socket, b"POST /partial HTTP/1.1\r\nHost: localhost\r\nContent-Length: 50\r\n\r\nabc");
    });
}

#[test]
fn many_requests_run_with_constant_native_stack() {
    server(HANDLER, false, |socket| {
        for _ in 0..2000 {
            send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert_eq!(response(socket, false).body, b"/one:");
        }
    });
}

#[test]
fn managed_response_producer_finishes_before_next_request() {
    server(
        r#"
produce : Http.BodyProducer {}
let produce write = write (Bytes.fromString "produced")
handle : Runtime -> Http.Request {} -> Result (Http.ServerResponse {}) String
let handle r request =
  if Url.path request.url = "/produce" then
    Ok (Http.produced 200 Headers.empty produce)
  else
    Ok (Http.Ready (Http.textResponse 200 "later"))
"#,
        true,
        |socket| {
            send(socket, b"GET /produce HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert_eq!(response(socket, false).body, b"produced");
            assert_eq!(response(socket, false).body, b"later");
        },
    );
}

#[test]
fn retained_producer_writer_cannot_write_into_the_next_response() {
    server_with(
        r#"
produce : Runtime -> Channel (Bytes -> Result Unit String / { Concurrency, Net, Tls }) -> String -> Http.BodyProducer {}
let produce r writers path write =
  if path = "/save" then
    let saved = r.concurrency.send writers write
    write (Bytes.fromString "first")
  else
    match r.concurrency.recv writers with
    | None -> Err "missing writer"
    | Some previous ->
      let result = previous (Bytes.fromString "corruption")
      write (Bytes.fromString (if result = Err "response writer scope has ended" then "safe" else "unsafe"))
handle : Runtime -> Channel (Bytes -> Result Unit String / { Concurrency, Net, Tls }) -> Http.Request {} -> Result (Http.ServerResponse {}) String
let handle r writers request =
  Ok (Http.produced 200 Headers.empty (produce r writers (Url.path request.url)))
"#,
        "  let writers = r.concurrency.channel 1\n  let app = handle r writers",
        true,
        |socket| {
            send(socket, b"GET /save HTTP/1.1\r\nHost: localhost\r\n\r\nGET /later HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert_eq!(response(socket, false).body, b"first");
            assert_eq!(response(socket, false).body, b"safe");
        },
    );
}

#[test]
fn failed_producer_closes_without_running_next_request() {
    server(
        r#"
produce : Http.BodyProducer {}
let produce write =
  let sent = write (Bytes.fromString "prefix")
  Err "failed producer"
handle : Runtime -> Http.Request {} -> Result (Http.ServerResponse {}) String
let handle r request = Ok (Http.produced 200 Headers.empty produce)
"#,
        true,
        |socket| {
            send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let mut wire = String::new();
            socket.read_to_string(&mut wire).unwrap();
            assert_eq!(wire.matches("HTTP/1.1 200").count(), 1);
            assert!(wire.ends_with("6\r\nprefix\r\n"), "{wire}");
        },
    );
}

#[test]
fn fragmented_request_line_and_body_survive_multiple_reads() {
    server(HANDLER, false, |socket| {
        send(socket, b"POST /on");
        std::thread::sleep(Duration::from_millis(30));
        send(socket, b"e HTTP/1.1\r\nhost: localhost\r\ncontent-length: 3\r\n\r\na");
        std::thread::sleep(Duration::from_millis(30));
        send(socket, b"bcGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:abc");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn clean_eof_after_a_response_ends_the_connection() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        socket.get_mut().shutdown(std::net::Shutdown::Write).unwrap();
        closed(socket);
    });
}

#[test]
fn truncated_second_body_closes_without_a_response() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nPOST /two HTTP/1.1\r\nHost: localhost\r\nContent-Length: 30\r\n\r\nabc");
        assert_eq!(response(socket, false).body, b"/one:");
        socket.get_mut().shutdown(std::net::Shutdown::Write).unwrap();
        closed(socket);
    });
}

#[test]
fn continue_is_sent_before_body_and_preserves_the_next_request() {
    server(HANDLER, false, |socket| {
        send(socket, b"POST /one HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n");
        assert_eq!(response(socket, true).status, 100);
        send(socket, b"abcGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:abc");
        assert_eq!(response(socket, false).body, b"/two:");
    });
}

#[test]
fn malformed_chunk_terminator_stops_the_request_sequence() {
    server(HANDLER, false, |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nPOST /bad HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\naXXGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        closed(socket);
    });
}

#[test]
fn native_server_reuses_connections_and_preserves_pipelined_bodies() {
    server_config(HANDLER, "  let app = handle r", false, None, true, |socket| {
        send(socket, b"POST /one HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabcGET /two HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:abc");
        assert_eq!(response(socket, false).body, b"/two:");
        send(socket, b"GET /close HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/close:");
        closed(socket);
    });
}

#[test]
fn tls_server_reuses_one_session_for_multiple_requests() {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    server_config(
        HANDLER,
        "  let app = handle r",
        false,
        Some((certificate.cert.pem(), certificate.key_pair.serialize_pem())),
        false,
        move |socket| {
            let session = rustls::ClientConnection::new(
                std::sync::Arc::new(config),
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            )
            .unwrap();
            let mut tls = BufReader::new(rustls::StreamOwned::new(
                session,
                socket.get_ref().try_clone().unwrap(),
            ));
            send(&mut tls, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\nPOST /two HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabc");
            assert_eq!(response(&mut tls, false).body, b"/one:");
            assert_eq!(response(&mut tls, false).body, b"/two:abc");
            send(&mut tls, b"GET /three HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert_eq!(response(&mut tls, false).body, b"/three:");
        },
    );
}

#[test]
fn accept_failure_cancels_an_idle_connection_before_joining() {
    let trigger = TcpListener::bind("127.0.0.1:0").unwrap();
    trigger.set_nonblocking(true).unwrap();
    let port = trigger.local_addr().unwrap().port();
    let (stop, stopped) = std::sync::mpsc::channel();
    let controller = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut socket = loop {
            match trigger.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    break socket;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "missing accept controller");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept controller: {error}"),
            }
        };
        socket.write_all(b"a").unwrap();
        stopped.recv_timeout(Duration::from_secs(30)).unwrap();
        socket.shutdown(std::net::Shutdown::Both).unwrap();
    });
    let handler = format!(
        r#"{HANDLER}
controlledNet : Runtime -> Connection -> Net
let controlledNet r trigger = {{ Net with
  accept listener =
    match r.net.recv trigger 1 with
    | Ok bytes -> if Bytes.isEmpty bytes then Err "accept stopped" else r.net.accept listener
    | Err error -> Err error,
  close connection = r.net.close connection,
  connect host port = r.net.connect host port,
  listen port = r.net.listen port,
  localPort listener = r.net.localPort listener,
  recv connection count = r.net.recv connection count,
  send connection bytes = r.net.send connection bytes,
  sendMany connection chunks = r.net.sendMany connection chunks,
  udpBind port = r.net.udpBind port,
  udpClose socket = r.net.udpClose socket,
  udpLocalPort socket = r.net.udpLocalPort socket,
  udpRecv socket count = r.net.udpRecv socket count,
  udpSend socket host port bytes = r.net.udpSend socket host port bytes
}}
"#
    );
    let setup = format!(
        "  let trigger = Result.withDefault control (r.net.connect \"127.0.0.1\" {port})\n  let r = {{ r with net = controlledNet r trigger }}\n  let app = handle r"
    );
    server_with(&handler, &setup, false, move |socket| {
        send(socket, b"GET /one HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(response(socket, false).body, b"/one:");
        stop.send(()).unwrap();
        closed(socket);
    });
    controller.join().unwrap();
}

#[test]
fn repeated_request_decoding_keeps_bounded_live_storage() {
    use fai_db::Setter;
    use fai_runtime as runtime;
    use fai_syntax::Symbol;

    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with("/Http.fai"))
        .unwrap();
    let source = format!(
        r#"{}
testDecodeLoop : Int -> Int -> Int
let testDecodeLoop count total =
  if count = 0 then total else
    let transport = {{ Transport with close _ = (), recv _ = Err "unexpected read", send _ = Ok () }}
    match parseRequestAfter transport (Bytes.fromString "POST /one HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n0\r\nX-End: yes\r\n\r\n") with
    | Ok (Some (request, false, rest)) ->
      match bodyText request.body with
      | Ok text -> testDecodeLoop (count - 1) (total + String.length text)
      | Err _ -> -1
    | _ -> -1
public testDecodeMany : Int -> Int
let testDecodeMany count = testDecodeLoop count 0
public main : Runtime -> Unit
let main _ = ()
"#,
        file.text(&db)
    );
    file.set_text(&mut db).to(source);
    let mut program = fai_driver::jit_compile(&db, file).unwrap();
    let function = program.function(Symbol::intern("testDecodeMany")).unwrap();
    let baseline = (runtime::live_count(), runtime::live_bytes());
    let measure = |count| {
        runtime::reset_allocations();
        let result = runtime::apply(function, &[runtime::make_int(count)]);
        assert_eq!(runtime::read_int(result), count * 2);
        let peak = (runtime::peak_live_count(), runtime::peak_live_bytes());
        runtime::fai_drop(result);
        assert_eq!((runtime::live_count(), runtime::live_bytes()), baseline);
        peak
    };
    let short = measure(8);
    let long = measure(4096);
    assert_eq!(short, long, "request count must not increase live storage");
}
