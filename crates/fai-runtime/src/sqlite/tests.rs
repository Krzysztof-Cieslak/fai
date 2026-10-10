//! Native SQLite ownership, binding, transaction, and error contracts.

use super::*;

fn memory() -> Arc<Session> {
    open(":memory:", false, 100, &cancellation_probe()).unwrap()
}

fn command(session: &Session, sql: &str, values: &[Cell]) -> i64 {
    execute(session, sql, values, &cancellation_probe()).unwrap()
}

fn all(session: &Arc<Session>, sql: &str, values: &[Cell]) -> Vec<Vec<Cell>> {
    let cursor = query(session, sql, values, &cancellation_probe()).unwrap();
    let mut rows = Vec::new();
    while let Some(row) = next(&cursor, &cancellation_probe()).unwrap() {
        rows.push(row);
    }
    rows
}

#[test]
fn values_round_trip_without_losing_null_empty_or_full_width_values() {
    let db = memory();
    let values = vec![
        Cell::Null,
        Cell::Integer(i64::MIN),
        Cell::Integer(i64::MAX),
        Cell::Real(-1.25),
        Cell::Text("a\0é😀".into()),
        Cell::Blob(vec![0, 255]),
        Cell::Text(String::new()),
        Cell::Blob(Vec::new()),
    ];
    assert_eq!(all(&db, "select ?, ?, ?, ?, ?, ?, ?, ?", &values), vec![values]);
}

#[test]
fn bindings_do_not_execute_injected_sql() {
    let db = memory();
    command(&db, "create table items(value text)", &[]);
    let text = Cell::Text("'); drop table items; --".into());
    assert_eq!(command(&db, "insert into items values (?)", std::slice::from_ref(&text)), 1);
    assert_eq!(all(&db, "select value from items", &[]), vec![vec![text]]);
}

#[test]
fn missing_bindings_are_rejected() {
    let db = memory();
    let error =
        query(&db, "select ?1, ?2", &[Cell::Integer(1)], &cancellation_probe()).err().unwrap();
    assert_eq!(error.code, 21);
    assert_eq!(error.message, "expected 2 parameters, got 1");
}

#[test]
fn surplus_bindings_are_rejected() {
    let db = memory();
    assert_eq!(
        query(&db, "select 1", &[Cell::Integer(1)], &cancellation_probe()).err().unwrap().code,
        21
    );
}

#[test]
fn repeated_named_parameters_share_a_binding() {
    let db = memory();
    assert_eq!(
        all(&db, "select :value, :value", &[Cell::Integer(7)]),
        vec![vec![Cell::Integer(7), Cell::Integer(7)]]
    );
}

#[test]
fn multiple_statements_after_empty_statements_are_rejected() {
    let db = memory();
    assert_eq!(
        query(&db, "select 1;; -- gap\n select 2", &[], &cancellation_probe()).err().unwrap().code,
        21
    );
}

#[test]
fn rejected_tail_is_not_prepared_for_side_effects() {
    let db = memory();
    assert_eq!(
        query(&db, "select 1; PRAGMA foreign_keys=OFF", &[], &cancellation_probe())
            .err()
            .unwrap()
            .code,
        21
    );
    assert_eq!(all(&db, "PRAGMA foreign_keys", &[]), vec![vec![Cell::Integer(1)]]);
}

#[test]
fn trailing_comments_and_semicolons_are_accepted() {
    let db = memory();
    assert_eq!(all(&db, "; select 7; ; /* end */ -- final", &[]), vec![vec![Cell::Integer(7)]]);
}

#[test]
fn transaction_sql_cannot_bypass_session_leases() {
    let db = memory();
    assert_eq!(
        execute(&db, "BEGIN", &[], &cancellation_probe()).unwrap_err().code & 255,
        i64::from(ffi::SQLITE_AUTH)
    );
}

#[test]
fn child_transactions_isolate_parent_aliases_and_commit() {
    let db = memory();
    command(&db, "create table items(value integer)", &[]);
    let tx = begin(&db, 1, &cancellation_probe()).unwrap();
    command(&tx, "insert into items values (7)", &[]);
    assert_eq!(
        execute(&db, "insert into items values (8)", &[], &cancellation_probe()).unwrap_err().code,
        -2
    );
    commit(&tx, &cancellation_probe()).unwrap();
    assert_eq!(all(&db, "select value from items", &[]), vec![vec![Cell::Integer(7)]]);
    assert_eq!(
        execute(&tx, "insert into items values (9)", &[], &cancellation_probe()).unwrap_err().code,
        -1
    );
}

#[test]
fn dropping_an_uncommitted_session_rolls_back() {
    let db = memory();
    command(&db, "create table items(value integer)", &[]);
    let tx = begin(&db, 0, &cancellation_probe()).unwrap();
    command(&tx, "insert into items values (7)", &[]);
    drop(tx);
    assert_eq!(all(&db, "select count(*) from items", &[]), vec![vec![Cell::Integer(0)]]);
}

#[test]
fn nested_savepoint_rollback_preserves_outer_work() {
    let db = memory();
    command(&db, "create table items(value integer)", &[]);
    let outer = begin(&db, 0, &cancellation_probe()).unwrap();
    command(&outer, "insert into items values (1)", &[]);
    let inner = begin(&outer, 0, &cancellation_probe()).unwrap();
    command(&inner, "insert into items values (2)", &[]);
    rollback(&inner).unwrap();
    commit(&outer, &cancellation_probe()).unwrap();
    assert_eq!(all(&db, "select value from items", &[]), vec![vec![Cell::Integer(1)]]);
}

#[test]
fn close_invalidates_retained_cursor_aliases() {
    let db = memory();
    let cursor =
        query(&db, "select 1 as value union all select 2", &[], &cancellation_probe()).unwrap();
    assert_eq!(cursor.columns[0].name, "value");
    assert_eq!(next(&cursor, &cancellation_probe()).unwrap(), Some(vec![Cell::Integer(1)]));
    close(&db).unwrap();
    assert_eq!(next(&cursor, &cancellation_probe()).unwrap_err().code, -1);
    close(&db).unwrap();
}

#[test]
fn exhausted_returning_cursor_never_reexecutes() {
    let db = memory();
    command(&db, "create table items(value integer)", &[]);
    let cursor =
        query(&db, "insert into items values (1) returning value", &[], &cancellation_probe())
            .unwrap();
    assert_eq!(next(&cursor, &cancellation_probe()).unwrap(), Some(vec![Cell::Integer(1)]));
    assert_eq!(next(&cursor, &cancellation_probe()).unwrap(), None);
    assert_eq!(next(&cursor, &cancellation_probe()).unwrap(), None);
    drop(cursor);
    assert_eq!(all(&db, "select count(*) from items", &[]), vec![vec![Cell::Integer(1)]]);
}

#[test]
fn automatic_rollback_invalidates_the_child_session() {
    let db = memory();
    command(&db, "create table items(value integer unique on conflict rollback)", &[]);
    let tx = begin(&db, 0, &cancellation_probe()).unwrap();
    command(&tx, "insert into items values (1)", &[]);
    assert_eq!(
        execute(&tx, "insert into items values (1)", &[], &cancellation_probe()).unwrap_err().code
            & 255,
        i64::from(ffi::SQLITE_CONSTRAINT)
    );
    rollback(&tx).unwrap();
    assert_eq!(all(&db, "select count(*) from items", &[]), vec![vec![Cell::Integer(0)]]);
}

#[test]
fn file_backed_read_only_connections_reject_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let text = path.to_str().unwrap();
    let db = open(text, false, 0, &cancellation_probe()).unwrap();
    command(&db, "create table items(value integer)", &[]);
    drop(db);
    let readonly = open(text, true, 0, &cancellation_probe()).unwrap();
    assert_eq!(
        execute(&readonly, "insert into items values (1)", &[], &cancellation_probe())
            .unwrap_err()
            .code
            & 255,
        i64::from(ffi::SQLITE_READONLY)
    );
}

#[test]
fn non_finite_parameters_are_not_silently_converted_to_null() {
    let db = memory();
    assert_eq!(
        query(&db, "select ?", &[Cell::Real(f64::NAN)], &cancellation_probe()).err().unwrap().code,
        20
    );
}

#[test]
fn invalid_utf8_text_is_reported_without_lossy_conversion() {
    let db = memory();
    let cursor = query(&db, "select cast(x'ff' as text)", &[], &cancellation_probe()).unwrap();
    assert_eq!(next(&cursor, &cancellation_probe()).unwrap_err().code, 20);
}

#[test]
fn one_connection_serializes_independent_threads() {
    let db = memory();
    command(&db, "create table items(value integer)", &[]);
    let left = Arc::clone(&db);
    let right = Arc::clone(&db);
    let a = std::thread::spawn(move || command(&left, "insert into items values (1)", &[]));
    let b = std::thread::spawn(move || command(&right, "insert into items values (2)", &[]));
    assert_eq!(a.join().unwrap() + b.join().unwrap(), 2);
    assert_eq!(all(&db, "select sum(value) from items", &[]), vec![vec![Cell::Integer(3)]]);
}

#[test]
fn ddl_does_not_report_a_previous_change_count() {
    let db = memory();
    command(&db, "create table items(value integer)", &[]);
    assert_eq!(command(&db, "insert into items values (1)", &[]), 1);
    assert_eq!(command(&db, "create table other(value integer)", &[]), 0);
}

#[test]
fn busy_connections_recover_after_the_lock_is_released() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("busy.db");
    let path = path.to_str().unwrap();
    let first = open(path, false, 0, &cancellation_probe()).unwrap();
    command(&first, "create table items(value integer)", &[]);
    let second = open(path, false, 0, &cancellation_probe()).unwrap();
    let transaction = begin(&first, 1, &cancellation_probe()).unwrap();
    assert_eq!(
        execute(&second, "insert into items values (1)", &[], &cancellation_probe())
            .unwrap_err()
            .code
            & 255,
        i64::from(ffi::SQLITE_BUSY)
    );
    rollback(&transaction).unwrap();
    assert_eq!(command(&second, "insert into items values (1)", &[]), 1);
}
