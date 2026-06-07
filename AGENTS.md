# Fai — Agent & Contributor Guide

> **Status:** Implemented through **reuse & in-place update** (M6). The compiler front end —
> lexer, layout, parser/AST, the incremental `parse`/`item_tree` queries, and the
> canonical formatter (M1) — plus name resolution, the module graph, and
> Hindley–Milner inference for the functional core (M2) are built. `fai check`
> type-checks, and the `fai query` code-intelligence commands work. The native
> backend (M3) is built: a typed Core IR (`fai-core`), reference counting
> (`fai-rc`), Cranelift code generation with both AOT and JIT (`fai-codegen`), and
> the runtime (`fai-runtime`) — so `fai build` produces a native executable and
> `fai run` executes it. The daemon layer (M3.5) is built: a per-workspace
> `fai-server` holds the warm query database and serves a thin CLI client over
> MessagePack JSON-RPC, backed by an on-disk content-addressed object cache;
> `--no-daemon` runs in-process. The data layer (M4) is built: **discriminated
> unions and transparent type aliases, `match` with exhaustiveness/redundancy
> checking, structural records with row polymorphism, a native `Float`, and
> structural ordering** — all compiling to native code (monomorphic records use
> constant-offset projections; *row-polymorphic* field access and `{ r with … }`
> update compile via **offset-evidence passing** — integer field offsets threaded
> in as leading arguments, like dictionaries). Interfaces & capabilities (M5) are
> built: **`interface` declarations and `{ Name with … }` instances** compile to
> dictionaries with type-directed method dispatch; the overloaded operators
> (`+ - * / %`, `= <>`, `< <= > >=`) are **methods of the std interfaces**
> `Num`/`Eq`/`Ord`, and programs may declare their own **symbolic operators**
> (F#-style precedence); and the host effects — **`Console`, `Clock`, `Random`,
> `FileSystem`, `Env`** — are **capabilities**: interface instances bundled in a
> `Runtime` record that `main` receives, with **row-polymorphic least authority**
> (a function requests `{ console : Console | _ }` and accepts any larger runtime),
> realized by that same offset-evidence passing. The
> standard library is a set of real compiled modules under **`std/`** (embedded
> at build time): an auto-imported `Prelude` (the core types `Option`/`Result`/
> `Dict`/`Set` with their constructors, the free functions
> `identity`/`const`/`not`/`compare`, the `Num`/`Eq`/`Ord` operator interfaces,
> and the capability interfaces with the `Runtime` bundle) and qualified operation
> modules (`List`, `Option`, `Result`, `Dict`, `Set`, `String`, `Int`, `Float` —
> e.g. `List.map`, `Int.toString`). The few Rust intrinsics are prelude-private,
> reached only as `Prim.*` inside `std/`. Reuse & in-place update (M6) are built:
> reference counting is **precise and ownership-based** (A-normal form, drop at
> last use, borrowing projections), a dead data cell is **reset and reused** in
> place for a same-size construction (so `map`/`filter` over a unique list
> allocate zero fresh cells, falling back to copying when shared), `{ r with … }`
 > updates a unique record in place, and **argument borrowing** lends
> inspect-only parameters at direct calls (with an owned-ABI wrapper for the
> first-class value form). Contracts (M7) are built: **`fai test` runs the
> first-class `example`/`forall` declarations**. The property-testing framework is
> **dogfooded in the standard library** (`std/Test.fai`: a pure splitmix64 `Gen`,
> an `Arbitrary 'a` bundle of generator/shrinker/renderer, type-directed
> combinators, and the `checkExample`/`checkForall` driver with shrinking); the
 > compiler synthesizes, per contract, a harness that composes those combinators
> for the binders' (monomorphized) types and JIT-runs it, decoding a `TestResult`
> and reporting a failure as a located **`FAI6001`** with a shrunk counterexample
> (an ungeneratable binder is **`FAI6002`**). User **records and ADTs** (including
> recursive ones like `Dict`/`Set`/`Tree`) generate too, via a synthesized
> top-level `Arbitrary` definition per type (a recursive type is a self-reference
> guarded by a size budget; every synthesized function is capture-free, closing
> over values by partial application). (Splitmix needs **bitwise `Int`
> intrinsics** — `Int.and/or/xor/complement/shiftLeft/shiftRight/shiftRightLogical`
> — and full-domain float generation needs `Float.fromBits`/`toBits`, both added
> as part of this work.) Later milestones (the LSP, …) define the *intended*
> interface we build toward. The design is locked (see the decision table below).

This document is the orientation guide for anyone — human or AI agent — working
on the Fai compiler. Read it first. For the staged build plan see `docs/PLAN.md`; for
the language by example see the `samples/` directory.

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
table **and** the decision log in `docs/PLAN.md`).

| Area | Decision |
|---|---|
| Family | Strict, **pure**, statically typed functional (ML/F#/Elm) |
| OOP | None, except **interfaces** (sets of function signatures); **interface instances** `{ Name with ... }` are the only constructor (→ existentials) |
| Modules | One top-level module per file; nesting allowed; **private by default**, `public` exports |
| Public API | Every `public` binding **requires an explicit type signature** (Haskell-style, on its own line above the definition) |
| Recursion | Module-level bindings are **mutually recursive** (no `rec` keyword) |
| Layout | **Indentation-significant** (offside rule); `fai fmt` pins exactly one canonical layout (2-space indent, no tabs) |
| Type variables | F#-style leading tick: `'a`, `'k 'v` |
| Equality | `=` (equal) / `<>` (not equal), structural; undefined on function-typed values (→ an `Eq` operator method at M5; see Operators) |
| Ordering | `< <= > >=` are **structural** over any non-function type (a runtime `compare`; constructor tags order by declaration, records by sorted label); undefined on functions. Generalizes like equality (→ an `Ord` operator method at M5; see Operators) |
| Arithmetic | `+ - * /` **overloaded over `Int`/`Float`** (F#-style); unconstrained numeric type **defaults to `Int`**; **no implicit `Int`/`Float` coercion** (use `Int.toFloat`/`Float.toInt`) (→ `Num` operator methods at M5; see Operators) |
| Operators | **Symbolic identifiers** with **F#-style precedence** (derived from the operator's symbols; no fixity declarations); written infix, named as `(op)`. Built-in operators are **std interface methods** — `Num` (`+ - * / %`), `Eq` (`= <>`), `Ord` (`< <= > >=`) — defined in `Prelude`; **user-defined operators** resolve like names (module-local + `Prelude`). `&&`/`\|\|` stay short-circuit sugar; `::` is the built-in `List` constructor. *(Built at M5.)* |
| Comments | `//` line, `(* ... *)` block, `///` doc |
| Misc syntax | `[1, 2, 3]` lists, `::` cons, `List 'a`; `\|>`, `>>`, `++`; `true`/`false`; `if/then/else`; 64-bit `Int`/`Float` |
| Algebraic types | Discriminated unions (`type T = \| A \| B 'a`); transparent type aliases (`type Id = …`, acyclic) |
| Tuples | **Structural**; values `(a, b)`, type `'a * 'b` (`*` binds tighter than `->`) |
| Records | **Structural with row polymorphism**; no duplicate labels (lacks constraints); `{ x = 1.0, y = 2.0 }`; dot access; `{ r with ... }` update; field punning in patterns; `type Point = { ... }` is a **transparent alias**; **closed by default** `{ x : T }`, anonymous-open `{ x : T \| _ }`, named-open `{ x : T \| 'r }` (named only to thread the tail to the result); **patterns mirror this** — `{ ... }` closed (names all fields), `{ ... \| _ }` open (ignore rest; required for row-poly scrutinees); extension/restriction (incl. binding a pattern tail) deferred to v2 |
| Inference | Hindley–Milner + let-generalization + **rows / row unification / lacks constraints**; exhaustiveness checking for `match` |
| Generics | **Uniform boxed representation + dictionary passing** (no monomorphization by default) |
| Interfaces | Compiled to **dictionaries**; instances (`{ Name with ... }`) are existential values |
| Effects | **Capabilities as explicit values** (interface instances flowing from `main`); **row-polymorphic capability records give least authority**; type-level effect rows deferred to v2 |
| Contracts | **First-class `example` / `forall` declarations** (`example: e` / `forall xs: e`; peers of `let`/`type`), resolved in module scope, type-checked to `Bool`, run by `fai test`; `///` is human prose only |
| Backend | **Cranelift** native code generation |
| Memory | **Perceus-style reference counting** (pure + strict ⇒ acyclic heaps ⇒ no cycle collector); reuse analysis enables in-place updates incl. `{ r with ... }` |
| Representation | Uniform 64-bit boxed/immediate values; canonical record field layout (sorted by label text); monomorphic field access is a **constant offset**; *row-polymorphic* field access and `{ r with … }` update use **offset-evidence passing** — per row lacks-constraint, an integer offset threaded in as a leading argument (like a dictionary), composing through call chains and baked into partial applications for first-class use; dictionaries for interfaces/generics |
| Determinism | Clock / random / env / IO are reachable only via capabilities |
| Standard library | Real compiled `.fai` modules under **`std/`**, embedded at build time. One **auto-imported** module, `Prelude`, owns the core types (`Option`/`Result`/`Dict`/`Set` + constructors) and the free functions `identity`/`const`/`not`/`compare`; all other operations are **qualified** under per-type modules (`List.map`, `Option.withDefault`, `Int.toString`, …). `Prelude`/`List`/`Option`/`Result`/`Dict`/`Set`/`String`/`Int`/`Float` are reserved module names. The few Rust **intrinsics** are prelude-private, reached only as `Prim.*` from inside `std/` (`FAI2014` elsewhere) and re-exported under clean names. (`Dict`/`Set` expose their node constructors until opaque types land.) |
| Compilation model | **Demand-driven (salsa) query engine**; per-workspace **daemon** holds the DB hot, thin CLI client; **content-addressed on-disk cache**; **JIT** for `run`/`test`, **AOT** for `build`; incremental at definition/SCC granularity |
| Tooling | `fai build/run/check/fmt/test/lsp` + read-only `fai query …` (code intelligence); per-workspace daemon (MessagePack JSON-RPC); global `--message-format=json`; stable error codes `FAInnnn`. Full reference: **`docs/CLI.md`** |

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

See the `samples/` directory for the full tour (ADTs, structural/row-polymorphic
records, interfaces + instances, capabilities, contracts, nested modules). Each
`.fai` file there is one self-contained module, verified by the test suite.

## 5. Repository layout

A single Cargo workspace. Each crate owns one compiler phase or tool. (Crates
appear as the milestones that need them land — see `docs/PLAN.md`.)

```
fai/
├── AGENTS.md            # this file
├── samples/             # language by example (canonical, tested .fai tour)
├── std/                 # standard library: real .fai modules, embedded at build time
├── docs/
│   ├── PLAN.md          # milestones, acceptance criteria, risks, decisions
│   └── CLI.md           # CLI + daemon-protocol reference
├── Cargo.toml           # workspace manifest + shared deps/lints      (M0)
├── Cargo.lock           # committed (reproducible builds)             (M0)
├── rust-toolchain.toml  # pinned toolchain (edition 2024)            (M0)
├── rustfmt.toml         # canonical Rust formatting                   (M0)
├── scripts/check.sh     # local mirror of the CI gates               (M0)
├── .github/workflows/   # CI: build, clippy -D, fmt --check, test     (M0)
├── crates/
│   ├── fai-cli/         # thin client binary `fai`: build/run/check/fmt/test/lsp + query (M0)
│   ├── fai-span/        # source ids, byte spans, line index, span resolver (M0)
│   ├── fai-diagnostics/ # diagnostic model + human & JSON renderers  (M0)
│   ├── fai-db/          # salsa database: inputs, interning, queries, durability (M0)
│   ├── fai-driver/      # command orchestration (cache + link land in M3) (M0)
│   ├── fai-tests/       # end-to-end & golden/snapshot tests + incremental verifier (M0)
│   ├── fai-syntax/      # lexer, parser (recursive descent + Pratt), item tree + AST (M1)
│   ├── fai-fmt/         # canonical formatter (AST → pretty)         (M1)
│   ├── fai-resolve/     # module graph, name resolution, visibility  (M1/M2)
│   ├── fai-types/       # HM inference, rows, dictionaries, exhaustiveness; embeds std/ (build.rs) (M2/M4)
│   ├── fai-ide/         # code intelligence (powers `fai query` + LSP) (M2/M8)
│   ├── fai-core/        # typed, desugared Core IR                   (M3)
│   ├── fai-rc/          # Perceus dup/drop + reuse analysis on Core  (M3/M6)
│   ├── fai-codegen/     # Core IR → Cranelift IR → objects (AOT) + JIT (M3)
│   ├── fai-runtime/     # Rust static lib: RC, allocator, builtins, capability hosts (M3)
│   ├── fai-contracts/   # example/forall checking + generators       (M7)
│   ├── fai-server/      # per-workspace daemon (MessagePack JSON-RPC) (M3+)
│   └── fai-lsp/         # language server (reuses fai-ide)           (M8)
```

Each phase crate (`fai-syntax`, `fai-resolve`, `fai-types`, `fai-core`,
`fai-rc`, `fai-codegen`) defines its phase as **salsa query groups** plugged into
`fai-db`; see §9.

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

This pipeline is **not** a batch of passes pushing data forward: each phase is a
set of **memoized salsa queries** that pull on demand (§9). `fai run`/`fai test`
take the **JIT** path (no link); `fai build` takes the **AOT** path.

## 7. Building, running, testing

**The compiler (this repo):**

```sh
cargo build                 # build all crates
cargo test                  # unit + golden/snapshot + e2e tests
cargo run -p fai-cli -- <args>
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
```

**Fai programs (the CLI we are building) — summary; full reference in `docs/CLI.md`:**

```sh
fai build path/to/Main.fai        # → native executable (AOT)
fai run   path/to/Main.fai        # build + run (JIT)
fai check [path]                  # typecheck only (fast, incremental)
fai fmt   [path]                  # canonical-format in place (idempotent)
fai test  [path]                  # run example/forall contracts (JIT)
fai lsp                           # start language server (stdio)
fai query <q> …                   # read-only code intelligence (refs/type/api/caps/…)
fai daemon <status|stop|…>        # manage the per-workspace daemon
# global: --message-format=json   # structured diagnostics/output for agents/tools
```

The CLI is a **thin client** to a per-workspace **daemon** that keeps the
incremental query database hot. `fai query` is a **read-only** introspection
surface for agents (definitions, usages, types, module APIs, capability
footprints, type search). See **`docs/CLI.md`** for every command, all flags, the JSON
output schemas, and the daemon (MessagePack JSON-RPC) protocol.

## 8. Rust coding conventions

- **Edition / toolchain:** Rust **edition 2024**, toolchain pinned (currently
  `1.96.0`) via `rust-toolchain.toml`; canonical Rust formatting pinned via
  `rustfmt.toml` (`use_small_heuristics = "Max"`). Lints are denied workspace-wide
  in `[workspace.lints]` (`warnings`, `unsafe_code`, `clippy::all`) and builds
  must also be clean under `clippy -D warnings`.
- **No hand-written `unsafe`** outside `fai-runtime` and `fai-codegen` memory
  primitives, and only with a `// SAFETY:` comment justifying each block. Crates
  that define salsa queries (`fai-db` and the phase crates such as `fai-syntax`)
  carry only salsa's macro-generated `unsafe` via a scoped `#![allow(unsafe_code)]`.
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
- **Comments and commits explain the code, not the process — this is a hard
  rule, enforced.** Code comments, doc comments, and Git commit messages (subject
  *and* body) must be self-contained and make sense to a reader who has never seen
  the roadmap. **Never** name a planning/process artifact:
  - **milestone names** — `M0`, `M2`, `M3`, `M3.5`, … (and never "this milestone");
  - **build-plan phases** — `Phase 2a`, "Phase 2.5", …;
  - **decision-log identifiers** — `Q7`, `D14`, `D45`, …;
  - **`docs/PLAN.md`** — write "noted as future work", not "see the plan".

  This holds **even when the change implements a milestone or a logged
  decision**: describe the behavior, not the roadmap step that produced it. So:
  - write "Add the native runtime", **not** "Implement the M3 runtime";
  - write "no reuse analysis yet", **not** "reuse is deferred to M6";
  - write "record the design decisions in the build plan", **not** "see D45–D55 in
    `docs/PLAN.md`".

  Pointers to the durable specs (`docs/CLI.md`, `AGENTS.md`) are fine when they
  document a real contract (e.g. a wire schema or a naming convention). A commit
  whose subject or body names a milestone, phase, or decision id **must be
  reworded before it merges** (reword local history with `git rebase`). Describe
  *what changed and why*, not the step in a roadmap that produced it.

## 9. Performance & incremental compilation

Compile throughput is *the* feature: the goal is sub-second edit→diagnostic and
edit→test loops for AI agents. Incrementality is **foundational, not deferred** —
the architecture is demand-driven from the front-end milestones. When in doubt,
measure (`cargo bench`, golden timing tests, and the tracked `edit→diagnostic` /
`edit→test` latency benchmarks).

**Query spine (salsa).** Every phase is a set of **memoized queries** (pure
`input → output`) in `fai-db`; the engine re-runs only what transitively changed.
Two properties carry the wins:

- **Early cutoff / firewalling** — if a re-run query yields the same result, its
  dependents don't re-run. A reformat or comment edit changes the file but not
  the parsed item → zero downstream work.
- **Definition/SCC granularity** — the cache unit is a definition (or an SCC of
  mutually-recursive defs), effectively per-function.

**Position independence (the enabler).** `parse` produces a stable **item tree**
keyed by position-independent `ItemId`s, with **spans in a side-table**. Semantic
queries depend on position-independent content; spans are resolved late, only for
diagnostics. Keep semantic query inputs free of absolute offsets, or
incrementality collapses (there's an edit-churn test that asserts "add a comment
→ near-zero recompute").

**Firewalls our design already gives us.** Required public signatures make a
module's `module_exports` cheap and stable, so editing a *private* body never
invalidates other modules, and public bodies typecheck independently (needing
only callees' signatures). Determinism makes content-addressing sound. The
uniform (non-monomorphized) representation means each generic is compiled once,
so a new call site elsewhere doesn't invalidate its code.

**Caching layers.** (1) in-memory salsa (hot, in the daemon); (2) on-disk
**content-addressed artifact cache** — `object_code(Def)` keyed by
`hash(rc(Def)) + target + compiler-version`, so cold runs reuse backend output;
(3) shared/remote cache later (portable by construction).

**Runtime topology.** A per-workspace **daemon** (`fai-server`) holds the live DB
and serves a thin CLI client over MessagePack JSON-RPC (see `docs/CLI.md`), with
request cancellation on input change and LRU eviction to bound memory. `fai-ide`
exposes code-intelligence queries to both the CLI (`fai query`) and the LSP.

**Execution.** `fai check` runs front-end queries only (no codegen/link); JIT
serves the `run`/`test` inner loop (no link); AOT (`fai build`) uses the object
cache plus a fast linker (mold/lld).

**Lower-level practices.**
- Hand-written lexer and parser; avoid regex on the hot path.
- Reuse allocations; prefer `&str`/`Symbol` over `String` clones; **intern**
  identifiers/labels/paths.
- `FxHashMap`/`FxHashSet` (rustc-hash) internally; deterministic ordering where
  observable.
- Parallelize across independent defs/modules with `rayon` (Cranelift codegen is
  embarrassingly parallel per function).
- Opt-in monomorphization for hot paths is an M9 optimization, never a
  correctness requirement — and the one feature that *hurts* incrementality, so
  it stays opt-in.
- An **incremental verifier** (compare incremental vs from-scratch) runs in CI;
  cache keys include compiler version + flags.

**Benchmarking.** Performance is guarded two ways:

- **Deterministic guards** (`crates/fai-tests/tests/perf_guards.rs`) assert
  incrementality properties via the query-execution **event log** (a count of
  which salsa queries re-ran), not wall-clock — so they gate in CI without
  flakiness. The headline guard: a localized edit's recompute is **independent of
  workspace size** (the cross-module firewall).
- **Wall-clock benches** (`crates/fai-tests/benches/`, [divan]) are for local
  profiling: `cargo bench -p fai-tests`. `inference.rs` covers cold check vs warm
  incremental edits over a synthetic corpus; `micro.rs` covers the inference
  primitives (unification, instantiation, rendering, deep bodies, large SCCs);
  `stress.rs` covers pathological scenarios (exponential type growth, wide/deep
  structures, instantiation- and constraint-heavy bodies, wide modules, deep
  dependency chains, contract- and error-heavy files). They are **not** a CI gate
  (shared runners are noisy); CI only compiles them (`build --all-targets`) to
  prevent bitrot. The deterministic [`corpus`](crates/fai-tests/src/corpus.rs)
  generator backs the corpus benches and the guards.

Known super-linear hot spots surfaced by the benches (M9 tuning targets, not
correctness issues): the **occurs check** re-walks the whole growing type per
binding (O(n²) on long application chains / exponential type growth);
**local-`let` generalization** recomputes environment free-variables per binding
(O(n²) in block size); and unification of very deep types repeats
`resolve_shallow` walks (wants union-find path compression).

## 10. Diagnostics & error codes

- Every diagnostic has: a stable **code** (`FAInnnn`), a **severity**, a
  **primary span**, optional **secondary spans/labels**, a **help** message, and
  optional machine-applicable **suggestions** (span + replacement).
- Two renderers from one model: a human renderer (carets/labels, colors) and a
  **JSON** renderer behind `--message-format=json`. The JSON schema is stable
  and versioned; agents and the LSP consume it.
- All structured CLI output — diagnostics **and** `fai query` results — carries a
  `schemaVersion` and is a stable, versioned API. The schemas and the daemon
  protocol are specified in **`docs/CLI.md`**.
- **Error codes are an API.** Allocate codes by phase and document each in the
  error-code catalog (M8): `FAI0xxx` tooling/CLI/driver, `FAI1xxx` lex/parse,
  `FAI2xxx` resolve/visibility, `FAI3xxx` types/rows, `FAI4xxx`
  exhaustiveness/patterns, `FAI5xxx` capabilities, `FAI6xxx` contracts, `FAI7xxx`
  backend (Core lowering / codegen / runtime). Each phase crate owns its codes as
  a `pub const CODES: &[CodeInfo]` slice, which the `fai-tests` catalog test
  aggregates to enforce format and uniqueness. Never renumber a shipped code.
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
4. New behavior has tests at the appropriate levels (see §13); new diagnostics
   have codes + catalog entries.
5. Any surface-language change is reflected in `AGENTS.md`, `docs/PLAN.md`, and
   the `samples/` directory.
6. Self-hosted check: every `.fai` file in `samples/` is verified by the test
   suite (parsed/formatted, and typechecked/run where applicable) so the docs
   cannot drift from the implementation.

## 13. Testing standards

Fai is a compiler: correctness *is* the product, so **test coverage is a
first-class deliverable, not an afterthought.** Aim for coverage that is both
**wide** (every construct, on every path) and **deep** (edge cases, error
recovery, and exact locations — not just happy paths). When in doubt, write the
test; under-testing a phase is a defect, not a shortcut.

- **Enumerate and test every edge case, every time.** Before a phase is "done",
  list its boundary conditions and write a test for each — empty and maximal
  inputs, off-by-one boundaries, deep nesting, adjacency without whitespace, and
  especially **interactions between sub-components** (e.g. comment markers inside
  strings, brackets that span block boundaries, multibyte text shifting offsets).
  Edge cases are where compiler bugs hide; hunting them down is part of the work,
  not an optional extra. A construct is not covered until its weird inputs are.
- **Test every phase at its own level.** Each phase crate (`fai-syntax`,
  `fai-resolve`, `fai-types`, `fai-core`, `fai-rc`, `fai-codegen`, …) carries
  fast unit tests for its logic plus golden/snapshot tests (`insta`) for its
  observable output (tokens, parse trees, diagnostics, formatted text, types,
  lowered IR). Cross-cutting end-to-end and incremental tests live in
  `fai-tests`.
- **Cover the whole matrix for each construct.** For every surface form, test:
  valid inputs; **malformed inputs** (which must yield a `Diagnostic`, never a
  panic or hang); error **recovery** (one run reports *many* diagnostics, and a
  single mistake never hides the rest); and edge cases — empty input, boundary
  values, deep nesting, large inputs, and **UTF-8 / multibyte** content with
  correct byte offsets.
- **Assert locations, not just outcomes.** Diagnostics are an API: assert the
  **code, the span (byte offsets), and the message** wherever the message is
  stable. Tokens, AST/IR nodes, and diagnostics all carry spans — test that those
  spans are *exact*, since everything downstream relies on them.
- **Negative tests matter as much as positive ones.** A compiler is judged by its
  behavior on wrong programs; exercise the failure paths deliberately and pin the
  recovery behavior.
- **Reach for property-based tests whenever an invariant exists.** Prefer
  generative property tests (`proptest`) over hand-picked examples for any law
  that should hold across *all* inputs, and add them as often as the property
  allows — generated, shrunk cases routinely find the edge cases examples miss.
  Typical invariants: the front end is **total** (never panics or hangs and
  always terminates) on arbitrary input; token/AST ranges stay in bounds and on
  `char` boundaries; layout preserves the significant tokens and balances its
  blocks; `fmt` idempotence and the `parse → print → parse` / `lex → render`
  round-trips; reference-count balance. Also run the language's own
  `example`/`forall` contracts over `samples/`.
- **Incrementality is tested, not assumed.** Whenever a query is added or changed,
  cover it with the incremental-vs-clean **verifier** and an edit-churn
  (early-cutoff) test.
- **Every bug fix ships with a regression test** that fails before the fix and
  passes after.
- **Tests are deterministic and reviewed.** No reliance on `HashMap` iteration
  order, wall-clock, or environment; review every snapshot diff by hand — never
  blanket-accept generated snapshots.
- **Keep the inner loop fast.** Prefer in-process tests (e.g. `fai_cli::run` with
  captured buffers, pure-function phase entry points) over spawning processes, so
  the suite stays quick enough to run constantly.

A change is not done until its tests make both the new behavior **and its failure
modes** hard to break unknowingly.
