//! Retry policy preserves original write, producer, and cancellation failures.

use fai_db::{Db, Setter};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn original_error(send: &str, recv: &str, body: &str, expected: &str) {
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
testRetry : Runtime -> Bool / {{ Concurrency, Net, Tls }}
let testRetry r =
  let client = MkClient (r.concurrency.channel 1) None
  let t = {{ Transport with close u = (), recv n = {recv}, send bytes = {send} }}
  match Url.parse "http://127.0.0.1:0/" with
  | Err e -> false
  | Ok url ->
    let req = {{ method = GET, url = url, headers = Headers.empty, body = {body} }}
    match attemptPooled r client "origin" t Bytes.empty req true with
    | Err e -> e = {expected:?}
    | Ok response -> false
public main : Runtime -> Unit / {{ Concurrency, Console, Net, Tls }}
let main r = r.console.writeLine (if testRetry r then "ok" else "failed")
"#,
        file.text(&db)
    );
    file.set_text(&mut db).to(source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

#[test]
fn partial_write_failure_is_not_retried() {
    original_error("Err \"partial write\"", "Ok Bytes.empty", "emptyBody", "partial write");
}

#[test]
fn body_producer_failure_is_not_retried() {
    original_error(
        "Ok ()",
        "Ok Bytes.empty",
        "Stream.failed \"producer failed\"",
        "producer failed",
    );
}

#[test]
fn cancellation_is_not_retried() {
    original_error("Ok ()", "Err \"operation cancelled\"", "emptyBody", "operation cancelled");
}
