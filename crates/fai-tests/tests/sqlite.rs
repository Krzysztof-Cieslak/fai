//! SQL package and native SQLite bridge integration on the real JIT runtime.

use std::path::Path;
use std::sync::Mutex;

use fai_db::Db;

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn run(body: &str, effects: &str) -> String {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    for name in ["sql/src/Sql.fai", "sql/src/SqlDecode.fai", "sqlite/src/Sqlite.fai"] {
        db.add_source(name.into(), std::fs::read_to_string(root.join(name)).unwrap());
    }
    let source = format!(
        r#"module SqliteTest
public type R = {{ clock : Clock, concurrency : Concurrency, console : Console, sqlite : SqliteHost.Sqlite }}
let runtime = {{ clock = stdClock, concurrency = stdConcurrency, console = stdConsole, sqlite = SqliteHost.native }}
{body}
public main : R -> Unit / {{ Console, {effects} }}
let main r = r.console.writeLine (if check r then "ok" else "failed")
"#
    );
    let id = db.add_source("SqliteTest.fai".into(), source);
    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(outcome.exit_code, 0, "{output}\n{:?}", outcome.diagnostics);
    output
}

#[test]
fn bound_values_and_typed_rows_round_trip() {
    assert_eq!(
        run(
            r#"
let values = [| Sql.Null, Sql.Integer (-9223372036854775808), Sql.Real 1.25, Sql.Text "a\u{0}é😀", Sql.Blob (Bytes.fromList [0, 255]), Sql.Blob Bytes.empty |]
let work session =
  match Sql.query session (Sql.statement "select ?, ?, ?, ?, ?, ?" values) with
  | Err error -> Err error
  | Ok rows -> Ok (Array.length rows = 1 && Sql.values (Array.unsafeGet 0 rows) = values)
check : R -> Bool / { SqliteHost.Sqlite }
let check r = Sqlite.withMemory r work = Ok true
"#,
            "SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}

#[test]
fn transactions_keep_parent_and_expired_aliases_out() {
    assert_eq!(
        run(
            r#"
let inside parent child =
  let blocked = Sql.execute parent (Sql.text "insert into t values (99)")
  let inserted = Sql.execute child (Sql.text "insert into t values (7)")
  Ok (child, Result.isErr blocked && Result.isOk inserted)
let work session =
  let created = Sql.execute session (Sql.text "create table t(value integer)")
  let saved = Sql.transaction Sql.Default session (inside session)
  match saved with
  | Err error -> Err error
  | Ok (expired, good) ->
    let closed = Sql.execute expired (Sql.text "insert into t values (8)")
    let rows = SqlDecode.query (SqlDecode.index 0 SqlDecode.int) session (Sql.text "select value from t")
    Ok (good && Result.isErr closed && rows = Ok [| 7 |])
check : R -> Bool / { SqliteHost.Sqlite }
let check r = Sqlite.withMemory r work = Ok true
"#,
            "SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}

#[test]
fn early_cursor_exit_invalidates_retained_rows() {
    assert_eq!(
        run(
            r#"
let work session =
  let escaped = Sql.withRows session (Sql.text "select 1 union all select 2") (fun rows -> Ok rows)
  match escaped with
  | Err error -> Err error
  | Ok rows ->
    let stale = Sql.next rows
    let fresh = SqlDecode.query (SqlDecode.index 0 SqlDecode.int) session (Sql.text "select 42")
    Ok (Result.isErr stale && fresh = Ok [| 42 |])
check : R -> Bool / { SqliteHost.Sqlite }
let check r = Sqlite.withMemory r work = Ok true
"#,
            "SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}

#[test]
fn cancellation_interrupts_sql_and_allows_later_queries() {
    assert_eq!(
        run(
            r#"
work : R -> Sql.Session { Clock, Concurrency, SqliteHost.Sqlite } -> Result Bool Sql.Error / { Clock, Concurrency, SqliteHost.Sqlite }
let work r session =
  let result = Async.timeout r.concurrency r.clock 50 (fun _ -> Sql.query session (Sql.text "with recursive n(x) as (values(1) union all select x+1 from n) select sum(x) from n"))
  let after = SqlDecode.query (SqlDecode.index 0 SqlDecode.int) session (Sql.text "select 42")
  Ok (result = None && after = Ok [| 42 |])
check : R -> Bool / { Clock, Concurrency, SqliteHost.Sqlite }
let check r = Sqlite.withMemory r (work r) = Ok true
"#,
            "Clock, Concurrency, SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}

#[test]
fn sqlite_native_handles_compare_by_identity() {
    assert_eq!(
        run(
            r#"
compareHandles : R -> SqliteHost.Session -> SqliteHost.Session -> Bool / { SqliteHost.Sqlite }
let compareHandles r a b =
  let good = a = a && a <> b
  let first = r.sqlite.close a
  let second = r.sqlite.close b
  good && first = Ok () && second = Ok ()
check : R -> Bool / { SqliteHost.Sqlite }
let check r =
  match r.sqlite.open ":memory:" false 0 with
  | Err _ -> false
  | Ok a ->
    match r.sqlite.open ":memory:" false 0 with
    | Err _ -> false
    | Ok b -> compareHandles r a b
"#,
            "SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}

#[test]
fn a_cancelled_transaction_rolls_back_before_the_parent_resumes() {
    assert_eq!(
        run(
            r#"
let inside child =
  let inserted = Sql.execute child (Sql.text "insert into items values (1)")
  Sql.query child (Sql.text "with recursive n(x) as (values(1) union all select x+1 from n) select sum(x) from n")
work : R -> Sql.Session { Clock, Concurrency, SqliteHost.Sqlite } -> Result Bool Sql.Error / { Clock, Concurrency, SqliteHost.Sqlite }
let work r session =
  let created = Sql.execute session (Sql.text "create table items(value integer)")
  let timed = Async.timeout r.concurrency r.clock 50 (fun _ -> Sql.transaction Sql.Default session inside)
  let after = SqlDecode.query (SqlDecode.index 0 SqlDecode.int) session (Sql.text "select count(*) from items")
  Ok (timed = None && after = Ok [| 0 |])
check : R -> Bool / { Clock, Concurrency, SqliteHost.Sqlite }
let check r = Sqlite.withMemory r (work r) = Ok true
"#,
            "Clock, Concurrency, SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}

#[test]
fn transaction_callbacks_forward_other_effects() {
    assert_eq!(
        run(
            r#"
inside : R -> Sql.Session { Console, SqliteHost.Sqlite } -> Result Unit Sql.Error / { Console, SqliteHost.Sqlite }
let inside r child =
  let wrote = r.console.writeLine "transaction"
  Result.map (fun _ -> ()) (Sql.execute child (Sql.text "create table items(value integer)"))
work : R -> Sql.Session { Console, SqliteHost.Sqlite } -> Result Unit Sql.Error / { Console, SqliteHost.Sqlite }
let work r session = Sql.transaction Sql.Default session (inside r)
check : R -> Bool / { Console, SqliteHost.Sqlite }
let check r = Sqlite.withMemory r (work r) = Ok ()
"#,
            "SqliteHost.Sqlite"
        ),
        "transaction\nok\n"
    );
}

#[test]
fn failed_deferred_constraint_commit_rolls_back() {
    assert_eq!(
        run(
            r#"
let insert child = Sql.execute child (Sql.text "insert into children values (99)")
let work session =
  let parent = Sql.execute session (Sql.text "create table parents(id integer primary key)")
  let child = Sql.execute session (Sql.text "create table children(id integer references parents(id) deferrable initially deferred)")
  let rejected = Sql.transaction Sql.Default session insert
  let empty = SqlDecode.query (SqlDecode.index 0 SqlDecode.int) session (Sql.text "select count(*) from children")
  let constraint = match rejected with | Err (Sql.Failure error) -> error.kind = Sql.Constraint && error.operation = "commit" | _ -> false
  Ok (constraint && empty = Ok [| 0 |])
check : R -> Bool / { SqliteHost.Sqlite }
let check r = Sqlite.withMemory r work = Ok true
"#,
            "SqliteHost.Sqlite"
        ),
        "ok\n"
    );
}
