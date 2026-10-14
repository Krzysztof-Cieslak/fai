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

Compression bridge changes require rebuilding the compiler/runtime. Then run
`fai test -C packages compression` and the package-local JIT/AOT integration with
`python3 scripts/package_integrations.py compression --no-dependents --fai /path/to/fai`.
The native compression tests exercise an independent zlib-rs backend and bounded
snapshot storage; run them alongside the Perceus space/storage targets.

Decimal contracts run with `fai test -C packages --package decimal` and
`--package decimal-codecs`. Their package-local JIT/AOT integrations cover large
coefficients, exact and rounded arithmetic, Float conversion, shared task use,
and exact JSON/YAML/SQL adapters. Neither package requires a compiler rebuild.

YAML contracts run with `fai test -C packages --package yaml`. Its `ci.json` pins
the published upstream data release and runs all 402 syntax/event cases, graph
rendering round trips and application-value checks under JIT and AOT. The separate
runtime fixture exercises large scalars, nested values and bounded alias expansion.
Use `python3 scripts/package_integrations.py yaml --no-dependents --fai /path/to/fai`.

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

Regex contracts and named scenarios run with `fai test -C packages regex`, including
shared programs, immutable cursors, binary data and callbacks.
`scripts/tests/regex_toolchain.py --fai /path/to/fai` retains relocated AOT and
compile-negative effect/type checks. Native
engine changes additionally run the runtime regex tests and space/storage guards.

Logging contracts and named scenarios run with `fai test -C packages logging`:
direct and queued logging, complete file records, scoped capture, cancellation
and backpressure. `scripts/tests/logging_toolchain.py --fai /path/to/fai` retains
AOT, console-wire and effect-negative checks. `.github/actions/logging` runs these
checks with the cached compiler when logging or a dependency is selected.

The LLM package runs pure contracts and named local-provider scenarios with
`fai test -C packages llm`, including tools, failures and SQLite persistence.
`scripts/tests/llm_toolchain.py --fai /path/to/fai` retains AOT,
effect-negative compiler fixtures and Unix terminal integration. `.github/actions/llm` uses the
same exact compiler cache and dependency-aware package selection. Live provider
smoke tests are opt-in through the Generate example and explicit environment keys.

Process package contracts run directly with `fai test -C packages process`.
`scripts/tests/process_toolchain.py --fai /path/to/fai` supplies the independent
Python child program to named binary-pipe, full-duplex, cancellation and pipeline
tests. It retains AOT, inherited-stdin, callback-effect and terminal checks.
`.github/actions/process` selects these checks for process
changes and uses the exact cached compiler. The interactive task runner is
`fai run -C packages process/examples/TaskRunner.fai -- git status --short`.

Use the compiler directly. Package tests combine pure `example`/`forall`
declarations with explicitly typed private `tests : Test.Suite Test.Context { ... }`
roots. Named cases get the same console, JSON and JUnit reporting as contracts:

```sh
fai check -C packages --package json
fai test -C packages --package json
fai test -C packages --package web
fai test -C packages --package logging --match 'logging backpressure' --junit target/logging.xml
fai test -C packages --package llm --list
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

JSON example values and web loopbacks are checked by their named suites; the
Python harnesses retain JIT/AOT output comparisons and the independent JSON oracle.
`scripts/tests/filesystem_toolchain.py` runs the filesystem fixture's named cases
and retains the independent native-filename probe and relocated JIT/AOT checks.
Filesystem and process interoperability reports are written below
`$RUNNER_TEMP/fai-junit` in CI. Their temporary projects supply environment/OS
inputs without introducing implicit requirements into ordinary package tests.

Activate a cached compiler once in the shell when working from a checkout:

```sh
export PATH="$(dirname "$(python3 scripts/compiler.py ensure --profile ci)"):$PATH"
```

PowerShell:

```powershell
$compiler = python scripts/compiler.py ensure --profile ci
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

Changes confined to integration metadata select the owning package's integrations,
without rerunning its contracts or its consumers. Package-local Fai tests and
executable entrypoints select that package's checks and integrations; library
source edits also select affected consumers. Package documentation alone needs
no compiler. Changes to the shared runner, classifier or workflow still select
the full infrastructure/compiler lane. Adding another ordinary Fai integration
therefore requires no workflow edit.

## Cached compilers for source packages

Public compiler distribution uses the native launcher and optimized `release`
archives described in [INSTALLATION.md](INSTALLATION.md) and
[RELEASING.md](RELEASING.md). The content-addressed development bundles below
retain their assertion-enabled profile and are not public release artifacts.

`scripts/compiler.py` manages an immutable, content-addressed compiler cache.
It requires Python 3.11+. A cache hit needs no Rust toolchain:

```sh
python3 scripts/compiler.py ensure --profile ci
python3 scripts/compiler.py ensure --profile ci --offline --no-build
python3 scripts/compiler.py key --profile ci
```

The default cache is `$XDG_CACHE_HOME/fai/compilers` (falling back to
`~/.cache/fai/compilers`), or `%LOCALAPPDATA%/fai/compilers` on Windows.
`FAI_COMPILER_CACHE` and `--cache-dir` override it. `ensure` prints the compiler
path on stdout; progress goes to stderr. It first checks the local exact-key
bundle, then looks for a matching successful GitHub CI artifact with `gh`, then
builds `fai-cli` if necessary. `--offline` skips remote lookup; `--no-build`
reports a missing bundle instead of invoking Cargo.

CI publishes the optimized, assertion-enabled `ci` compiler already produced by
its Rust test build. Full compiler jobs package that binary once and reuse it for
native package checks and relocation fixtures; they do not build a second
`package-dev` compiler. Package-only jobs restore the exact `ci` bundle or build
only `fai-cli` with that profile on a miss. A full job missing its prepared bundle
fails instead of silently rebuilding or substituting another artifact.

`--profile package-dev` retains the more highly optimized local-development
profile and remains the script's default for compatibility. Both profiles keep
assertions and runtime leak checks, but have distinct cache keys and provenance;
an archive from one cannot be restored as the other. Use the same explicit profile
with `key`, `ensure`, `pack`, and `restore`. The cache key covers the named inputs in `scripts/compiler-inputs.json`,
the native target and host compatibility, build environment and bundle schema.
It is independent of branch names, commits, timestamps and checkout paths.
Package sources and documentation under `packages/`, and the Python presentation
under `scripts/performance_report/`, are outside those inputs. New compiler files,
Cargo configuration, dependency changes and embedded `std/`
sources invalidate the key. The compiler reports its source digest and complete
executable identity through `fai build-info`.

Bundles are checksum-verified ZIPs containing a manifest and the executable:

```sh
python3 scripts/compiler.py pack --profile ci --output compiler.zip
python3 scripts/compiler.py restore --profile ci --archive compiler.zip
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

Rust test jobs use Cargo's `ci` profile (`--cargo-profile ci` for nextest):
optimization level 1 and reduced debug information, with debug assertions and
overflow checks explicitly enabled. Runtime counters and native leak guards
remain enabled. The nextest `unit`, `cross-os`, and optional `proptest` filters
are unchanged. Build and test execution are separate workflow steps so their
times remain visible; doctests, test-partition validation, and the relocation
launcher reuse the same Cargo profile. The task-migration regression still runs
separately with `--release` to check the fully optimized runtime.

The build steps run `scripts/compiler.py build-tests --test-profile unit` (or
`cross-os`), which invokes the same nextest compilation command and records its
`fai` executable as a `ci` bundle. The source identity is checked before and after
compilation; native target, profile and enabled assertions are verified before
installation. Later nextest execution uses the same build environment, avoiding a
profile/provenance change that would invalidate the compiler's Cargo artifacts.

Cross-platform Rust builds derive Cargo package and target selectors from the
existing `cross-os` filter in `.config/nextest.toml` and `cargo metadata`.
Selected packages retain their test targets; named integration binaries select
their owning packages. Excluded property-test binaries need not be compiled.
Cargo's target-name selectors are shared across selected packages, so same-named
targets and required support binaries may still build, but the unchanged nextest
filter decides which tests run. Unknown selector syntax or missing targets fails
closed rather than silently dropping coverage.

`python scripts/cross_os.py plan` shows the derived Cargo arguments;
`python scripts/cross_os.py run` runs that scope with the existing `ci` Cargo and
`cross-os` nextest profiles. Linux retains its full workspace build and runs
`python scripts/cross_os.py verify`: it lists tests under both the full build and
the narrowed selectors, comparing exact binary/test identities, not just counts.
Any missing or additional selected test fails the required Linux job. Platform
jobs continue to compile and execute their own platform-specific test cases.

Required Rust jobs and cold compiler builds share `.github/actions/rust-cache`.
Its `fai-rust-v1` namespace does not restore the historical empty dependency
entries. Only jobs that actually run Rust invoke the action; its normal success
hook saves dependency artifacts. Compiler bundle hits still need no Rust cache.
Job summaries report compiled rlib/rmeta counts and bytes after restoration and
building, distinct from the cache service's key-hit status. An empty exact hit
warns rather than blocking Cargo's normal rebuild. Dependency artifacts remain
disposable acceleration, never evidence that tests passed.

The workflow always starts and classifies the diff against the actual PR base
(or the previous commit for a push). A stacked PR is compared with its immediate
predecessor, not unconditionally with `main`.

| Changes | Work performed |
|---|---|
| Package library source, e.g. `packages/json/src/**` | Package and affected consumers' formatting, type checks, contracts, and integrations |
| Package test source or executable entrypoint | Owning package's checks and integrations, not production consumers |
| Package-local `ci.json` integration metadata | Owning package's integrations, without contract checks or downstream tests |
| Package/root README, `AGENTS.md`, `llms.txt`/`llm.txt`, or Markdown/text under `docs/` | Lightweight change/catalog validation; no compiler or Rust build |
| `packages/fai.toml` or `packages/fai.lock` | Affected members, override consumers and changed lock-graph roots plus dependents; whole-workspace locked validation; no Rust suite on a cache hit |
| `scripts/performance_report/**` | Python syntax and two presentation fixtures; no Cargo or compiler bundle |
| `editors/vscode/**` only | TypeScript checks, grammar tests and VSIX packaging; no Rust or compiler bundle |
| Explicit Python-tooling files | Selected unit tests; release tooling also runs cached-compiler archive/relocation checks, without the Rust workspace suite |
| Compiler, runtime, embedded `std/`, build/CI infrastructure, or unknown paths | Full Rust checks and all packages; compiler execution fixtures and VS Code checks |
| Root documentation and `docs/*.md` / `docs/*.txt` only | Change/catalog validation |

Dependency impact uses both the base and head catalogs, so deleting or changing
an edge does not hide an affected dependent. Normal dependencies propagate library
changes transitively. A development dependency affects its consuming root's tests
and integrations, but does not invalidate that root's production consumers.
Renames include both old and new paths. Source ownership comes from each manifest's
`source-dir`, `test-dir`, executable entries and platform source roots. The deepest
matching root wins, with production retained on ties or uncertainty. Legacy
metadata and unrecognized package files retain conservative consumer checks.
New source-package directories must include `fai.toml`; malformed catalogs and
missing dependencies fail classification. An unavailable comparison base selects
the full lane conservatively.

Docs-only diffs retain the four required check names but do not restore a compiler
or run Cargo. Documentation mixed with code never suppresses the code's checks.
Integration-only diffs still prepare the compiler and validate all locked graphs.
The selector publishes separate integration/package-check lists; required jobs
reject missing or inconsistent lists instead of silently skipping checks.
Mixed package/presentation changes run both relevant lanes. Changes to the Rust
report-data exporter or CI infrastructure retain the full compiler checks.

VS Code changes use `.github/actions/vscode` once under the existing required
`fmt + clippy` check. It installs the committed npm lock with Node 24, runs
`npm run check-types`, `npm test`, and `npm run package`, then uploads
`fai-vscode-extension` containing the VSIX. Packaged extension documentation also
uses this lane. Root documentation-only changes still run neither Node nor Rust.
Mixed editor/package/reporting changes run all relevant checks without promoting
an otherwise isolated change to a Rust build. Full compiler checks include the
editor lane because grammar tests read the source samples. The manual Editors
workflow reuses the same action rather than keeping a separate check sequence.
For a VS Code-only or docs-only diff, the otherwise-idle platform-check wrappers run on Ubuntu,
retaining their required names without waiting for native macOS/Windows capacity.
Mixed changes that need platform checks retain their usual native runners.

Python-tooling selection is an exact file allowlist in `scripts/ci_tooling.py`,
not a blanket `scripts/` exemption:

| Group | Files and checks |
|---|---|
| `release` | `scripts/release.py`, the two `scripts/install/install.*` templates, their unit tests and the PowerShell archive harness; release/installer unit tests and native archive/relocation smoke checks |
| `performance` | `scripts/performance.py`, `scripts/performance_publish.py`, and `scripts/tests/test_performance.py`; publication/orchestration unit tests plus existing presentation fixtures |
| `junit` | `scripts/junit-summary.py` and `scripts/tests/test_junit_summary.py`; valid, missing and malformed JUnit fixtures |

The release lane checks both Windows PowerShell and PowerShell 7 on Windows and
shell syntax on Unix. Its native fixture uses the existing `ci` compiler: release
archive creation preserves its exact bytes and original build metadata, then the
relocated compiler runs contracts, JIT, AOT and daemon checks with empty Rust
homes. It never relabels a CI binary as an official release. Actual publication
still requires the complete release qualification workflow. On a compiler-cache
miss, only the usual CLI bootstrap may build; no Rust workspace test suite is
selected by these tooling edits.

Missing test files, unknown groups and inconsistent selector outputs fail closed.
Compiler/bootstrap scripts, the shared tooling runner, source-selection code,
release target definitions and other unknown paths keep full validation. Mixed
tooling/package/editor/presentation changes retain every relevant check. Run a
tooling group locally with `python scripts/ci_tooling.py --groups '["junit"]'`.

Workspace metadata is compared structurally against the same base. Literal member
additions/removals select their packages. Changed in-tree overrides select the
replaced package and its consumers; member-scoped overrides select their named
roots. Each root/platform lock graph is compared together with all its referenced
source descriptors, so a shared or external dependency update selects every
affected root. Selection then includes base/head transitive dependents. Adding an
isolated package, its local override, and its platform graphs therefore tests only
that package, not its unchanged dependencies or unrelated libraries.

Member/record reordering, comments, and the lock's informational `resolved-with`
field do not select package tests. Global settings such as compiler pins or lock
targets, unscoped external overrides, nonliteral/unknown paths, unsupported
metadata and metadata creation/removal conservatively select all packages.
Malformed compared structures fail selection. Compiler-owned validation remains
authoritative even for metadata-only diffs with no selected tests. Selection
reasons appear in the workflow summary. Mixed changes to Rust, embedded std,
CI scripts or workflows still select the full compiler lane.

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
fai test --locked -C packages --package PACKAGE --seed 42 --count 128 --junit /absolute/path/PACKAGE.xml
fai check --no-examples --locked -C packages --package PACKAGE
```

Contract tests run before the separate check so compilation failures also reach
the JUnit report. CI writes per-package reports beneath the job's temporary
`fai-junit` directory, outside restored caches. An `always()` summary/upload step
preserves available XML on test failures. `scripts/junit-summary.py` accepts a
report file or a directory, aggregates package reports and uses suite duration
when per-case timings are unavailable.

Linux, macOS, and Windows each restore an exact compiler bundle and put its
directory on PATH. On a valid cache hit there are no Cargo, rustup, nextest,
Rust tests, Clippy, or compiler-fixture invocations. A miss first tries a published
matching `ci` bundle, then builds only `fai-cli`. Source/package tests still run on a
miss; it does not trigger the full Rust suite.

Compiler/default-branch runs (and successful cold-cache package runs) publish `compiler.zip`
artifacts named by the compiler fingerprint, with 30-day retention. The local
bootstrap and later stacked PRs can reuse them. Exact Actions caches provide the
faster common path; there is no loose restore-key fallback to an older compiler.
Workflow summaries record the selected lane, affected packages, compiler key,
target, profile, and whether the bundle came from the test build, cache, an artifact,
or a new standalone build.
