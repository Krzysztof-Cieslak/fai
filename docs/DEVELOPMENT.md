# Development workflows

Terminal library contracts are ordinary Fai tests: `fai test -C packages tui`.
Headless lifecycle checks run with `fai run -C packages
tui/examples/RuntimeChecks.fai`; the form, SQLite browser and simulated agent
examples require a real terminal. CI additionally runs Unix PTY fixtures for
keyboard input, resize and restoration. `.github/actions/tui` is selected for
TUI package changes and uses the same exact compiler-bundle cache as other packages.

PostgreSQL source-package integration uses the ordinary Fai CLI against a
disposable database. See `packages/postgres/README.md` for `FAI_PG_URL`, optional
TLS roots, and the JIT/AOT commands. `.github/actions/postgres` provisions the CI
service and executes the Fai examples directly whenever dependency-aware
selection includes `postgres`; pure contracts still run through `fai test`.

## Daily package development

Semver contracts run with `fai test -C packages --package semver`. Its package-local
integration programs cover large decimal components, long identifier/requirement
lists and stable version selection through JIT and AOT. Run them with
`python3 scripts/package_integrations.py semver --no-dependents --fai /path/to/fai`.

Glob contracts run with `fai test -C packages --package glob`. The nested
`glob-examples` package declares native traversal and Git-oracle fixtures in its
`ci.json`; run them with `python3 scripts/package_integrations.py glob-examples
--no-dependents --fai /path/to/fai`. The Git fixture uses installed Git with
isolated configuration and compares both explicit case policies.

Encoding contracts run with `fai test -C packages --package encoding`. Its package-local
`RuntimeChecks` integration covers all RFC 4648 codecs on large buffers and long
fixed-chunk streams through JIT and AOT. Run it with
`python3 scripts/package_integrations.py encoding --no-dependents --fai /path/to/fai`.
Dependency-aware checks include its PostgreSQL, LLM and TUI consumers.

UUID contracts run with `fai test -C packages uuid`. Its `ci.json` declares a
native fixture for injected and OS-backed entropy/clock generation. Run
`python3 scripts/package_integrations.py uuid --fai /path/to/fai` to exercise
that fixture and selected dependents, including SQLite UUID interoperability,
through JIT and AOT. UUID source and package-local integration metadata use the
cached-compiler lane.

CSV contracts run with `fai test -C packages csv`. Its Fai `RuntimeChecks`,
`StreamChecks` and `StressChecks` examples exercise scoped files, mixed effects,
early stop, writer progress and bounded streaming through JIT and AOT. The CSV
example package declares those programs in `csv/examples/ci.json`; the generic integration runner
executes them on each platform using the prepared compiler.

Configuration development uses native Fai contracts in `packages/toml`,
`packages/dotenv`, and `packages/config`, with the same cached compiler. The Fai
`config/examples/RuntimeChecks.fai` fixture exercises scoped temporary files,
environment values and CLI overrides. `toml/examples/Conformance.fai` checks the
pinned upstream corpus and normalized-output round trips. Their respective
`ci.json` files declare the programs, inputs and pinned corpus. CI runs both JIT
and AOT using the same generic runner.

Regex package contracts run with `fai test -C packages regex`.
`scripts/tests/regex_toolchain.py --fai /path/to/fai` exercises the pure native
engine through a relocated JIT/AOT compiler, including shared programs, immutable
cursors, binary data, callbacks and compile-negative effect/type fixtures. Native
engine changes additionally run the runtime regex tests and space/storage guards.

Logging contracts run with `fai test -C packages logging`.
`scripts/tests/logging_toolchain.py --fai /path/to/fai` checks direct and queued
logging through JIT/AOT, complete file records, scoped capture, cancellation,
backpressure and effect-negative fixtures. `.github/actions/logging` runs the same
checks with the cached compiler when logging or a dependency is selected.

The LLM package runs its pure contracts with `fai test -C packages llm`.
`scripts/tests/llm_toolchain.py --fai /path/to/fai` runs deterministic local-provider
JIT/AOT checks, real file/process tools, SQLite persistence, effect-negative
compiler fixtures and Unix terminal integration. `.github/actions/llm` uses the
same exact compiler cache and dependency-aware package selection. Live provider
smoke tests are opt-in through the Generate example and explicit environment keys.

Process package contracts run directly with `fai test -C packages process`.
`scripts/tests/process_toolchain.py --fai /path/to/fai` runs real binary-pipe,
full-duplex, cancellation and pipeline fixtures through JIT and AOT and verifies
callback effects. `.github/actions/process` selects these checks for process
changes and uses the exact cached compiler. The interactive task runner is
`fai run -C packages process/examples/TaskRunner.fai -- git status --short`.

Use the compiler directly. Package tests are ordinary Fai `example` and `forall`
declarations, including those in each package's `test/` directory:

```sh
fai check -C packages --package json
fai test -C packages --package json
fai test -C packages --package web
fai test -C packages --package sql
fai test -C packages --package sqlite
fai test -C packages json/test/CodecSpec.fai --match treeRoundTrip
fai fmt -C packages --package json
```

The `packages/fai.toml` workspace declares its members and explicit local
replacements of their portable Git dependencies. Each selected package retains
a warm database for its library and dependencies. Omit `--no-daemon` for the local loop.
`fai check --no-examples` gives type-only feedback; normal `check` also evaluates
closed examples. `fai test -C packages` tests all source packages.

Activate a cached compiler once in the shell when working from a checkout:

```sh
export PATH="$(dirname "$(python3 scripts/compiler.py ensure)"):$PATH"
```

PowerShell:

```powershell
$compiler = python scripts/compiler.py ensure
$env:Path = "$(Split-Path $compiler);$env:Path"
```

After changing compiler/runtime/std sources, run setup again to select the new
matching bundle. Package edits keep the same binary and warm daemon. Native
contracts continue to run directly through `fai test`.

Each package's `fai.toml` supplies its name, source ownership, dependencies, and
executable entries to both the compiler and repository CI. Normal and development
edges participate in CI impact selection; only normal edges determine dependency
cycles. Shared example modules live in nested example packages, allowing test
helpers to depend on their library without adding a production dependency back
to those helpers. Optional `ci.json` files declare integration programs and pinned
fixtures, using the manifest's identity and dependencies.

Measured on the development Linux host with the assertion-enabled optimized
compiler: a cold JSON type check was 106 ms, a warm check 4 ms, and the warm
143-contract JSON suite 308 ms. These are observations, not timing gates.

## Package-local integration programs

Add ordinary Fai integration programs to the owning package's `ci.json` rather
than adding a package-specific GitHub action or editing the shared workflow:

This file contains integration settings only. Its package name and dependency
edges come from the adjacent `fai.toml`.

```json
{
  "schemaVersion": 1,
  "integrations": [
    {
      "name": "runtime",
      "entry": "examples/RuntimeChecks.fai",
      "args": ["--port", "8100"],
      "env": {"EXAMPLE_PORT": "7100"},
      "stdoutLines": ["Example runtime checks passed"]
    }
  ]
}
```

`entry` is a forward-slash `.fai` path relative to that package, and must stay
inside it. Each integration name is unique within the package. The runner uses
the existing compiler to execute JIT, build AOT, and execute the native binary,
with identical application arguments and environment overrides. Commands use
argument arrays, never shell evaluation. Both executions use the shared
`packages/` directory as their working directory; compiler requests select the
entrypoint's owning manifest, including nested example packages. Temporary
native executables are removed when the run ends, including after failures.

All programs must exit successfully. Optional `stdoutLines` require complete
lines in stdout; optional `stdout` instead requires an exact UTF-8 output match.
They are mutually exclusive. `stdoutLines` accepts either LF or CRLF line
endings; `stdout` compares the exact bytes, including line endings. No expectation
means only the exit status is checked. Assertions and result markers remain in
the Fai programs themselves.

Optional fields:

| Field | Default | Meaning |
|---|---|---|
| `args` | `[]` | Application arguments, after `--` for JIT |
| `env` | `{}` | Per-program overrides of the inherited environment |
| `modes` | `["jit", "aot"]` | Nonempty, distinct execution modes |
| `platforms` | `["linux", "macos", "windows"]` | Nonempty, distinct supported hosts |
| `timeoutSeconds` | `180` | Positive timeout for each execution/build command |

Arguments and environment values support `${package}`, `${workspace}`, `${temp}`
and `${fixture:name}` path substitutions. Substitutions are performed once and do
not expand shell expressions or environment variables. Environment names must
be distinct ignoring case, matching Windows behavior. Unknown metadata fields,
duplicate JSON keys, invalid paths/options and unknown substitutions fail catalog
validation before execution.

A package can declare read-only external data repositories under `fixtures`:

```json
"fixtures": [
  {
    "name": "toml-test",
    "repository": "https://github.com/toml-lang/toml-test.git",
    "revision": "ff49d109861c1ad25af53f687f2aef19ab650600"
  }
]
```

Use `"args": ["${fixture:toml-test}/tests"]` to pass its path. Repositories use
HTTPS and a full pinned commit hash, verified after checkout. Only fixtures used
by selected programs are fetched, once per package/run. They live outside the
source checkout and are removed with the other temporary outputs. Checkout sets
`core.autocrlf=false`; repository attributes still apply. See `packages/toml/ci.json`
for a complete example.

Run the same integrations locally:

```sh
python3 scripts/package_integrations.py csv --fai /path/to/fai
python3 scripts/package_integrations.py toml --list
python3 scripts/package_integrations.py config --no-dependents --fai /path/to/fai
```

Local selection includes transitive dependents unless `--no-dependents` is used.
CI uses `--ci` to consume its already computed, validated package selection.
The runner never builds the Rust compiler: compiler preparation is the separate
cached-bundle step. Packages without integration entries need no extra action.

Changes confined to these manifests, their Fai programs and package documentation
select the package lane. Changes to the shared runner, classifier or workflow
still select the full infrastructure/compiler lane. Adding another ordinary Fai
integration therefore requires no workflow edit.

## Cached compilers for source packages

Public compiler distribution uses the native launcher and optimized `release`
archives described in [INSTALLATION.md](INSTALLATION.md) and
[RELEASING.md](RELEASING.md). The content-addressed development bundles below
retain their assertion-enabled profile and are not public release artifacts.

`scripts/compiler.py` manages an immutable, content-addressed compiler cache.
It requires Python 3.11+. A cache hit needs no Rust toolchain:

```sh
python3 scripts/compiler.py ensure
python3 scripts/compiler.py ensure --offline --no-build
python3 scripts/compiler.py key
```

The default cache is `$XDG_CACHE_HOME/fai/compilers` (falling back to
`~/.cache/fai/compilers`), or `%LOCALAPPDATA%/fai/compilers` on Windows.
`FAI_COMPILER_CACHE` and `--cache-dir` override it. `ensure` prints the compiler
path on stdout; progress goes to stderr. It first checks the local exact-key
bundle, then looks for a matching successful GitHub CI artifact with `gh`, then
builds `fai-cli` if necessary. `--offline` skips remote lookup; `--no-build`
reports a missing bundle instead of invoking Cargo.

The `package-dev` profile is optimized but retains assertions and runtime leak
checks. The cache key covers the named inputs in `scripts/compiler-inputs.json`,
the native target and host compatibility, build environment and bundle schema.
It is independent of branch names, commits, timestamps and checkout paths.
Package sources and documentation under `packages/`, and the Python presentation
under `scripts/performance_report/`, are outside those inputs. New compiler files,
Cargo configuration, dependency changes and embedded `std/`
sources invalidate the key. The compiler reports its source digest and complete
executable identity through `fai build-info`.

Bundles are checksum-verified ZIPs containing a manifest and the executable:

```sh
python3 scripts/compiler.py pack --output compiler.zip
python3 scripts/compiler.py restore --archive compiler.zip
```

The executable embeds the standard library, runtime archive and non-system
native import libraries. AOT still needs the platform C linker/SDK; Windows
requires the MSVC developer environment. It does not need the producer's Cargo
registry or build directories. Remote reuse accepts successful default-branch
builds and same-repository ancestral PR builds, supporting native PR stacks;
push workflows only consume default-branch producers. Identity and checksums
are checked before an archive becomes a local cache entry.

Daemons are keyed by the full executable build identity as well as the workspace
and version. Switching compiler builds cannot silently retain an old warm
database. The backend object-cache identity remains narrower, preserving its
existing semantic cache boundary.

## Tooling checks

Filesystem changes also run `scripts/tests/filesystem_toolchain.py --fai
/path/to/fai`: a relocated-compiler JIT/AOT fixture covering native paths,
temporary scopes, cancellation, metadata/links, and atomic replacement. API and
migration details are in [FILESYSTEM.md](FILESYSTEM.md).

```sh
python3 -m unittest discover -s scripts/tests -v
python3 scripts/tests/smoke_compiler.py --fai /path/to/fai
```

The smoke test relocates the executable, gives it empty Rust homes, checks warm
daemon reuse, and runs contracts, JIT and AOT. `--other-fai /path/to/another/build`
also verifies that two builds with the same package version use different daemons.
Compiler maintainers can additionally run `scripts/tests/json_toolchain.py` and
`scripts/tests/web_toolchain.py` with `--fai /path/to/fai`. These check native
execution, numeric agreement with Python, HTTP transport and effect forwarding;
they are compiler/toolchain checks, separate from the direct package contracts.

## CI selection and compiler reuse

The workflow always starts and classifies the diff against the actual PR base
(or the previous commit for a push). A stacked PR is compared with its immediate
predecessor, not unconditionally with `main`.

| Changes | Work performed |
|---|---|
| `packages/json/**` | JSON and Web formatting, type checks, and Fai contracts |
| `packages/web/**` | Web formatting, type checks, and Fai contracts |
| `packages/sql/**` | SQL and SQLite formatting, type checks, and Fai contracts |
| `packages/sqlite/**` | SQLite formatting, type checks, and Fai contracts |
| Package-local `ci.json` integrations or Fai entrypoints | Affected packages' native contracts and declared JIT/AOT programs, using the prepared compiler |
| `packages/fai.toml` or `packages/fai.lock` | All source packages through the cached-compiler lane, including locked graph validation; no Rust suite on a cache hit |
| `scripts/performance_report/**` | Python syntax and two presentation fixtures; no Cargo or compiler bundle |
| Compiler, runtime, embedded `std/`, build/CI infrastructure, or unknown paths | Full Rust checks and all packages; compiler execution fixtures |
| Root documentation and `docs/*.md` / `docs/*.txt` only | Change/catalog validation |

Dependency impact uses both the base and head catalogs, so deleting or changing
an edge does not hide an affected dependent. Renames include both old and new
paths. New source-package directories must include `fai.toml`; malformed catalogs
and missing dependencies fail classification. An unavailable comparison base
selects the full lane conservatively.
Mixed package/presentation changes run both relevant lanes. Changes to the Rust
report-data exporter or CI infrastructure retain the full compiler checks.

Workspace-level membership, replacements and lock graphs can affect any package,
so those two metadata files conservatively select every current package. Adding a
new library and updating its workspace registration therefore stays in the package
lane. Mixed changes to Rust, embedded std, CI scripts or workflows still select
the full compiler lane.

The prepared compiler runs `fai resolve --locked -C packages` before native package
operations. This validates all declared root/platform graphs, including non-host
targets, without updating the committed lock. Invalid manifests, missing/stale
graphs and malformed locks fail the job. Workspace validation runs even when the
package selection is empty; an orphan workspace lock requires its manifest.
Legacy directory trees with neither workspace file keep their existing checks.

The protected check names (`fmt + clippy`, `unit + e2e tests (linux)`, and the
two platform checks) are stable across lanes. A package-only run still has those
names, but its Rust steps are skipped. The selection job and each job's summary
show the active lane; compiler preparation reports cache, artifact or build.

The four protected check names remain unchanged. They explicitly require a
successful classification, so a failed selector cannot pass through skipped
jobs. Their job conditions still run after a failed selector but honor workflow
cancellation, so superseded runs release the PR's concurrency slot. On package-only
changes their source checks are direct commands:

```sh
python scripts/ci.py validate-packages --fai /path/to/fai
fai fmt --check --locked -C packages --package PACKAGE
fai check --no-examples --locked -C packages --package PACKAGE
fai test --locked -C packages --package PACKAGE --seed 42 --count 128
```

Linux, macOS, and Windows each restore an exact compiler bundle and put its
directory on PATH. On a valid cache hit there are no Cargo, rustup, nextest,
Rust tests, Clippy, or compiler-fixture invocations. A miss first tries a published
matching bundle, then builds only `fai-cli`. Source/package tests still run on a
miss; it does not trigger the full Rust suite.

Compiler/default-branch runs (and successful cold-cache package runs) publish `compiler.zip`
artifacts named by the compiler fingerprint, with 30-day retention. The local
bootstrap and later stacked PRs can reuse them. Exact Actions caches provide the
faster common path; there is no loose restore-key fallback to an older compiler.
Workflow summaries record the selected lane, affected packages, compiler key,
target, and whether the bundle came from cache, an artifact, or a new build.
