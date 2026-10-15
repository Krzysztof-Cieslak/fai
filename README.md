# Fai

A small, strict, pure, statically typed functional language in the ML / F# / Elm
family, with a native compiler written in Rust.

Fai is designed for **fast, reliable development by humans and AI agents**. Its
type system, explicit effects, and built-in contracts make code easier to reason
about and help catch mistakes in generated code. An incremental compiler and
structured, integrated tooling give agents fast, precise feedback for tight
edit→check→test loops.

Efficient execution is part of that design, too. Native compilation and
predictable, automatic memory management with reuse help generated programs
perform well without hand-managed lifetimes. The aim is a language that is
straightforward to generate, quick to iterate on, and practical to run.

> [!IMPORTANT]
> **Early release:** language and library APIs may change. Use a
> [project toolchain pin](docs/INSTALLATION.md#updates-and-side-by-side-compilers)
> to keep a chosen compiler version, and consult the [release notes](changelog.md)
> when upgrading.

## Install

Linux and macOS:

```sh
curl -fsSL https://github.com/Krzysztof-Cieslak/fai/releases/latest/download/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://github.com/Krzysztof-Cieslak/fai/releases/latest/download/install.ps1 | iex
```

Prebuilt releases support Linux/glibc, macOS, and Windows on x86-64 and ARM64.
Installation is per-user and does not require Rust. On Linux and macOS, open a
new shell after installation so `fai` is on your PATH. See the
[installation guide](docs/INSTALLATION.md) for OS baselines and prerequisites;
building standalone native executables also requires the platform's linker and
development tools.

Use `fai upgrade` for the compiler, `fai installer upgrade` for the launcher,
and `fai update` for project dependencies.

## Hello, Fai

Save this as `Hello.fai`:

```fai
module Hello

public main : Runtime -> Unit / { Console }
let main runtime = runtime.console.writeLine "Hello, Fai!"
```

Run it:

```sh
fai run Hello.fai
```

The program prints `Hello, Fai!`. No project manifest is needed for a standalone
file. To start a package instead, use `fai init hello`.

Intent can live next to the code as executable examples and properties. Save
this as `Collections.fai`:

```fai
module Collections

/// Apply f to every element.
public map : ('a -> 'b / 'e) -> List 'a -> List 'b / 'e
let map f xs =
  match xs with
  | [] -> []
  | x :: rest -> f x :: map f rest
example: map (fun x -> x + 1) [1, 2, 3] = [2, 3, 4]
forall xs: map (fun x -> x) xs = xs
```

```sh
fai check Collections.fai
fai test Collections.fai
```

`check` type-checks the file and evaluates closed examples. `test` also checks
properties against generated inputs, shrinking failing cases to counterexamples.
Passing a property test is evidence, not a proof for every possible input.

Explore the [language examples](samples/) for unions, records, interfaces,
concurrency, and more.

## Design ideas

### Familiar syntax, explicit boundaries

Fai draws its surface from the ML tradition and takes much of its shape from F#:
significant indentation, `let` bindings, `match` expressions, F#-style type
variables (`'a`), pipelines (`|>`), and operator precedence derived from an
operator's symbols. `fai fmt` gives code a single canonical layout.

Type inference keeps local code concise. Discriminated unions and structural
records model data, and exhaustive `match` checking catches missing cases before
execution. Every exported value requires an explicit type signature, so callers
can work from a module's interface without reading its implementation.

### Capabilities and effects you can see

Fai's row-based effect system is inspired by [Koka](https://koka-lang.github.io/).
The clock, randomness, environment, file system, console, and network are explicit
capabilities, passed from `main` through ordinary values. A function can request
only the fields it needs using an open record type.

A function's signature also describes the effects a call may perform, using an
**effect row** written after `/`. Effects are inferred and must be accounted for
in public signatures; a function type without an effect annotation is pure.
Effect bounds can be conservative through abstractions. Holding a capability is
pure; invoking its methods incurs effects.

```fai
module Capabilities

public save : { console : Console, fs : FileSystem | _ } -> Path -> String -> Unit / { Console, FileSystem }
let save env path note =
  match env.fs.writeFile path note with
  | Err error -> env.console.writeLine (FileSystem.errorToString error)
  | Ok unit -> env.console.writeLine ("saved: " ++ note)

public main : Runtime -> Unit / { Console, FileSystem }
let main runtime =
  match Path.fromString "fai-note.txt" with
  | Err error -> runtime.console.writeLine (Path.errorToString error)
  | Ok path -> save runtime path "hello"
```

`save` accepts a larger runtime but uses only the console and file system. Effects
also travel with captured capabilities and through higher-order functions:
mapping a console-using function incurs `Console` too.

### Executable contracts and structured feedback

`example` and `forall` are real, type-checked declarations, not specially formatted
comments. They keep executable cases and laws beside the functions they describe;
`///` doc comments remain human prose. Dedicated tests can use named `Test.Suite`
cases and properties, with scoped fixtures and reproducible generator settings.

Diagnostics carry stable error codes and source locations, with versioned JSON
output for tools. The command line and language server share the same
code-intelligence engine for types, definitions, references, and other queries.

### Incremental native compilation and automatic memory management

The compiler uses a demand-driven [Salsa](https://github.com/salsa-rs/salsa) query
engine, with definition-level incremental work, parallel compilation, and an
on-disk object cache. A per-workspace daemon retains the warm database across
commands. Stable exported signatures help keep implementation edits local, and
comment-only edits can reuse semantic results.

Native code comes from [Cranelift](https://cranelift.dev/): ahead-of-time for
standalone executables, and just-in-time for `run` and `test` without a link step.
Automatic memory management uses
[Perceus-style reference counting and reuse analysis](https://www.microsoft.com/en-us/research/wp-content/uploads/2020/11/perceus-tr-v1.pdf).
Last-use analysis releases values when they are no longer needed, and reuse
analysis updates uniquely owned data in place. Operations such as mapping a
unique list or updating a record can reuse existing storage while preserving
immutable value semantics. Sequence fusion removes intermediate collections,
further reducing allocations—all without manual frees or a tracing collector.

## Performance

Performance is a core design goal: fast edit→check and edit→test loops, efficient
native execution, and economical memory use. We measure these across arithmetic,
collections, strings, records, and higher-order functions.

Our [benchmark suite](docs/BENCHMARK.md) compares native, ahead-of-time Fai programs
with Rust and OCaml. Warm execution, whole-process time, and memory footprint are
measured separately, with explicit workload definitions and correctness checks.

The [performance reports](docs/PERFORMANCE_REPORTS.md) retain per-workload results,
source revisions, compiler configurations, hardware, and raw evidence so the
measurements can be examined in context.

## Libraries and examples

The standard library is written in Fai over native runtime primitives. The
auto-imported `Prelude` provides `Option` and `Result` with their constructors,
common functions, and operator and capability interfaces. It also re-exports
opaque collection types such as `Dict`, `Set`, `HashDict`, and `HashSet`; their
constructors remain hidden. Operations are qualified: `List.map`, `Array.map`,
`Dict.insert`, `String.split`, and `Int.toString`.

Beyond collections, the standard library includes streams, structured concurrency,
file I/O, networking, HTTP/TLS, and date/time types. Optional
[source packages](packages/) add application libraries:

- [JSON codecs](packages/json/) and [web servers](packages/web/).
- [HTTP clients](packages/http-client/) and [server-sent events](packages/sse/).
- [SQLite](packages/sqlite/) and [PostgreSQL](packages/postgres/).
- [Command-line parsing](packages/cli/) and [terminal interfaces](packages/tui/).
- [LLM clients and a coding-assistant example](packages/llm/README.md#coding-assistant).

Packages are declared in `fai.toml` and imported explicitly; they are not embedded
in the compiler. The initial package releases require Fai 0.1.x. See the
[package guide](docs/PACKAGES.md) to add dependencies and each package's README
for its installation command and changelog.

## Tooling

| Command | Purpose |
| --- | --- |
| `fai init <name>` | Create a package. |
| `fai run [path]` | Compile and run via JIT. |
| `fai build [path]` | Compile to a standalone native executable. |
| `fai check [path]` | Type-check and evaluate closed examples; `--no-examples` for type-checking only. |
| `fai test [path]` | Run inline contracts and named test suites, including properties. |
| `fai fmt [path]` | Apply the canonical format; `--check` to verify without editing. |
| `fai lsp` | Start the language server. |
| `fai query <q>` | Read-only code intelligence: definitions, references, types, APIs, and more. |
| `fai daemon <cmd>` | Manage the per-workspace daemon. |

Use `--message-format=json` for structured diagnostics and command results;
program output remains program output. See the [CLI reference](docs/CLI.md) for
flags, schemas, and the daemon protocol.

A Visual Studio Code extension lives in [`editors/vscode/`](editors/vscode/): a
thin client for the `fai lsp` language server plus syntax highlighting.

## Documentation

- [`llms.txt`](llms.txt) — a single, self-contained guide to **writing Fai programs**, aimed at AI agents (and humans new to the language). Point your agent here.
- [`samples/`](samples/) — the language by example.
- [`docs/INSTALLATION.md`](docs/INSTALLATION.md) — installation, upgrades, and compiler selection.
- [`docs/PACKAGES.md`](docs/PACKAGES.md) — projects, dependencies, and publishing packages.
- [`docs/CLI.md`](docs/CLI.md) — the command-line and daemon reference.
- [`docs/ERROR_CODES.md`](docs/ERROR_CODES.md) — the catalog of `FAInnnn` diagnostics.

## Contributing

Bug reports and small reproductions are welcome in the
[issue tracker](https://github.com/Krzysztof-Cieslak/fai/issues).
For building from source and contributing, start with [AGENTS.md](AGENTS.md) and
the [development guide](docs/DEVELOPMENT.md). Design decisions live in
[docs/MEMORY.md](docs/MEMORY.md). Please follow the
[code of conduct](CODE_OF_CONDUCT.md); licensing is in [LICENSE](LICENSE).
