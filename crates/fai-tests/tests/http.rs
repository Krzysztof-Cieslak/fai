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

        client : Runtime -> Int -> String / { Net }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/p") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text ->
              Int.toString resp.status ++ " " ++ Option.withDefault "?" (Headers.get "server" resp.headers) ++ " " ++ text

        public main : Runtime -> Unit / { Concurrency, Console, Net }
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

        client : Runtime -> Int -> String / { Net }
        let client runtime port =
          match Http.get runtime ("http://127.0.0.1:" ++ Int.toString port ++ "/world") with
          | Err e -> "error: " ++ e
          | Ok resp ->
            match Http.bodyText resp.body with
            | Err e -> "body error: " ++ e
            | Ok text -> Int.toString resp.status ++ " " ++ text

        body : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net }
        let body runtime listener port nursery =
          let server = runtime.concurrency.spawn nursery (fun u -> Http.serveListener runtime listener handler)
          let report = client runtime port
          let cancelled = runtime.concurrency.cancel server
          runtime.console.writeLine report

        public main : Runtime -> Unit / { Concurrency, Console, Net }
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
