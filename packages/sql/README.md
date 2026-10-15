# Shared SQL access for Fai

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

```sh
fai add github:Krzysztof-Cieslak/fai --subdir packages/sql --as sql --version '^0.1.0'
```

`Sql` and `SqlDecode` are a driver-independent source library. SQL text and
placeholder conventions belong to the chosen database; parameters are always
separate values. The package depends on the standalone `uuid` source package.

```fai
let user = SqlDecode.map2
  (fun id name -> { id = id, name = name })
  (SqlDecode.field "id" SqlDecode.int)
  (SqlDecode.field "name" SqlDecode.string)

let find session id =
  SqlDecode.query user session
    (Sql.statement "select id, name from users where id = ?" [| Sql.Integer id |])
```

## Values and rows

`Sql.Value` is `Null`, `Integer Int`, `Real Float`, `Text String`, or `Blob Bytes`.
It also supports `Boolean`, exact `Decimal` (`SqlDecimal`), validated `Uuid`
(`Uuid.Uuid`, also named `SqlUuid.Uuid`), `Date`, `Time`, `Timestamp`, `TimestampTz`, and JSON document text.
Temporal values explicitly represent positive/negative infinity; SQL time has
an `EndOfDay` form for 24:00:00. Decimal spelling/scale never pass through Float.
Conversions are explicit: integer and real decoders do not silently convert each
other. `SqlDecode.bool` accepts integers 0/1; `nullable` handles NULL. A missing
column is distinct from a present NULL. `field` uses exact column names and
rejects ambiguous duplicates; `index` always uses a zero-based ordinal.

Rows retain ordered column metadata and immutable cell snapshots. `Sql.row`
checks their lengths; `columns` and `values` expose the contents. `SqlDecode`
supports `map`, `map2`, `andMap`, custom conversions and pure validation. Query
decoding adds row/column locations to errors.

`SqlUuid` is a compatibility facade over the shared UUID type. Its `parse`
function still accepts only canonical 8-4-4-4-12 text, accepts uppercase hex, and
returns the existing `"invalid UUID"` String error on failure. `toString` still
renders canonical lowercase text. New application code can pass values from
`Uuid.parse`, `Uuid.v4`, or `Uuid.v7` directly to `Sql.Uuid`; `SqlDecode.uuid`
returns that same shared type. Every well-formed 128-bit value remains accepted,
including nil, max, non-RFC variants and reserved version numbers.

## Sessions and ownership

`Sql.Session 'e` is an opaque bundle of effect-parameterized driver operations.
`Sql.session` constructs it from execute/query/begin/commit/rollback functions,
so application tests can supply pure fake sessions. `Sql.fromList` supplies a
finite pure row source. Native drivers provide resource-backed implementations.

`execute` returns the directly affected row count. `query` buffers raw rows;
`SqlDecode.query` buffers typed results. `withRows session statement consume`
keeps a cursor in scope and closes it on success or failure. Use `Sql.next` or
`Sql.fold` for bounded-memory traversal. Pull each cursor state once; a driver
must reject native reads after close. Returned rows are independent snapshots.

`transaction isolation session work` passes a pinned child session to `work`,
commits an Ok result, and rolls back an Err or failed commit. Drivers reject
unsupported isolation levels and prevent unrelated aliases from joining a
transaction. Nested transaction behavior is documented by the driver.

`Sql.Error` preserves its category, operation, backend name/code and optional
row/column positions. `CleanupFailed` retains both the primary error and a failed
cleanup rather than replacing the original failure. Generated IDs are read with
RETURNING or driver-specific SQL, not an implicit connection-wide convention.
Backend codes are strings, accommodating both numeric SQLite codes and SQLSTATE
codes from other database drivers.

```sh
fai test -C packages sql
```
