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
> self-imposed CPU limit; the daemon survives a runaway worker. Two simplifications
> versus the full spec below: the daemon currently serializes requests (no
> concurrent reads / cancellation yet) and returns each non-streaming command's
> **already-rendered** stdout/stderr rather than structured per-method results, so
> warm output is byte-identical to a one-shot run. Not yet implemented:
> `fai daemon tap`, `fai test` (contracts), Windows resource limits, and a Windows
> CI (the named-pipe transport compiles but is untested). See `AGENTS.md` for
> project conventions, `docs/PLAN.md` for milestones, and the `samples/` directory
> for the language itself.

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

The **workspace root** is the nearest ancestor directory containing a project
marker (reserved for v2's `fai.toml`) or, failing that, the current directory.
Sources are **all `.fai` files under the root**, excluding hidden and ignored
directories. (A `fai.toml` manifest with explicit roots/targets/dependencies is
deferred to v2.)

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

### `fai check [path]`
Typecheck only (front-end queries; no codegen/link). The fast inner-loop command.

- **Arguments:** `path` — a file or directory; default: the whole workspace.
- **Streaming:** emits diagnostics as they are found.
- **Output (json):** `{ "schemaVersion": 1, "diagnostics": [Diagnostic], "ok": bool }`
- **Exit:** `0` if no errors; `1` if any error diagnostic.

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
laws are checked with generated inputs and shrunk on failure.

- **Options:** `--match <pat>` (run only contracts whose symbol matches).
- **Streaming:** per-contract pass/fail events.
- **Output (json):** `{ "schemaVersion": 1, "total": int, "passed": int, "failed": [{ "symbol": SymbolRef, "contract": Contract, "counterexample": string? }] }`
- **Exit:** `0` if all pass; `1` otherwise.

### `fai fmt [path] [--check]`
Canonically format in place (idempotent).

- **Options:** `--check` (do not write; exit `1` if any file would change).
- **Output (json):** `{ "schemaVersion": 1, "changed": [string] }`
- **Exit:** `0`; with `--check`, `1` if any file is unformatted.

### `fai lsp`
Start the Language Server on stdio (standard LSP, JSON over `Content-Length`).
Editors speak this; agents use `fai query` instead.

---

## 6. Daemon commands

```
fai daemon status      # is a daemon running for this workspace? print pid, versions, uptime, memory
fai daemon start       # start (idempotent; no-op if already running)
fai daemon stop        # graceful shutdown
fai daemon restart     # stop + start (e.g. to pick up a new compiler version)
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
- **Debugging:** `--protocol-log <file>` / `fai daemon tap` decode frames to
  JSON; a dev-only `--protocol=json` switches the wire to plaintext JSON-RPC.

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
- **Concurrency:** reads run concurrently. An input change bumps the salsa
  revision and **cancels** in-flight reads, which restart on the new revision.
- **Cancellation:** `$/cancelRequest { id }`; a client disconnect cancels its
  outstanding requests.

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

### 7.7 Notifications (server → client)

| Notification | Payload | Used by |
|---|---|---|
| `$/progress` | `{ id, message, done?, total? }` | build/check/test progress |
| `$/diagnostic` | `{ id, diagnostic: Diagnostic }` | streamed diagnostics |
| `$/testEvent` | `{ id, symbol, contract, status, counterexample? }` | `test` |
| `$/output` | `{ id, stream: "stdout"\|"stderr", chunk: bytes }` | `run`/`test` worker output |
| `$/log` | `{ level, message }` | daemon logs |

A streaming command emits notifications keyed by the request `id`, then sends the
final `result`.

### 7.8 Execution model (`run` / `test`)

The daemon JIT-compiles the program (reusing cached function code), then spawns
an **isolated worker process** carrying the JIT image and the requested
capabilities. The worker's stdout/stderr stream back as `$/output`; stdin is
forwarded as needed (piped in v1; full PTY behavior is a later refinement). The
worker's exit code (or crash/timeout) is reported in the final `result`. The
daemon enforces timeouts and resource limits on the worker, so a runaway agent
program can never take down the daemon.

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
- The **diagnostic codes** `FAInnnn` (never renumbered; see `AGENTS.md` §10).
- The **daemon protocol** (`protocolVersion`) and **query method names**.

Within a major version, changes are additive and backward-compatible. Breaking
changes bump the relevant version number and are documented in the changelog.
