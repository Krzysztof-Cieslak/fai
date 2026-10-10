# SQLite for Fai

`Sqlite` implements the shared `Sql.Session` API using an embedded SQLite engine.
Include both `packages/sql/src` and `packages/sqlite/src` in your workspace.
The standard `SqliteHost` module supplies the native capability; application code
passes it explicitly, usually in a custom runtime record:

```fai
let runtime = { console = stdConsole, sqlite = SqliteHost.native }

public main : { console : Console, sqlite : SqliteHost.Sqlite } -> Unit / { Console, SqliteHost.Sqlite }
let main env =
  let result = Sqlite.withMemory env (fun session ->
    SqlDecode.query (SqlDecode.index 0 SqlDecode.int) session
      (Sql.statement "select ? + ?" [| Sql.Integer 20, Sql.Integer 22 |]))
  match result with
  | Ok values -> env.console.writeLine (Int.toString (Array.unsafeGet 0 values))
  | Err error -> env.console.writeLine (Sql.errorToString error)
```

## Connections and options

`withMemory env work` creates a private in-memory database. For a file, use
`withConnection env options path work`. The callback receives a shared SQL
session; every result path closes the database. Retained sessions and cursors
fail after scope exit. Returned row snapshots remain ordinary immutable data.

`Sqlite.defaults` configures a writable database, a 5000 ms busy timeout and
`Deferred` transactions. Options are ordinary record updates:

- `readOnly`: open an existing database read-only.
- `busyTimeoutMs`: nonnegative milliseconds; zero fails immediately when busy.
- `transactionMode`: `Deferred`, `Immediate`, or `Exclusive` for outer scopes.

Foreign keys start enabled. Filenames are literal paths, with `:memory:` as the
explicit in-memory spelling. Empty paths and embedded NULs are rejected.

## Binding and decoding

Supply exactly one statement. Trailing comments, whitespace and empty semicolons
are accepted; additional SQL statements are rejected. Use separate calls for
scripts. Parameters use SQLite's `?`, `?NNN`, `:name`, `@name` or `$name` syntax,
bound by SQLite parameter index. Repeated names share one binding; sparse `?NNN`
indices still require the complete indexed parameter array.

Values are `Sql.Null`, `Integer`, `Real`, `Text` and `Blob`. Text and blobs retain
embedded NULs and distinguish empty values from NULL. Integer bindings support
the complete signed 64-bit range. Non-finite real parameters are rejected instead
of being silently converted to NULL. Invalid UTF-8 database text is an error;
binary content belongs in a blob. SQLite's own affinity rules still apply.

`Sql.execute` requires a command without returned columns. For SELECT, PRAGMA
results, and INSERT/UPDATE/DELETE RETURNING, use `Sql.query`, `SqlDecode.query`, or
`Sql.withRows`. Each cursor is stepped incrementally and finalized on scope exit.
Result metadata exists even for zero rows. SQL decoders do not silently coerce
integers, reals, text or blobs; use explicit SQL casts or custom decoders.

## Transactions and concurrency

`Sql.transaction Sql.Default session work` passes a pinned child session, commits
Ok results, and rolls back errors or failed commits. `Sql.Serializable` is also
supported. Other isolation levels return `Unsupported`. Nested transactions use
savepoints. Close active cursors before beginning a transaction or savepoint.

Use the child session inside the callback. Parent aliases are rejected while a
child owns the transaction, so unrelated work cannot accidentally join it.
Committed or rolled-back child aliases are invalid. Ending a transaction also
finalizes its outstanding cursors. Raw transaction-control SQL and ATTACH/DETACH
are rejected; transaction ownership belongs to the session API.

Cursor cleanup finalizes a statement; it does not undo autocommitted writes.
Wrap operations in `Sql.transaction` when an application failure must roll back
their effects, including row-producing DML.

Each native connection serializes operations. In a scheduled task, SQLite work
runs on the blocking pool. Busy waits and SQLite progress callbacks observe that
task's cancellation, while rollback/close/finalization remain available during
cleanup. Without a scheduler, calls run directly on the calling thread.

Errors retain the SQLite extended result code and backend name. The shared
categories distinguish constraints, busy/locked operations, cancellation, closed
resources, invalid transaction ownership and unsupported operations. If cleanup
also fails, `Sql.CleanupFailed` preserves the original failure.

## Run and test

```sh
fai test -C packages sql
fai test -C packages sqlite
fai run -C packages sqlite/examples/SqliteExample.fai
fai run -C packages sqlite/examples/TransactionChecks.fai
fai run -C packages sqlite/examples/ValueChecks.fai
fai run -C packages sqlite/examples/FileDatabase.fai -- visits.db
```

Package contracts cover pure values, decoding, options and fake-session behavior.
Native in-memory/file-backed tests cover binding, transactions, cancellation,
cursor lifetime and JIT/AOT execution. The compiler embeds SQLite; applications
do not need a separately installed SQLite library.
