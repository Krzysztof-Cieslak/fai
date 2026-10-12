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
> harness per contract using the dogfooded `std/testing/Test.fai` library, and checks each
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
| `--package <name-or-path>` / `-p` | Select one member of a versioned workspace. |
| `--no-local` | Ignore `fai.local.toml` and select the shared project context. |
| `--locked` | Require a valid selected graph; exact source restoration is permitted. |
| `--offline` | Use cached metadata and content without Git network access. |
| `--frozen` | Combine `--locked` and `--offline`. |
| `--target <triple>` | Metadata target selection, repeatable; compilation currently supports the host only. |
| `--no-vendor` | Explicitly bypass configured vendor sources and use the normal source provider. |
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

Versioned `fai.toml` manifests load package-owned library, executable, and test
sources through the same CLI, daemon, and language-server pipeline. The complete
package-system design and implementation status are in **`docs/PACKAGES.md`**.

Package-aware compiler environments support file-local `import dependency.Module`
and `public import` re-exports. Their aliases are supplied by the project loader;
the package loader's current status and intended commands are in `docs/PACKAGES.md`.
An API query additionally returns `modules` when public module re-exports exist:
each entry has the alias `name`, original module `target`, and original source `span`.

`fai build-info` prints JSON without opening a workspace: `schemaVersion`,
`version`, `sourceId`, `toolBuildId`, `backendBuildId`, `target`, `profile`,
`debugAssertions`, `rustcVersion`, and `nativeLibraries`. The whole-tool identity
includes CLI, formatter, IDE and daemon sources; the native object cache retains
its separate backend identity. Native dependency archives needed beyond system
libraries are embedded and extracted alongside the runtime when linking, so a
copied compiler needs the platform linker/SDK but not its original Cargo home.

At a package directory, `check` and `test` select that package's targets. At a
workspace directory they select its members; `--package` narrows the selection.
`src/Main.fai` supplies the conventional executable, other shared files belong
to the library, and `tests/` supplies additional test-only sources. `[[bin]]`
declares additional entry files; `--bin <name>` or `package.default-run` selects
one for `build`/`run`. Two entry files may both declare `module Main`.

Path and Git dependencies, including explicit root replacements, are loaded recursively.
Consumed packages contribute their library and normal dependencies. Development
aliases are available only to additional tests; production source keeps its
normal environment during checks. Dependency code is validated, while its
contracts execute only when selected. Active platform sections select host
sources, dependencies, and native settings.

Root `fai.local.toml` overrides activate automatically and take precedence over
shared overrides. Commands report the local context on stderr; `--no-local`
selects shared configuration. The daemon retains separate warm databases for
these contexts. Ordinary commands retain valid graph selections, conservatively
repair stale unlocked graphs, and restore exact Git source bytes. A lock incompatible
with the selected compiler fails instead of silently choosing older dependencies.
Exact root compiler pins are selected before daemon startup from the registered
installed-compiler inventory. A matching invoked compiler is retained.

Without `-C`, commands discover the selected file's owning manifest or the
nearest manifest above the current directory. A source file outside any project
loads only that file and std; semantic commands require a file selection there.
Explicit `-C` directory sessions and native-only manifests retain recursive
discovery during migration.

`check`, `fmt`, and `test` accept workspace-relative or absolute file/directory
selections, including `./` and normalized `..` components inside the workspace.
An explicit missing or outside-workspace selection reports `FAI0002` and exits
with code `3` (the common hard-error JSON envelope in JSON mode). Existing empty
directories and an empty workspace selected without a path remain valid.

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

On Windows/MSVC, the driver passes linker inputs through a quoted UTF-16
response file, so large programs are not limited by the process command-line
length. The response file and generated objects share the temporary build
directory and are removed together after linking.

Compiler-generated native symbols use the versioned `fai2_` encoding and are an
internal ABI; rebuild generated objects when updating the compiler. This replaces
the ambiguous `fai_<module>_<member>` encoding. Explicit symbol strings in
`foreign` declarations are passed through as written.

Compiler inputs with package ownership include their stable package and target
scope in generated symbol identity. Identically named modules in separate package
instances or executable scopes therefore remain distinct in objects and worker
bundles.

Foreign functions can be passed to higher-order helpers or partially applied;
their expanded declared signatures determine marshalling on each call path.
String arguments stay alive until any returned string has been copied, so a
native result may refer to an input buffer. Other returned buffers remain owned
by the native library.

### Daemon, briefly

Most commands connect to (or auto-spawn) a **per-workspace daemon** that keeps
the query database warm, so repeated commands are near-instant. The daemon is an
implementation of the same `fai` binary; `--no-daemon` forces a one-shot
in-process run. Full protocol in §7; lifecycle commands in §6.

---

## Package dependency commands

```sh
fai init hello                 # application with src/Main.fai
fai init helpers --lib         # library package
fai add github:owner/library --as library --version '^1.2.0'
fai add --path ../helpers --dev
fai remove library
fai resolve                    # conservative lock repair, declared target matrix
fai fetch                      # prepare and materialize exact sources
fai update library             # prioritize this dependency, retain other pins when feasible
fai update                     # full update within authored requirements
fai deps                       # read-only recorded graph inspection
fai deps why library
fai --no-local vendor           # snapshot shared locked Git sources
fai --no-local --frozen check   # use the snapshot without cache or network
```

`add` also accepts `--subdir`, `--branch`, and a full `--rev`. With no Git version
intent it solves conservatively first, then records `^<selected-version>`. Bare
full versions supplied explicitly are exact. Default aliases come from the selected
package name with hyphens changed to underscores. `--dev` selects the development
table for add/remove. Failed edits preserve the existing manifest and lock.

Add/remove prepare the host graph by default. Explicit `--target` arguments replace
that selection. Resolve/fetch/update use `lock-targets`, or the host when absent.
All requested graph results are prepared before the active lock is replaced;
unrelated graphs retain their selections. Local-context edits modify the shared
manifest and `fai.local.lock`, report affected shared graphs stale, and retain
the shared lock bytes.

Package commands accept `--message-format=json`. Results carry `schemaVersion`,
`command`, `ok`, `context`, `lockfile`, `roots`, `targets`, `changes`, `staleGraphs`,
`diagnostics`, and `notices`. Changes identify the logical package, before/after
version and source, reason, and affected `root@target` graphs. `deps` adds the
selected `graph` records and optional `why` edges. It never refreshes refs or
restores source content.

### Vendored snapshots

`fai vendor [--dir <directory>]` prepares the selected lock matrix, copies verified
Git package trees into `vendor/sources/<checksum>/`, and records exact source
descriptors and graph receipts in `vendor/fai-vendor.toml`. The root manifest gains
`[vendor] directory = "vendor"`. Index, configuration, and any changed lock are
published through the metadata journal. Existing unrelated files are retained.

When configured, required Git sources must have matching, unmodified vendor
entries; missing or altered sources fail with `FAI0820`. There is no automatic
network fallback. `--no-vendor` explicitly bypasses the snapshot, and running
`vendor` again repairs tool-owned source trees. Mutable paths outside the active
project cannot be captured as a self-contained vendor snapshot.

### Publishing

```sh
fai publish patch --plan
fai publish patch
fai publish json=minor web=patch
fai publish --resume
```

Version intent is required: `patch`, `minor`, `major`, or an exact full version.
Prerelease transitions use exact versions. A batch names selected packages and
publishes their canonical path-scoped tags in one atomic push. `--remote` and
`--branch` resolve an otherwise ambiguous destination.

Publishing starts from clean committed code. It prepares version-only edits in
a candidate Git tree, then independently validates each package as a normal
consumer dependency and as a development root. Candidate repository refs let
batch members use planned compatible releases while retaining every authored
dependency range. Local overrides, vendoring, and existing developer locks do
not determine consumer validity. Host code and contracts run locally; other
declared targets receive metadata validation only.

The API report compares public declarations and reports additions, removals,
and changes. It is advisory and does not choose or veto a version bump. A
comparison involving unexpanded re-exports or an unavailable baseline is marked
partial. `--plan` performs the same candidate validation without changing source
files, branches, or tags; Git object and build caches may be populated.

Publication journals its exact candidate tree, commit, version edits, and tags
before changing the checkout. Git refs and the index use checked replacements;
the branch and tags are pushed with `--atomic`. Failed pushes retain the prepared
identity for `--resume`; release tags are never force-moved. Existing development
locks remain development records and are not rewritten with self-referential
future commit IDs. Prepare affected locks after the release.

JSON results include `status`, `commit`, `tree`, `remote`, `branch`, `packages`,
`apiChanges`, `apiComplete`, `validatedHost`, `metadataTargets`, and `notices`,
alongside the versioned success envelope.

### Installed compilers

```sh
fai toolchain register /path/to/fai
fai toolchain list --message-format=json
```

Registration probes `build-info` and records the release, host target, absolute
executable path, and full build identity. One registration exists per release/host;
re-registration explicitly replaces it. The inventory is under
`FAI_TOOLCHAIN_DIR`, `$FAI_HOME/toolchains`, or `~/.fai/toolchains`.

An exact `[toolchain] version` pin dispatches to a verified registered executable
before opening the project or daemon. Missing or changed registrations report
`FAI0809`. Toolchains are never downloaded automatically.

## 3. Conventions

### Symbol addressing

Commands that take a `<symbol>` accept either form:

- **Name path** (preferred — stable across edits):
  `Module.Submodule.name`, e.g. `Collections.map`. Disambiguate colliding names
  with `--kind`.
- **Position**: `path/to/File.fai:LINE:COL` (1-based), resolving the symbol at
  that location.

### Positions & spans

Positions are **1-based** `line`/`column`; columns count Unicode scalar values,
so both `é` and `😀` occupy one column. CLI position targets use the same mapping
as reported diagnostics. Zero coordinates, missing lines, and columns beyond a
line's end are rejected rather than clamped. The end-of-line/EOF coordinate is
valid, though it may not belong to a definition. LF starts a new line; a preceding
CR counts as one scalar, matching the diagnostic mapping.

Every span also carries **byte offsets** for exact machine use:

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

### `fai build [path] [--bin <name>] [--release] [--out <file>]`
Ahead-of-time compile to a native executable (Cranelift → object cache → link).

Without an entry selector, versioned projects validate and warm library artifacts
and build every selected executable. Default outputs live under
`.fai/build/<target>/<profile>/<package-instance>/`; `--out` requires one executable.
Batch JSON adds an `artifacts` array, with `artifact` retained for a single output.

Build and run validate the runtime-to-`main` application (`FAI0007` for an invalid
shape): the selected runtime builder takes no arguments, its value matches
`main`'s argument, and `main` returns `Unit`. A row-polymorphic `main` receives
generated field-offset evidence. Runtime initialization runs inside the scheduler
when initialization or the instantiated entry requires it.

- **Options:** `--release` (optimize), `--out <file>` (output path).
- **Output (json):** `{ "schemaVersion": 1, "artifact": string, "diagnostics": [Diagnostic], "ok": bool }`
- **Exit:** `0` on success; `1` if compilation failed.

### `fai run [path] [--bin <name>] [-- <args>...]`
Build (via JIT) and run. Lowest edit→run latency: no linking; executed in an
isolated worker spawned by the daemon (capabilities provided by the host).

- **Arguments:** everything after `--` is passed to the program as `Env.args ()`,
  preserving empty, spaced, Unicode, and flag-like values. With no arguments it
  returns `[]`. Worker-control arguments are excluded, in both daemon and
  `--no-daemon` modes, matching a built executable's arguments after its name.
- **Streaming:** the program's stdout/stderr stream live; stdin bytes and EOF are
  forwarded independently. Input uses one 8 KiB credit at a time, so a program
  that is not reading applies backpressure. Exit, timeout, and disconnect close
  the worker without waiting for terminal input. This is byte-pipe forwarding,
  not PTY emulation.
- **Exit:** the program's exit code (or `124` on timeout, `4` on compile error).

If the daemon is unavailable before submission, the CLI can run locally. After
submission may have started, a lost result is `FAI0008` (exit `3`) and the program
is not restarted. The same no-replay rule applies to `build` and writing `fmt`
requests. A live daemon reaps a run worker when its client disconnects, including
while the program is silent.

### `fai test [path] [--match <pat>]`
Run the `example` / `forall` contracts (JIT). Examples are evaluated; `forall`
laws are checked with generated inputs and shrunk on failure. Each contract runs
in a supervised isolated worker, so a body that traps on a generated input fails
*that* contract (a located `FAI6003`) without aborting the run.

- **Options:** `--match <pat>` (run only contracts whose subject/module matches),
  `--seed <n>`, `--count <n>` (trials per property), `--max-size <n>`.
- **Streaming:** each validated per-contract pass/fail event is delivered as soon
  as its worker frame arrives (`$/testEvent`, from the daemon), before later
  contracts finish. An acknowledged result is emitted once, including when a
  later contract crashes or times out and the worker resumes after it.
- **Output (json):** `{ "schemaVersion": 1, "total": int, "passed": int, "notRun": int, "seed": int, "events": [TestEvent], "diagnostics": [Diagnostic], "ok": bool }`, where a `TestEvent` is `{ "source": { "file": string, "origin": "user"|"standardLibrary" }, "ordinal": int, "symbol": string?, "kind": "example"|"forall", "status": "passed"|"failed"|"crashed"|"timedOut"|"notRun", "counterexample": string?, "seed": int, "trials": int, "maxSize": int }`.
  The contract key is `(source, ordinal)`: `file` is the logical source path
  (workspace-relative for user files, with `/` separators), `origin` distinguishes
  embedded sources from same-spelled user paths, and `ordinal` is zero-based within
  that file. Filtering and worker restarts preserve the key. `symbol` is only an
  optional human label. Final events are sorted by file, origin, and ordinal;
  streaming events arrive when known, including `notRun` before worker execution.
  Subjectless human progress labels include the file. The `source` field is an
  additive schema-version-1 extension; clients must tolerate unknown JSON fields.
- **Exit:** `0` if all pass; `1` otherwise.

Test diagnostic coordinates refer to the source revision compiled for that run,
even if another daemon request edits, replaces, or removes the file meanwhile.

### `fai fmt [path] [--check]`
Canonically format in place (idempotent).

- **Options:** `--check` (do not write; exit `1` if any file would change).
- **Output (json):** `{ "schemaVersion": 1, "changed": [string] }`
- **Exit:** `0`; with `--check`, `1` if any file is unformatted.

### `fai lsp`
Start the Language Server on stdio (standard LSP, JSON over `Content-Length`).
Editors speak this; agents use `fai query` instead.

Saved example diagnostics are invalidated when any effective workspace input
changes, including an open dependency. No-op edits preserve the cached result;
example execution remains save-driven.

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

Standard-library definitions use real, read-only `file:` documents extracted
from the running compiler's embedded sources. They are content-versioned under
`$FAI_CACHE_DIR/.lsp-std-sources` (or a `fai` directory in the system temporary
directory), shared across workspace roots. Navigation inside these documents
uses their embedded source identities; editor changes never replace the
compiler's standard library. No editor-specific virtual-document provider is
required.

The server registers `**/*.fai` create/change/delete watchers when the client
supports dynamic registration, and accepts `workspace/didChangeWatchedFiles`
notifications. Disk changes refresh closed dependencies and republish diagnostics
for open files. Open buffers remain authoritative even if their disk file changes
or is deleted; closing a buffer restores the current disk file or removes it.
Real dependency changes invalidate saved example results; no-op events preserve
the warm query state.

On **save** (not on every keystroke), the saved file's closed `example` contracts
are evaluated in an isolated worker and a failing one is published as `FAI6001`
alongside its type diagnostics; the results persist across edits to other files
and are cleared when the file itself is edited. Set the `examples` initialization
option to `false` to disable this (the type-check is unaffected).

Every request receives a terminal JSON-RPC response with its original id.
Malformed parameters return `InvalidParams` (`-32602`), unsupported methods
return `MethodNotFound` (`-32601`), and an unserializable result returns
`InternalError` (`-32603`). Unsupported notifications are ignored.

Rename validates the replacement with the language lexer and re-resolves a
temporary edited workspace before returning edits. It rejects keyword/wildcard
names, declaration collisions, and reference capture with an `InvalidParams`
error explaining the failure. Operators, record puns, and as-pattern binders are
not offered for token-only rename. Open unsaved buffers remain authoritative.

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
it entirely (one-shot in-process). It shuts down after an idle timeout
(`FAI_DAEMON_IDLE_TIMEOUT`, default 600 seconds), measured from completion of the
last active request. Compilation, supervised workers, output streaming, and the
final response keep it active; passive tap subscribers do not.

---

## 7. Daemon protocol (IPC / wire)

The client↔daemon link is **JSON-RPC 2.0 semantics encoded with MessagePack**.
(The LSP endpoint is unaffected — it remains standard LSP: JSON over
`Content-Length`.)

### 7.1 Transport & discovery

- **Transport:** unix-domain socket (POSIX); named pipe (Windows).
- **Path:** `${XDG_RUNTIME_DIR:-$TMPDIR}/fai/<workspace-hash>-<compilerVersion>-<build-prefix>.sock`.
  The build prefix is the first 16 hexadecimal characters of the whole-tool
  identity. Different builds of the same package version use separate daemons.
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
  "params": { "protocolVersion": 5, "compilerVersion": "0.1.0", "compilerBuildId": "…",
              "schemaVersion": 1, "workspaceRoot": "/abs/path",
              "clientInfo": { "name": "fai-cli", "version": "0.1.0" } } }
// ← response
{ "result": { "serverCapabilities": { "streaming": true, "query": true },
              "compilerVersion": "0.1.0", "compilerBuildId": "…", "protocolVersion": 5, "schemaVersion": 1 } }
```

Because the client and daemon are the **same binary**, a version mismatch means a
*stale* daemon: the client sends `exit`, then respawns and re-initializes.
The protocol requires the full executable build identity in both
directions; matching `0.1.0` version strings alone cannot reuse a stale compiler.
Source-qualified contract events remain required.
Protocol 5 carries a `project` options record (`no_local`, `locked`, `offline`,
`no_vendor`) on command, run, and test requests. Each context is
prepared independently; located manifest failures use the same result envelope
as one-shot execution, including when opening a project fails.
Dependency restoration runs before acquiring the compiler database lock. Prepared
metadata is checked again before publication so an older restore cannot replace
newer manifest or lock inputs.

The language server restores projects in the background after opening or saved
manifest/lock changes. Requests remain available while restoration waits on Git or
metadata locks. Source overlays remain authoritative; unsaved manifests receive
syntax feedback without publishing locks. Superseded restore generations are
discarded and coalesced into the newest request. `$/fai/projectStatus` notifications
carry `schemaVersion: 1`, a `generation`, and `state: restoring|ready|failed`; VS Code
shows dependency progress and failures in its status bar.

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
| `$/testEvent` | `TestEvent` (per the `fai test` schema: `source`, `ordinal`, `symbol?`, `kind`, `status`, `counterexample?`, `seed`, `trials`, `maxSize`) | `test` |
| `$/output` | `{ id, stream: "stdout"\|"stderr", chunk: bytes }` | `run` worker output |
| `inputReady` | one credit for up to 8192 stdin bytes | server → run client |
| `input` / `inputEof` | one credited byte chunk / terminal EOF | run client → server |
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
