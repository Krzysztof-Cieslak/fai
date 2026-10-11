//! Native filesystem ownership, error records and lossless paths through the JIT.

use fai_db::Db;

// Scheduler task output shares the runtime's process-wide capture sink.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_case(body: &str) -> String {
    let _guard = SERIAL.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let source = format!(
        r#"module FileChecks
let path = Result.withDefault Path.empty (Path.fromString {:?})
public main : Runtime -> Unit / {{ Concurrency, Console, FileSystem }}
let main r =
  let scheduled = r.concurrency.scope (fun _ -> ())
{}
"#,
        path.to_str().unwrap().replace('\\', "/"),
        body
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("FileChecks.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    output
}

#[test]
fn close_expires_all_aliases_and_is_idempotent() {
    assert_eq!(
        run_case(
            r#"  let wrote = r.fs.writeFile path "hello"
  match r.fs.openRead path with
  | Err error -> r.console.writeLine (FileSystem.errorToString error)
  | Ok reader ->
    let closed = r.fs.closeReader reader
    let closedAgain = r.fs.closeReader reader
    let read = r.fs.readChunk reader 1
    r.console.writeLine (if closed = Ok () && closedAgain = Ok () && (match read with | Err error -> error.kind = FileSystem.Closed && error.path = Some path | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn scoped_writer_expires_returned_alias() {
    assert_eq!(
        run_case(
            r#"  let scope = File.withWriter r FileSystem.CreateNew path (fun writer -> Ok writer)
  match scope with
  | Err _ -> r.console.writeLine "scope failed"
  | Ok writer ->
    let result = r.fs.writeChunk writer (Bytes.fromString "late")
    r.console.writeLine (if (match result with | Err error -> error.kind = FileSystem.Closed | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn exclusive_create_keeps_existing_contents() {
    assert_eq!(
        run_case(
            r#"  let wrote = r.fs.writeFile path "keep"
  let created = r.fs.openWriter path FileSystem.CreateNew
  let contents = r.fs.readFile path
  r.console.writeLine (if contents = Ok "keep" && (match created with | Err error -> error.kind = FileSystem.AlreadyExists && error.nativeCode <> None | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn binary_contents_are_checked_only_at_text_boundary() {
    assert_eq!(
        run_case(
            r#"  let bytes = Bytes.fromList [255, 0, 1]
  let wrote = r.fs.writeBytes path bytes
  let read = r.fs.readBytes path 3
  let text = r.fs.readFile path
  let limited = r.fs.readBytes path 2
  r.console.writeLine (if read = Ok bytes && (match text with | Err error -> error.kind = FileSystem.InvalidEncoding | _ -> false) && (match limited with | Err error -> error.kind = FileSystem.TooLarge | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn zero_read_is_not_end_of_file() {
    assert_eq!(
        run_case(
            r#"  let wrote = r.fs.writeFile path "data"
  match r.fs.openRead path with
  | Err _ -> r.console.writeLine "open failed"
  | Ok reader ->
    let invalid = r.fs.readChunk reader 0
    let read = r.fs.readChunk reader 1
    r.console.writeLine (if read = Ok (Bytes.fromString "d") && (match invalid with | Err error -> error.kind = FileSystem.InvalidArgument | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}
