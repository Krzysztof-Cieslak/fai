# Fai

A small, strict, pure, statically typed functional language in the ML / F# / Elm
family, with a native compiler written in Rust.

Fai is built around one idea: **code should be easy to reason about and verify**
— for humans and for AI agents alike. Every design choice serves that goal, from
effects you can see in a type to specifications the compiler checks for you.

## Hello, Fai

```fai
module Hello

public main : Runtime -> Unit
let main runtime = runtime.console.writeLine "Hello, Fai!"
```

Intent lives next to the code and is checked, not just written down:

```fai
module Collections

/// Apply f to every element.
public map : ('a -> 'b) -> List 'a -> List 'b
let map f xs =
  match xs with
  | [] -> []
  | x :: rest -> f x :: map f rest
example: map (fun x -> x + 1) [1, 2, 3] = [2, 3, 4]
forall xs: map (fun x -> x) xs = xs
```

The `samples/` directory is the full tour of the language, each file a
self-contained module verified by the test suite.

## Design ideas

### Familiar ML syntax, inspired by F#

Fai draws its surface from the ML tradition and takes much of its shape from F#:
significant indentation, `let` bindings, `match` expressions, F#-style type
variables (`'a`), pipelines (`|>`), and operator precedence derived from an
operator's symbols. The grammar is small and regular, so there is essentially one
correct way to write a given program — and little to learn before reading it.

### A strong type system that models your data

Fai has a strong, ML-style type system with full inference, so most types go
unwritten yet every value is checked. You describe a domain with algebraic data
types — discriminated unions and records — and the compiler holds you to it:
`match` is checked for exhaustiveness, so a case you forgot is a compile error,
not a crash at runtime. Illegal states become unrepresentable, and the type
checker turns whole classes of bugs into feedback you see before you run.

### Built from the ground up for tight feedback loops

Responsiveness is not a tuning pass bolted on at the end — it is the architecture.
Fast feedback matters more than peak throughput, especially for an agent
iterating in a loop, so the whole compiler is demand-driven and incremental at
the granularity of a single definition: edit one function and only what genuinely
depends on it is rechecked, while a reformat or a comment change costs nothing
downstream. Independent definitions and modules are compiled in parallel, and
results are cached on disk and reused across runs. The result is edit→error and
edit→test loops that stay near-instant — and stay that way as a project grows,
because a localized change does a localized amount of work no matter how large
the codebase is. Under the hood the compiler is a demand-driven query engine
built on [Salsa](https://github.com/salsa-rs/salsa).

### Effects are visible, not ambient

Fai is pure: nothing happens behind your back. The clock, randomness, the
environment, the file system, and the console are not global facilities — they
are ordinary values passed in from `main` as a `Runtime`. A function's type
therefore tells you exactly what it can reach, and a function asks for only the
capabilities it needs, so it is handed no more authority than that.

```fai
module Capabilities

public save : { console : Console, fs : FileSystem | _ } -> String -> String -> Unit
let save env path note =
  match env.fs.writeFile path note with
  | Err message -> env.console.writeLine message
  | Ok unit -> env.console.writeLine ("saved: " ++ note)

public main : Runtime -> Unit
let main runtime = save runtime "/tmp/fai-note.txt" "hello"
```

`save` requests a console and a file system and nothing else; it accepts any
larger runtime, but can never touch the clock or the network. Side effects are
honest and auditable, and programs are deterministic by default.

### Intent that the compiler checks

Comments describe intent to humans and quietly drift out of date. Fai lets you
state facts and laws as first-class declarations — `example` for concrete cases
and `forall` for properties — that live beside the code, are type-checked, and
are run as property tests with shrinking by `fai test`. The intent is proved or
refuted, not merely documented. `///` doc comments remain human prose;
everything inside an `example` or `forall` is real, checked code.

### Boundaries you can trust

Types are inferred inside a module, but every `public` value carries an explicit
signature on its own line. You can understand and depend on a module from its
exported signatures alone, without reading a single body. The same boundary keeps
work local: changing a private implementation cannot ripple out to the modules
that use it.

### Memory managed for you, predictably

No manual frees, and no garbage-collector pauses. Because the language is pure
and strict, the compiler knows exactly when each value is used for the last time
and releases it there. When a value isn't shared, the compiler reuses its memory
in place — so transforming data you already own (mapping a list, updating a
record) can run without allocating at all. You get automatic memory management
with predictable, pause-free performance. Under the hood we're using [Perceus-style
reference counting with reuse analysis](https://www.microsoft.com/en-us/research/wp-content/uploads/2020/11/perceus-tr-v1.pdf).

### Output meant to be read by tools

Every diagnostic has a stable error code and a precise source location, and the
whole output can be emitted as JSON. Parsing recovers from mistakes, so one error
never hides the rest. The same engine answers code-intelligence questions for
both the command line and the editor, so a human or an agent always gets precise,
parseable, consistent feedback.

### One way to write it

A single canonical format, enforced by `fai fmt` and idempotent, removes
stylistic choices entirely. Generated code and hand-written code converge on the
same shape — easier to review, to diff, and to produce correctly the first time.

## Standard library

The standard library is written in Fai itself. One module, `Prelude`, is
auto-imported and owns the core types (`Option`, `Result`, `Dict`, `Set` and
their constructors), common free functions (`identity`, `const`, `not`,
`compare`), and the shared interfaces for operators and capabilities. Everything
else is reached through qualified per-type modules — `List`, `Option`, `Result`,
`Dict`, `Set`, `String`, `Int`, `Float` — e.g. `List.map`, `Int.toString`.

The arithmetic, equality, and ordering operators are methods of shared
interfaces, and programs may declare their own symbolic operators with
F#-style precedence.

## Tooling

The `fai` binary is a thin client backed by a warm per-workspace daemon that
keeps the incremental database hot, so repeated commands are near-instant.
Native code is generated with [Cranelift](https://cranelift.dev/) — ahead-of-time
(AOT) for `fai build`, and just-in-time (JIT) for the fast `fai run` and
`fai test` loops.

| Command            | Purpose                                                                                                           |
| ------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `fai build <path>` | Compile to a native executable.                                                                                   |
| `fai run <path>`   | Compile and run with the lowest edit→run latency.                                                                 |
| `fai check [path]` | Type-check only — the fast inner-loop command.                                                                    |
| `fai fmt [path]`   | Apply the one canonical format (idempotent; `--check` to verify).                                                 |
| `fai test [path]`  | Run the `example` / `forall` contracts.                                                                           |
| `fai lsp`          | Start the language server for editors.                                                                            |
| `fai query <q>`    | Read-only code intelligence (definitions, references, types, module APIs, capability footprints, type search, …). |
| `fai daemon <cmd>` | Manage the per-workspace daemon.                                                                                  |

Add `--message-format=json` to any command for structured, versioned output.
The full reference, JSON schemas, and daemon protocol are in
[`docs/CLI.md`](docs/CLI.md).

## Documentation

- [`samples/`](samples/) — the language by example, the source of truth for the syntax.
- [`docs/CLI.md`](docs/CLI.md) — the command-line and daemon reference.
- [`docs/ERROR_CODES.md`](docs/ERROR_CODES.md) — the catalog of `FAInnnn` diagnostics.
- [`docs/MEMORY.md`](docs/MEMORY.md) — the design memory: locked decisions and standing risks.
- [`AGENTS.md`](AGENTS.md) — orientation for contributors, including building from source.
