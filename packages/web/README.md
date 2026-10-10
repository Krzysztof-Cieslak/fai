# web — a micro web framework for Fai

A small, [Giraffe](https://github.com/giraffe-fsharp/Giraffe)-inspired web
framework built on Fai's networking stack (`std/networking/Http.fai`), with
[TokenRouter](https://github.com/giraffe-fsharp/Giraffe.TokenRouter)-style
routing. It is an ordinary Fai library — not part of the embedded standard
library — and is intended to move to its own repository once Fai grows a
packaging story.

```
packages/web/
  src/Web.fai        # the handler core: combinators, responses, request access, serve
  src/Router.fai     # the route tree: route / subRoute / verb groupers / router
  examples/Main.fai  # a runnable server
  examples/JsonWebExample.fai  # typed JSON request/response server
  test/WebSpec.fai   # behavioural contracts
  test/WebJsonSpec.fai  # JSON request/response contracts
```

## Using it today

This library depends on the sibling `packages/json` and `packages/sse` source
libraries. There is no package manager yet, so a consuming app and its libraries must live
under one workspace root (every `.fai` file beneath the root is compiled, and
modules find each other by their `module` header — there are no imports). Point
`fai` at that root:

```sh
fai run -C packages web/examples/Main.fai
fai run -C packages web/examples/JsonWebExample.fai
```

## The handler model

The single building block is an `HttpHandler` — a function from a request context
to an `Outcome`:

```fai
type Outcome 'e =
  | Continue (HttpContext 'e)   // proceed to the next handler in a chain
  | Halt (HttpContext 'e)       // finish: send the accumulated response
  | Produce (HttpContext 'e) (Http.BodyProducer 'e) // scoped response production
  | Skip                        // decline: let an alternative (or the router) try
  | Fail String                 // error: becomes a 500

type HttpContext 'e = { params : List (String * String), request : Http.Request 'e, response : Http.Response 'e }
type HttpHandler 'e = HttpContext 'e -> Outcome 'e / 'e
```

These outcomes make continuation, response production, alternatives, and failure
explicit. Composition is a plain value transformation. The effect variable `'e`
forwards whatever capabilities a handler uses.

### Combinators

- `compose a b` — run `a`; if it asks to `Continue`, run `b` on the updated
  context.
- `chain handlers` — run handlers left to right while each asks to `Continue`
  (middleware pipeline). Stops at the first `Halt`/`Produce`/`Skip`/`Fail`.
- `choose handlers` — try handlers until one does not `Skip`.

Cross-module symbolic operators are not available in Fai, so the API is
list/function-based rather than `>=>`-based. If you want the fish operator inside
your own module, alias it locally: `let (>=>) = Web.compose`.

### Responses

`text`, `html`, `bytes`, `respond status body`, `created`, `noContent`,
`badRequest`, `unauthorized`, `forbidden`, `notFound`, `serverError`, `redirect`
all finish the response and `Halt`, preserving unrelated headers accumulated by
middleware. The responder supplies its status and content type (or removes the
type for an empty response); an old `Content-Length` is cleared so HTTP framing
can compute the new body's length. Redirects replace `Location`.

`setStatus`, `setHeader`, and `addHeader` modify the accumulated response and
`Continue`. `setHeader` replaces all same-name fields case-insensitively;
`addHeader` appends, so repeated fields such as `Set-Cookie` remain distinct:

```fai
Web.chain [
  Web.setHeader "X-Request-Id" "abc",
  Web.addHeader "Set-Cookie" "a=1",
  Web.addHeader "Set-Cookie" "b=2",
  Web.text "ok"
]
```

### JSON

`json value` responds 200 with a `Json.Value`; `jsonWith encoder value` uses a
`JsonEncode.Encoder` for an application type. `respondJson status value` and
`respondJsonWith status encoder value` accept an explicit status. They set
`Content-Type: application/json`, preserve middleware headers, and remove stale
Content-Length. Encoding failures become `Fail` before producing a response.

```fai
type User = { id : Int, name : String }

let userDecoder = JsonDecode.map2
  (fun id name -> { id = id, name = name })
  (JsonDecode.field "id" JsonDecode.int)
  (JsonDecode.field "name" JsonDecode.string)

encodeUser : JsonEncode.Encoder User
let encodeUser user = JsonEncode.object [
  ("id", JsonEncode.int user.id),
  ("name", JsonEncode.string user.name)
]

let createUser = Web.bindJson userDecoder (Web.respondJsonWith 201 encodeUser)
```

`readJson decoder context` drains the body once and returns
`Result 'a Web.JsonBodyError`, forwarding the body's effect row. It accepts
`application/json` and concrete `application/...+json` media types,
case-insensitively, with parameters; bytes must be UTF-8. Missing/unsupported
Content-Type is rejected before reading the body.

`bindJson decoder handler` reads and decodes once, then invokes `handler value`
with the context. Invalid JSON, duplicate keys, or schema errors become **400**;
unsupported media types become **415**. Body transport failures become `Fail`.
The body and handler effects propagate through the resulting `HttpHandler`.

For custom error handling, use `readJson` directly. `JsonBodyError` distinguishes
`UnsupportedJsonMediaType (Option String)`, `JsonBodyReadError String`, and
`InvalidJson JsonDecode.ReadError`. `jsonBodyErrorToString` renders them while
their structured byte locations and field/index paths remain available.

### Server-sent events

The shared `sse` package provides validated frames and the wire codec.
`Web.sse frames` streams a `Stream Sse.Frame`, preserving its effects and the
middleware's unrelated headers. It sets `Content-Type: text/event-stream`, adds
`Cache-Control: no-cache` when none was supplied, and uses chunked framing.
Stale Content-Length and Content-Encoding fields are removed.

```fai
let events = Stream.fromList [Sse.comment "ready", Sse.event "hello"]
let handler = Web.sse events
```

`Web.sseWith runtime options frames` adds optional automatic idle heartbeats.
`SseServer.defaults` disables them; `{ heartbeatIntervalMs = Some 15000 }` enables
a configurable 15-second interval. Nonpositive enabled intervals fail validation
before a producer starts. No producer/timer runs while merely building a handler.

The managed HTTP send path writes headers before starting production. One source
task and one timer feed a bounded, acknowledged queue; timer ticks never cancel a
pending source read. All wire writes are serialized. A completed source, write
failure, or cancellation stops and joins both tasks. HEAD and bodyless responses
never start them. `Web.lastEventId context` reads the incoming Last-Event-ID header;
the application decides how to replay its event history.

`Produce` is a terminal handler outcome like `Halt`. `Web.outcomeResponse` exposes
its response metadata for pure tests; its body is produced only during serving.
Run the self-contained heartbeat example with:

```sh
fai run -C packages web/examples/SseExample.fai
```

### Reading the request

`path`, `method`, `param "id"` (a captured route parameter), `intParam "id"`,
`query "q"` (a query-string value), and `header "Accept"`.

## Routing

```fai
let app =
  Router.router (Web.notFound "Not Found") [
    Router.get [
      Router.route "/" (Web.text "index"),
      Router.route "/user/{id}" showUser
    ],
    Router.post [Router.route "/submit" handleSubmit],
    Router.subRoute "/api" [
      Router.get [Router.route "/ping" (Web.text "pong")]
    ]
  ]
```

- `route pattern handler` — a `{name}` segment captures that path segment, read
  back with `Web.param "name"`.
  Names belong to that route: another route may share the capture position using
  a different name, including routes distinguished only by HTTP method.
- `subRoute prefix children` — share a path prefix.
- `get`/`post`/`put`/`delete`/`patch`/`head`/`options` — restrict child routes to
  a method (lowercase, since `GET`/`POST` are `Http.Method` constructors).
- `router fallback routes` — compile to a handler; `fallback` runs when no route
  matches the path, no registered method matches, or the matched handler skips.
  Each candidate receives its own parameter names. A skipped candidate's captures
  do not leak to another candidate or to the fallback, which receives the original context.

Matching walks the path one segment at a time (static segments first, then a
capture), so lookup cost is proportional to the path length.
A static prefix without a terminal route is a path miss and allows a capture
alternative: `/user/new/edit` does not hide `/user/{id}` for `/user/new`.
A real static endpoint wins path selection; if its method gates or handlers all
`Skip`, the configured fallback runs rather than a different capture pattern.

## Capabilities (the dependency-injection replacement)

Handlers reach capabilities — a clock, a logger, a database connection — by
ordinary closure capture. Build the routing table inside a function that has the
runtime (or a narrower capability record) in scope, and the effect row of the
resulting handler records exactly what it uses:

```fai
let app runtime =
  Router.router (Web.notFound "Not Found") [
    Router.get [Router.route "/now" (fun ctx -> Web.text (Int.toString (runtime.clock.now ())) ctx)]
  ]
// app : Runtime -> Web.HttpHandler { Clock }
```

## Serving

```fai
public main : Runtime -> Unit / { Concurrency, Net, Tls }
let main runtime =
  match Web.serve runtime 8080 app with
  | Err e -> runtime.console.writeLine ("server error: " ++ e)
  | Ok u -> u
```

`serve env port app`, `serveTls env port cert key app`, and
`serveListener env listener app` wrap the corresponding `Http` server functions,
turning an `HttpHandler` into the request→response function the server expects (a
`Skip` becomes a 404; a `Fail` becomes a 500).

## Testing your handlers

`Web.mockContext method target` builds a request context without opening a
socket, and `Web.outcomeResponse` reads back the response an outcome produced, so
handlers are testable with ordinary `example` contracts. See `test/WebSpec.fai`.

The package carries ordinary Fai contracts, runnable directly with an existing compiler:

```sh
fai test -C packages web
fai test -C packages web/test/WebJsonSpec.fai
```

Repeated calls reuse the warm daemon. Library edits do not require rebuilding
the compiler. Keep `web` and `json` beside each other when moving them to another
source workspace. `ci.json` declares the dependency for repository CI selection;
the Fai test runner does not require it.

The effectful `.fai` programs in `test/` also serve as compiler execution fixtures.
Compiler CI runs their transport, header, and shutdown checks through JIT and AOT,
including one-, two-, and four-worker scheduler pools.

## Status

Implemented: the handler core, the router, response/request helpers, and the
serve adapters (HTTP and HTTPS), and typed JSON requests/responses. Not yet built: typed path-segment
combinators, and `chunked`/streaming response helpers beyond what `Http`
provides directly.
