# Fai — Agent & Contributor Guide

> **Status:** Pre-implementation. The design is locked (see the decision table
> below); the Rust workspace is **not scaffolded yet** (that is milestone **M0**
> in `Plan.md`). Commands described here define the *intended* interface and
> conventions; treat them as the contract we build toward.

This document is the orientation guide for anyone — human or AI agent — working
on the Fai compiler. Read it first. For the staged build plan see `Plan.md`; for
the language by example see `Samples.md`.

---

## 1. What is Fai

Fai is a small, **strict, pure, statically typed functional language** in the
ML / F# / Elm family, with a native, ahead-of-time compiler written in Rust.

It deliberately has **no object orientation and no .NET/runtime host**: no
classes, no inheritance, no namespaces. The single OO-flavored feature is the
**interface** — a named set of related function signatures — whose only
constructor is an **interface instance**, `{ InterfaceName with <methods> }`
(giving existential, dynamically dispatched values).

`Fai` is a working name; the file extension is `.fai`.

## 2. Design goals (why "AI-first")

Fai is designed so that **AI agents can generate, verify, and iterate on code
with high confidence**. Every design choice serves one of these goals:

1. **Verifiable boundaries.** Full type inference internally, but every
   `public` value carries an explicit signature. Agents can reason about a
   module from its exported signatures alone.
2. **Determinism & purity by default.** No ambient side effects. Clock, random,
   environment, file system, and network are **capabilities** — ordinary values
   threaded in from `main`. A function's reach is visible in its type.
3. **Machine-readable feedback.** The compiler emits structured JSON
   diagnostics (and an LSP) with stable error codes, so agents get precise,
   parseable error locations and fixes.
4. **Low-entropy syntax.** One canonical format (enforced by `fai fmt`) and a
   small, regular grammar, so there is essentially one correct way to write a
   given program.
5. **Intent that is checked.** `///` docs describe intent for humans, while
   first-class `example` and `forall` declarations state checked facts and laws
   that the compiler type-checks and the test runner verifies. Agents write
   intent; the toolchain proves or refutes it.
6. **Fast compiles.** Tight feedback loops matter more for agents than peak
   runtime speed. The architecture favors compile throughput (uniform value
   representation, no monomorphization by default, parallel/incremental builds).

## 3. Locked design decisions

These are settled. Changing one is a deliberate, documented event (update this
table **and** the decision log in `Plan.md`).

| Area | Decision |
|---|---|
| Family | Strict, **pure**, statically typed functional (ML/F#/Elm) |
| OOP | None, except **interfaces** (sets of function signatures); **interface instances** `{ Name with ... }` are the only constructor (→ existentials) |
| Modules | One top-level module per file; nesting allowed; **private by default**, `public` exports |
| Public API | Every `public` binding **requires an explicit type signature** (Haskell-style, on its own line above the definition) |
| Recursion | Module-level bindings are **mutually recursive** (no `rec` keyword) |
| Layout | **Indentation-significant** (offside rule); `fai fmt` pins exactly one canonical layout (2-space indent, no tabs) |
| Type variables | F#-style leading tick: `'a`, `'k 'v` |
| Equality | `=` (equal) / `<>` (not equal), structural; undefined on function-typed values |
| Arithmetic | `+ - * /` **overloaded over `Int`/`Float`** (F#-style); unconstrained numeric type **defaults to `Int`**; **no implicit `Int`/`Float` coercion** (use `intToFloat`/`floatToInt`) |
| Comments | `//` line, `(* ... *)` block, `///` doc |
| Misc syntax | `[1, 2, 3]` lists, `::` cons, `List 'a`; `\|>`, `>>`, `++`; `true`/`false`; `if/then/else`; 64-bit `Int`/`Float` |
| Algebraic types | Discriminated unions (`type T = \| A \| B 'a`) |
| Tuples | **Structural**; values `(a, b)`, type `'a * 'b` (`*` binds tighter than `->`) |
| Records | **Structural with row polymorphism**; no duplicate labels (lacks constraints); `{ x = 1.0, y = 2.0 }`; dot access; `{ r with ... }` update; field punning in patterns; `type Point = { ... }` is a **transparent alias**; **closed by default** `{ x : T }`, anonymous-open `{ x : T \| _ }`, named-open `{ x : T \| 'r }` (named only to thread the tail to the result); **patterns mirror this** — `{ ... }` closed (names all fields), `{ ... \| _ }` open (ignore rest; required for row-poly scrutinees); extension/restriction (incl. binding a pattern tail) deferred to v2 |
| Inference | Hindley–Milner + let-generalization + **rows / row unification / lacks constraints**; exhaustiveness checking for `match` |
| Generics | **Uniform boxed representation + dictionary passing** (no monomorphization by default) |
| Interfaces | Compiled to **dictionaries**; instances (`{ Name with ... }`) are existential values |
| Effects | **Capabilities as explicit values** (interface instances flowing from `main`); **row-polymorphic capability records give least authority**; type-level effect rows deferred to v2 |
| Contracts | **First-class `example` / `forall` declarations** (`example: e` / `forall xs: e`; peers of `let`/`type`), resolved in module scope, type-checked to `Bool`, run by `fai test`; `///` is human prose only |
| Backend | **Cranelift** native code generation |
| Memory | **Perceus-style reference counting** (pure + strict ⇒ acyclic heaps ⇒ no cycle collector); reuse analysis enables in-place updates incl. `{ r with ... }` |
| Representation | Uniform 64-bit boxed/immediate values; canonical record field layout; **offset-evidence passing** for polymorphic field access; dictionaries for interfaces/generics |
| Determinism | Clock / random / env / IO are reachable only via capabilities |
| Tooling | `fai build/run/check/fmt/test/lsp`; global `--message-format=json`; stable error codes `FAInnnn` |

## 4. Language at a glance

```fai
module Hello

public main : Runtime -> Unit
let main runtime =
  runtime.console.writeLine "Hello, Fai!"
```

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

See `Samples.md` for the full tour (ADTs, structural/row-polymorphic records,
interfaces + instances, capabilities, contracts, nested modules).

## 5. Repository layout

A single Cargo workspace. Each crate owns one compiler phase or tool. (Crates
appear as the milestones that need them land — see `Plan.md`.)

```
fai/
├── Agents.md            # this file
├── Plan.md              # milestones, acceptance criteria, risks, decisions
├── Samples.md           # language by example
├── Cargo.toml           # workspace manifest                         (M0)
├── crates/
│   ├── fai-cli/         # binary: build/run/check/fmt/test/lsp       (M0)
│   ├── fai-span/        # source files, byte spans, source maps      (M0)
│   ├── fai-diagnostics/ # diagnostic model + human & JSON renderers  (M0)
│   ├── fai-syntax/      # lexer, parser (recursive descent + Pratt), AST (M1)
│   ├── fai-fmt/         # canonical formatter (AST → pretty)         (M1)
│   ├── fai-resolve/     # module graph, name resolution, visibility  (M1/M2)
│   ├── fai-types/       # HM inference, rows, dictionaries, exhaustiveness (M2/M4)
│   ├── fai-core/        # typed, desugared Core IR                   (M3)
│   ├── fai-rc/          # Perceus dup/drop + reuse analysis on Core  (M3/M6)
│   ├── fai-codegen/     # Core IR → Cranelift IR → object files      (M3)
│   ├── fai-runtime/     # Rust static lib: RC, allocator, builtins, capability hosts (M3)
│   ├── fai-contracts/   # example/forall checking + generators       (M7)
│   ├── fai-driver/      # pipeline orchestration + caching           (M3)
│   └── fai-lsp/         # language server                            (M8)
└── tests/               # end-to-end & golden/snapshot tests         (M0)
```

## 6. Compiler pipeline

```
source (.fai)
  → lex                 (fai-syntax)   tokens + spans
  → parse               (fai-syntax)   AST, with error recovery
  → resolve             (fai-resolve)  modules, names, public/private, recursion SCCs
  → infer / typecheck   (fai-types)    HM + rows + lacks; require public sigs;
                                       exhaustiveness; insert dictionaries & field-offset evidence
  → desugar             (fai-core)     typed Core IR
  → check contracts     (fai-contracts) examples/properties (at `fai test`)
  → reference counting  (fai-rc)       insert dup/drop; reuse analysis (Perceus)
  → codegen             (fai-codegen)  Core IR → Cranelift IR → object files
  → link                (fai-driver)   object files + fai-runtime → native executable
```

`fai check` stops after typecheck. `fai fmt` needs only lex+parse. `fai test`
runs through contract checking. `fai build`/`run` run the whole pipeline.

## 7. Building, running, testing

**The compiler (this repo):**

```sh
cargo build                 # build all crates
cargo test                  # unit + golden/snapshot + e2e tests
cargo run -p fai-cli -- <args>
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

**Fai programs (the CLI we are building):**

```sh
fai build path/to/Main.fai        # → native executable
fai run   path/to/Main.fai        # build + run
fai check path/to/Main.fai        # typecheck only (fast)
fai fmt   [path]                  # canonical-format in place (idempotent)
fai test  [path]                  # run example/forall contracts
fai lsp                           # start language server (stdio)
# global: --message-format=json   # structured diagnostics for agents/tools
```

## 8. Rust coding conventions

- **Edition / toolchain:** Rust 2021+, pinned via `rust-toolchain.toml` (M0).
  Builds must be warning-clean under `clippy -D warnings`.
- **No `unsafe`** outside `fai-runtime` and `fai-codegen` memory primitives, and
  only with a `// SAFETY:` comment justifying each block.
- **Errors:** library crates return `Result` with typed errors; never `panic!`
  on user-reachable input. Reserve `panic!`/`unwrap` for compiler invariants
  that are genuinely impossible to violate, and prefer `debug_assert!`.
- **No `unwrap`/`expect` on user input paths.** A malformed program must produce
  a `Diagnostic`, never a crash.
- **IDs over pointers.** AST/IR nodes are stored in arenas (`Vec`) and referenced
  by newtyped indices (`ExprId`, `TypeId`, `Symbol`, …), not `Box`/`Rc` graphs.
- **Intern strings** (identifiers, labels) to `Symbol`; compare by id.
- **Spans everywhere.** Every AST/IR node and diagnostic carries a `Span`. Never
  discard source locations.
- **Determinism.** No `HashMap` iteration order in output; use `FxHashMap` for
  speed and `BTreeMap`/sorted vecs where ordering is observable.
- Public items get doc comments; modules start with a `//!` summary.

## 9. Performance guidelines

Compile throughput is a feature. When in doubt, measure (`cargo bench`, golden
timing tests on large inputs).

- Hand-written lexer and parser; avoid regex on the hot path.
- Single-pass where practical; reuse allocations; prefer `&str`/`Symbol` over
  `String` clones.
- `FxHashMap`/`FxHashSet` (rustc-hash) for internal maps.
- Parallelize across independent modules with `rayon` once the module graph
  exists (M8/M9).
- Keep the value representation uniform (no monomorphization) so codegen stays
  proportional to source size; opt-in monomorphization for hot paths is an M9
  optimization, never a correctness requirement.
- Incremental recompilation (salsa or equivalent) is introduced for the LSP in
  M8/M9 — design query boundaries with this in mind, but do not prematurely
  add it.

## 10. Diagnostics & error codes

- Every diagnostic has: a stable **code** (`FAInnnn`), a **severity**, a
  **primary span**, optional **secondary spans/labels**, a **help** message, and
  optional machine-applicable **suggestions** (span + replacement).
- Two renderers from one model: a human renderer (carets/labels, colors) and a
  **JSON** renderer behind `--message-format=json`. The JSON schema is stable
  and versioned; agents and the LSP consume it.
- **Error codes are an API.** Allocate codes by phase and document each in the
  error-code catalog (M8): `FAI1xxx` lex/parse, `FAI2xxx` resolve/visibility,
  `FAI3xxx` types/rows, `FAI4xxx` exhaustiveness/patterns, `FAI5xxx`
  capabilities, `FAI6xxx` contracts. Never renumber a shipped code.
- Parsing **recovers** and reports multiple errors per run; one mistake should
  not hide the rest.

## 11. Extending the compiler

| To change… | Edit… |
|---|---|
| Tokens / literals | `fai-syntax` lexer (note the `'a'` char vs `'a` type-var rule, §below) |
| Grammar / AST | `fai-syntax` parser + AST; update `fai-fmt`; add parser snapshot tests |
| Name resolution / visibility | `fai-resolve` |
| Types, rows, inference, dictionaries | `fai-types` |
| Desugaring / IR shape | `fai-core` |
| Reference counting / reuse | `fai-rc` |
| Native codegen | `fai-codegen` (Cranelift) |
| Runtime values / builtins / capabilities | `fai-runtime` |
| Contracts / generators | `fai-contracts` |
| A new diagnostic | `fai-diagnostics` (allocate a code, document it) |
| CLI subcommands / flags | `fai-cli` + `fai-driver` |

**Lexer subtlety to preserve:** a leading tick is a character literal when it
closes (`'a'`, `'\n'`) and a **type variable** otherwise (`'a`, `'r`). This is
the F# rule; keep it covered by tests.

**Reserved keywords include** `module`, `let`, `type`, `interface`, `match`,
`with`, `if`, `then`, `else`, `fun`, `public`, and the contract-declaration
keywords **`example`** and **`forall`**. Contracts are ordinary
declarations (peers of `let`), not comment text, so the symbols inside them
resolve through normal name resolution and are fully type-checked.

Every language-surface change must update **all three** docs and add tests
(parser snapshot, type golden, and/or e2e) in the same change.

## 12. Definition of Done / CI

A change is done when:

1. `cargo build` is clean and `cargo clippy --all-targets -- -D warnings` passes.
2. `cargo fmt --all -- --check` passes (Rust side).
3. `cargo test` passes, including golden/snapshot and e2e tests.
4. New behavior has tests; new diagnostics have codes + catalog entries.
5. Any surface-language change is reflected in `Agents.md`, `Plan.md`, and
   `Samples.md`.
6. Self-hosted check: every `.fai` example in `Samples.md` is verified by the
   test suite (parsed/checked, and run where applicable) so the docs cannot
   drift from the implementation.
