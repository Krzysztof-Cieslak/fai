//! HTTP client and server end-to-end tests.
//!
//! These compile and run self-contained programs in-process through the JIT
//! (`jit_run_program`), capturing console output and asserting a clean,
//! **leak-free** exit (the runtime's exit-time live-object check returns 70 on a
//! leak). Each program drives the `Http` client and/or server over a real loopback
//! TCP connection on the M:N scheduler.

use std::sync::Mutex;

use fai_db::Db;
use indoc::indoc;

/// Serializes these tests: the console-capture sink, the live-object counter, and
/// the scheduler are process-global. (Under nextest each test is its own process,
/// so the lock is uncontended.)
static SERIAL: Mutex<()> = Mutex::new(());

/// Compiles and JIT-runs `src`, returning its captured stdout and exit code.
fn run(src: &str) -> (String, i32) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    run_program(src)
}

fn run_program(src: &str) -> (String, i32) {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Prog.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();

    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, file);
    let out = fai_runtime::capture_take();
    (out, outcome.exit_code)
}

fn read_raw_request(connection: &mut std::net::TcpStream) {
    use std::io::Read;
    connection.set_read_timeout(Some(std::time::Duration::from_secs(15))).unwrap();
    let mut request = Vec::new();
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let mut bytes = [0; 1024];
        let count = connection.read(&mut bytes).unwrap();
        assert_ne!(count, 0, "connection ended before request headers");
        request.extend_from_slice(&bytes[..count]);
    }
}

#[track_caller]
fn rejects_raw_response(response: &'static [u8]) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        drop(listener);
        read_raw_request(&mut connection);
        connection.write_all(response).unwrap();
    });
    let source = format!(
        r#"
module Prog
public main : Runtime -> Unit / {{ Console, Net, Tls }}
let main runtime =
  match Http.get runtime "http://127.0.0.1:{port}/" with
  | Err e -> runtime.console.writeLine "rejected"
  | Ok response ->
    match Http.bodyText response.body with
    | Err e -> runtime.console.writeLine "rejected"
    | Ok text -> runtime.console.writeLine "accepted"
"#
    );
    let (out, code) = run(&source);
    assert_eq!(code, 0, "{out}");
    server.join().unwrap();
    assert_eq!(out, "rejected\n");
}

#[test]
fn raw_invalid_content_length_is_rejected() {
    rejects_raw_response(b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n");
}

#[test]
fn raw_invalid_chunk_size_is_rejected() {
    rejects_raw_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\ng\r\n");
}

#[test]
fn raw_truncated_chunk_trailers_are_rejected() {
    rejects_raw_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\nX: unfinished\r\n",
    );
}

#[test]
fn pooled_chunked_response_retains_surplus_after_its_trailers() {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        drop(listener);
        read_raw_request(&mut connection);
        connection.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\none\r\n0\r\nX-Note: done\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ntwo").unwrap();
        read_raw_request(&mut connection);
    });
    let source = format!(
        r#"
module Prog
fetch : Runtime -> Http.Client -> String / {{ Concurrency, Net, Tls }}
let fetch runtime client =
  match Http.getOn runtime client "http://127.0.0.1:{port}/" with
  | Err e -> "error: " ++ e
  | Ok response -> Result.withDefault "body error" (Http.bodyText response.body)
two : Runtime -> Http.Client -> String / {{ Concurrency, Net, Tls }}
let two runtime client =
  let first = fetch runtime client
  let second = fetch runtime client
  first ++ "|" ++ second
public main : Runtime -> Unit / {{ Concurrency, Console, Net, Tls }}
let main runtime = runtime.console.writeLine (Http.withClient runtime (two runtime))
"#
    );
    let (out, code) = run(&source);
    assert_eq!(code, 0, "{out}");
    server.join().unwrap();
    assert_eq!(out, "one|two\n");
}

#[track_caller]
fn pooled_bodyless_response(method: &str, first_head: &'static [u8]) {
    use std::io::Write;
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let mut connection = redirect_connection(&listener);
        drop(listener);
        read_raw_request(&mut connection);
        connection.write_all(first_head).unwrap();
        read_raw_request(&mut connection);
        connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
    });
    let source = format!(
        r#"module Prog
fetch : Runtime -> Http.Client -> String / {{ Concurrency, Net, Tls }}
let fetch r client =
  match Url.parse "http://127.0.0.1:{port}/" with
  | Err e -> e
  | Ok url ->
    let req = {{ method = Http.{method}, url = url, headers = Headers.empty, body = Http.emptyBody }}
    match Http.requestOn r client req with
    | Err e -> e
    | Ok first ->
      let empty = Http.bodyText first.body
      match Http.getOn r client "http://127.0.0.1:{port}/" with
      | Err e -> e
      | Ok second -> if empty = Ok "" then Result.withDefault "error" (Http.bodyText second.body) else "unexpected body"
public main : Runtime -> Unit / {{ Concurrency, Console, Net, Tls }}
let main r = r.console.writeLine (Http.withClient r (fetch r))
"#
    );
    let (out, code) = run_program(&source);
    peer.join().unwrap();
    assert_eq!((out.as_str(), code), ("ok\n", 0));
}

#[test]
fn pooled_head_response_immediately_releases_its_connection() {
    pooled_bodyless_response("HEAD", b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\n\r\n");
}

#[test]
fn pooled_no_content_response_immediately_releases_its_connection() {
    pooled_bodyless_response("GET", b"HTTP/1.1 204 No Content\r\n\r\n");
}

#[test]
fn pooled_not_modified_response_immediately_releases_its_connection() {
    pooled_bodyless_response("GET", b"HTTP/1.1 304 Not Modified\r\nContent-Length: 500\r\n\r\n");
}

fn request_with_body(connection: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut request = redirect_request(connection).into_bytes();
    let head_end = request.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&request[..head_end]);
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < head_end + length {
        let mut bytes = [0; 4096];
        let count = connection.read(&mut bytes).unwrap();
        assert!(count > 0);
        request.extend_from_slice(&bytes[..count]);
    }
    String::from_utf8(request).unwrap()
}

#[track_caller]
fn counted_pool_attempts(method: Option<&str>, broken_response: &'static [u8]) -> (usize, String) {
    use std::io::Write;
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let mut connection = redirect_connection(&listener);
        request_with_body(&mut connection);
        connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
        let second = request_with_body(&mut connection);
        connection.write_all(broken_response).unwrap();
        drop(connection);
        let mut attempts = 1;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if stopped.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((mut retry, _)) => {
                    retry.set_nonblocking(false).unwrap();
                    let repeated = request_with_body(&mut retry);
                    assert_eq!(repeated, second);
                    attempts += 1;
                    retry.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "pool attempt did not finish");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => panic!("retry accept: {error}"),
            }
        }
        attempts
    });
    let request = method.map_or_else(
        || format!("Http.getOn r client \"http://127.0.0.1:{port}/\""),
        |method| format!("Http.requestOn r client {{ method = Http.{method}, url = url, headers = Headers.empty, body = Stream.defer (payload r) }}"),
    );
    let source = format!(
        r#"module Prog
payload : Runtime -> Unit -> Stream Bytes {{ Console }} / {{ Console }}
let payload r u =
  let wrote = r.console.writeLine "body"
  Http.stringBody "data"
session : Runtime -> Http.Client -> Bool / {{ Concurrency, Console, Net, Tls }}
let session r client =
  let warm = Http.getOn r client "http://127.0.0.1:{port}/"
  match Url.parse "http://127.0.0.1:{port}/" with
  | Err e -> false
  | Ok url ->
    match {request} with
    | Err e -> false
    | Ok response -> true
public main : Runtime -> Unit / {{ Concurrency, Console, Net, Tls }}
let main r =
  let accepted = Http.withClient r (session r)
  r.console.writeLine (if accepted then "accepted" else "rejected")
"#
    );
    let source = if method.is_none() {
        source.replace(
            "session : Runtime -> Http.Client -> Bool / { Concurrency, Console, Net, Tls }",
            "session : Runtime -> Http.Client -> Bool / { Concurrency, Net, Tls }",
        )
    } else {
        source
    };
    let (out, code) = run_program(&source);
    stop.send(()).unwrap();
    let attempts = peer.join().unwrap();
    assert_eq!(code, 0, "{out}");
    (attempts, out)
}

#[test]
fn pooled_post_and_its_body_are_not_replayed_after_a_partial_head() {
    assert_eq!(
        counted_pool_attempts(Some("POST"), b"HTTP/1.1 200 OK\r\nContent-Length:"),
        (1, "body\nrejected\n".into())
    );
}

#[test]
fn a_general_get_with_a_one_use_body_is_not_replayed() {
    assert_eq!(counted_pool_attempts(Some("GET"), b""), (1, "body\nrejected\n".into()));
}

#[test]
fn get_on_does_not_replay_a_partial_response_head() {
    assert_eq!(
        counted_pool_attempts(None, b"HTTP/1.1 200 OK\r\nContent-Length:"),
        (1, "rejected\n".into())
    );
}

#[test]
fn get_on_retries_one_empty_response_eof_on_a_reused_connection() {
    assert_eq!(counted_pool_attempts(None, b""), (2, "accepted\n".into()));
}

#[track_caller]
fn server_bodyless_response(method: &str, status: i32, keeps_length: bool) {
    use fai_db::Setter;
    use std::io::Read;
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let mut connection = redirect_connection(&listener);
        connection.set_read_timeout(Some(std::time::Duration::from_secs(15))).unwrap();
        let mut response = String::new();
        connection.read_to_string(&mut response).unwrap();
        response
    });
    let mut db = fai_db::FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with("/Http.fai"))
        .unwrap();
    let source = format!(
        r#"{}
testSuppressedBody : Runtime -> Unit -> Stream Bytes {{ Console }} / {{ Console }}
let testSuppressedBody r u =
  let forced = r.console.writeLine "BODY FORCED"
  stringBody "payload"
public main : Runtime -> Unit / {{ Console, Net, Tls }}
let main r =
  match r.net.connect "127.0.0.1" {port} with
  | Err e -> r.console.writeLine e
  | Ok connection ->
    let body = Stream.defer (testSuppressedBody r)
    let resp = {{ response {status} body with headers = Headers.fromList [("Content-Length", "99"), ("Transfer-Encoding", "chunked")] }}
    match sendResponse {method} (plainTransport r connection) resp with
    | Err e -> r.console.writeLine e
    | Ok u -> ()
"#,
        file.text(&db)
    );
    file.set_text(&mut db).to(source);
    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, file);
    let out = fai_runtime::capture_take();
    assert_eq!(outcome.exit_code, 0, "{:?}", outcome.diagnostics);
    let wire = peer.join().unwrap();
    assert_eq!(out, "");
    let (head, body) = wire.split_once("\r\n\r\n").unwrap();
    assert!(body.is_empty(), "{wire}");
    assert!(!head.to_ascii_lowercase().contains("transfer-encoding"), "{wire}");
    assert_eq!(head.to_ascii_lowercase().contains("content-length: 99"), keeps_length, "{wire}");
}

#[test]
fn server_head_suppresses_body_without_forcing_it() {
    server_bodyless_response("HEAD", 200, true);
}

#[test]
fn server_no_content_suppresses_body_and_length_headers() {
    server_bodyless_response("GET", 204, false);
}

#[test]
fn server_not_modified_retains_length_metadata_without_a_body() {
    server_bodyless_response("GET", 304, true);
}

#[track_caller]
fn server_expectation(expectation: &str, accepted: bool) {
    use fai_db::Setter;
    use std::io::{Read, Write};
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\nExpect: {expectation}\r\n\r\n"
    );
    let peer = std::thread::spawn(move || {
        let mut connection = redirect_connection(&listener);
        connection.set_read_timeout(Some(std::time::Duration::from_secs(15))).unwrap();
        connection.write_all(head.as_bytes()).unwrap();
        if accepted {
            let mut interim = [0u8; 25];
            connection.read_exact(&mut interim).unwrap();
            assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
            connection.write_all(b"data").unwrap();
        }
        let mut response = String::new();
        connection.read_to_string(&mut response).unwrap();
        response
    });
    let mut db = fai_db::FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with("/Http.fai"))
        .unwrap();
    let source = format!(
        r#"{}
public main : Runtime -> Unit / {{ Net, Tls }}
let main r =
  match r.net.connect "127.0.0.1" {port} with
  | Err e -> ()
  | Ok connection ->
    let transport = plainTransport r connection
    match parseRequest transport with
    | Err e -> transport.close ()
    | Ok request ->
      let text = Result.withDefault "error" (bodyText request.body)
      match sendResponse request.method transport (textResponse 200 text) with
      | Ok u -> ()
      | Err e -> transport.close ()
"#,
        file.text(&db)
    );
    file.set_text(&mut db).to(source);
    let outcome = fai_driver::jit_run_program(&db, file);
    assert_eq!(outcome.exit_code, 0, "{:?}", outcome.diagnostics);
    let wire = peer.join().unwrap();
    if accepted {
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        assert!(wire.ends_with("\r\n\r\ndata"), "{wire}");
    } else {
        assert!(wire.starts_with("HTTP/1.1 417"), "{wire}");
        assert!(wire.ends_with("\r\n\r\n"), "{wire}");
    }
}

#[test]
fn server_acknowledges_continue_before_reading_the_body() {
    server_expectation("100-continue", true);
}

#[test]
fn server_rejects_unsupported_expectations_without_waiting_for_a_body() {
    server_expectation("something-else", false);
}

fn redirect_connection(listener: &std::net::TcpListener) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    // The peer starts before the Fai client is JIT-compiled; allow cold debug
    // compilation separately from the much shorter connected-I/O timeouts.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        match listener.accept() {
            Ok((connection, _)) => return connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(std::time::Instant::now() < deadline, "redirect connection did not arrive");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => panic!("redirect accept: {error}"),
        }
    }
}

fn redirect_request(connection: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    connection.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();
    let mut request = Vec::new();
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let mut bytes = [0; 1024];
        let count = connection.read(&mut bytes).unwrap();
        assert!(count > 0, "redirect peer closed before sending its request");
        request.extend_from_slice(&bytes[..count]);
    }
    String::from_utf8(request).unwrap()
}

#[track_caller]
fn relative_redirect(reference: &str, target: &'static str, cross_origin: bool) {
    use std::io::Write;

    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = first.local_addr().unwrap().port();
    let second = cross_origin.then(|| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let next_port = second.as_ref().map_or(port, |listener| listener.local_addr().unwrap().port());
    let reference = reference.replace("{port}", &next_port.to_string());
    let peer = std::thread::spawn(move || {
        let mut initial = redirect_connection(&first);
        let request = redirect_request(&mut initial);
        assert!(request.to_ascii_lowercase().contains("authorization: bearer secret"));
        initial.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {reference}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
        drop(initial);
        let mut redirected = redirect_connection(second.as_ref().unwrap_or(&first));
        let request = redirect_request(&mut redirected);
        assert_eq!(request.lines().next().unwrap(), format!("GET {target} HTTP/1.1"));
        assert_eq!(request.to_ascii_lowercase().contains("authorization:"), !cross_origin);
        redirected
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });
    let source = format!(
        r#"module Prog
public main : Runtime -> Unit / {{ Console, Net, Tls }}
let main r =
  match Url.parse "http://127.0.0.1:{port}/dir/page?q=1" with
  | Err e -> r.console.writeLine e
  | Ok url ->
    let request = {{ body = Http.emptyBody, headers = Headers.set "Authorization" "Bearer secret" Headers.empty, method = Http.GET, url = url }}
    match Http.request r request with
    | Err e -> r.console.writeLine e
    | Ok response -> r.console.writeLine (Result.withDefault "body error" (Http.bodyText response.body))
"#
    );
    let (output, code) = run_program(&source);
    peer.join().unwrap();
    assert_eq!(code, 0, "{output}");
    assert_eq!(output, "ok\n");
}

#[test]
fn network_path_redirects_change_origin_and_strip_authorization() {
    relative_redirect("//127.0.0.1:{port}/dir/../next", "/next", true);
}

#[test]
fn fragment_only_redirects_preserve_the_query() {
    relative_redirect("#part", "/dir/page?q=1", false);
}

#[test]
fn query_only_redirects_keep_the_path() {
    relative_redirect("?next=2", "/dir/page?next=2", false);
}

#[test]
fn client_gets_a_response_from_a_raw_loopback_server() {
    // The `Http` client performs a real GET against a hand-rolled TCP server (raw
    // `Net`, not `Http.serve`), exercising request serialization and response parsing
    // (status line, headers — looked up case-insensitively — and a Content-Length
    // body) end to end. The two run concurrently over loopback.
    let src = indoc! {r#"
        module Prog

        serveOne : Runtime -> Listener -> Unit / { Net }
        let serveOne runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn ->
            let req = runtime.net.recv conn 4096
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: raw\r\n\r\nhello"
            let sent = runtime.net.send conn (Bytes.fromString resp)
            runtime.net.close conn

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/p") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text ->
              Int.toString resp.status ++ " " ++ Option.withDefault "?" (Headers.get "server" resp.headers) ++ " " ++ text

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveOne runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "200 raw hello\n");
}

#[test]
fn server_and_client_round_trip_then_shut_down() {
    // `Http.serveListener` serves a handler on a spawned task; the client GETs it and
    // reads the handler's response; then the server task is cancelled (graceful
    // shutdown), the structured scope joins it, and the program exits cleanly. Drives
    // request parsing + response serialization on the server side and the full client.
    let src = indoc! {r#"
        module Prog

        let handler req = Ok (Http.textResponse 200 ("hi " ++ Url.path req.url))

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/world") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> Int.toString resp.status ++ " " ++ text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListener runtime listener handler)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            runtime.concurrency.scope (fun nursery -> body runtime listener port nursery)
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit (server cancelled and joined)");
    assert_eq!(out, "200 hi /world\n");
}

#[test]
fn https_client_and_server_round_trip() {
    // A full HTTPS round trip over loopback: an `Http.serveListenerTls` server
    // presents a fresh self-signed certificate (minted here with rcgen for the IP
    // `127.0.0.1`), and the client GETs it over TLS, trusting that certificate via
    // `getWith`'s extra-roots option. This drives the whole stack — the rustls
    // handshake pumped over `Net` by the Fai `tlsTransport`, then encrypted request
    // and response framing — end to end, then cancels the server.
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])
        .expect("generate self-signed cert");
    let cert_pem = cert.cert.pem().replace('\n', "\\n");
    let key_pem = cert.key_pair.serialize_pem().replace('\n', "\\n");

    let src = format!(
        r#"
        module Prog

        let certPem = "{cert_pem}"

        let keyPem = "{key_pem}"

        let handle req = Ok (Http.textResponse 200 "secure hello")

        client : Runtime -> Int -> String / {{ Net, Tls }}
        let client runtime port =
          match Http.getWith runtime (Some (Bytes.fromString certPem)) ("https://127.0.0.1:" ++ Int.toString port ++ "/secure") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> Int.toString resp.status ++ " " ++ text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / {{ Concurrency, Console, Net, Tls }}
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListenerTls runtime listener (Bytes.fromString certPem) (Bytes.fromString keyPem) handle)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / {{ Concurrency, Console, Net, Tls }}
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            runtime.concurrency.scope (fun nursery -> body runtime listener port nursery)
    "#
    );
    let (out, code) = run(&src);
    assert_eq!(code, 0, "clean, leak-free exit (TLS handshake + request/response + shutdown)");
    assert_eq!(out, "200 secure hello\n");
}

#[track_caller]
fn https_large_echo(chunked: bool) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert_pem = cert.cert.pem().replace('\n', "\\n");
    let key_pem = cert.key_pair.serialize_pem().replace('\n', "\\n");
    let headers = if chunked {
        "Headers.add \"Transfer-Encoding\" \"chunked\" Headers.empty"
    } else {
        "Headers.empty"
    };
    let response = if chunked {
        "Http.chunkedResponse 200 Headers.empty (Http.stringBody text)"
    } else {
        "Http.textResponse 200 text"
    };
    let source = format!(
        r#"
module Prog
let certPem = "{cert_pem}"
let keyPem = "{key_pem}"
let payload = String.joinArray "" (Array.repeat 65536 "0123456789abcdef")
let handle req =
  match Http.bodyText req.body with
  | Err e -> Ok (Http.textResponse 500 e)
  | Ok text -> Ok ({response})
client : Runtime -> Int -> String / {{ Net, Tls }}
let client runtime port =
  let url = "https://127.0.0.1:" ++ Int.toString port ++ "/echo"
  match Url.parse url with
  | Err e -> e
  | Ok parsed ->
    let req = {{ method = Http.POST, url = parsed, headers = {headers}, body = Http.stringBody payload }}
    match Http.requestOnce runtime (Some (Bytes.fromString certPem)) req with
    | Err e -> "request: " ++ e
    | Ok response ->
      match Http.bodyText response.body with
      | Err e -> "response: " ++ e
      | Ok text -> if text = payload then "ok" else "corrupt"
body : Runtime -> Listener -> Int -> Nursery -> Unit / {{ Concurrency, Console, Net, Tls }}
let body runtime listener port nursery =
  let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListenerTls runtime listener (Bytes.fromString certPem) (Bytes.fromString keyPem) handle)
  let first = client runtime port
  let second = client runtime port
  let cancelled = runtime.concurrency.cancel server
  runtime.console.writeLine (first ++ " " ++ second)
public main : Runtime -> Unit / {{ Concurrency, Console, Net, Tls }}
let main runtime =
  match runtime.net.listen 0 with
  | Err e -> runtime.console.writeLine e
  | Ok listener -> runtime.concurrency.scope (fun nursery -> body runtime listener (runtime.net.localPort listener) nursery)
"#
    );
    let (out, code) = run(&source);
    assert_eq!(code, 0, "clean, leak-free HTTPS echo");
    assert_eq!(out, "ok ok\n");
}

#[test]
fn https_megabyte_request_and_response_round_trip() {
    https_large_echo(false);
}

#[test]
fn https_megabyte_chunks_round_trip() {
    https_large_echo(true);
}

#[test]
fn client_decodes_a_chunked_response() {
    // A raw server replies with Transfer-Encoding: chunked (two chunks, then the
    // terminating zero chunk). The client must decode the chunks and reassemble the
    // body — there is no Content-Length.
    let src = indoc! {r#"
        module Prog

        serveOne : Runtime -> Listener -> Unit / { Net }
        let serveOne runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn ->
            let req = runtime.net.recv conn 4096
            let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n6\r\n World\r\n0\r\n\r\n"
            let sent = runtime.net.send conn (Bytes.fromString resp)
            runtime.net.close conn

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/p") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> text

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveOne runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "Hello World\n");
}

#[test]
fn response_body_streams_chunk_by_chunk() {
    // The response body is a lazy stream of decoded chunks, not one materialized
    // blob: a chunked response with three chunks yields a three-element stream, so
    // `Stream.toList` (which preserves element boundaries) sees exactly three.
    let src = indoc! {r#"
        module Prog

        serveOne : Runtime -> Listener -> Unit / { Net }
        let serveOne runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn ->
            let req = runtime.net.recv conn 4096
            let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n2\r\nbc\r\n3\r\ndef\r\n0\r\n\r\n"
            let sent = runtime.net.send conn (Bytes.fromString resp)
            runtime.net.close conn

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/p") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Stream.toList resp.body with
            | Err e -> "stream error: " ++ e
            | Ok chunks -> Int.toString (List.length chunks)

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveOne runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "3\n");
}

#[test]
fn server_reads_a_posted_request_body() {
    // The client POSTs a body (sent with Content-Length); the server's handler reads
    // the request body and echoes it back — exercising the server-side body read.
    let src = indoc! {r#"
        module Prog

        let handle req =
          match Http.bodyText req.body with
          | Err e -> Ok (Http.textResponse 400 "bad")
          | Ok b -> Ok (Http.textResponse 200 ("echo:" ++ b))

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.post runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/e") Headers.empty (Http.stringBody "ping") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListener runtime listener handle)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            runtime.concurrency.scope (fun nursery -> body runtime listener port nursery)
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "echo:ping\n");
}

#[test]
fn server_streams_a_chunked_response() {
    // The handler returns a chunked response built from a multi-element body stream;
    // the server sends it as chunked transfer-encoding (one frame per element, no
    // buffering, via Stream.uncons), and the client (which decodes chunked) sees the
    // reassembled body.
    let src = indoc! {r#"
        module Prog

        let handle req =
          Ok (Http.chunkedResponse 200 Headers.empty (Stream.fromList [Bytes.fromString "Hello", Bytes.fromString ", ", Bytes.fromString "world"]))

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/s") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListener runtime listener handle)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            runtime.concurrency.scope (fun nursery -> body runtime listener port nursery)
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "Hello, world\n");
}

#[test]
fn client_sends_a_chunked_request() {
    // The client sends a request whose headers select chunked transfer-encoding, with
    // a multi-element body stream; it is streamed chunk by chunk (not drained to a
    // Content-Length). The server decodes the chunked request body and echoes it.
    let src = indoc! {r#"
        module Prog

        let handle req =
          match Http.bodyText req.body with
          | Err e -> Ok (Http.textResponse 400 "bad")
          | Ok b -> Ok (Http.textResponse 200 ("got:" ++ b))

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.post runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/u") (Headers.set "Transfer-Encoding" "chunked" Headers.empty) (Stream.fromList [Bytes.fromString "pi", Bytes.fromString "ng"]) with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListener runtime listener handle)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            runtime.concurrency.scope (fun nursery -> body runtime listener port nursery)
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "got:ping\n");
}

#[test]
fn client_sends_basic_auth_and_a_form_body() {
    // The client POSTs a urlencoded form with a Basic-auth Authorization header; the
    // server reads both back and echoes them, exercising basicAuth/base64 + formBody.
    let src = indoc! {r#"
        module Prog

        let handle req =
          let auth = Option.withDefault "none" (Headers.get "Authorization" req.headers)
          match Http.bodyText req.body with
          | Err e -> Ok (Http.textResponse 400 "bad")
          | Ok b -> Ok (Http.textResponse 200 (auth ++ "|" ++ b))

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          let headers = Headers.set "Authorization" (Http.basicAuth "u" "p") (Headers.set "Content-Type" "application/x-www-form-urlencoded" Headers.empty)
          match Http.post runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/f") headers (Http.formBody [("x", "1"), ("y", "a b")]) with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListener runtime listener handle)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            runtime.concurrency.scope (fun nursery -> body runtime listener port nursery)
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "Basic dTpw|x=1&y=a%20b\n");
}

#[test]
fn client_follows_a_redirect() {
    // The first request to /old gets a 302 with a *relative* `Location: /new`; the
    // client resolves it against the request URL, drops the redirect response (closing
    // its connection), and re-requests /new on a fresh connection, returning the final
    // 200. The raw server accepts two connections in turn. Exercises Url.resolve and
    // the whole follow loop (a 302 of a GET stays a GET).
    let src = indoc! {r#"
        module Prog

        serveRedirect : Runtime -> Listener -> Unit / { Net }
        let serveRedirect runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn1 ->
            let req1 = runtime.net.recv conn1 4096
            let resp1 = "HTTP/1.1 302 Found\r\nLocation: /new\r\nContent-Length: 8\r\n\r\nREDIRECT"
            let sent1 = runtime.net.send conn1 (Bytes.fromString resp1)
            let closed1 = runtime.net.close conn1
            match runtime.net.accept listener with
            | Err e -> ()
            | Ok conn2 ->
              let req2 = runtime.net.recv conn2 4096
              let resp2 = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfinal"
              let sent2 = runtime.net.send conn2 (Bytes.fromString resp2)
              runtime.net.close conn2

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/old") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> Int.toString resp.status ++ " " ++ text

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveRedirect runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "200 final\n");
}

#[test]
fn request_once_does_not_follow_redirects() {
    // `requestOnce` is the single-shot escape hatch: it returns the 302 itself rather
    // than following it, so the server accepts exactly one connection and the client
    // reports status 302.
    let src = indoc! {r#"
        module Prog

        serveOne : Runtime -> Listener -> Unit / { Net }
        let serveOne runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn ->
            let req = runtime.net.recv conn 4096
            let resp = "HTTP/1.1 302 Found\r\nLocation: /new\r\nContent-Length: 0\r\n\r\n"
            let sent = runtime.net.send conn (Bytes.fromString resp)
            runtime.net.close conn

        client : Runtime -> Int -> String / { Net, Tls }
        let client runtime port =
          match Url.parse ("http://127.0.0.1:" ++ Int.toString port ++ "/old") with
          | Err e -> "url error: " ++ e
          | Ok url ->
            match Http.requestOnce runtime None { body = Http.emptyBody, headers = Headers.empty, method = Http.GET, url = url } with
            | Err e -> "error: " ++ e
            | Ok resp -> Int.toString resp.status

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveOne runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "302\n");
}

#[test]
fn pool_reuses_a_keep_alive_connection() {
    // The pooling client (`Http.withClient` + `getOn`) makes two requests to the same
    // origin. The raw server accepts exactly ONE connection and serves both requests on
    // it (keep-alive), so a clean "one|two" proves the second request reused the first
    // request's connection (a second request without reuse would need a second accept,
    // which never comes). The pool tears every connection down on scope exit.
    let src = indoc! {r#"
        module Prog

        serveKeepAlive : Runtime -> Listener -> Unit / { Net }
        let serveKeepAlive runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn ->
            let req1 = runtime.net.recv conn 4096
            let s1 = runtime.net.send conn (Bytes.fromString "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\none")
            let req2 = runtime.net.recv conn 4096
            let s2 = runtime.net.send conn (Bytes.fromString "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ntwo")
            runtime.net.close conn

        target : Int -> String
        let target port = "http://127.0.0.1:" ++ Int.toString port ++ "/x"

        twoGets : Runtime -> Http.Client -> Int -> String / { Concurrency, Net, Tls }
        let twoGets runtime client port =
          match Http.getOn runtime client (target port) with
          | Err e -> "e1:" ++ e
          | Ok r1 ->
            match Http.bodyText r1.body with
            | Err e -> "b1:" ++ e
            | Ok t1 ->
              match Http.getOn runtime client (target port) with
              | Err e -> "e2:" ++ e
              | Ok r2 ->
                match Http.bodyText r2.body with
                | Err e -> "b2:" ++ e
                | Ok t2 -> t1 ++ "|" ++ t2

        client : Runtime -> Int -> String / { Concurrency, Net, Tls }
        let client runtime port =
          Http.withClient runtime (fun c -> twoGets runtime c port)

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveKeepAlive runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "one|two\n");
}

#[test]
fn pool_retries_a_stale_connection() {
    // The server serves the first request then closes that connection (a server
    // dropping an idle keep-alive connection), and serves the second request on a fresh
    // connection. The pool reuses the first connection for the second request, finds it
    // dead before any response, and retries once on a fresh connection — so a clean
    // "one|two" proves the stale-connection retry.
    let src = indoc! {r#"
        module Prog

        serveDropThenFresh : Runtime -> Listener -> Unit / { Net }
        let serveDropThenFresh runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn1 ->
            let req1 = runtime.net.recv conn1 4096
            let s1 = runtime.net.send conn1 (Bytes.fromString "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\none")
            let closed1 = runtime.net.close conn1
            match runtime.net.accept listener with
            | Err e -> ()
            | Ok conn2 ->
              let req2 = runtime.net.recv conn2 4096
              let s2 = runtime.net.send conn2 (Bytes.fromString "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ntwo")
              runtime.net.close conn2

        target : Int -> String
        let target port = "http://127.0.0.1:" ++ Int.toString port ++ "/x"

        twoGets : Runtime -> Http.Client -> Int -> String / { Concurrency, Net, Tls }
        let twoGets runtime client port =
          match Http.getOn runtime client (target port) with
          | Err e -> "e1:" ++ e
          | Ok r1 ->
            match Http.bodyText r1.body with
            | Err e -> "b1:" ++ e
            | Ok t1 ->
              match Http.getOn runtime client (target port) with
              | Err e -> "e2:" ++ e
              | Ok r2 ->
                match Http.bodyText r2.body with
                | Err e -> "b2:" ++ e
                | Ok t2 -> t1 ++ "|" ++ t2

        client : Runtime -> Int -> String / { Concurrency, Net, Tls }
        let client runtime port =
          Http.withClient runtime (fun c -> twoGets runtime c port)

        public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveDropThenFresh runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "one|two\n");
}

#[test]
fn client_posts_a_multipart_form() {
    // `Http.postMultipart` sends a multipart/form-data body (a text field and a file
    // part) with a random boundary. The raw server echoes the received request bytes
    // back; the client confirms the body carries both parts' Content-Disposition lines
    // and contents, so the multipart framing round-tripped over HTTP.
    let src = indoc! {r#"
        module Prog

        serveEcho : Runtime -> Listener -> Unit / { Net }
        let serveEcho runtime listener =
          match runtime.net.accept listener with
          | Err e -> ()
          | Ok conn ->
            match runtime.net.recv conn 65536 with
            | Err e -> ()
            | Ok received ->
              let header = "HTTP/1.1 200 OK\r\nContent-Length: " ++ Int.toString (Bytes.length received) ++ "\r\n\r\n"
              let sent = runtime.net.send conn (Bytes.concat (Bytes.fromString header) received)
              runtime.net.close conn

        checkContains : String -> String
        let checkContains text =
          if String.contains text "name=\"greeting\"" && String.contains text "hello" && String.contains text "filename=\"a.txt\"" && String.contains text "DATA" then
            "ok"
          else
            "missing"

        client : Runtime -> Int -> String / { Net, Random, Tls }
        let client runtime port =
          match Http.postMultipart runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/upload") [Http.field "greeting" "hello", Http.filePart "f" "a.txt" "text/plain" (Bytes.fromString "DATA")] with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> checkContains text

        public main : Runtime -> Unit / { Concurrency, Console, Net, Random, Tls }
        let main runtime =
          match runtime.net.listen 0 with
          | Err e -> runtime.console.writeLine ("listen failed: " ++ e)
          | Ok listener ->
            let port = runtime.net.localPort listener
            match Async.parallel2 runtime.concurrency (fun u -> serveEcho runtime listener) (fun u -> client runtime port) with
            | (served, report) -> runtime.console.writeLine report
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "ok\n");
}
