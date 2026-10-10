//! Scoped HTTP response production and writer lifetime over loopback connections.

use std::sync::Mutex;

use fai_db::Db;

static SERIAL: Mutex<()> = Mutex::new(());

fn run(body: &str) -> String {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Producer.fai".into(), body.to_owned());
    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(outcome.exit_code, 0, "{output}\n{:?}", outcome.diagnostics);
    output
}

#[test]
fn head_is_sent_before_the_producer_waits() {
    let output = run(r#"module Producer
produce : Runtime -> Channel Unit -> Http.BodyProducer { Concurrency }
let produce r ready write =
  let started = r.concurrency.recv ready
  write (Bytes.fromString "hello")
session : Runtime -> Listener -> Nursery -> Unit / { Console, Concurrency, Net, Tls }
let session r listener nursery =
  let ready = r.concurrency.channel 1
  let server = r.concurrency.spawn nursery (fun _ -> Http.serveManagedListener r listener (fun _ -> Ok (Http.produced 200 Headers.empty (produce r ready))))
  let response = Http.get r ("http://127.0.0.1:" ++ Int.toString (r.net.localPort listener) ++ "/")
  let signalled = r.concurrency.send ready ()
  let result = match response with | Err e -> Err e | Ok response -> Http.bodyText response.body
  let stopped = r.concurrency.cancel server
  r.console.writeLine (if result = Ok "hello" then "ok" else "bad")
public main : Runtime -> Unit / { Console, Concurrency, Net, Tls }
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (session r listener)
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn head_does_not_start_a_body_producer() {
    let output = run(r#"module Producer
produce : Runtime -> Http.BodyProducer { Console }
let produce r write =
  let called = r.console.writeLine "unexpected producer"
  write (Bytes.fromString "body")
session : Runtime -> Listener -> Nursery -> Unit / { Console, Concurrency, Net, Tls }
let session r listener nursery =
  let server = r.concurrency.spawn nursery (fun _ -> Http.serveManagedListener r listener (fun _ -> Ok (Http.produced 200 Headers.empty (produce r))))
  let result =
    match Url.parse ("http://127.0.0.1:" ++ Int.toString (r.net.localPort listener) ++ "/") with
    | Err e -> Err e
    | Ok url ->
      match Http.requestOnce r None { url = url, method = Http.HEAD, headers = Headers.empty, body = Http.emptyBody } with
      | Err e -> Err e
      | Ok response -> Http.bodyText response.body
  let stopped = r.concurrency.cancel server
  r.console.writeLine (if result = Ok "" then "ok" else "bad")
public main : Runtime -> Unit / { Console, Concurrency, Net, Tls }
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (session r listener)
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn retained_writer_fails_after_production_ends() {
    let output = run(r#"module Producer
produce : Runtime -> Channel (Bytes -> Result Unit String / { Concurrency, Net, Tls }) -> Http.BodyProducer { Concurrency }
let produce r writers write =
  let started = write Bytes.empty
  let saved = r.concurrency.send writers write
  Ok ()
session : Runtime -> Listener -> Nursery -> Unit / { Console, Concurrency, Net, Tls }
let session r listener nursery =
  let writers = r.concurrency.channel 1
  let server = r.concurrency.spawn nursery (fun _ -> Http.serveManagedListener r listener (fun _ -> Ok (Http.produced 200 Headers.empty (produce r writers))))
  let result =
    match Http.get r ("http://127.0.0.1:" ++ Int.toString (r.net.localPort listener) ++ "/") with
    | Err e -> Err e
    | Ok response -> Http.bodyText response.body
  let sent =
    match r.concurrency.recv writers with
    | None -> Ok ()
    | Some write -> write (Bytes.fromString "late")
  let stopped = r.concurrency.cancel server
  r.console.writeLine (if result = Ok "" && sent = Err "response writer scope has ended" then "ok" else "bad")
public main : Runtime -> Unit / { Console, Concurrency, Net, Tls }
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (session r listener)
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn producer_failure_ends_an_unfinished_body() {
    let output = run(r#"module Producer
produce : Http.BodyProducer {}
let produce write =
  let sent = write (Bytes.fromString "prefix")
  Err "producer failed"
session : Runtime -> Listener -> Nursery -> Unit / { Console, Concurrency, Net, Tls }
let session r listener nursery =
  let server = r.concurrency.spawn nursery (fun _ -> Http.serveManagedListener r listener (fun _ -> Ok (Http.produced 200 Headers.empty produce)))
  let failed =
    match Http.get r ("http://127.0.0.1:" ++ Int.toString (r.net.localPort listener) ++ "/") with
    | Err _ -> false
    | Ok response -> Result.isErr (Http.bodyText response.body)
  let stopped = r.concurrency.cancel server
  r.console.writeLine (if failed then "ok" else "bad")
public main : Runtime -> Unit / { Console, Concurrency, Net, Tls }
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (session r listener)
"#);
    assert_eq!(output, "ok\n");
}
