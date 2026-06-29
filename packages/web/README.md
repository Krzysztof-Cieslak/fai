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
  test/WebSpec.fai   # behavioural contracts
```

## Using it today

There is no package manager yet, so a consuming app and this library must live
under one workspace root (every `.fai` file beneath the root is compiled, and
modules find each other by their `module` header — there are no imports). Point
`fai` at that root:

```sh
fai run -C packages/web examples/Main.fai
```

## The handler model

The single building block is an `HttpHandler` — a function from a request context
to an `Outcome`:

```fai
type Outcome 'e =
  | Continue (HttpContext 'e)   // proceed to the next handler in a chain
  | Halt (HttpContext 'e)       // finish: send the accumulated response
  | Skip                        // decline: let an alternative (or the router) try
  | Fail String                 // error: becomes a 500

type HttpContext 'e = { params : List (String * String), request : Http.Request 'e, response : Http.Response 'e }
type HttpHandler 'e = HttpContext 'e -> Outcome 'e / 'e
```

These three outcomes are Giraffe's "Continue / Return / Skip" made explicit
(Fai is direct-style with typed effects, so there is no `Task` and no
continuation-passing — composition is a plain value transformation). The effect
variable `'e` forwards whatever capabilities a handler uses.

### Combinators

- `compose a b` — run `a`; if it asks to `Continue`, run `b` on the updated
  context.
- `chain handlers` — run handlers left to right while each asks to `Continue`
  (middleware pipeline). Stops at the first `Halt`/`Skip`/`Fail`.
- `choose handlers` — try handlers until one does not `Skip`.

Cross-module symbolic operators are not available in Fai, so the API is
list/function-based rather than `>=>`-based. If you want the fish operator inside
your own module, alias it locally: `let (>=>) = Web.compose`.

### Responses

`text`, `html`, `bytes`, `respond status body`, `created`, `noContent`,
`badRequest`, `unauthorized`, `forbidden`, `notFound`, `serverError`, `redirect`
all build a response and `Halt`. `setStatus` and `setHeader` modify the
accumulated response and `Continue`.

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
- `subRoute prefix children` — share a path prefix.
- `get`/`post`/`put`/`delete`/`patch`/`head`/`options` — restrict child routes to
  a method (lowercase, since `GET`/`POST` are `Http.Method` constructors).
- `router fallback routes` — compile to a handler; `fallback` runs when no route
  matches the path, no registered method matches, or the matched handler skips.

Matching walks the path one segment at a time (static segments first, then a
capture), so lookup cost is proportional to the path length.

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

## Status

Implemented: the handler core, the router, response/request helpers, and the
serve adapters (HTTP and HTTPS). Not yet built: JSON support (a `Json` module is
the natural next addition, wiring up a `Web.json` responder), typed path-segment
combinators, and `chunked`/streaming response helpers beyond what `Http`
provides directly.
