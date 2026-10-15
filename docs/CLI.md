# Fai — CLI Reference

This reference follows the current repository sources. For an installed release,
use the documentation at its Git tag; development documentation can include
unreleased changes.

The CLI creates projects, manages dependencies, checks and formats source, builds
native executables, runs programs and tests, and provides editor intelligence.
`fai build` uses ahead-of-time compilation; `fai run` and `fai test` use JIT
compilation in supervised workers. `fai check` also evaluates closed examples by
default; use `--no-examples` for type checking alone. `fai test` runs both inline
`example`/`forall` contracts and named test suites, including generated properties
with shrinking.

Development commands normally share a warm per-workspace daemon and an on-disk
object cache. `--no-daemon` selects in-process command handling; program and test
execution still use isolated workers. Read requests use database snapshots, with
in-flight work cancelled and retried when source changes. The daemon and worker
support Linux, macOS, and Windows. Non-streaming daemon replies carry rendered
stdout/stderr; test results stream as `TestEvent` frames. The command and
protocol sections below document flags, outputs, resource limits, and failure
behavior.

Start with the [README quick start](../README.md#hello-fai),
[installation guide](INSTALLATION.md), and [language examples](../samples/) if you
are new to Fai. Compiler upgrades (`fai upgrade`) and launcher management are
described in the installation guide; `fai update` below updates project
dependencies.

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
`FAI_LINKER` explicitly selects the Windows linker. Otherwise the driver prefers
the MSVC developer environment's `VCToolsInstallDir` before searching `PATH`, so
Git Bash's same-named Unix utility cannot shadow it. Link failures retain the
linker's stdout and stderr in their diagnostic, including through the daemon.

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

Resolve/fetch retains an already valid graph without refreshing release refs.
An explicit resolve may repair a compiler-incompatible graph; ordinary locked
commands report the incompatibility. Unused overrides neither require checkout
directories nor invalidate unrelated graph fingerprints. Replacing a branch's
source repository also replaces its old branch binding.

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
Both planning and publication reject a project with `[package] publish = false`.
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

The installed `fai` entry point is a native launcher. It selects an exact compiler
before interpreting compiler-specific arguments. See [INSTALLATION.md](INSTALLATION.md)
for bootstrap commands, platforms, and native build prerequisites.

```sh
fai upgrade                       # install stable and select it globally
fai installer upgrade             # upgrade the native launcher
fai installer --version
fai toolchain install 0.2.0        # preserve the current default
fai toolchain default 0.2.0
fai toolchain pin 0.2.0            # update the active root's fai.toml
fai toolchain which --message-format=json
fai toolchain list --available
fai toolchain uninstall 0.1.0
fai toolchain doctor
fai +0.2.0 check
```

`fai update` remains the dependency-update command. Compiler upgrades change the
global default; project pins remain exact. `+VERSION` precedes `FAI_TOOLCHAIN`,
then the project pin and global default supply selection. A conflicting explicit
selection and project pin fails. Missing compilers produce installation
instructions; ordinary compiler commands never download them. Management commands
work without an installed compiler and support `--message-format=json`.

Installer JSON results have `schemaVersion`, `ok`, `command`, and `result`;
failures have `schemaVersion`, `ok: false`, and `error`. `which` returns `compiler`
and `reason` (argument, environment, project, or default) in `result`. Local `list`
retains its original `schemaVersion`/`toolchains` envelope.

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
  `Module.Submodule.name`, e.g. `Collections.map`. Use a source position when a
  name alone is ambiguous.
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
Run the `example` / `forall` contracts and named `Test.Suite` cases (JIT). Examples are evaluated; `forall`
laws are checked with generated inputs and shrunk on failure. Each contract runs
in a supervised isolated worker, so a body that traps on a generated input fails
*that* contract (a located `FAI6003`) without aborting the run.

- **Options:** `--match <pat>` (run only contracts whose subject/module matches),
  `--seed <n>`, `--count <n>` (trials per property), `--max-size <n>`,
  `--junit <path>` (also write a JUnit XML artifact), `--list` (discovery only;
  incompatible with `--junit`), and `--deny-focused` (reject focus markers before
  filtering, as enabled by repository CI).
- **Output (human):** a selection header, one live line per completed contract,
  detailed failures and an end-of-run summary. For example:

  ```text
  running 2 contracts (1 example, 1 property)
  seed: 0; trials per property: 100; max size: 100

  test Lists.reverse [example, Lists.fai:10:1] ... ok
  test Lists.reverse [forall, Lists.fai:11:1] ... ok

  test result: ok. 2 passed; 0 failed; 0 crashed; 0 timed out; 0 not run; 0 filtered out; finished in 0.12s
  ```

  Labels include the kind and source location even when several contracts share
  a subject. Statuses are `ok`, `FAILED`, `CRASHED`, `TIMED OUT`, and `NOT RUN`;
  the existing `--color` policy applies to live lines and the summary. Failure
  diagnostics retain their codes, locations and shrunk counterexamples. Failed,
  crashed or timed-out properties also show the seed/count/size options to reuse
  with the same path and filter. No-match and empty selections are explicit.
  Contracts blocked before execution are counted separately, not as failed
  assertions. Unavailable generators are `not run`, not ignored successes.
- **Streaming:** each validated per-contract pass/fail event is delivered as soon
  as its worker frame arrives (`TestEvent`, from the daemon), before later
  contracts finish. An acknowledged result is emitted once, including when a
  later contract crashes or times out and the worker resumes after it.
- **Output (json):** the test report uses **`schemaVersion: 2`** (diagnostic and
  other command schemas remain unchanged). The envelope retains `total`,
  `passed`, `notRun`, `seed`, `config`, `summary`, `events`, `diagnostics`, and `ok`,
  and adds `focused`. Event `kind` is `example`, `forall`, or `test`; `status` is
  `passed`, `failed`, `crashed`, `timedOut`, `notRun`, `errored`, `skipped`, or
  `listed`. Existing contract event fields and identities retain their meanings.
  Named events add a stable `id`, a package-qualified `suite` identity, exact
  `path` name components, structured
  `failures` (`kind`, `message`, `expected`, `actual`, `context`, `file`,
  `location`), optional skip `reason`, and bounded captured `output`.
  A named event's `source` and `location` identify its `Test.test` declaration,
  including declarations in imported helpers; its `id` remains rooted in the
  selected private `tests` binding, not that helper's file or line number.
  `TestConfig` is `{ "seed": int, "trials": int, "maxSize": int }`. Trials are
  the configured limit per property, not an observed count of predicate calls
  (a property may fail early, and shrinking also calls its predicate).
  `TestSummary` contains integer fields `total`, `examples`, `properties`,
  `named`, `passed`, `failed`, `crashed`, `timedOut`, `notRun`, `errored`, `skipped`,
  `listed`, `blocked`, `filteredOut`, and `elapsedMs`. The outcome counts sum to
  `total`; `examples + properties + named` also equals `total`. `filteredOut`
  counts cases excluded by the name filter or active focus and is outside
  `total`. `blocked` counts selected
  runnable contracts that were not executed; these have no completed event.
  `elapsedMs` is monotonic wall time from plan preparation through execution,
  including synthesis/JIT compilation and worker restarts, excluding project
  loading. It varies between equivalent runs. JSON mode emits only the final
  envelope on stdout, without human progress text.
  The contract key is `(source, ordinal)`: `file` is the logical source path
  (workspace-relative for user files, with `/` separators), `origin` distinguishes
  embedded sources from same-spelled user paths, and `ordinal` is zero-based within
  that file. Filtering and worker restarts preserve the key. `symbol` is only an
  optional human label. Contract events are sorted by file, origin, and ordinal;
  named events retain deterministic suite traversal order. Streaming events arrive
  when known, including `notRun` before contract worker execution.
  Locations are one-based, with Unicode-scalar columns, and refer to the compiled
  source revision. Human labels without coordinates fall back to the file and a
  one-based ordinal; JSON identity always retains its zero-based ordinal.
  Named events must be keyed by `id`, not the legacy `ordinal` field. Their ID
  encodes source, package/suite identity, the `tests` binding, and the exact name path, so delimiters in
  names cannot collide. Clients migrating from version 1 must handle the new
  event kinds and states. Existing top-level `total`, `passed`, `notRun`, and
  `seed` retain their meanings.
- **Exit:** `0` if all pass; `1` otherwise.

Test diagnostic coordinates refer to the source revision compiled for that run,
even if another daemon request edits, replaces, or removes the file meanwhile.

#### Named suites

Package test sources may define a private, explicitly typed `tests` value:

```fai
tests : Test.Suite Test.Context {}
let tests = Test.describe "arithmetic" [
  Test.test "addition" (fun _ -> Expect.equal Int.toString 4 (2 + 2))
]
```

Legacy directory/standalone selections also recognize this binding. Production
and dependency package sources do not become test roots accidentally. Suite
construction must be pure and argument-free, with a closed effect bound and the
standard `Test.Context`. It is discovered in an isolated worker before any test
body or runtime initialization executes. Duplicate sibling names, empty names,
and empty skip reasons are discovery errors. Discovery can evaluate ordinary
pure helper code, but traps and loops fail discovery rather than the daemon.

`Test.Context` contains `runtime`, the run's `seed`, `trials`, `maxSize`, a stable case `id`, and an
absolute, per-case `tempRoot`. Complex bodies use named functions with explicit
signatures, for example `body : Test.Context -> Test.Check / { FileSystem }`.
This supplies nominal capability types and permits ordinary multiline `let`
layout without putting local blocks inside a parenthesized list element.
The suite's effect row admits its cases' effects; mixed `Test.Suite` list
elements join that bound without changing ordinary list unification.

Return a `Test.Check`. `Expect.equal renderer expected actual` records values
on failure; `Expect.isTrue`, `Expect.fail`, and `Expect.ok` handle predicates,
explicit failures, and `Result` values. `Expect.all` combines already-evaluated
checks, while `Expect.andThen` defers later work until success. `Test.error`
reports operation errors separately from assertions. Obvious discarded checks
are warnings (`FAI6011`), not thrown exceptions or linear-use enforcement.

`Test.property name arbitrary predicate` constructs a named pure property. Supply
a `Test.Arbitrary` (for example `Test.int`, `Test.list Test.string`, or
`Test.tuple2 Test.int Test.string`); the predicate returns a `Test.Check`.
The runner's `--seed`, `--count` and `--max-size` control generation exactly as
for `forall`. A failing check is shrunk and retains its assertion location and
expected/actual values, with the counterexample and reproduction settings in its
context. Discovery and skipped properties do not run generators or predicates.
A property is one named case, not a separate worker for each generated input.
Use inline `example`/`forall` declarations for documentation beside library
implementations, and named cases/properties for dedicated test sources.

`Test.withFixture` adapts each leaf's context through a scoped callback.
`Test.bracket acquire release suite` supplies a fresh resource per selected
leaf; both acquisition and release return `Result _ String`. It retains body
failures and release errors together and masks sticky cancellation during
release. Each selected case has a fresh worker and temporary root. Parent-owned
temporary storage is removed after the worker exits. This is not an OS sandbox:
tests have the permissions of their explicit capabilities, and hard termination
cannot execute user cleanup for arbitrary external resources.

`Test.focus suite` selects that subtree. After name filtering, active focus
selects the union of focused branches and filters other named cases and
contracts. Skip wins over focus; markers inside entirely skipped subtrees do
not activate focus. `Test.skip reason suite` is intentional, visible skipping;
it still typechecks but does not run fixtures or bodies. `--deny-focused` checks
all discovered markers before name filtering, including skipped branches.
The human report prominently labels focused runs. No shorthand aliases or
automatic retries/parallel execution are provided.

Named worker preparation/discovery is bounded by `FAI_TEST_DISCOVERY_TIMEOUT_MS`
(default 10000). Once ready, each named case uses `FAI_TEST_TIMEOUT_MS` (default
300000); expiration requests cooperative cancellation, followed by
`FAI_TEST_CLEANUP_TIMEOUT_MS` grace (default 5000), then hard termination.
Test stdin is EOF; control messages use a separate channel. Output is captured,
not mixed into the worker protocol, with up to 512 KiB retained from each output
stream. See `samples/NamedTests.fai` and
`packages/process/test/NamedSpec.fai` for filesystem and process examples.

#### JUnit export

```sh
fai test --junit target/test-results/fai.xml
fai test --message-format=json --junit target/test-results/fai.xml
```

JUnit export leaves normal human/JSON stdout unchanged. The path is relative to
the selected project root (`-C`), or absolute. Parent directories are created.
The CLI writes a temporary sibling and atomically replaces the destination only
after a complete report is available; the daemon returns XML, never receives or
writes the artifact path. Report-write errors go to stderr and exit `3`, even if
the tests passed; JSON stdout still describes the test outcome as one envelope.

The XML uses a `testsuites` root with one `testsuite` for the invocation. Every
selected contract becomes a `testcase`, grouped by source through `classname`.
Named cases use their exact serialized name path; intentional skips become
`skipped` elements, never execution errors. Focus-filtered cases are omitted.
Names include the subject, kind and one-based file ordinal so contracts sharing
a binding remain distinct. `file` and `line` identify the compiled source.
False assertions are `failure` elements; crashes, timeouts, unavailable generators,
and blocked contracts are `error` elements. Diagnostic codes, locations and
counterexamples appear in their details. Filtered-out contracts are not emitted.
Non-contract compiler/setup errors become a synthetic `test run` error case,
including failures before a test plan exists. Consequently XML case counts can
exceed the selected contract count; all XML counts describe the emitted cases.
An empty successful selection produces a valid zero-case report.

Suite properties record the seed, configured trial limit, maximum size, selected
count and filtered-out count. Suite time is the measured preparation/execution
duration in seconds; per-case times are omitted rather than fabricated. Reports
are produced on ordinary test and preparation failures, but a killed/disconnected
CLI or invalid command-line arguments cannot guarantee an artifact. Existing
files remain intact if publication fails.

Most CI systems can consume JUnit XML directly. In this repository,
`scripts/junit-summary.py <file-or-directory>` renders it into GitHub Actions'
step summary. Native package CI retains one XML file per package and uploads
available reports even when a test step fails.

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

The client↔daemon link is a **versioned, length-prefixed MessagePack protocol**.
It serializes the Rust enums and records in
[`crates/fai-server/src/protocol.rs`](../crates/fai-server/src/protocol.rs), not
JSON-RPC `method`/`params` envelopes. The JSON examples below show decoded frame
bodies as printed by `--protocol-log` and `fai daemon tap`; JSON is not a wire
transport option. The LSP endpoint is separate and remains standard LSP JSON-RPC
with `Content-Length` framing.

### 7.1 Transport & discovery

- **Transport:** unix-domain socket (POSIX); named pipe (Windows).
- **Unix path:** `<runtime-dir>/fai/<workspace-hash>-<compiler-version>-<build-prefix>.sock`.
  The runtime directory is selected from `FAI_RUNTIME_DIR`, `XDG_RUNTIME_DIR`,
  `TMPDIR`, then `/tmp`. Workspace and whole-tool build prefixes are 16 hexadecimal
  characters. Different compiler builds use separate endpoints.
- **Windows name:** `fai-<workspace-hash>-<compiler-version>-<build-prefix>.sock`
  in the local named-pipe namespace.
- **Permissions:** Unix socket files are set to `0600`. The transport is local;
  it does not open a TCP port.
- **Spawn race:** a successful bind owns the endpoint; a losing process connects
  to the existing daemon. A stale Unix socket is reclaimed after a failed connect.
- **Lifecycle:** idle-timeout shutdown or explicit `Shutdown`. `Exit` closes only
  the requesting connection and does not shut down the daemon.

### 7.2 Framing & encoding

- Each frame starts with a `u32` little-endian body length, followed by MessagePack
  serialized with `rmp_serde::to_vec_named`. Readers reject bodies over 64 MiB.
- Enums are externally tagged: a payload variant is a single-key object such as
  `{"Initialize": {...}}`; a unit variant is a string such as `"Status"`.
  Record fields use their Rust snake-case names. Variant names are case-sensitive.
- There are no request IDs. Clients issue one operation at a time on a connection,
  reading any streamed frames until a terminal `{"Result": ...}` frame. The
  connection can then carry another operation. A run additionally accepts credited
  stdin frames while it is executing.
- **Debugging:** `--protocol-log <file>` decodes one connection's frames to JSON;
  `fai daemon tap` subscribes to a live decode of the frames on *every other*
  connection (the daemon broadcasts each frame, best-effort, to attached taps),
  printing `#<conn> <arrow> <json>` per frame until interrupted. There is no
  `--protocol=json` option.

### 7.3 Handshake & versioning

Clients start with `Initialize`. The version and build ID must identify the exact
compiler being used; the values below are illustrative:

```jsonc
// → request
{ "Initialize": { "protocol_version": 7, "compiler_version": "0.0.1",
                  "compiler_build_id": "<exact whole-tool build ID>",
                  "workspace_root": "/abs/path" } }
// ← response
{ "Result": { "Initialized": { "protocol_version": 7, "compiler_version": "0.0.1",
                              "compiler_build_id": "<exact whole-tool build ID>" } } }
```

Protocol version **7**, compiler release, and full whole-tool build identity must
match in both directions. A matching release number alone is insufficient.
Older published compilers can use an earlier protocol; use their matching source
and handshake rather than assuming this repository's current protocol version.
A mismatch returns `{"Result": {"Error": "compiler mismatch: ..."}}`.
The handshake does not contain a CLI-output `schemaVersion`, client metadata, or
a capability-negotiation object.

`Command`, `Run`, and `Test` carry a `project` options record (`no_local`, `locked`,
`offline`, `no_vendor`); omitted policy fields default to false. Each context is
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

- **Per invocation:** connect → initialize → issue an operation (with any dirty
  files) → read its streamed frames and terminal result → disconnect.
- **Concurrency:** reads run concurrently (each on a cloned database snapshot,
  off-lock). An input change bumps the salsa revision and **cancels** in-flight
  reads, which restart on the new revision.
- **Cancellation:** there is no general client cancellation request or
  `$/cancelRequest` frame. A disconnected `Run` client causes its worker to be
  killed and reaped, including when the program is silent. Other operations do
  not have a general disconnect-cancellation guarantee; their normal resource
  limits and result-delivery failures still apply.
- **No replay after uncertain execution:** the CLI does not automatically rerun
  a submitted `Run`, `Build`, or writing `Fmt` when its result is lost. It reports
  `FAI0008` instead of potentially repeating effects.

### 7.5 File-state sync

The daemon keeps salsa inputs in sync with disk:

- For command preparation it performs an **incremental scan** — compares
  mtime/size and re-hashes changed files — then updates inputs for what actually
  changed.
- A client that knows what it edited may put a **dirty set** in its `Command`,
  `Run`, or `Test` payload. Content, when supplied, is the authoritative
  already-written text; the hash is advisory. A command frame can look like:

```json
{
  "Command": {
    "project": { "no_local": false, "locked": false, "offline": false, "no_vendor": false },
    "spec": { "Check": { "path": "src/Main.fai", "examples": true } },
    "opts": { "format": "Json", "color": false },
    "dirty": [ { "path": "src/Main.fai", "hash": null,
                 "content": "module Main\n" } ]
  }
}
```

The ordinary CLI sends an empty dirty set. This IPC field is not a supported
speculative-edit API. Unsaved editor overlays belong to the separate LSP session.

### 7.6 Request variants

| Operation | Decoded request shape |
|---|---|
| Handshake | `{"Initialize": InitParams}` |
| Check, build, format, or query | `{"Command": {"project": ProjectOptions, "spec": CommandSpec, "opts": RenderOpts, "dirty": [...]}}` |
| Run | `{"Run": {"project": ProjectOptions, "path": "...", "args": [...], "dirty": [...]}}` |
| Test | `{"Test": {"project": ProjectOptions, "path": null, "match": null, "seed": null, "count": null, "max_size": null, "list": false, "deny_focused": false, "junit": false, "opts": RenderOpts, "dirty": [...]}}` |
| Status, tap, daemon shutdown, connection close | `"Status"`, `"Tap"`, `"Shutdown"`, `"Exit"` |
| Run stdin | `{"Input": [byte, ...]}` or `"InputEof"` |

`CommandSpec` is itself tagged: `Check`, `Build`, `Fmt`, or `Query`. `Query` wraps
the tagged query variants from
[`crates/fai-driver/src/query.rs`](../crates/fai-driver/src/query.rs), such as
`{"Query": {"Api": {"module": "Collections"}}}`. `RenderOpts.format` is `"Human"`
or `"Json"`; it selects rendered CLI output, not the MessagePack framing.

### 7.7 Server messages and terminal responses

| Variant | Decoded payload | Used by |
|---|---|---|
| `Output` | `{"stream": "Stdout"\|"Stderr", "chunk": [byte, ...]}` | Run output; test header and package notices |
| `TestEvent` | A serialized contract/named-test event, described in §5 | Test results |
| `JunitReport` | Complete XML string, before the terminal response | Tests requesting `junit: true` |
| `InputReady` | Unit variant: `"InputReady"`; grants one chunk of at most 8192 bytes or EOF | Run stdin |
| `TapFrame` | `{"conn": number, "direction": "Inbound"\|"Outbound", "json": "..."}` | Passive traffic observation |
| `Result` | A tagged `Response` | Terminal response |

`Response` is `Initialized`, `Command`, `Status`, `RunExit`, `Test`, `Error`, or
the unit variant `Ok`. `Command` and `Test` contain a rendered
`{"stdout": "...", "stderr": "...", "exit": 0}` record. For example, a successful
run ends with `{"Result": {"RunExit": 0}}`; a shutdown acknowledgement is
`{"Result": "Ok"}`. There are no separate progress, diagnostic, or log frame
variants. Diagnostics are part of the rendered command/test result.

`Tap` is acknowledged with `{"Result": "Ok"}` and then turns the connection into
a passive subscriber. Its `TapFrame` values describe other connections until it
disconnects. Delivery is best-effort: a subscriber that falls behind drops frames
rather than throttling the connection producing them.

### 7.8 Execution model (`run` / `test`)

The daemon builds a portable IR bundle warm (reusing cached function code), then
ships it to an **isolated worker process** that JIT-compiles and runs it. For
`run`, the worker carries the requested capabilities; its stdout/stderr stream
back as `Output` frames and its exit code (including crash/timeout status) is the
terminal `RunExit` response. Stdin uses byte pipes and credit-based forwarding,
not a PTY. For `test`, isolated workers execute selected contracts and named
cases; the daemon streams `TestEvent` frames and returns the rendered report in
`Result(Test(...))`. A contract crash or timeout is recorded without aborting
the remaining selection. Worker timeout and resource controls are described in
§5; isolation keeps a worker failure separate from the warm compiler database.

A program that uses concurrency or networking runs on the runtime's M:N scheduler.
Its **worker-thread count** defaults to the host's available parallelism — which
on Linux respects the process's cgroup CPU quota and scheduler affinity, so a
containerized or CPU-pinned run scales to its budget automatically — and can be
overridden with the **`FAI_WORKERS`** environment variable (a positive integer;
`FAI_WORKERS=1` forces fully sequential multiplexing). Blocking host calls (file
I/O, DNS) run on a separate pool sized up to `FAI_BLOCKING_THREADS` (default 512).

### 7.9 Errors & security

- Protocol/handshake failures use `{"Result": {"Error": "message"}}`. Command
  failures normally use the rendered `Command`/`Test` response with a nonzero
  `exit`. With JSON rendering, their `stdout` contains the versioned diagnostic
  envelope and stable `FAInnnn` codes; there is no JSON-RPC `error.data` object.
- The transport is local-only; Unix socket files are set to `0600`. It is not
  a remote service or a sandbox for running untrusted native code.

---

## 8. Code intelligence — `fai query`

`fai query <subcommand>` is the **read-only** code-intelligence surface for
agents. Shared behavior:

- **Output is JSON by default.** The global `--message-format=human` selects the
  human renderer, which currently also prints JSON for query results.
- **Addressing:** name path (`Module.name`) or `file:line:col` (§3).
- **Best-effort:** partial results are returned even when the workspace has
  errors.
- **Bounding:** list-producing result schemas report `"truncated": true` when
  capped. The current CLI does not expose the driver's optional `limit` inputs.
- Queries reuse the warm daemon; latency depends on the query and workspace.

### `fai query symbols`
List/search symbols.

- The current CLI lists workspace symbols without module/kind/name filters.
- **Output:** `{ "schemaVersion": 1, "symbols": [SymbolRef], "truncated": bool }`

```
$ fai query symbols
{ "schemaVersion": 1, "symbols": [ { "path": "Collections.map", "kind": "function",
  "signature": "('a -> 'b) -> List 'a -> List 'b", "...": "..." } ], "truncated": false }
```

### `fai query def <target>`
Resolve to definition site(s).

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "definitions": [Location] }`

### `fai query refs <target>`
Find all use sites.

- The current CLI takes only the target; inclusion/filter options are not exposed.
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

- **Options:** `--transitive`.
- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "dependents": [SymbolRef], "transitive": bool, "truncated": bool }`

### `fai query callers <symbol>` / `fai query callees <symbol>`
Inbound / outbound call edges (call hierarchy).

- **Output:** `{ "schemaVersion": 1, "target": SymbolRef, "edges": [ { "symbol": SymbolRef, "sites": [Location] } ] }`

### `fai query search <type-pattern>`
Hoogle-style search: find functions whose type matches a pattern (unification up
to row polymorphism), e.g. `List 'a -> Int`.

- The current CLI takes the type pattern without a result-limit flag.
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
- The **query command names** and their structured results.

Breaking output-schema changes bump the relevant schema version and are documented
in the changelog. The daemon protocol is an internal, separately versioned
compiler interface: incompatible wire changes increment `PROTOCOL_VERSION`, and
clients must match the compiler version and complete build identity as well.
It does not promise interoperability across compiler releases or builds.
