//! Native filesystem ownership, error records and lossless paths through the JIT.

use fai_db::Db;

// Scheduler task output shares the runtime's process-wide capture sink.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_case(body: &str) -> String {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

#[test]
fn enumerated_native_name_reopens_without_unicode_conversion() {
    assert_eq!(
        run_case(
            r#"  let directory = Option.withDefault Path.empty (Path.parent path)
  let units = if Path.nativeEncoding = "unix" then [255] else [0, 216]
  let name = Result.withDefault Path.empty (Path.fromNativeBytes (Bytes.fromList units))
  let native = Path.join directory name
  let written = r.fs.writeBytes native (Bytes.fromString "contents")
  match r.fs.openDirectory directory with
  | Err error -> r.console.writeLine (FileSystem.errorToString error)
  | Ok cursor ->
    let item = r.fs.nextDirectory cursor
    let closed = r.fs.closeDirectory cursor
    match item with
    | Ok (Some entry) ->
      let bytes = r.fs.readBytes entry.path 8
      let metadata = r.fs.metadata entry.path
      let same = r.fs.sameFile native entry.path
      r.console.writeLine (if entry.name = name && bytes = Ok (Bytes.fromString "contents") && same = Ok true && (match metadata with | Ok value -> value.length = 8 && value.kind = FileSystem.RegularFile && value.modified <> None | _ -> false) then "ok" else "bad")
    | _ -> r.console.writeLine "missing entry""#
        ),
        "ok\n"
    );
}

#[test]
fn directory_walk_threads_state_and_sorted_names() {
    assert_eq!(
        run_case(
            r#"  let root = Option.withDefault Path.empty (Path.parent path)
  let sub = Path.join root (Result.withDefault Path.empty (Path.fromString "sub"))
  let made = r.fs.createDirectory sub
  let first = r.fs.writeFile path "a"
  let second = r.fs.writeFile (Path.join sub (Result.withDefault Path.empty (Path.fromString "leaf"))) "b"
  let names = Directory.list r 10 root
  let result = Directory.walk r { Directory.walkOptions with order = Directory.SortedOrder } root 0 (fun depth entry count -> Ok (count + 1, Directory.Continue))
  r.console.writeLine (if result.value = 3 && result.visited = 3 && result.outcome = Ok Directory.Exhausted && (match names with | Ok entries -> List.length entries = 2 | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn directory_stop_and_limit_retain_committed_state() {
    assert_eq!(
        run_case(
            r#"  let root = Option.withDefault Path.empty (Path.parent path)
  let made = r.fs.writeFile path "a"
  let stopped = Directory.walk r Directory.walkOptions root 0 (fun depth entry count -> Ok (count + 1, Directory.Stop))
  let limited = Directory.walk r { Directory.walkOptions with maxEntries = 0 } root 7 (fun depth entry count -> Ok (count + 1, Directory.Continue))
  r.console.writeLine (if stopped.value = 1 && stopped.outcome = Ok Directory.Stopped && limited.value = 7 && Result.isErr limited.outcome && stopped.cleanupError = None then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn temporary_scope_removes_entry_and_expires_writer() {
    assert_eq!(
        run_case(
            r#"  let parent = Option.withDefault Path.empty (Path.parent path)
  let scoped = Temp.withFile r parent (fun temp -> Ok (temp.path, temp.writer))
  match scoped with
  | Err _ -> r.console.writeLine "scope failed"
  | Ok (temporary, writer) ->
    let exists = FileSystem.entryExists r.fs temporary
    let late = r.fs.writeChunk writer Bytes.empty
    r.console.writeLine (if exists = Ok false && (match late with | Err error -> error.kind = FileSystem.Closed | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn persisted_temporary_survives_scope_and_flushes() {
    assert_eq!(
        run_case(
            r#"  let parent = Option.withDefault Path.empty (Path.parent path)
  let save temp =
    let written = r.fs.writeChunk temp.writer (Bytes.fromString "saved")
    Temp.persist r.fs temp.resource path false
  let scoped = Temp.withFile r parent save
  let contents = r.fs.readFile path
  r.console.writeLine (if scoped = Ok path && contents = Ok "saved" then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn atomic_write_commits_and_cleans_the_sibling() {
    assert_eq!(
        run_case(
            r#"  let first = r.fs.writeFile path "old"
  let result = File.writeTextAtomic r { File.atomicOptions with durability = File.SyncFile } path "new"
  let contents = r.fs.readFile path
  let parent = Option.withDefault Path.empty (Path.parent path)
  let entries = Directory.list r 10 parent
  r.console.writeLine (if result.phase = File.Committed && result.outcome = Ok () && result.cleanupError = None && contents = Ok "new" && (match entries with | Ok entries -> List.length entries = 1 | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn atomic_callback_failure_preserves_the_destination() {
    assert_eq!(
        run_case(
            r#"  let first = r.fs.writeFile path "old"
  let fail writer =
    let written = r.fs.writeChunk writer (Bytes.fromString "partial")
    Err "callback failed"
  let result = File.withAtomicWriter r File.atomicOptions path fail
  let contents = r.fs.readFile path
  let entries = Directory.list r 10 (Option.withDefault Path.empty (Path.parent path))
  r.console.writeLine (if result.phase = File.NotCommitted && result.outcome = Err (File.CallbackError "callback failed") && contents = Ok "old" && (match entries with | Ok entries -> List.length entries = 1 | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn atomic_no_clobber_failure_reports_attempted_commit() {
    assert_eq!(
        run_case(
            r#"  let first = r.fs.writeFile path "old"
  let result = File.writeTextAtomic r { File.atomicOptions with overwrite = false } path "new"
  let contents = r.fs.readFile path
  r.console.writeLine (if result.phase = File.CommitUnknown && Result.isErr result.outcome && contents = Ok "old" && result.cleanupError = None then "ok" else "bad")"#
        ),
        "ok\n"
    );
}

#[test]
fn seek_repositions_a_reader_and_rejects_negative_offsets() {
    assert_eq!(
        run_case(
            r#"  let written = r.fs.writeFile path "abcd"
  match r.fs.openRead path with
  | Err _ -> r.console.writeLine "open failed"
  | Ok reader ->
    let at = r.fs.seekReader reader 2
    let tail = r.fs.readChunk reader 9223372036854775807
    let invalid = r.fs.seekReader reader (-9223372036854775807 - 1)
    r.console.writeLine (if at = Ok 2 && tail = Ok (Bytes.fromString "cd") && (match invalid with | Err error -> error.kind = FileSystem.InvalidArgument | _ -> false) then "ok" else "bad")"#
        ),
        "ok\n"
    );
}
