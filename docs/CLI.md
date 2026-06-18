# Fai — CLI Reference

> **Status:** `fai check`, `fai fmt`, and the `fai query` family are implemented,
> as are `fai build` and `fai run` for the native subset (`Int`/`Bool`/`String`,
> functions, `let`, `if`, arithmetic, and `Console.writeLine` via `main`):
> `fai build` produces a native executable. The per-workspace **daemon is
> implemented**: `check`, `query`, `fmt`, and `build` route through a warm
> `fai-server` (auto-spawned; `--no-daemon` and graceful in-process fallback both
> work), managed via `fai daemon status|start|stop|restart`, and backed by an
> on-disk content-addressed object cache. **`fai run` is daemon-supervised**: the
> warm front end ships a portable IR bundle to an isolated worker that JITs and
> runs it, with streamed output, a wall-clock timeout (exit `124`), and a
> self-imposed CPU limit; the daemon survives a runaway worker. Read commands are
> served **concurrently** (cloned database snapshots run off-lock) and an input
> change **cancels** in-flight reads, which retry on the new revision. One
> simplification versus the full spec below: the daemon returns each non-streaming
> command's **already-rendered** stdout/stderr rather than structured per-method
> results, so warm output is byte-identical to a one-shot run. **`fai test` is
> implemented**:
> it collects the `example`/`forall` contracts, synthesizes a property-testing
> harness per contract using the dogfooded `std/Test.fai` library, and checks each
> in a **supervised isolated worker** (the same machinery as `fai run`) — so a
> generated input that drives a body into a runtime trap (e.g. division by zero)
> fails *that contract* as a located **`FAI6003`** and the run continues (the
> supervisor records the abort and resumes after it). Failures are located
> `FAI6001` diagnostics with a shrunk counterexample (an ungeneratable binder is
> `FAI6002`); it takes `[path]`/`--match`/`--seed`/`--count`/`--max-size`, and
> generates values for built-in types, records, and (recursive) ADTs. The daemon
> serves `fai test`, streaming per-contract results as `$/testEvent`; warm output
> is byte-identical to `--no-daemon`. `fai daemon tap` streams a live JSON decode
> of the workspace daemon's traffic for debugging. The daemon and worker run on
> Linux, macOS, and Windows — each covered in CI, including the named-pipe
> transport and the daemon end-to-end suite — and the worker's resource limits are
> enforced by `setrlimit` on Unix and a Job Object on Windows. See `AGENTS.md` for
> project conventions, `docs/MEMORY.md` for the design decisions, the issue
> tracker for the roadmap, and the `samples/` directory for the language itself.

---

## 1. Overview & philosophy

The `fai` binary is a single executable that operates in three roles:

- **Client** — short-lived CLI invocations (`fai check`, `fai query refs`, …).
- **Daemon** — a long-lived per-workspace server holding the incremental
  (salsa) query database hot in memory. Auto-spawned on demand.
- **LSP server** — `fai lsp`, a standard Language Server over stdio for editors.

Design priorities, in order: **fast feedback for AI agents**, **machine-readable
output**, **determinism**, and a **read-only introspection surface** that agents
can drive without risk. The CLI and the LSP share one code-intelligence engine
(`fai-ide`), so their answers never diverge.

Key properties:

- The agent-facing **`fai query …`** family is **read-only**. It never mutates
  source. (Edits are the agent's job; `fmt`/`build`/`test` are separate.)
- **`query` commands default to JSON**; build/dev commands default to
  human-readable text. Both honor `--message-format`.
- All structured output is **versioned** (`schemaVersion`) and **stable** — it is
  an API. Diagnostics carry stable `FAInnnn` codes (see `AGENTS.md` §10).
- Results are **best-effort under errors**: a workspace that doesn't fully
  typecheck still answers queries with partial results.

---

## 2. Invocation & global behavior

```
fai <command> [subcommand] [arguments] [flags]
```

### Global flags

| Flag | Meaning |
|---|---|
| `--message-format <human\|json>` | Output format. Default: `human` for build/dev commands, `json` for `query`. |
| `--project <dir>` / `-C <dir>` | Workspace root. Default: nearest ancestor containing a project marker, else the current directory. |
| `--no-daemon` | Run the request in-process; do not spawn/connect to a daemon. |
| `--color <auto\|always\|never>` | Colorize human output. Default `auto`. |
| `--quiet` / `--verbose` | Decrease / increase log verbosity. |
| `--protocol-log <file>` | (debug) Append a JSON decode of all daemon traffic for this invocation. |
| `--version` / `--help` | Print version / usage and exit. |

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success (no errors; for `check`/`test`, nothing failed). |
| `1` | Completed, but the operation reported failures (type errors, failing contracts, `fmt --check` drift, etc.). |
| `2` | Usage error (bad arguments/flags). |
| `3` | Workspace/IO error (cannot read sources, bad project root). |
| `4` | Internal compiler error (please report; carries a code). |
| `124` | Timed out (e.g. a JIT'd program exceeded its limit). |
| `130` | Interrupted (SIGINT). |

### Workspace model (v1)

The **workspace root** is the current directory (or `--project <dir>`). Sources
are **all `.fai` files under the root**, excluding hidden and ignored
directories. (A `fai.toml` manifest with explicit roots/targets is still deferred
to v2; only the native-dependency section below is read today.)

### Native dependencies (`fai.toml`)

A program that calls user `foreign` functions declares the native libraries and
object files to link (AOT `build`) and load (JIT `run`) in an **optional
`fai.toml`** at the workspace root:

```toml
[native]
library-dirs = ["native"]    # added as `-L` search paths (and library search)
libraries    = ["mymath"]    # linked `-lmymath`; loaded as `libmymath.<ext>`
objects      = ["native/extra.o"]  # extra object/archive files (AOT only)
```

Relative paths resolve against the root. `fai build` threads `-L`/`-l` and the
object files into the system linker, producing a self-contained executable. `fai
run` JIT-loads the shared libraries named by `libraries` (found under
`library-dirs`), so `objects`-only dependencies work for `build` but not `run`
(put run-reachable foreign code in a shared library). A library that cannot be
loaded, or a malformed manifest, is a clear error. The file is absent for the
common, pure program.

### Daemon, briefly

Most commands connect to (or auto-spawn) a **per-workspace daemon** that keeps
the query database warm, so repeated commands are near-instant. The daemon is an
implementation of the same `fai` binary; `--no-daemon` forces a one-shot
in-process run. Full protocol in §7; lifecycle commands in §6.

---

## 3. Conventions

### Symbol addressing

Commands that take a `<symbol>` accept either form:

- **Name path** (preferred — stable across edits):
  `Module.Submodule.name`, e.g. `Collections.map`. Disambiguate colliding names
  with `--kind`.
- **Position**: `path/to/File.fai:LINE:COL` (1-based), resolving the symbol at
  that location.

### Positions & spans

Positions are **1-based** `line`/`column`. Every span also carries **byte
offsets** for exact machine use:

```json
{ "file": "src/Collections.fai",
  "start": { "line": 12, "column": 3 },
  "end":   { "line": 12, "column": 6 },
  "byteStart": 211, "byteEnd": 214 }
```

### JSON output

Every JSON document includes `"schemaVersion"` (integer). The schema is stable
and versioned; fields are only added in compatible ways within a major version.
Diagnostics follow the model in `AGENTS.md` §10 (stable `FAInnnn` codes).

---

## 4. Common JSON types

```jsonc
// Position — 1-based.
Position  = { "line": int, "column": int }

// Span — source range with byte offsets.
Span      = { "file": string, "start": Position, "end": Position,
              "byteStart": int, "byteEnd": int }

// Location — a span plus an optional one-line preview.
Location  = { "span": Span, "preview": string? }

// SymbolRef — a named, addressable definition.
SymbolRef = { "path": string,         // e.g. "Collections.map"
              "name": string,
              "kind": "function" | "type" | "interface" | "constructor"
                    | "field" | "module" | "value",
              "module": string,
              "visibility": "public" | "private",
              "signature": string?,    // for public bindings, the written sig
              "span": Span }

// TypeRepr — a rendered type (string form; structured form may be added later).
TypeRepr  = { "display": string }      // e.g. "('a -> 'b) -> List 'a -> List 'b"

// Doc / Contract — human prose and checked facts attached to a symbol.
Doc       = { "markdown": string }
Contract  = { "kind": "example" | "forall",
              "binders": [string],     // [] for example
              "source": string,        // e.g. "forall xs: map id xs = xs"
              "span": Span }

// Capability — an effect a function can reach.
Capability = { "name": string,         // e.g. "console"
               "type": string,         // e.g. "Console"
               "origin": "parameter" | "transitive",
               "via": [string] }       // call path for transitive caps

// Diagnostic — see AGENTS.md §10.
Diagnostic = { "code": string, "severity": "error"|"warning"|"info",
               "message": string, "primary": Span,
               "secondary": [{ "span": Span, "label": string }],
               "help": string?, "suggestions": [{ "span": Span, "replacement": string }] }
```

---

## 5. Build & dev commands

### `fai check [path] [--no-examples]`
Typecheck the selection, then evaluate its closed `example` contracts. The fast
inner-loop command: the type-check is front-end-only (no codegen/link), and once
it is clean each closed `example` is evaluated in an isolated worker (the same one
`fai test` uses), reporting a failing one as a located `FAI6001` — so a wrong
example is caught here, not only by `fai test`. `forall` laws and any example that
cannot be compiled are left to `fai test`, and aborts/timeouts are dropped (only
definite failures are reported). A file with no examples runs nothing extra; a
runaway example is bounded by `FAI_CHECK_TIMEOUT_MS` (default 10s).

- **Arguments:** `path` — a file or directory; default: the whole workspace.
- **Options:** `--no-examples` (type-check only; skip example evaluation).
- **Streaming:** emits diagnostics as they are found.
- **Output (json):** `{ "schemaVersion": 1, "diagnostics": [Diagnostic], "ok": bool }`
- **Exit:** `0` if no errors; `1` if any error diagnostic (including a failing example).

```
$ fai check
ok: 0 errors, 2 warnings
```

### `fai build <path> [--release] [--out <file>]`
Ahead-of-time compile to a native executable (Cranelift → object cache → link).

- **Options:** `--release` (optimize), `--out <file>` (output path).
- **Output (json):** `{ "schemaVersion": 1, "artifact": string, "diagnostics": [Diagnostic], "ok": bool }`
- **Exit:** `0` on success; `1` if compilation failed.

### `fai run <path> [-- <args>...]`
Build (via JIT) and run. Lowest edit→run latency: no linking; executed in an
isolated worker spawned by the daemon (capabilities provided by the host).

- **Arguments:** everything after `--` is passed to the program.
- **Streaming:** the program's stdout/stderr stream live; stdin is forwarded.
- **Exit:** the program's exit code (or `124` on timeout, `4` on compile error).

### `fai test [path] [--match <pat>]`
Run the `example` / `forall` contracts (JIT). Examples are evaluated; `forall`
laws are checked with generated inputs and shrunk on failure. Each contract runs
in a supervised isolated worker, so a body that traps on a generated input fails
*that* contract (a located `FAI6003`) without aborting the run.

- **Options:** `--match <pat>` (run only contracts whose subject/module matches),
  `--seed <n>`, `--count <n>` (trials per property), `--max-size <n>`.
- **Streaming:** per-contract pass/fail events (`$/testEvent`, from the daemon).
- **Output (json):** `{ "schemaVersion": 1, "total": int, "passed": int, "notRun": int, "seed": int, "events": [TestEvent], "diagnostics": [Diagnostic], "ok": bool }`, where a `TestEvent` is `{ "ordinal": int, "symbol": string?, "kind": "example"|"forall", "status": "passed"|"failed"|"crashed"|"timedOut"|"notRun", "counterexample": string?, "seed": int, "trials": int, "maxSize": int }`.
- **Exit:** `0` if all pass; `1` otherwise.

### `fai fmt [path] [--check]`
Canonically format in place (idempotent).

- **Options:** `--check` (do not write; exit `1` if any file would change).
- **Output (json):** `{ "schemaVersion": 1, "changed": [string] }`
- **Exit:** `0`; with `--check`, `1` if any file is unformatted.

### `fai lsp`
Start the Language Server on stdio (standard LSP, JSON over `Content-Length`).
Editors speak this; agents use `fai query` instead.

Supported requests: incremental `textDocument` sync (with `didSave`) and pushed
`publishDiagnostics` (re-published for every open file, so a cross-module edit
refreshes its dependents); `hover` (type, `///` doc prose, and attached
contracts), `definition`, `completion` (with `completionItem/resolve` filling the
chosen item's `///` docs and contracts lazily), `signatureHelp`, `documentSymbol`,
`workspace/symbol`, `references`, `prepareRename`/`rename`, `codeAction` (quick
fixes), `inlayHint` (inferred binder types), `semanticTokens` (full), and
document `formatting` (whole-document, range, and on-type — a newline reformats
the construct just completed). The position encoding is
negotiated at initialization (UTF-8 when the client offers it, else UTF-16). Open
buffers are analyzed as unsaved overlays, so every answer tracks the in-editor
text.

On **save** (not on every keystroke), the saved file's closed `example` contracts
are evaluated in an isolated worker and a failing one is published as `FAI6001`
alongside its type diagnostics; the results persist across edits to other files
and are cleared when the file itself is edited. Set the `examples` initialization
option to `false` to disable this (the type-check is unaffected).

---

## 6. Daemon commands

```
fai daemon status      # is a daemon running? print pid, versions, uptime, command latency, and peak read concurrency
fai daemon start       # start (idempotent; no-op if already running)
fai daemon stop        # graceful shutdown; returns once the daemon has exited
fai daemon restart     # stop + start (e.g. to pick up a new compiler version); returns once a fresh daemon is ready
fai daemon tap         # stream a JSON decode of this workspace's daemon traffic (debug)
```

The daemon auto-spawns on the first command that needs it; `--no-daemon` bypasses
it entirely (one-shot in-process). It shuts down after an idle timeout.

---

## 7. Daemon protocol (IPC / wire)

The client↔daemon link is **JSON-RPC 2.0 semantics encoded with MessagePack**.
(The LSP endpoint is unaffected — it remains standard LSP: JSON over
`Content-Length`.)

### 7.1 Transport & discovery

- **Transport:** unix-domain socket (POSIX); named pipe (Windows).
- **Path:** `${XDG_RUNTIME_DIR:-$TMPDIR}/fai/<workspace-hash>-<compilerVersion>.sock`.
  The compiler version is in the path, so different versions never collide.
- **Permissions:** `0600`, owner-only. Local only — no network.
- **Spawn race:** startup takes an exclusive lock / atomic bind; the loser
  connects to the winner.
- **Lifecycle:** idle-timeout shutdown; explicit `shutdown`/`exit`.

### 7.2 Framing & encoding

- Each message is a **length-prefixed frame**: `u32` little-endian byte length,
  followed by a **MessagePack**-encoded JSON-RPC 2.0 object.
- Message kinds: **request** (`id`, `method`, `params`), **response**
  (`id`, `result` | `error`), **notification** (`method`, `params`, no `id`).
- **Debugging:** `--protocol-log <file>` decodes one connection's frames to JSON;
  `fai daemon tap` subscribes to a live decode of the frames on *every other*
  connection (the daemon broadcasts each frame, best-effort, to attached taps),
  printing `#<conn> <arrow> <json>` per frame until interrupted. A dev-only
  `--protocol=json` switches the wire to plaintext JSON-RPC.

### 7.3 Handshake & versioning

The first request is `initialize`:

```jsonc
// → request
{ "method": "initialize",
  "params": { "protocolVersion": 1, "compilerVersion": "0.1.0",
              "schemaVersion": 1, "workspaceRoot": "/abs/path",
              "clientInfo": { "name": "fai-cli", "version": "0.1.0" } } }
// ← response
{ "result": { "serverCapabilities": { "streaming": true, "query": true },
              "compilerVersion": "0.1.0", "protocolVersion": 1, "schemaVersion": 1 } }
```

Because the client and daemon are the **same binary**, a version mismatch means a
*stale* daemon: the client sends `exit`, then respawns and re-initializes.

### 7.4 Session & consistency model

- **Stateless per invocation:** connect → (optionally declare dirty files) →
  issue request(s) → stream results → disconnect.
- **Concurrency:** reads run concurrently (each on a cloned database snapshot,
  off-lock). An input change bumps the salsa revision and **cancels** in-flight
  reads, which restart on the new revision. *(Implemented.)*
- **Cancellation:** input-change cancellation is implemented (above).
  Client-initiated `$/cancelRequest { id }` and cancelling a client's outstanding
  requests on disconnect are **not yet implemented** (future work).

### 7.5 File-state sync

The daemon keeps salsa inputs in sync with disk:

- On each request it performs an **incremental scan** — compares mtime/size and
  re-hashes only changed files — then updates inputs for what actually changed.
- A client that knows what it edited may send an explicit **dirty set** as a
  fast path (skips the scan):

```jsonc
"params": { "...": "...",
            "dirty": [ { "path": "src/A.fai", "hash": "blake3:…" },
                       { "path": "src/B.fai", "content": "module B\n…" } ] }
```

Hypothetical **overlays** (evaluate proposed content without writing it) are
deferred; the request shape reserves room for them.

### 7.6 Methods

| CLI command | RPC method |
|---|---|
| (handshake) | `initialize`, `shutdown`, `exit` |
| `fai check` | `check` |
| `fai build` | `build` |
| `fai run` | `run` |
| `fai test` | `test` |
| `fai fmt` | `fmt` |
| `fai query <q>` | `query/<q>` (`symbols`, `def`, `refs`, `type`, `docs`, `outline`, `api`, `dependents`, `callers`, `callees`, `search`, `caps`) |
| `fai daemon tap` | `tap` |

### 7.7 Notifications (server → client)

| Notification | Payload | Used by |
|---|---|---|
| `$/progress` | `{ id, message, done?, total? }` | build/check/test progress |
| `$/diagnostic` | `{ id, diagnostic: Diagnostic }` | streamed diagnostics |
| `$/testEvent` | `TestEvent` (per the `fai test` schema: `ordinal`, `symbol?`, `kind`, `status`, `counterexample?`, `seed`, `trials`, `maxSize`) | `test` |
| `$/output` | `{ id, stream: "stdout"\|"stderr", chunk: bytes }` | `run` worker output |
| `$/log` | `{ level, message }` | daemon logs |

A streaming command emits notifications keyed by the request `id`, then sends the
final `result`.

A `tap` request is acknowledged with an `Ok` result and then turns the connection
into a passive subscriber: the daemon streams a `tapFrame` (`{ conn, direction,
json }`) for every frame read or written on any *other* connection — a JSON
decode of live traffic — until the subscriber disconnects. Delivery is
best-effort (a subscriber that falls behind drops frames rather than throttling
the connection producing them).

### 7.8 Execution model (`run` / `test`)

The daemon builds a portable IR bundle warm (reusing cached function code), then
ships it to an **isolated worker process** that JIT-compiles and runs it. For
`run`, the worker carries the requested capabilities; its stdout/stderr stream
back as `$/output` (stdin is forwarded as needed — piped in v1; full PTY behavior
is a later refinement) and its exit code (or crash/timeout) is the final
`result`. For `test`, the worker checks each contract and streams a per-contract
`$/testEvent`; if a contract aborts (a runtime trap) or exceeds the time limit,
the supervisor records *that* contract as aborted and re-spawns a worker to resume
after it, so one bad contract never aborts the run — the final `result` is the
rendered report. Either way the daemon enforces timeouts (`FAI_RUN_TIMEOUT_MS` /
`FAI_TEST_TIMEOUT_MS`) and a self-imposed CPU limit on each worker, so a runaway
program can never take down the daemon.

A program that uses concurrency or networking runs on the runtime's M:N scheduler.
Its **worker-thread count** defaults to the host's available parallelism — which
on Linux respects the process's cgroup CPU quota and scheduler affinity, so a
containerized or CPU-pinned run scales to its budget automatically — and can be
overridden with the **`FAI_WORKERS`** environment variable (a positive integer;
`FAI_WORKERS=1` forces fully sequential multiplexing). Blocking host calls (file
I/O, DNS) run on a separate pool sized up to `FAI_BLOCKING_THREADS` (default 512).

### 7.9 Errors & security

- Failures use JSON-RPC `error` objects; compiler-specific failures carry
  `error.data.code = "FAInnnn"`.
- The socket is local-only with `0600` permissions; the protocol never opens a
  network port.

---

## 8. Code intelligence — `fai query`

`fai query <subcommand>` is the **read-only** code-intelligence surface for
agents. Shared behavior:

- **Output is JSON by default** (`--human` for a readable rendering).
- **Addressing:** name path (`Module.name`) or `file:line:col` (§3).
- **Best-effort:** partial results are returned even when the workspace has
  errors.
- **Bounding:** list-producing commands accept `--limit <n>` and report
  `"truncated": true` when results were capped (result cursors may come later).
- Served from the warm daemon, so answers are typically sub-millisecond.

### `fai query symbols`
List/search symbols.

- **Options:** `--module <M>`, `--kind <k>`, `--match <pat>`, `--limit <n>`.
- **Output:** `{ "schemaVersion": 1, "symbols": [SymbolRef], "truncated": bool }`

```
$ fai query symbols --module Collections --kind function
{ "schemaVersion": 1, "symbols": [ { "path": "Collections.map", "kind": "function",
  "signature": "('a -> 'b) -> List 'a -> List 'b", "...": "..." } ], "truncated": false }
```

### `fai query def <target>`
Resolve to definition site(s).

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "definitions": [Location] }`

### `fai query refs <target>`
Find all use sites.

- **Options:** `--include-definition`, `--kind <k>`, `--limit <n>`.
- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "references": [Location], "truncated": bool }`

```
$ fai query refs Collections.map
{ "schemaVersion": 1, "target": { "path": "Collections.map", "...": "..." },
  "references": [ { "span": { "...": "..." }, "preview": "  xs |> map inc" } ],
  "truncated": false }
```

### `fai query type <target>`
The inferred/declared type at a symbol or position.

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef?, "type": TypeRepr }`

### `fai query docs <target>`
Docs and attached contracts.

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "doc": Doc?, "contracts": [Contract] }`

### `fai query outline <File|Module>`
Nested symbol outline with signatures.

- **Output:** `{ "schemaVersion": 1, "outline": [OutlineNode] }`
  where `OutlineNode = { "symbol": SymbolRef, "children": [OutlineNode] }`.

### `fai query api <Module>`
The module's **public interface**: every exported binding with its signature,
docs, and contracts. The agent-friendly way to understand a module from its
boundary alone.

- **Output:** `{ "schemaVersion": 1, "module": string,
  "exports": [ { "symbol": SymbolRef, "doc": Doc?, "contracts": [Contract] } ] }`

### `fai query dependents <target>`
Reverse dependencies (blast radius) of a symbol or module — what a change would
affect. Computed from the same dependency graph the incremental engine maintains.

- **Options:** `--transitive`, `--limit <n>`.
- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "dependents": [SymbolRef], "transitive": bool, "truncated": bool }`

### `fai query callers <symbol>` / `fai query callees <symbol>`
Inbound / outbound call edges (call hierarchy).

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "edges": [ { "symbol": SymbolRef, "sites": [Location] } ] }`

### `fai query search <type-pattern>`
Hoogle-style search: find functions whose type matches a pattern (unification up
to row polymorphism), e.g. `List 'a -> Int`.

- **Options:** `--limit <n>`.
- **Output:** `{ "schemaVersion": 1, "query": string, "results": [ { "symbol": SymbolRef, "type": TypeRepr, "score": number } ], "truncated": bool }`

### `fai query caps <symbol>`
The capability/effect footprint of a function: which capabilities it (transitively)
requires. Because effects are explicit capability values, direct capabilities are
read from the signature; transitive ones are aggregated over the call graph.

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "capabilities": [Capability] }`

```
$ fai query caps App.greetUser
{ "schemaVersion": 1, "target": { "path": "App.greetUser", "...": "..." },
  "capabilities": [ { "name": "console", "type": "Console", "origin": "parameter", "via": [] } ] }
```

---

## 9. Recipes (agent workflows)

- **Understand a module before editing:** `fai query api <Module>` (boundary),
  then `fai query outline <File>` (structure).
- **Plan a safe change:** `fai query dependents <symbol> --transitive` to size
  the blast radius, edit, then `fai check` (incremental — only the affected
  defs recompute).
- **Discover an API by shape:** `fai query search "List 'a -> Option 'a"`.
- **Audit effects:** `fai query caps <entrypoint>` to see exactly which
  capabilities a code path can reach.
- **Navigate:** `fai query def <symbol>` / `fai query refs <symbol>`.
- **Tight TDD loop:** edit → `fai test --match <Module>` (JIT, streamed
  per-contract results).

---

## 10. Stability & versioning

These are treated as **public, versioned APIs**:

- The **JSON output schemas** (`schemaVersion`).
- The **diagnostic codes** `FAInnnn` (never renumbered; every code is documented
  in the catalog **`ERROR_CODES.md`**).
- The **daemon protocol** (`protocolVersion`) and **query method names**.

Within a major version, changes are additive and backward-compatible. Breaking
changes bump the relevant version number and are documented in the changelog.
