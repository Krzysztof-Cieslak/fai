# Development workflows

## Daily package development

Use the compiler directly. Package tests are ordinary Fai `example` and `forall`
declarations, including those in each package's `test/` directory:

```sh
fai check -C packages json
fai test -C packages json
fai test -C packages web
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
