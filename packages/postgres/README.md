# PostgreSQL

An explicit-capability PostgreSQL v3 client written in Fai. Include this directory
and its `sql` and `json` dependencies under your application's workspace root.

The protocol modules (`PgWire`, `PgAuth`, `PgTypes`, `PgConfig`, `PgConnect`)
provide bounded framing, checked scalar codecs, verified TLS, SCRAM-SHA-256 and
SCRAM-SHA-256-PLUS. Cryptography uses the standard `Crypto` primitives; SQL and
wire-protocol logic stay in Fai. `PgControl` supervises deadlines and cleanup.

```sh
fai fmt --check -C packages postgres
fai check --no-examples -C packages postgres
fai test -C packages postgres --seed 42 --count 128
```

Contracts run directly in Fai without a server. They cover malformed framing,
full-width integers, scalar representations, strict base64, and the published
SCRAM-SHA-256 proof vector.

## Connections and queries

`Postgres.withConnection runtime options callback` owns one connection. The runtime
requires `net`, `tls`, `clock`, `concurrency`, and secure `entropy` capabilities.
The callback receives the same `Sql.Session` interface used by SQLite. Its effect
row is open, allowing application effects such as console output.

Use `Sql.statement "select $1::bigint" [| Sql.Integer 42 |]` to bind parameters.
The server describes parameter types before binding; ambiguous types require an
explicit SQL cast. One statement is allowed per call. `Sql.execute` is for
commands without result columns, and `Sql.query`/`Sql.withRows` return rows.
`SqlDecode` works unchanged. `examples/Basic.fai` reads `FAI_PG_URL` explicitly;
optional `FAI_PG_ROOT` supplies an extra trusted PEM root certificate.

Transactions use `Sql.transaction`. Child sessions exclusively own the connection;
parent aliases cannot participate. Nested transactions use savepoints. An aborted
transaction cannot report a successful commit, even if the callback caught its
SQL error. Use the session API for transaction/cursor control rather than raw SQL.

Results use named portals and bounded fetches (`fetchRows`, `maxBatchBytes`,
`maxColumnBytes`, `maxFrameBytes`). Only one cursor is active on a session. Outside
an explicit transaction, a cursor owns an internal transaction. Closing it commits
successful SQL, including an early stop; a server error rolls it back. To roll
back because of a client-side decoder or consumer error, use an explicit
`Sql.transaction`. Cursor and transaction aliases expire when their scope ends.

Operation deadlines include waiting for session ownership and are per API call,
not a deadline on an entire cursor or transaction. Cancellation uses a separate
connection to the original numeric peer and then discards the original connection.
It never reuses an uncertain protocol state or permits a late cancel to target a
later borrower. SQL commands are never automatically replayed.

## Pooling

`Postgres.withPool runtime options poolOptions callback` owns a bounded pool.
Inside it, `Postgres.withSession pool callback` acquires an exclusive session.
FIFO admission prevents later waiters from taking a connection ahead of the queue
head. Acquisition is cancellable and bounded by both waiter count and deadline.
Dead idle connections are checked before exposing a lease; only this validation
may reconnect. An application command is never retried.

Returned connections run `ROLLBACK` when needed, then `DISCARD ALL`. Reset must
succeed before reuse. Escaped sessions, transaction children, and cursors retain
their original generation and cannot access another borrower's connection.
Idle expiry closes idle connections; maximum lifetime never interrupts a lease
or transaction and retires the connection on return. Minimum capacity is warmed
on entry and replenished by maintenance. Scope exit closes the pool, wakes
waiters, closes active connections, and joins maintenance.

| Connection option | Default |
| --- | --- |
| `connectTimeoutMs`, `operationTimeoutMs` | `Some 30000` |
| `cleanupTimeoutMs` | `5000` |
| `fetchRows` | `128` |
| `maxFrameBytes`, `maxColumnBytes`, `maxBatchBytes` | 64 MiB, 16 MiB, 32 MiB |
| `maxScramIterations` | 1,000,000 |
| `tls`, `channelBinding` | verified TLS, prefer channel binding |

| Pool option | Default |
| --- | --- |
| `minConnections`, `maxConnections`, `maxWaiters` | `0`, `10`, `128` |
| `acquireTimeoutMs` | `Some 30000` |
| `idleTimeoutMs`, `maxLifetimeMs` | `Some 300000`, `Some 1800000` |
| `validationTimeoutMs`, `maintenanceIntervalMs` | `5000`, `1000` |

Optional durations accept `None` to disable them. Cleanup/validation/maintenance
durations must remain positive. Connection options also contain host, port, user,
password, database, and application name; no ambient credentials are read.

## Scalar mapping

| PostgreSQL | `Sql.Value` |
| --- | --- |
| boolean | `Boolean` |
| smallint/integer/bigint, oid | `Integer` (checked range on binding) |
| real/double precision | `Real` (binary results preserve IEEE special values) |
| text/varchar/char/name | `Text` (strict UTF-8, no NUL on binding) |
| bytea | `Blob` |
| numeric/decimal | `Decimal` (`SqlDecimal`, exact text and scale) |
| uuid | `Uuid` (`SqlUuid`, canonical spelling) |
| date, time, timestamp, timestamptz | `Date`, `Time`, `Timestamp`, `TimestampTz` |
| json/jsonb | `Json` (validated document text) |

Temporal infinities and `24:00:00` are explicit SQL values. Timestamptz represents
an instant, independent of the session time zone. Sub-microsecond input is rejected.
Binding to `real` requires exact float32 representability; use `$1::float8::float4`
when intentional server-side rounding is desired. Numeric never passes through
Float. SQL casts/type modifiers can intentionally round or transform inputs.

Unsupported OIDs (including arrays, ranges and custom types) require an explicit
cast to a supported type. COPY, replication, multi-statement calls, MD5 and
cleartext-password authentication are not supported. Trust authentication is
accepted unless channel binding is required. Notices and asynchronous
notifications are validated and consumed without a user callback.

## Integration checks

Against a disposable database configured for SCRAM authentication:

```sh
export FAI_PG_URL='postgres://user:password@localhost:5432/test?sslmode=disable'
fai run --no-daemon -C packages postgres/examples/Integration.fai
fai build -C packages postgres/examples/Integration.fai --out ./pg-check
./pg-check
fai run --no-daemon -C packages postgres/examples/WireChecks.fai
```

For TLS, omit `sslmode=disable` and set `FAI_PG_ROOT` to a PEM CA certificate.
The TLS integration fixture requires SCRAM channel binding. `WireChecks` owns its
own loopback scripted server and checks fragmentation, truncation, authentication
order and incomplete query responses. No external service is needed for it.

CI provisions PostgreSQL on Linux, macOS and Windows and runs these Fai programs
directly through JIT and AOT. Linux additionally tests a private-CA TLS endpoint.
Package-only edits use the exact cached compiler; the ordinary package contracts
remain direct `fai test` commands.
