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

Use the compiler directly. Package tests are ordinary Fai `example` and `forall`
declarations, including those in each package's `test/` directory:

```sh
fai check -C packages json
fai test -C packages json
fai test -C packages web
fai test -C packages sql
fai test -C packages sqlite
fai test -C packages json/test/CodecSpec.fai --match treeRoundTrip
fai fmt -C packages json
```

The stable `packages/` workspace root lets the daemon retain one warm database
for libraries and their dependencies. Omit `--no-daemon` for the local loop.
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
matching bundle. Package edits keep the same binary and warm daemon. There is
no Python test wrapper in this workflow.

Each package's `ci.json` declares its name and dependencies for repository CI.
It does not change `fai test` or define another test framework. The package tests
remain runnable wherever their Fai sources and dependencies are available.

Measured on the development Linux host with the assertion-enabled optimized
compiler: a cold JSON type check was 106 ms, a warm check 4 ms, and the warm
143-contract JSON suite 308 ms. These are observations, not timing gates.

## Cached compilers for source packages

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
Package sources and documentation under `packages/` are outside those inputs;
new compiler files, Cargo configuration, dependency changes and embedded `std/`
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
| Compiler, runtime, embedded `std/`, build/CI infrastructure, or unknown paths | Full Rust checks and all packages; compiler execution fixtures |
| Root documentation and `docs/*.md` / `docs/*.txt` only | Change/catalog validation |

Dependency impact uses both the base and head catalogs, so deleting or changing
an edge does not hide an affected dependent. Renames include both old and new
paths. New source-package directories must include `ci.json`; malformed catalogs
and missing dependencies fail classification. An unavailable comparison base
selects the full lane conservatively.

The four protected check names remain unchanged. They explicitly require a
successful classification, so a failed selector cannot pass through skipped
jobs. On package-only changes their source checks are direct commands:

```sh
fai fmt --check -C packages PACKAGE
fai check --no-examples -C packages PACKAGE
fai test -C packages PACKAGE --seed 42 --count 128
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
