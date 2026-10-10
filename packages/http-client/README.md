# Typed HTTP clients for Fai

`HttpClient` is an ordinary Fai source package over the standard HTTP/1.1 and
TLS implementation. Include this package and `packages/json/src` beneath your
workspace root. Request construction and response decoding are pure; executing a
request takes capabilities from the application.

```fai
let user = JsonDecode.map2
  (fun id name -> { id = id, name = name })
  (JsonDecode.field "id" JsonDecode.int)
  (JsonDecode.field "name" JsonDecode.string)

let fetch runtime =
  HttpClient.withClient runtime (HttpClient.config "https://api.example.com/v1/") (fun client ->
    HttpClient.get "users/42"
    |> HttpClient.expectJson user
    |> HttpClient.send client)
```

`withClient` validates configuration, opens a connection-pool scope, and closes
the pool when the callback finishes. The callback returns `Result 'a Error`.
Keep the client inside this scope. Concurrent requests can share a client.

## Requests

`get`, `head`, `post`, `put`, `patch`, `delete`, and `request method target`
construct immutable requests. Pipe them through:

| Builder | Behavior |
|---|---|
| `header name value` | Replace request headers of this name |
| `addHeader name value` | Append, preserving duplicate fields |
| `removeHeader name` | Remove request and client-default fields |
| `query name value` | Append a percent-encoded pair, preserving duplicates |
| `bytesBody mediaType bytes` | Supply immutable binary data |
| `textBody text` | UTF-8 text/plain body |
| `jsonBody encoder value` | Encode JSON once, before any network activity |
| `formBody pairs` | URL-encoded form, preserving pair order |

Names of headers are case-insensitive. Request headers replace the complete
same-name group of client defaults. Defaults apply only to the base URL's origin;
an absolute request to another origin receives only explicitly supplied request
headers. Host and transfer framing are generated from the prepared request.

Targets use standard URL resolution. With base `https://example.com/v1/`, `users`
addresses `/v1/users`, `/users` addresses the origin root, and an absolute URL
selects its own origin. A trailing slash on a base path is significant. HTTP and
HTTPS URLs are supported; URL userinfo is rejected. Use an Authorization header
with `Http.bearer` or `Http.basicAuth` instead.

`prepare config request` returns a validated `Prepared` wire description without
I/O, including the resolved URL, headers, method and bytes. Invalid configuration,
method/header tokens, URLs, and JSON encoding fail before a connection is opened.

## Responses

`send client request` returns `Result (Response 'a) Error`. A response has `body`
and `head`; the head carries status, reason, headers, final URL and attempt count.

A raw request returns bytes for every status, including 4xx and 5xx. Add an
expectation to require a 2xx result:

- `expectBytes` retains binary data.
- `expectText` checks UTF-8 and returns a String.
- `expectEmpty` consumes and discards the body, returning Unit.
- `expectJson decoder` requires `application/json` or a concrete
  `application/...+json` media type and preserves JSON syntax and schema errors.

JSON media types are case-insensitive and may include parameters. An empty body
is not JSON: use `expectEmpty` for a 204 or HEAD response. Typed decoding rejects
duplicate object keys throughout a JSON tree, including unknown fields.

`decodeResponse config request response` runs the same buffered status, size,
media-type and decoder rules purely, making application contracts deterministic.

## Configuration and failures

`config baseUrl` returns a record that can be updated with ordinary record syntax.
Defaults are a 30,000 ms overall deadline, a 16 MiB buffered-body limit, an 8 KiB
error preview, empty default headers, and bundled TLS roots. `extraRoots` adds
trusted PEM roots for the lifetime of that pool. `timeoutMs = None` disables the
deadline. Timeouts must be positive when supplied; body limits may be zero.

The deadline covers the network operation through complete body consumption.
Cancellation is cooperative, and the scope joins its worker before returning.
Deadline expiration and caller cancellation have distinct error variants.

Errors preserve typed JSON encoding errors, JSON byte/field locations, response
metadata, and the original transport message. An `UnexpectedStatus` carries a
bounded byte preview and a truncation flag. `errorToString` provides a readable
summary while leaving these structured details accessible.

## Redirects and retries

Redirects are enabled with `maxRedirects = 10`; set `followRedirects = false` to
receive 3xx responses directly. A 303 changes non-HEAD methods to GET, 301/302
change POST to GET, and 307/308 preserve the method and encoded bytes. Relative
Location values resolve against the current URL. Missing Location returns the
response; malformed/ambiguous Location, HTTPS downgrade and hop exhaustion fail.

On a cross-origin hop, all configured default-header names and the names in
`sensitiveHeaders` are stripped. The latter defaults to Authorization, Cookie and
Proxy-Authorization. Defaults are not re-applied during redirects. Configure
additional sensitive names for request-specific credentials.

Retries are disabled by default. Set `retry = HttpClient.retryTransient` to allow
two additional GET/HEAD attempts on 429, 502, 503 and 504, with a 100 ms initial
delay and 2 s maximum exponential backoff. The retry record is configurable.
Retry-After delta-seconds and IMF-fixdate values are honored even when they exceed
the backoff cap; the overall deadline still applies. Exhaustion returns the last
response through the ordinary status/decoder rules. Transport errors and POSTs
are not automatically retried.

Redirects and retries have independent counts but share one deadline. A discarded
response is released before sleeping or opening the next exchange. Encoders run
once; replay uses immutable prepared bytes. The application receives only the
final response, whose head records its URL and total attempt count.

`HttpClientPolicy` exposes the pure decisions and Retry-After parser for scripted
tests. Its `decide` function takes a response status/headers, counters and explicit
wall-clock seconds, so these contracts perform no I/O or sleeps.

```sh
fai run -C packages http-client/examples/RetryRedirect.fai
# ok attempts=3 path=/done
```

## Scoped streaming

`withResponse client consume request` gives the callback a
`Response (Stream Bytes { Concurrency, Net, Tls })`. The callback returns a
`Result 'a Error`, and its own effects are forwarded. It runs once, after all
redirect/retry decisions. The overall deadline remains active until it returns.

```fai
HttpClient.get "exports/latest"
|> HttpClient.expectBytes
|> HttpClient.withResponse client (fun response ->
  Result.mapError HttpClient.ConsumerFailure
    (Stream.toFile runtime "export.bin" response.body))
```

Raw requests expose any final status; `expectBytes` requires 2xx before invoking
the callback. Streaming consumes the raw bytes rather than a buffered decoder.
The buffered-body size limit does not apply to a successful streamed response.
Status failures still carry the bounded error preview.

Consume a body once and only inside its callback. Full consumption permits
connection reuse after callback completion. Early return, cancellation, and an
unfinished body close the connection. The underlying response gate prevents a
retained body closure from reading a later exchange on a reused connection.
Callbacks are not retried, including when they return an error.

`decodeStream config request response` buffers a supplied stream with the normal
size/status/JSON rules and preserves body-read errors. A custom consumer can map
its own failure to `ConsumerFailure`, or retain response metadata using `BodyRead`.

Run the self-contained loopback examples and lifecycle checks:

```sh
fai run -C packages http-client/examples/TypedClient.fai
fai run -C packages http-client/examples/RetryRedirect.fai
fai run -C packages http-client/examples/LifecycleChecks.fai
fai run -C packages http-client/examples/DownloadCheck.fai -- /tmp/download-check.txt
fai run -C packages http-client/examples/Download.fai -- https://example.com/data output.bin
```

The lifecycle checks cover early return while the pool remains open, cancellation,
deadline expiry during a stalled body, exactly one callback after a retry, a stream
larger than the configured buffering limit, and consumer failures. They also check
deadline expiration inside callbacks and retry delays, and status rejection before
callback invocation. `DownloadCheck` streams a loopback response to the supplied
file and verifies its complete UTF-8 contents, covering FileSystem effect forwarding.

## Reading server-sent events

`HttpClient.withEvents client options consume request` reads one SSE connection,
including POST requests. `consume` receives a `Sse.Event` and returns
`Ok ContinueEvents`, `Ok StopEvents`, or a structured client error. Events are
delivered sequentially; a slow callback applies backpressure and does not count
as network-read idle time. Callback errors stop the operation.

```fai
HttpClient.post "chat"
|> HttpClient.jsonBody encoder input
|> HttpClient.withEvents client HttpClient.eventOptions onEvent
```

`EventOptions` (also `SseOptions.Events`) has configurable `connectTimeoutMs`,
`readIdleTimeoutMs`, `subscriptionTimeoutMs`, and codec `limits`. Defaults are
30,000 ms to establish the final response, 60,000 ms waiting for each body read,
and no overall duration limit. Every timeout accepts `None` to disable it; an
enabled value must be positive. These settings replace the ordinary client's
overall timeout for this operation, without changing that client.

The reader sets Accept, requires status 200 with text/event-stream, and accepts
204 as a successful `EventNoContent` stop. Its result reports `EventEof` or
`EventStopped` otherwise, with the last committed ID, latest retry hint and event
count. An incomplete event at EOF is discarded. Parsing stops at each protocol
observation, so stopping after one event cannot commit a later ID from the same
network chunk. Protocol limits, media/status errors, connect/read-idle deadlines,
caller cancellation and consumer errors remain distinguishable.

```sh
fai run -C packages http-client/examples/EventReaderChecks.fai
```

## Reconnecting SSE subscriptions

`subscribeEvents client options consume target` makes a bodyless GET subscription.
`subscribeEventsWith client options observe consume target` additionally reports
`EventOpening`, `EventConnected`, and `EventReconnecting` notices, including the
next connection number, chosen delay, committed state and any transport failure.
Notices and event callbacks run sequentially. Either can stop with an error;
an event callback can finish successfully with `StopEvents`.

`subscriptionOptions` defaults to reconnecting until stopped. EOF, connection
loss, connect timeout and read-idle timeout reconnect; 204 stops successfully.
Other statuses, wrong Content-Type, codec-limit failures and callback errors are
terminal. The subscription owns reconnection rather than stacking HTTP retries.
It never retries a POST. Use `withEvents` for a single POST response.

All timings are configurable:

| Option | Default |
|---|---|
| `events.connectTimeoutMs` | `Some 30000` |
| `events.readIdleTimeoutMs` | `Some 60000` |
| `events.subscriptionTimeoutMs` | `None` |
| `reconnectDelayMs` | `3000` |
| `minReconnectDelayMs` | `0` |
| `maxReconnectDelayMs` | `None` |
| `maxReconnects` | `None` |
| `initialLastEventId` | `""` |

Enabled timeouts must be positive. Reconnect delays/bounds/counts must be
nonnegative and ordered. A server's valid `retry:` hint replaces the initial
delay, subject to configured bounds. The optional overall deadline includes every
connection, callback and sleep; heartbeat bytes reset the read-idle wait.

The client commits ID-only blocks and sends Last-Event-ID when the current cursor
is nonempty. Empty IDs remove the header. Incomplete final blocks do not advance
it. Each connection gets fresh wire framing seeded with the committed ID, so a
heartbeat or an event without an ID retains that cursor. Only completed
observations update progress. Resume values that cannot be represented as HTTP header
values return an error rather than being altered. Redirected subscriptions resume
at the final URL, stripping origin-scoped headers and preventing an old origin's
cursor from being sent to a new origin.

```sh
fai run -C packages http-client/examples/SubscriptionChecks.fai
fai run -C packages http-client/examples/EventHeartbeatCheck.fai
fai run -C packages http-client/examples/EventRedirectCheck.fai
```

## Native contracts

```sh
fai fmt -C packages http-client
fai check -C packages http-client
fai test -C packages http-client
```

The package depends on `json`; its `ci.json` participates in dependency-aware
package checks. Repeated edits reuse the cached compiler and warm daemon.
