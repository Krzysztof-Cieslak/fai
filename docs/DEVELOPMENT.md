# Development workflows

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
