# Fai — Language by Example

The `.fai` files in this directory demonstrate the language's syntax and standard
library. Compiler tests check parsing, canonical formatting, and type checking;
execution and contract suites cover runnable examples and properties. Multi-file
applications and package examples have their own integration tests.

From the repository root, start with:

```sh
fai run samples/Hello.fai
fai check samples/Capabilities.fai
fai test samples/Properties.fai
```

Each file declares a module. Many are standalone; examples that refer to another
file need their project context. Operations use qualified names such as
`Float.sqrt` and `Int.toString`. Core types and capability interfaces, including
`Console` and `Runtime`, are available through the auto-imported `Prelude`.

## Conventions

- Indentation is significant (offside rule); canonical layout is 2 spaces, no
  tabs (pinned by `fai fmt`).
- `public` exports a binding; `internal` restricts it to the same package origin.
  Bindings without either modifier are private to their file.
- Every exported binding has an explicit signature on its own line above it.
- Type variables are F#-style: `'a`, `'k`, `'v`.
- Equality is `=`, inequality is `<>` (both structural).

## Canonical formatting (what `fai fmt` enforces)

- 2-space indentation, no tabs; one statement/branch per line.
- `match` arms align with the `match` keyword; each arm starts with `| `.
- Multi-line record/list elements use a leading-comma layout.
- A binding groups with the `example`/`forall` declarations directly beneath it
  (no blank line within the group); exactly one blank line separates groups; the
  file ends with a newline.
- `fmt` is idempotent: formatting already-formatted code is a no-op.

The canonical layout keeps generated and handwritten code consistent for review.

## Named test suites

[`NamedTests.fai`](NamedTests.fai) demonstrates pure named checks, an effectful
temporary-file scenario, and explicit skipping. Run it with `fai test
samples/NamedTests.fai`; use `--list` for discovery without running callbacks.

## Package imports

The [`package_imports/`](package_imports/README.md) example demonstrates explicit
external imports, public module re-exports, and shared nominal type identity.
Its dedicated compiler integration test supplies the package environments and
checks formatting, types, and contracts.

## `algorithms/` — runtime benchmark samples

The `algorithms/` subdirectory includes arithmetic, sorting, collections, strings,
and other runtime workloads. Dedicated compiler tests check formatting, types,
contracts, and results against reference implementations.

Cross-language performance reports compare native AOT executables, while the JIT
suite provides Fai regression coverage. See the
[benchmark guide](../docs/BENCHMARK.md) for the workload contracts and methodology,
and the [performance trackers](../docs/PERFORMANCE_REPORTS.md) for current results.
