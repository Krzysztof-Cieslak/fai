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
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Prog.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();

    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, file);
    let out = fai_runtime::capture_take();
    (out, outcome.exit_code)
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
