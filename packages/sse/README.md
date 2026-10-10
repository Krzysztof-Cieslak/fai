# Server-sent events for Fai

`Sse` is a shared protocol codec used by HTTP clients and servers. It depends
only on the standard library. Include `packages/sse/src` in your workspace.

```fai
let frames = Stream.fromList [Sse.comment "connected", Sse.event "hello"]
let bytes = Sse.encodeStream frames
let decoded = Sse.events Sse.defaults bytes
```

## Reading

`Sse.events limits byteStream` yields `{ data, eventType, lastEventId }` records.
`Sse.updates` also yields ordered `Checkpoint id` and `Retry milliseconds`
observations. An ID-only block affects reconnection even though it dispatches no
event. `Sse.parse limits bytes` decodes a complete buffer to a list of updates.

For typed errors and exact consumption boundaries, create `Sse.decoder limits`
and call `Sse.next decoder chunk index`. Its `Progress` contains the new decoder,
the next byte index, and at most one update. Continue with that index and chunk;
when no update is returned the chunk is exhausted. Discard an unfinished decoder
at EOF. `decoderFrom limits lastId` starts fresh connection framing while retaining
a subscription's committed ID; only an explicit empty ID clears it. Processing
stops at each update, preserving events before a later error
and letting a consumer stop without advancing its cursor into later chunk data.

The codec follows the WHATWG event-stream rules: UTF-8 replacement decoding, one
leading BOM, LF/CRLF/CR, exact field names, one optional space after the colon,
multiline data, default `message` type, ID persistence/reset, and numeric retry
hints. Invalid retry fields and unknown fields are ignored; IDs containing NUL
are ignored. Unrepresentable positive retry hints saturate at the largest Int.
An unfinished final event is not dispatched. No vendor-specific sentinel data is
interpreted. Stream adapters render errors as strings; the incremental API keeps
structured error kinds, zero-based byte offsets, and one-based line numbers.

Limits default to 64 KiB of wire bytes per line and 1 MiB of decoded UTF-8 data
per event. They are configurable, nonnegative, and exclude line terminators.

## Writing

- `event data` builds a message; `withType name frame` and `withId id frame`
  validate and attach metadata, returning `Result Frame Error`.
- `comment text` builds heartbeat/comment lines.
- `retry milliseconds` builds a validated nonnegative reconnect hint.
- `checkpoint id` builds an ID-only block; an empty ID clears the cursor.
- `encode frame` returns complete UTF-8 bytes with LF framing and the final blank
  line; `encodeStream frames` does this lazily with the source's effects.

Frames are opaque and validated. Names cannot contain CR/LF; IDs additionally
cannot contain NUL. Data and comment line endings normalize to LF. Empty and
trailing data lines are preserved.

## Scoped server production

`SseServer.produce runtime options frames` is an `Http.BodyProducer` for use with
`Http.produced` and managed HTTP/HTTPS listeners. It runs while the response is
actually sent. `SseServer.defaults` disables automatic heartbeats; set
`heartbeatIntervalMs = Some milliseconds` to enable them. The interval is fully
configurable and must be positive. Explicit `Sse.comment` frames always work.

With heartbeats enabled, source and timer tasks feed a bounded queue, acknowledge
each item, and are cancelled/joined when the writer exits. A timer tick cannot
discard a pending source event. Recent data suppresses unnecessary heartbeats.
`Web.sse` and `Web.sseWith` integrate this with ordinary Web routes.

## Native contracts

```sh
fai fmt -C packages sse
fai test -C packages sse
```

Contracts cover malformed UTF-8, split code points/BOM/CRLF, ID-only blocks,
limits and locations, partial input, and encoding/decoding properties.
