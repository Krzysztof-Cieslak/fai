# Fai — Implementation Plan

This is the tactical build plan: milestones with concrete deliverables and
acceptance criteria, the sequencing rationale, a risk register, and the decision
log. For project conventions see `AGENTS.md`; for the language itself see the
`samples/` directory.

---

## Strategy & sequencing rationale

1. **De-risk the backend early.** Native codegen via Cranelift + a
   reference-counted runtime + linking is the highest-uncertainty part of the
   project. We therefore drive a **thin vertical slice all the way to a running
   native binary (M3)** on a tiny language subset, *before* widening the
   language. Integration risk is paid down first, not last.
2. **Incremental from day one.** The **salsa query spine**, the
   position-independent item tree, and the per-workspace **daemon** are
   foundational, not a late add-on: every phase is a memoized query from the
   start, and the `fai check` / `fai query` loop is incremental by **M2**. Only
   *optimizations* (Perceus reuse M6; parallelism, remote cache, and opt-in
   monomorphization M9) come later.
3. **Front-end before types, types before data.** Get a forgiving parser and the
   formatter working (M1), then Hindley–Milner for the functional core (M2),
   then the slice (M3), then the data layer — ADTs + pattern matching +
   structural records with rows (M4).
4. **Capabilities follow interfaces.** Interfaces compile to dictionaries;
   capabilities are just interface instances threaded from `main`. So M5
   delivers both at once.
5. **Optimize only once it runs and is correct.** Perceus reuse (M6) and the
   parallel/remote-cache/monomorphization work (M9) come after correctness — the
   *incremental architecture* is foundational, but *performance tuning* is not.
6. **Docs are tested.** Every `.fai` file in `samples/` is checked by the
   test suite from M1 onward, so documentation cannot drift.

Milestones are vertical where possible: each should leave `main` building,
linting clean, and green.

---

## Milestones

### M0 — Workspace scaffolding & toolchain
**Status:** complete — all deliverables landed and the acceptance gates pass.

**Goal:** an empty but coherent Cargo workspace with the diagnostics/span
foundation and a test harness, so every later milestone plugs in cleanly.

**Deliverables**
- `Cargo.toml` workspace; `rust-toolchain.toml` pinning a stable Rust.
- Crates created as stubs: `fai-span`, `fai-diagnostics`, `fai-db`, `fai-cli`,
  `fai-driver`.
- `fai-span`: `SourceId`, `Span` (byte offsets), `SourceMap`, line/column mapping.
- `fai-diagnostics`: `Diagnostic` model (code, severity, primary/secondary spans,
  labels, help, suggestions); human renderer + `--message-format=json` renderer;
  a stable, versioned JSON schema.
- `fai-db`: the **salsa database** skeleton — input queries (`source_text`),
  interning, revisions, durability tiers — that every later phase plugs query
  groups into. (salsa is pinned and wrapped here so the engine stays swappable.)
- `fai-cli`: argument parsing; subcommand stubs (`build/run/check/fmt/test/lsp` +
  `query`/`daemon`) that return "not implemented" diagnostics; global
  `--message-format`.
- `tests/`: golden/snapshot harness (e.g. `insta`) wired into `cargo test`;
  scaffolding for the **incremental verifier** (incremental vs from-scratch).
- CI script running build + `clippy -D warnings` + `fmt --check` + `cargo test`.

**Acceptance**
- `cargo build`, `cargo clippy -D warnings`, `cargo fmt --check`, `cargo test`
  all pass.
- `fai --help` and `fai check --message-format=json` emit well-formed output.
- A trivial salsa query memoizes and re-runs correctly (smoke test).

**Crates:** `fai-span`, `fai-diagnostics`, `fai-db`, `fai-cli`, `fai-driver`.

---

### M1 — Lexer, parser, AST, formatter (no types)
**Status:** complete — lexer, offside layout, recursive-descent/Pratt parser with
error recovery, the position-independent item tree, the `parse`/`item_tree` salsa
queries (early cutoff proven), and the idempotent canonical formatter are all
implemented; `fai check` and `fai fmt` run end to end. Every acceptance bullet
below is covered by tests.

**Goal:** parse the core surface syntax with error recovery and format it
canonically.

**Deliverables**
- `fai-syntax`: hand-written lexer (incl. the `'a'` char vs `'a` type-variable
  rule), tokens with spans.
- Recursive-descent parser with a Pratt expression sub-parser; **error
  recovery** (synchronize on layout/keywords; report multiple errors).
- AST in arenas, referenced by newtyped ids; spans on every node.
- **Position-independent item tree** keyed by stable `ItemId`s, with an
  `AstId` map and **spans in a side-table** — semantic queries depend on the
  item tree, not byte offsets, so whitespace/comment edits cut off at parse.
- `parse` is a **salsa query** (`source_text → item tree + diagnostics`) in
  `fai-db`; this is where early-cutoff first pays off.
- Offside-rule layout handling (indentation → virtual block tokens).
- Surface covered: module header, `let`, lambdas (`fun`), application, literals
  (`Int`, `Float`, `String`, `Bool`, `Unit`, char), `if/then/else`, operators
  (`+ - * / |> >> ++ = <>` …), tuples, lists, parenthesization, `//`/`(* *)`/`///`
  comments, and **`example` / `forall` contract declarations** (parsed as
  ordinary declarations — their bodies are real expressions, not comment text).
- `fai-fmt`: AST → canonical layout (2-space indent); **idempotent**.
- `fai check` runs lex+parse and reports syntax diagnostics; `fai fmt` works.

**Acceptance**
- Parser snapshot tests for valid + invalid inputs (recovery produces ≥N errors).
- `fai fmt` is idempotent: `fmt(fmt(x)) == fmt(x)` on a corpus.
- All non-type-dependent files in `samples/` parse and round-trip through
  `fai fmt` unchanged.
- **Edit-churn test:** inserting a comment / reformatting re-runs `parse` but the
  item tree is unchanged → near-zero downstream recompute (early cutoff proven).

**Crates:** `fai-syntax`, `fai-fmt`, `fai-db`, (+`fai-resolve` skeleton).

---

### M2 — Hindley–Milner inference for the functional core
**Status:** complete — `fai-resolve` (name resolution, the module graph,
visibility, per-module SCCs), `fai-types` (HM representation, unification,
let-generalization, the required-signature rule, contract typing), and `fai-ide`
(the eight `fai query` commands) are implemented; `fai check` type-checks and the
cross-module firewall is proven by the incremental verifier. See decisions
**D36–D44** for the choices made while implementing it.

> **Test-corpus note:** the real-world integration fixtures under
> `crates/fai-tests/tests/fixtures/typed/` (the poker model in `Card.fai`,
> `HandEval.fai`, `Poker.fai`, plus `Geometry.fai`, `Rational.fai`, `Matrix2.fai`,
> `Combinators.fai`) still encode domain data as `Int`/`Float`/`Bool`/tuples
> (vectors and matrices are bare tuples, hands are 5-tuples). They remain valid
> programs and typecheck clean; rewriting them to dogfood records/ADTs/`match`
> (and revisiting the tuple-shaped `//~ LOCAL` assertions in
> `crates/fai-tests/tests/real_world_locals.rs`) is tracked as enrichment work in
> the data-layer milestone below. Capability-using samples such as `Hello.fai`
> stay in the future-surface bucket until the interfaces milestone lands the
> `Runtime`/`console` capability surface.

**Goal:** type the pure functional core; enforce that every `public` binding has
an explicit signature.

**Deliverables**
- `fai-resolve`: single top-level module per file; name resolution; visibility
  (`public`/private); **dependency analysis / SCCs** so module-level bindings are
  mutually recursive and generalized correctly.
- **Queries & firewall:** `resolve`, `module_exports` (a module's public-signature
  interface), and `infer` (per def/SCC) are **salsa queries**. Editing a *private*
  body recomputes only that def's chain; `module_exports` is unchanged, so other
  modules don't re-check — `fai check` is now **incremental**, at per-def/SCC
  granularity.
- `fai-types`: HM type representation; unification; let-generalization;
  principal types for: primitives, functions/closures, application, `if`, `let`,
  **tuples**, lists.
- **Required-signature rule:** missing signature on a `public` binding is an
  error; a signature that disagrees with the inferred type is an error
  (signature is checked, not trusted).
- Equality typing: `=`/`<>` admitted on non-function types; using them on
  function-typed values is a type error.
- **Overloaded arithmetic:** `+ - * /` resolve over `Int`/`Float`; unconstrained
  numeric types **default to `Int`**; **no implicit `Int`/`Float` coercion**.
  Genuine ambiguity is reported (with a help to annotate or convert).
- **Contract typing:** `example`/`forall` bodies are resolved in module scope and
  checked to type `Bool`; `forall` binder types are inferred from use; contracts
  must be **pure** (no capability values in scope). (Execution lands in M7.)
- Diagnostics: type mismatch, occurs-check, unbound name, missing public
  signature, ambiguous types (incl. unresolved numeric defaulting) — each with a
  stable code, human + JSON.
- `fai-ide` + the **core `fai query` commands** (`symbols`, `def`, `refs`,
  `type`, `docs`, `outline`, `api`, `dependents`) built on these queries, with
  JSON output per `docs/CLI.md`. The same `fai-ide` layer powers the LSP (M8).

**Acceptance**
- Golden type tests (expected type or expected diagnostic) over a corpus.
- `fai check` reports precise, well-located type errors in both formats.
- Every public function in `samples/` typechecks against its written signature.
- **Firewall test (incremental verifier):** editing a private body invalidates
  only that def's chain; editing a public signature invalidates its dependents
  and nothing more.
- `fai query def/refs/type/api` return correct results on the corpus, including
  partial results when the workspace has errors.

**Crates:** `fai-resolve`, `fai-types`, `fai-db`, `fai-ide`.

---

### M3 — End-to-end native thin slice ⚠️ (highest-risk milestone)
**Status:** complete — `fai-core` (typed, desugared Core IR + lowering), `fai-rc`
(plain dup/drop), `fai-codegen` (Core → Cranelift IR through one path feeding both
a per-`Def` AOT object emitter and an in-process JIT), and `fai-runtime` (tagged
values, reference counting, closures/`apply_n`, primitives, the `Console` host,
and an entry shim with a live-object leak check) are implemented. `fai build`
produces a self-contained native executable and `fai run` executes via the JIT in
an isolated worker process; the per-`Def` `object_code` cache, the
reachable-from-`main` closure, and AOT linking live in `fai-driver`. The thin
subset is `Int`/`Bool`/`String`, functions, `let`, `if`, arithmetic, and
`Console.writeLine` reached via `main`; a reachable construct outside it reports
`FAI7001`. See decisions **D45–D55** for the choices made while implementing it.

**Goal:** compile a tiny program to a **native executable that runs**, exercising
the whole backend toolchain on the smallest possible language.

**Subset:** `Int`/`Bool`/`String`, functions, `let`, `if`, arithmetic, and a
single built-in capability (`Console.writeLine`) reached via `main`.

**Deliverables**
- `fai-core`: typed, desugared Core IR (the canonical lowered form).
- `fai-rc`: **plain** dup/drop insertion (no reuse analysis yet) over Core IR.
- `fai-codegen`: Core IR → Cranelift IR, with **two emitters from one path** —
  **AOT** object files (`fai build`) and a **JIT** module (`fai run`/`fai test`);
  calling convention; boxed/immediate value representation; string constants.
- `fai-runtime` (Rust static lib): allocator, RC primitives (`dup`/`drop`/free),
  boxed value layout, `String` builtins, the `Console` capability host, and the
  entry shim that constructs `Runtime` and calls `main`; symbols resolvable by
  both the linker (AOT) and the JIT.
- `fai-driver`: **content-addressed object cache** — `object_code(Def)` keyed by
  `hash(rc(Def)) + target + compiler-version` — plus AOT link (fast linker) and
  the **JIT runner** that executes `fai run` in an isolated worker process.

**Acceptance**
- `fai build hello.fai` produces a native binary; `fai run hello.fai` (JIT)
  prints via the Console capability and exits 0.
- A handful of e2e programs (arithmetic, string concat, conditional) produce
  correct stdout under `cargo test`.
- **Cache hit:** rebuilding after editing one function reuses cached
  `object_code` for the untouched functions (measured).
- No leaks: a debug allocator/counter reports zero live objects at exit.

**Crates:** `fai-core`, `fai-rc`, `fai-codegen`, `fai-runtime`, `fai-driver`.

---

### M3.5 — Daemon, persistence & protocol
**Status:** complete (one deferral). The on-disk content-addressed object cache
(`fai-core`'s portable `fingerprint_def` + the driver's disk layer around
`object_code`) and the per-workspace daemon (`fai-server`: MessagePack JSON-RPC
over `interprocess` local sockets, `initialize` handshake, stat-gated/hash-confirmed
file-state sync, idle shutdown) are built. The CLI is a thin client that routes
`check`/`query`/`fmt`/`build` through the warm daemon and runs `run` under daemon
supervision — the warm front end ships a portable IR bundle (`fai-core`'s `wire`)
to an isolated worker that JITs and executes it, with streamed `$/output`, a
wall-clock timeout, and a self-imposed `RLIMIT_CPU`; an unreachable daemon falls
back to in-process. `fai daemon status|start|stop|restart` manage it. **Deferred:**
`fai daemon tap` (cross-connection traffic broadcast) and a Windows CI (the
`interprocess`/named-pipe path compiles but is untested). Concurrent reads and
cancellation remain performance-milestone work. See decisions **D56–D65**.

**Goal:** make the warm-database speedups available to the CLI *across*
invocations — the heart of the agent feedback loop. (Inserted milestone; later
numbers unchanged.)

**Deliverables**
- `fai-server`: per-workspace **daemon** holding the live salsa `Database`;
  **MessagePack JSON-RPC** over a unix socket / named pipe (full spec in
  `docs/CLI.md` §7) — `initialize` handshake + version negotiation, request
  cancellation on input change, `$/progress`/`$/diagnostic`/`$/output` streaming.
- `fai-cli` becomes a **thin client**: auto-spawn/connect, `--no-daemon`
  fallback, `fai daemon status|start|stop|restart|tap`.
- **File-state sync:** incremental disk scan (mtime/size → re-hash changed) plus
  an optional client dirty-set fast path.
- **On-disk persistence** of the content-addressed artifact cache (from M3), so
  cold starts reuse backend output.
- Isolated **worker process** for `fai run`/`fai test` (stdio streamed back;
  timeouts and resource limits enforced by the daemon).

**Acceptance**
- A warm `fai check` / `fai query` is dramatically faster than the cold run
  (tracked edit→diagnostic latency benchmark).
- An upgraded binary restarts a stale daemon (version handshake).
- A panicking/runaway program under `fai run` cannot take down the daemon.

**Crates:** `fai-server`, `fai-cli`, `fai-driver`, `fai-db`.

---

### M4 — Data: ADTs, pattern matching, structural records with rows
**Status:** complete — discriminated unions and transparent aliases, `match`
with Maranget exhaustiveness/redundancy checking, structural records with row
polymorphism (parallel row union-find, lacks constraints, row-var
generalization), a native boxed `Float`, and a single structural `compare` all
typecheck and compile to native code. Monomorphic records use constant-offset
projection; a *row-polymorphic* field access/update reachable from `main` reports
`FAI7002`, pending the offset-evidence work tracked with the interfaces
milestone. The standard library (`Option`/`Result`, list combinators,
`compare`/`sort`/`sortBy`, `Dict`/`Set`, string ops) ships as a real prelude
module. See decisions **D66–D72** for the choices made while implementing it.

> **Test-corpus follow-up:** the `samples/` tour is promoted to the
> typecheck-clean set and exercised end-to-end. The larger real-world *fixtures*
> under `crates/fai-tests/tests/fixtures/typed/` (the poker model in `Card.fai`,
> `HandEval.fai`, `Poker.fai`, plus `Geometry.fai`, `Rational.fai`, `Matrix2.fai`)
> still encode their domain data as tuples — they remain valid programs and
> typecheck clean, but do not yet dogfood records/ADTs. Rewriting them (and the
> tuple-shaped `//~ LOCAL` assertions in `real_world_locals.rs`) is enrichment
> work, not a correctness gap.

**Goal:** the full data layer, including the project's largest type-system
feature (row-polymorphic structural records).

**Deliverables**
- `fai-syntax`/`fai-resolve`: `type` declarations — discriminated unions and
  record-type *aliases*; constructor and field resolution.
- `fai-types`:
  - ADTs with type parameters; constructors as functions.
  - **Structural records via row polymorphism**: rows as a kind, **row
    unification**, **lacks constraints** (no duplicate labels), generalization
    over row variables; record literals, dot access, `{ r with ... }`.
  - **Record-type annotations: closed by default** (`{ x : T }`); `{ x : T | _ }`
    elaborates to a fresh anonymous open row (the common accessor/capability
    case); `{ x : T | 'r }` names the tail to thread it to the result. Governs
    *written signatures* only — inference always infers open rows for field
    access; **no subtyping**.
  - **Exhaustiveness & redundancy checking** for `match` (incl. literals, cons,
    tuples, records with field punning).
  - **Record patterns mirror type openness:** a bare `{ ... }` is closed and must
    name *all* fields (missing field → diagnostic: "name it or add `| _`"); a
    `{ ... | _ }` tail is open (ignore the rest) and is **required** for
    row-polymorphic scrutinees (the abstract tail is unmatchable). Binding the
    tail (`{ x | rest }`, restriction) is **v2**. The pattern row-tail `|` is
    contextual (distinct from the `match`-arm separator, which is outside braces)
    — add a parser test.
- `fai-core`/`fai-codegen`/`fai-runtime`:
  - Constructor representation (tag + fields; nullary → immediate).
  - **Canonical record field layout** (by interned label id).
  - Constant-offset access for monomorphic records; **offset-evidence passing**
    for row-polymorphic field access.
  - Pattern-match compilation to decision trees/switches.
  - `List`, `Option`, `Result`, `String` standard module surface.

**Acceptance**
- Golden tests: exhaustiveness errors fire correctly; row inference produces the
  expected principal types (e.g. `getX : { x : 'a | _ } -> 'a`).
- e2e: programs using ADTs, `map`/`fold`, records, `{ with }`-update, and field
  punning compile and run with correct output and zero leaks.

**Crates:** `fai-syntax`, `fai-resolve`, `fai-types`, `fai-core`, `fai-codegen`,
`fai-runtime`.

---

### M5 — Interfaces, instances & capabilities
**Goal:** the one OO-flavored feature and the effect model it powers.

**Deliverables**
- `interface` declarations (named sets of function signatures).
- **Interface instances** (`{ Name with <methods> }`, ML method sugar `m args = …`)
  as the only constructor → existential values compiled to **dictionaries**
  (records of closures capturing existential state). No `new`; methods don't see
  each other as bare names (record semantics — factor shared logic to module
  level). Parser must disambiguate `{ Name with … }` (instance) from
  `{ e with … }` (record update) by the head (interface name vs record expr), and
  from `match e with …` — add tests.
- `fai-types`: interface types, dictionary insertion, existential packing/
  unpacking; integration with rows so capability records work.
- Capabilities: built-in capability interfaces (`Console`, `Clock`, `Random`,
  `FileSystem`, `Env`) and a `Runtime` record alias bundling them; the runtime
  constructs `Runtime` and passes it to `main`.
- **Least authority via rows:** a function may request `{ console : Console | _ }`
  and accept any larger runtime.
- `fai-runtime`: host implementations for each capability.
- **Operators as interface methods + user-defined operators (D75):** a generic
  operator-character lexer and **F#-style precedence** (derived from the
  operator's symbols, no fixity declarations); the overloaded operators become
  std interface methods — `Num` (`+ - * / %`), `Eq` (`= <>`), `Ord`
  (`< <= > >=`) — defined in `Prelude`, with the M2 constraint flavors replaced by
  these interface constraints (monomorphic uses still lower to the direct
  primitive); user-defined operators resolve like names (module-local +
  `Prelude`); formatter support for arbitrary operators. `&&`/`||` stay
  short-circuit sugar and `::` stays the `List` constructor.

**Acceptance**
- e2e: a program that takes only the capabilities it needs, builds a derived
  capability via an interface instance (`{ Name with … }`, e.g. a prefixing
  `Console`), and runs.
- Type error when code attempts an effect without holding the capability.

**Crates:** `fai-syntax`, `fai-resolve`, `fai-types`, `fai-core`, `fai-codegen`,
`fai-runtime`.

---

### M6 — Perceus reuse & in-place update
**Status:** complete. (1) **Precise, ownership-based reference counting**: `fai-rc`
normalizes each function to A-normal form and inserts dup/drop precisely
(duplicate only when a value is still live afterward; drop at the last use rather
than scope end; per-branch drops), with projections (`DataField`/`DataTag`)
**borrowing** their base so a matched value survives its projections and is
released once by reference counting. (2) **Reset/reuse**: a dead data cell's
release becomes a `Reset` at its death point that yields a reuse token (its raw
memory if unique, else null), threaded forward to a same-size construction that
builds into it in place (`fai_drop_reuse`/`fai_reuse`); `if` pushes the decision
into branches (reset-and-reuse where one reconstructs, drop where not). A
`map`/`filter`/`inc` over a *unique* list allocates **zero** fresh cells, while a
shared list falls back to copying (the runtime rc==1 guard), proven by a
differential allocation count. (3) **In-place update & drop specialization**:
`{ r with … }` overwrites the record in place when it is unique (the
row-polymorphic path via `fai_record_update`, the monomorphic path via the reuse
mechanism — lowering reads a record's unchanged fields from a single base local so
it stays uniquely referenced), copying only when shared; and code generation omits
dup/drop of statically-immediate values (a `local → type` map). (4) **Argument
borrowing**: a saturated call to a top-level function is emitted as a **direct
call** to its code; a per-function inference (`borrow_signature`) lends parameters
that are only inspected (e.g. `length`/`sum`'s list) while owning those that
escape or are matched-and-reconstructed (e.g. `map`/`inc`, so reuse still fires);
a callee treats a borrowed parameter like a capture, a direct caller lends it
(no duplication), and the first-class value form uses an owned-ABI wrapper that
releases the borrowed arguments (so `apply_n`/escaping use stays sound without a
whole-program escape analysis). An abstract reference-count interpreter over the
IR guards soundness (ownership, borrowing, reuse tokens) across a corpus and whole
programs. Inspect-only **primitive borrowing**, the **cross-module borrowing
fixpoint**, deeper **drop specialization**, and **tail-recursion modulo cons** are
correctness-neutral follow-ups (see M9).

**Goal:** turn correctness-first RC into competitive performance.

**Deliverables**
- `fai-rc`: reuse analysis (reuse tokens), drop specialization, borrowing of
  arguments to avoid dup/drop churn.
- In-place reuse for same-size constructors and for `{ r with ... }` when the
  refcount is 1.

**Acceptance**
- `map`/`filter`/`fold` over a unique list allocate ~zero fresh cells
  (measured); benchmarks show the expected reduction vs M3-style plain RC.
- Correctness unchanged: full test suite green, zero leaks.

**Crates:** `fai-rc` (+ codegen support).

---

### M7 — Contracts: examples & properties
**Status:** complete (Stage 1). `fai test` collects, synthesizes, JIT-runs, and
reports the `example`/`forall` contracts. The property-testing framework is a
**dogfooded standard-library module** (`std/Test.fai`): a pure splitmix64 `Gen`,
an `Arbitrary 'a = { gen, shrink, show }` bundle, type-directed combinators
(`int`/`bool`/`float`/`string`/`unit`/`list`/`tuple2..4`/`option`/`result`), and
the `checkExample`/`checkForall` driver (sized trials, greedy shrinking). The
compiler (`fai-contracts`) types a contract's binders (defaulting residual type
variables to `Int`), synthesizes a harness — a *property* function plus an entry
that calls `Test.checkForall`/`checkExample` with an `Arbitrary` composed from
the combinators for the binders' types — reference-counts and JIT-compiles it
with its reachable callees (`fai-codegen`'s `JitProgram`), then applies it and
decodes the returned `TestResult`. A failure is a located **`FAI6001`** with the
shrunk counterexample (binder names + rendered value); a binder with no generator
(a function type, `>4` binders, or an open record row) is **`FAI6002`**. `fai test`
takes `[path]`/`--match`/`--seed`/`--count`/`--max-size`, runs in-process, and
asserts the runtime's live-object count returns to zero. **User records and ADTs**
(including recursive ones like `Dict`/`Set`/`Tree`) now generate too: the compiler
synthesizes a top-level `Arbitrary` definition per type — referenced as a `Global`,
so a recursive type is a self-reference guarded by the size budget, and every
synthesized function is capture-free (a captured value becomes a leading parameter
supplied by partial application). The whole `samples/` + `std/` contract corpus
runs and passes. Remaining follow-ups: **isolated-worker execution** (the
in-process runner aborts if a generated input triggers a runtime trap, e.g.
division by zero — worker isolation contains that) with `$/testEvent` streaming,
and revisiting **full-domain float** generation (it surfaces precision-edge
counterexamples on float-arithmetic laws). See decisions **D80–D87**.

**Deliverables**
- `fai-contracts`: collect the typed `example`/`forall` declarations from each
  module (associating each with the top-level symbols its body references, for
  reporting/docs).
- `example` evaluated (constant cases may be checked as early as `fai check`);
  `forall` exercised with **type-driven generators** (QuickCheck-style),
  shrinking on failure.
- `fai test` runs all contracts and reports pass/fail with codes + JSON.

**Acceptance** (met)
- `fai test` passes for all contracts in `samples/`; a deliberately wrong
  `example`/`forall` fails with a precise, located diagnostic (+ shrunk
  counterexample for properties).

**Follow-up work (deferred; correctness-neutral unless noted):**
- **Isolated-worker execution for `fai test`.** The runner is in-process, so a
  generated input that triggers a runtime trap (e.g. integer division by zero in
  a property body — observed running the `Poker` fixture) **aborts the process**.
  Run contracts in the supervised worker that `fai run` already uses (timeouts +
  resource limits), and stream per-contract results as `$/testEvent` so the
  daemon serves `fai test` (today it routes in-process; results are a rendered
  `TestOutput`). This is the one *robustness* gap, not just an optimization.
- **Generators for the remaining type shapes.** Mutually-recursive ADTs, and
  recursion reachable only through a collection field (e.g. `Rose (List Rose)`),
  are not size-guarded — generation could diverge; add a fuel parameter threaded
  through generation. **`Char`** generation awaits native `Char` support (a
  `Char` binder is reported `FAI6002`). Allow **user-defined/custom generators**
  (e.g. invariant-respecting `Arbitrary` instances) to override the synthesized
  ones.
- **Full-domain float generation** (incl. NaN/inf) surfaces precision-edge
  counterexamples on float-arithmetic laws (observed running the `Geometry`
  fixture). Revisit: offer a finite-float generator, or shrink float
  counterexamples toward simple values.
- **Constant `example` evaluation at `fai check`.** Examples with no free
  variables could be checked during `fai check` (folded), surfacing failures
  without a separate `fai test`.
- **Configurable trials/size per property** and a richer JSON `TestOutput`
  (per-contract events, seeds) once the worker streams results.
- **Explicit contract-purity diagnostic.** Purity is currently enforced by
  construction (a contract has no `Runtime` in scope), but an explicit check
  would give a clearer error than a downstream type mismatch.
- **Pre-existing crash uncovered (not M7-specific):** a **single-line union
  declaration** (`type T = A | B …` on one line) panics the exhaustiveness
  checker (`fai-types/src/exhaustive.rs` `default_matrix`, "non-empty row"); the
  multi-line form is fine. A malformed/over-terse program must yield a
  `Diagnostic`, never panic — fix in the parser or exhaustiveness check (M8
  surface-completeness work).

**Crates:** `fai-contracts` (+ `fai-cli`, `fai-driver`, `fai-core`, `fai-rc`,
`fai-codegen`, `fai-runtime`, `fai-types`, `fai-resolve`, `fai-syntax`,
`fai-fmt`, `fai-ide`).

---

### M8 — Surface completeness, LSP v1, advanced code intelligence, error-code catalog
**Goal:** make it pleasant and complete enough for real use.

**Deliverables**
- Nested modules; remaining pattern forms; broader standard library.
- `fai-fmt` completeness across the whole grammar; formatter conformance tests.
- `fai-lsp`: an LSP v1 over stdio — push diagnostics, hover (types),
  go-to-definition, and document formatting — **reusing `fai-ide`** (the same
  engine behind `fai query`). Completion, references, rename, symbols, signature
  help, hover docs, code actions, and the editor client/grammars are their own
  milestone (M10).
- **Advanced `fai query`:** `callers`/`callees` (call hierarchy), `dependents`
  (transitive), `caps` (capability footprint), and `search` (Hoogle-style type
  search; needs a type-normalized index).
- **Error-code catalog** documenting every `FAInnnn`, plus the **`docs/CLI.md` JSON
  output schemas** (a public, versioned API).

**Acceptance**
- LSP serves diagnostics/hover/go-to-def on a sample project over stdio.
- `fai query search "List 'a -> Int"` and `fai query caps <fn>` return correct
  results on the corpus.
- Catalog covers every emitted code; a test asserts no undocumented code ships.

**Crates:** `fai-lsp`, `fai-ide`, `fai-fmt`, `fai-resolve`, `fai-diagnostics`.

---

### M9 — Performance at scale (incrementality is already foundational)
**Status:** in progress. The **inference-tuning** targets and **primitive
borrowing** are done (see decisions **D92–D94**): the solver shares its types via
`Rc`, borrows and memoizes its occurs/free-variable walks, generalizes local
`let`s by binding level (rather than recomputing the environment's free
variables), and path-compresses variable chains — turning the unify/occurs/
generalization/chain shapes that were O(n²)/O(n³) into linear ones (e.g. unifying
a depth-256 arrow drops from ~3.7ms to ~28µs, a value-`let` chain of 800 from
~31ms to ~1ms); and the inspect-only primitives (`=`, `compare`, the `String`
readers) now borrow boxed operands via non-consuming runtime variants. Always-on
thread-local **work counters** (`fai-types/src/perf.rs`) gate the solver's
asymptotic complexity deterministically in `perf_guards.rs`. **Intra-build
parallelism** is also done (see decisions **D95–D96**): per-definition code
generation (the AOT object loop) and the lower/reference-count gathers for the
run paths run across a `rayon` pool, each worker on its own cheap database-handle
clone (salsa coordinates the shared memoization) — ~2× on a 200-definition build
here; and the JIT path code-generates each function in parallel (building IR and
linking the shared module serially) — ~1.4× on the same program. Still to come:
the shared/remote cache (deferred — issue #15), daemon hardening (LRU/memory
bounds, cross-request concurrency), opt-in monomorphization (deferred — issue
#16), and the remaining reuse/borrowing follow-ups (TRMC, cross-module
borrowing).

**Deliverables**
- `rayon` parallelism across independent defs/modules (parallel salsa queries;
  per-function Cranelift codegen).
- **Shared/remote artifact cache** layered on the local content-addressed cache
  (team/CI dedup; portable by construction). *(Deferred — tracked in issue #15.)*
- Daemon hardening: LRU eviction / memory bounds; latency profiling.
- **Opt-in monomorphization** for hot generic paths (optimization only — never a
  correctness requirement, and the one feature that *hurts* incrementality, so it
  stays opt-in). *(Deferred — tracked in issue #16.)*
- **Reuse/borrowing follow-ups** layered on the M6 work, all correctness-neutral:
  - **Tail-recursion modulo cons (TRMC):** flatten a self-tail-recursive
    constructor-returning function (e.g. `map`, `filter`) into an in-place-building
    loop using destination passing, removing the stack growth. Cell reuse already
    makes such a function allocate zero fresh cells over a unique list (with N
    reset tokens live on the stack); TRMC additionally removes the O(N) stack and
    improves locality. A substantial separate transform.
  - **Cross-module argument borrowing:** an inter-procedural borrow fixpoint (over
    call-graph SCCs) so a function borrows parameters it only forwards to other
    modules' borrowing functions; the current inference is self-contained
    (self-recursion only, conservative across functions).
  - **Primitive borrowing** for inspect-only primitives (`=`/`compare`/string
    reads) on boxed operands, guarded by operand type so the hot `match` tag-test
    path keeps consuming its (immediate) operands.
  - **Drop specialization:** inline a known monomorphic data cell's child drops
    and free to skip the descriptor dispatch (deferred from M6 as marginal after
    reuse and carrying memory-safety risk).
- Compile-throughput + `edit→diagnostic` / `edit→test` benchmarks with CI
  regression guards. (Foundation landed early during M2: a deterministic
  query-count guard suite — `crates/fai-tests/tests/perf_guards.rs`, proving the
  firewall makes a localized edit's recompute independent of workspace size — and
  wall-clock divan benches in `crates/fai-tests/benches/` over a synthetic
  `corpus` generator. M9 extends these to codegen/`edit→test` and adds trend
  tracking.)
- **Inference tuning targets** surfaced by the M2 micro/stress benchmarks
  (`crates/fai-tests/benches/{micro,stress}.rs`), all correctness-neutral:
  - the **occurs check** re-walks the whole (growing) type on every variable
    binding → O(n²) on long curried-application chains and exponential type
    growth (defer/skip occurs via union-find ranks, or rank-based path
    compression);
  - **local-`let` generalization** recomputes environment free-variables per
    binding → O(n²) in block size for long `let`-chains (cache the env var set);
  - **unification of very deep types** repeats `resolve_shallow` walks (add path
    compression to the union-find).

**Acceptance**
- Documented throughput + latency targets on a large synthetic corpus; no
  regressions beyond a set threshold in CI.
- Remote-cache hit reproduces a clean build's artifacts on a fresh checkout.

**Crates:** `fai-driver`, `fai-db`, `fai-server`, `fai-types`, `fai-codegen`,
(+ most front-end crates).

---

### M10 — Editor integration: LSP v2, VS Code extension & grammars
**Goal:** ship a first-class editor experience for Fai — grow the language server
from the v1 surface (push diagnostics, hover, go-to-definition, document
formatting) into full editing, and deliver the client and grammars an editor
actually needs. The server features reuse `fai-ide`, so every one shares the warm
session and the `fai query` answers; open buffers are already overlaid as
in-memory edits, so each works on unsaved text.

**Deliverables — language-server completeness (LSP v2)**
- **Completion** (`textDocument/completion`): in-scope names, qualified members
  after `Module.`, record fields after a value's `.`, and constructors in
  patterns; each item carries a kind, the rendered type signature as detail, and
  docs (lazily, via `completionItem/resolve`).
- **Find references** (`textDocument/references`) and **rename**
  (`textDocument/rename` + `prepareRename`): cross-file workspace edits over the
  resolution reference graph, honoring visibility and qualified paths.
- **Document & workspace symbols** (`textDocument/documentSymbol`,
  `workspace/symbol`): the `fai-ide` outline/symbols, nested-module aware.
- **Signature help** (`textDocument/signatureHelp`): parameter types while
  writing an application.
- **Richer hover:** include `///` doc prose and attached `example`/`forall`
  contracts (v1 shows the type only).
- **Code actions / quick fixes** (`textDocument/codeAction`): apply the
  machine-applicable diagnostic suggestions (span + replacement) the diagnostics
  model already carries; "add the missing public signature"; qualify an ambiguous
  name.
- **Inlay hints** (inferred types for `let`/parameter bindings) and **semantic
  tokens** (semantic highlighting) — both optional, both from the existing
  type/resolution data.
- **Editing fidelity:** incremental (range) document sync and `didSave` handling
  (v1 is full-document sync); range / on-type **formatting**; client
  position-encoding negotiation (UTF-8/UTF-16) layered on the existing line map.
- **Dependent diagnostics:** re-publish an open file's diagnostics when a
  cross-module change invalidates it (v1 pushes diagnostics only for the edited
  file), or adopt pull-based diagnostics (`textDocument/diagnostic`).

**Deliverables — editor integration & grammars**
- **VS Code extension** (`editors/vscode/`): a thin TypeScript client
  (`vscode-languageclient`) that launches `fai lsp` over stdio and surfaces it; a
  `fai` language contribution (`.fai`, aliases), a `language-configuration.json`
  (comment tokens `//` and `(* *)`, bracket pairs, auto-closing/surrounding
  pairs, offside-aware indentation), settings (server-binary path, trace) and a
  "restart server" command; bundled with esbuild and packaged to a `.vsix` with
  `vsce`. The client is intentionally thin — all intelligence stays in the
  server.
- **TextMate grammar** (`editors/vscode/syntaxes/fai.tmLanguage.json`):
  editor-agnostic syntax highlighting consumed by VS Code (and usable by GitHub
  Linguist). Scopes for keywords (incl. `module`/`interface`/`match`/`with`/`as`/
  `example`/`forall`), the three comment forms (`//`, `(* *)`, `///` doc),
  string/char literals, numeric literals (`0xFF`, `1_000`, floats), operators,
  constructors/modules (upper-case idents), and type variables — faithfully
  encoding the lexer's **`'a'` char-literal vs `'a` type-variable** rule.
- **Tree-sitter grammar** (`tree-sitter-fai/`, *stretch*): a `grammar.js`
  mirroring `fai-syntax`, the generated parser, and `queries/` (highlights,
  locals, folds, indents) for editors that consume tree-sitter (Neovim, Helix,
  Zed, GitHub). A second, independent encoding of the grammar, so it is scoped as
  a stretch goal and must be kept in step with the canonical parser (see R18).

**Acceptance**
- On a sample multi-file project: completion offers in-scope and qualified
  members with types; references and rename are correct and complete across
  modules; document symbols mirror `fai query outline`; a quick fix applies a
  diagnostic's suggested edit.
- The packaged VS Code extension installs, connects to `fai lsp`, and shows
  diagnostics + highlighting on a `samples/` file; the TextMate grammar tokenizes
  every `samples/` file with no `invalid`/unscoped spans; the tree-sitter grammar
  (if built) parses every `samples/` file without `ERROR` nodes.

**Crates / dirs:** `fai-lsp`, `fai-ide`, `fai-resolve`, `fai-types`, `fai-fmt`
(server side); new non-Cargo trees `editors/vscode/` and `tree-sitter-fai/`
(TypeScript / JS, built and tested by their own tooling, wired into CI
separately from the Cargo workspace).

---

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | Cranelift integration + linking harder than expected | Med | High | Pulled forward to **M3** on a tiny subset; keep runtime ABI minimal and stable. |
| R2 | RC correctness (leaks / double-free), esp. with closures & existentials | Med | High | Plain RC first (M3); debug leak counter in every e2e test; reuse (M6) added only after green. |
| R3 | Row polymorphism (unification, lacks, evidence passing) is intricate | Med | High | Lacks constraints (no scoped labels) to bound complexity; defer extension/restriction to v2; extensive golden tests. |
| R4 | Offset-evidence codegen for polymorphic field access | Med | Med | Reuse the dictionary-passing mechanism already needed for interfaces/generics; monomorphic access stays constant-offset. |
| R5 | Offside-rule parsing ambiguities / poor recovery | Med | Med | Well-specified layout algorithm; the canonical formatter pins one layout; snapshot tests for tricky indentation. |
| R6 | Exhaustiveness checking bugs (rows/literals) | Med | Med | Implement a known algorithm (Maranget-style); golden tests for false pos/neg. |
| R7 | `'a'` char vs `'a` type-var lexing | Low | Med | Single documented lexer rule; dedicated tests (`AGENTS.md` §11). |
| R8 | Scope creep from "AI-first" features | Med | Med | Effect rows, extension/restriction, package manager are explicitly **v2**. |
| R9 | Docs drifting from implementation | Med | Low | Self-hosted check: `samples/` files are part of the test suite (DoD #6). |
| R10 | Overloaded arithmetic adds inference complexity / "ambiguous numeric type" noise | Low | Med | Restrict overloading to the built-in numeric set (`+ - * /`) with a simple `Int`-defaulting rule; clear help text steering to annotation or `intToFloat`/`floatToInt`. |
| R11 | salsa API churn / version instability | Med | Med | Pin a version; wrap behind `fai-db` so the engine is swappable; keep query definitions framework-agnostic. |
| R12 | Incremental-cache correctness (stale results → wrong diagnostics) | Med | High | Incremental-vs-clean **verifier** in CI; content-addressed keys stamped with compiler version + flags; determinism is a locked invariant. |
| R13 | Span/position instability collapses incrementality | Med | High | Position-independent item tree + spans in a side-table; edit-churn test asserts "add a comment → near-zero recompute". |
| R14 | Daemon lifecycle: stale/version-mismatch, spawn races, memory growth | Med | Med | Version handshake + auto-restart; version-stamped socket path + spawn-lock; LRU eviction + idle-timeout shutdown. |
| R15 | JIT'd user code crashes/hangs the toolchain | Med | High | Run in an isolated **worker process** with timeouts/resource limits; the daemon survives worker death. |
| R16 | Large mutually-recursive SCCs reduce per-def granularity | Low | Med | SCCs computed from actual references (usually small); consider a lint for accidental large cycles. |
| R17 | Type-search (`query search`) indexing/matching complexity | Low | Med | Ship as an M8 goal; type-normalized index; unify up to row polymorphism; bound results with `--limit`. |
| R18 | The editor grammars (TextMate, tree-sitter) re-encode the lexer/parser and drift from the canonical `fai-syntax` | Med | Low | The hand-written `fai-syntax` stays the single source of truth; grammars are highlighting/structure aids only. Pin both with tests over `samples/` (TextMate: no unscoped spans; tree-sitter: no `ERROR` nodes), so drift fails CI. Keep the tree-sitter grammar a stretch goal to bound the dual-maintenance cost. |

---

## Decision log

Resolved during planning (see the locked table in `AGENTS.md` §3):

- **D1 Backend:** Cranelift native codegen (over interpreter / bytecode VM / LLVM
  / transpile). Rationale: native speed with fast compiles; avoids LLVM build cost.
- **D2 Memory:** Perceus-style reference counting. Rationale: strict + pure ⇒
  acyclic heaps ⇒ no cycle collector; enables in-place reuse.
- **D3 Generics:** uniform boxed representation + dictionary passing (no
  monomorphization by default). Rationale: protects compile throughput; no code
  bloat. Monomorphization is an opt-in M9 optimization.
- **D4 Effects:** capabilities as explicit values (interface instances from
  `main`); **no** type-level effect rows in v1. Rationale: simple, auditable,
  implementable now; rows can layer on later.
- **D5 Signatures:** Haskell-style explicit signature on its own line above each
  `public` binding; signatures are checked, not trusted.
- **D6 Layout:** indentation-significant (offside); one canonical layout pinned
  by `fai fmt` (2-space indent).
- **D7 Type variables / equality:** F#-style `'a`; `=`/`<>` (parser
  disambiguates `=` binding vs. equality by position).
- **D8 Tuples:** structural; value `(a, b)`, type `'a * 'b`.
- **D9 Records:** **structural with row polymorphism**; lacks constraints (no
  duplicate labels); `type X = { ... }` is a transparent alias; extension/
  restriction deferred to v2. Rationale: better inference + row-polymorphic
  capability least-authority; reuses evidence-passing machinery.
  **Openness:** record type annotations are **closed by default** (`{ x : T }`);
  `{ x : T | _ }` is anonymous-open (common case), `{ x : T | 'r }` names the
  tail only to thread it to the result. Chosen over open-by-default (which would
  invert the default for data records/literals and still need named rows for
  updates) and over width subtyping (incompatible with principal HM inference).
  Governs written signatures only; inference is unchanged; no subtyping.
  **Patterns mirror this (P-A):** `{ ... }` closed (names all fields),
  `{ ... | _ }` open (ignore rest; required for row-poly scrutinees); binding a
  pattern tail (restriction) is v2. Chosen over always-open patterns so `{ ... }`
  means the same thing in types and patterns.
- **D11 Arithmetic:** F#-style overloaded `+ - * /` over `Int`/`Float` (over
  OCaml-style `+.`); unconstrained numerics default to `Int`; no implicit
  `Int`/`Float` coercion. Rationale: "similar to F#", one operator set; the
  built-in overload set is small and bounded (we already special-case `=`/
  comparison). *(Amended by D75: `+ - * / %` become methods of a std `Num`
  interface at M5.)*
- **D12 Contracts:** **first-class `example`/`forall` declarations** (peers of
  `let`), placed immediately after the binding they describe, *not* a doc-comment
  extension. Rationale: symbols inside contracts resolve via normal name
  resolution (real diagnostics, types, LSP), laws can span multiple functions,
  and it is simpler to build than embedding checked code in `///` comments.
  `///` stays human prose. Considered and rejected: contracts inside `///`
  (doctest-style) — murky scoping + lexer/formatter complexity.
  Separator is `:` (`example: e` / `forall xs: e`); `=` was rejected because
  contract bodies are usually equalities, which would put two `=` on one line.
- **D13 Interface instances:** construct with **`{ Name with <methods> }`** (ML
  method sugar `m args = …`); the OO `new` and the "object expression" term are
  dropped, but the braces are kept so it mirrors record update `{ r with … }`.
  Interfaces stay **nominal**. Rationale: reads as FP (a named dictionary of
  functions) while keeping clear identity and diagnostics; no implicit instance
  resolution exists (interfaces are explicit values), so nominal identity isn't
  needed for dispatch. Rejected: bare `Name with …` (no braces), tagged record
  `Name { … }`, and full structural collapse to record-of-function types (would
  remove the interface concept). Notes: `{ … with … }` is disambiguated by its
  head (interface name → instance; record expr → update), and from `match … with`;
  instance methods don't see sibling methods as bare names.
- **D14 Incremental engine:** **salsa** as the compiler spine from the front-end
  milestones (not deferred). Every phase is a memoized query in `fai-db`;
  parse produces a **position-independent item tree** with spans in a side-table
  so trivial edits cut off early; granularity is per-def/SCC. salsa is pinned and
  wrapped behind `fai-db` so the engine is swappable. Rejected: hand-rolled
  engine (subtle, risky) and deferring incrementality to a late milestone
  (painful retrofit — query boundaries touch every crate).
- **D15 Runtime topology:** a per-workspace **daemon** (`fai-server`) holds the
  hot DB; the CLI is a **thin client** over **MessagePack-encoded JSON-RPC 2.0**
  (length-prefixed; unix socket / named pipe; `0600`; version-stamped path). LSP
  stays standard JSON. Stateless requests; cancellation on input change;
  **file-state sync** by incremental disk scan + optional client dirty-set;
  hypothetical overlays deferred. Full spec in `docs/CLI.md` §7. Rejected: text
  JSON-RPC on this link (binary is more compact for large dumps; a JSON tap keeps
  debuggability), stateless-CLI-only (no warm DB), and FS-watch (race-prone with
  agents).
- **D16 Execution:** **JIT** (Cranelift) for `run`/`test`/contracts — no link on
  the inner loop — and **AOT** for `build`. JIT'd user code runs in an isolated
  **worker process** (crash/timeout safety; capability sandboxing); stdio is
  streamed over the protocol.
- **D17 Caching:** local **content-addressed artifact cache** — `object_code`
  keyed by `hash(rc(Def)) + target + compiler-version` — designed so a
  shared/remote cache layers on later (M9). Determinism makes this sound; an
  incremental-vs-clean **verifier** runs in CI.
- **D18 Code intelligence:** a **read-only** `fai query` surface (namespaced),
  sharing the `fai-ide` engine with the LSP; addressing by name path or
  `file:line:col`; JSON by default; best-effort under errors. **No write/refactor
  commands** (no `rename`/`fix`) — agents perform edits themselves. Full command
  reference in `docs/CLI.md`.

Resolved while implementing **M0** (cross-cutting conventions later milestones
must honor):

- **D19 Edition & toolchain:** Rust **edition 2024**, toolchain pinned to
  `1.96.0` (`rust-toolchain.toml`); `resolver = "3"`; `Cargo.lock` committed.
  Canonical formatting pinned via `rustfmt.toml` (`use_small_heuristics = "Max"`).
- **D20 Lints:** denied workspace-wide in `[workspace.lints]` — `warnings`,
  `unsafe_code`, `clippy::all`. `unsafe_code` is `deny` (not `forbid`) so `fai-db`
  can carry salsa's macro-generated `unsafe impl`s via a scoped crate allow; the
  query-defining phase crates (e.g. `fai-syntax`) scope the same allow for salsa's
  `tracked`/`Update` macros (still no hand-written unsafe). `missing_docs` is
  **not** denied (it fights
  macro-generated public items); docs on public items stay a convention.
- **D21 Tooling error codes:** the **`FAI0xxx`** range is owned by the
  tooling/driver layer (`fai-driver`): `FAI0001` not-implemented, `FAI0002`
  workspace/I/O. Codes live as per-crate `CODES` slices; `fai-tests` aggregates
  them for the format/uniqueness catalog test.
- **D22 Span model & source authority:** `fai-span` is engine-agnostic
  (`SourceId`, `ByteOffset`, file-relative `TextRange`, file-qualified `Span`,
  `LineIndex` with 1-based **character** columns). The salsa `SourceFile` input is
  the **authoritative** source text; rendering resolves spans through the
  `SpanResolver` trait (impls: `SourceMap` for tests/one-shot, `DbSpanResolver`
  backed by the database). Machine output uses **workspace-relative** paths.
- **D23 Diagnostics flow:** deeper phases emit into the salsa **accumulator**
  `Diag`; callers collect at the boundary. One model, two renderers (human +
  JSON wire schema, `schemaVersion = 1`); output is ordered deterministically by
  `(file, byteStart, code)`.
- **D24 Database shape:** a single `#[salsa::db]` trait `Db` plus the concrete
  `FaiDatabase`, both in `fai-db`; downstream phases add tracked *functions* over
  `&dyn fai_db::Db` rather than new DB traits. `fai-db` and the query-defining
  phase crates depend on `salsa` directly (its macros resolve `salsa::` from the
  crate root); other crates use `fai-db`'s re-exports. Identifier interning
  will use a separate non-salsa `Symbol`; salsa interning is reserved for derived
  keys.
- **D25 Client seam:** driver command entry points take `&dyn Db` and return a
  `CommandResult` the CLI (and, later, the daemon) renders; envelope schema types
  live in `fai-driver`. The CLI is in-process-testable via
  `fai_cli::run(args, out, err) -> exit_code`. Tests/e2e + the incremental
  verifier live in the `fai-tests` crate (the literal top-level `tests/` from the
  original layout became `crates/fai-tests`).

Resolved while planning the syntax front end (lexer, parser, AST, incremental
queries, and formatter):

- **D26 Identifier interning:** a non-salsa `Symbol` wrapping `lasso::Spur`,
  resolved through a process-global `LazyLock<ThreadedRodeo>`, homed in
  `fai-syntax` (`lasso` added to `[workspace.dependencies]`). Keeps `Symbol` a
  `Copy` value with no `'db` lifetime, so the item tree is a plain `Eq` value and
  early cutoff stays sound within a process. Rejected: a db-scoped interner
  (forces `'db` through the lexer/parser) and hand-rolling (lasso is mature).
- **D27 Syntax tree & firewall:** per-category **arena AST** (`Expr`/`Pat`/
  `Type`/`Item` with newtyped ids) carrying **inline file-relative spans**. The
  incremental firewall is the **span-free item tree** (the value semantic queries
  depend on) plus `ItemId` = arena index as the stable id; the "AstId map" is
  `parse` output indexed by `ItemId`. Inline spans cost nothing incrementally
  because the firewall is the item tree, not the syntax tree. Per-body
  local-arena lowering (for body-level cutoff) is deferred to M2.
- **D28 Lexer:** emits **significant tokens** (`{ kind, range }`) plus a side
  `Vec<Comment>` (`Line`/`Block`/`Doc`); no whitespace/newline tokens (layout
  derives line/column from `LineIndex`). Character-literal vs type-variable is
  decided by **try-char-then-backtrack** (the documented "char when it closes,
  else type var" rule). Numeric grammar is full: decimal/`0x`/`0o`/`0b` integers
  with `_` separators and floats with optional fraction/exponent (a trailing
  identifier char is an invalid-suffix error). Escapes (string & char):
  `\n \t \r \0 \\ \" \' \u{…}`. Block comments **nest**; `///` is a distinct
  doc-comment kind.
- **D29 Layout:** a restricted **offside pre-pass** turns indentation into
  virtual `LayoutOpen`/`LayoutSep`/`LayoutClose` tokens so the parser stays
  layout-agnostic. A new line at the block's reference column starts a new item
  unless its first token is a **continuation token** (an infix operator, `else`,
  `then`, or `|`); greater indent continues, lesser closes. Blocks open after the
  module header, `=`, `->`, `then`, and `else` (when the next token begins a new
  line); a block body must indent strictly past its enclosing block (`FAI1021`).
  Tabs count as one column (quiet) and are normalized by `fai fmt`. Not the full
  Haskell layout algorithm — the canonical formatter normalizes input.
- **D30 Parser & AST shapes:** Pratt expression parsing, precedence tight→loose
  `.` > application > unary `-` > `* / %` > `+ -` > `:: ++` (right) >
  comparison/equality (left) > `&&` > `||` > `>>` > `|>`. Curried `App`; flat
  `Block { stmts, tail }` (sequential, non-recursive local `let`s); explicit
  `Paren` nodes; literals stored as their raw lexeme; `else` required; patterns
  limited to var/`_`/tuple/paren; types are var/con/app/arrow/tuple (record types
  deferred to M4). The binding `=` is consumed by the declaration parser, so `=`
  in expressions is always equality. **Error nodes in every category** with
  multi-level recovery (synchronize on layout `Sep`/`Close` and item keywords).
  `public` is accepted on signature and binding items; sig↔binding association and
  the "public needs a signature" rule are M2. A reserved-but-unimplemented
  construct (`type`, records, `match`, `interface`, nested `module … =`) emits
  **`FAI1030` "not yet supported"** and recovers, going dormant as M4/M5/M8 land.
  The module header is required, first, and the single top-level module
  (`FAI1022`).
- **D31 Comments:** attached **fine-grained to all nodes** via a per-category
  side-table keyed by node id (no node-struct bloat), placed Prettier-style
  (enclosing node → preceding/following sibling → same-line ⇒ trailing, own-line
  ⇒ leading, none ⇒ dangling). In the canonical formatter an **own-line comment
  forces its surrounding group to break**, so a commented construct never
  collapses. Doc is *derived* from leading `///` entries.
- **D32 Formatter:** `fai-fmt` is a **pure crate** (`format_module(&ParsedModule)
  -> String`) lowering the AST to a Wadler/Prettier **document IR** printed at
  **width 100**. It is **fully canonical** — input line breaks are ignored and
  the AST carries no layout hints — collapsing anything that fits and using fixed
  broken shapes otherwise (blocks always multi-line; branches via `then`/`else`
  blocks; leading-comma lists; signature + binding + contracts grouped with
  exactly one blank line between groups; trailing newline). Explicit parens and
  literal spellings are preserved verbatim.
- **D33 Front-end queries:** pure cores (`lex`/`layout`/`parse_module`/
  `build_item_tree`) wrapped by thin `#[salsa::tracked]` functions in
  `fai-syntax`. `parse(db, file) -> Arc<ParsedModule>` (AST + attached comments +
  a `has_errors` flag) emits parse diagnostics via the `Diag` accumulator.
  `item_tree(db, file)` is span-free and `Eq`/`Update` (names/kinds/visibility/
  order; `Error` items as anonymous entries) — the early-cutoff firewall;
  signature types are added in M2. `fai-db` gains `Db::all_source_files` and
  re-exports `salsa::Update`.
- **D34 `check`/`fmt` wiring:** the driver computes, the CLI does I/O.
  `check(db, files)` parses the filtered files and reports `Diag` (`ok` = no
  error-severity diagnostics). `fmt(db, files)` returns per-file results; the CLI
  writes changed files unless `--check`; the JSON envelope is `FmtOutput
  { schemaVersion, changed, diagnostics }` (the additive `diagnostics` reports
  files skipped for parse errors). The optional `[path]` argument is resolved to a
  `SourceFile` set by the CLI. The front end is one-shot in-process (the daemon is
  M3.5).
- **D35 Samples as files:** the language tour lives as canonical `.fai` files in
  **`samples/`** (one self-contained module per file), replacing the former
  `Samples.md`. The test suite buckets each file by parse result: zero diagnostics
  ⇒ must round-trip under `fai fmt` and be idempotent; ≥1 `FAI1030` ⇒
  future-surface, skipped; any diagnostic without `FAI1030` ⇒ failure (a real
  syntax bug). A known-module guard asserts the implemented-surface modules stay
  clean; files auto-promote to the round-trip set as later milestones land.

Resolved while implementing **M2** (the type-system layer):

- **D36 Cross-module access:** **qualified only**, no imports and no implicit
  workspace scope. A bare name resolves local → this-module top-level → prelude,
  never to another module. Another module's public binding is reached *only* as
  `Module.name`, which already parses as `Field { base: Var(Upper), field }` and
  is reinterpreted at resolution (depth-1; the `Upper`/lower casing convention
  decides module-ref vs record-field-access). No grammar change. Rejected:
  implicit workspace scope (ambiguous), `import` declarations (deferred; not
  needed for an agent-first language where terseness matters less).
- **D37 Module identity & uniqueness:** a module **is** its file (`SourceId` is
  the identity, stable under reformatting); the header name is a validated-unique
  display/addressing label. Two files with the same module name is an error
  (`FAI2007`) on **each** colliding file; their bindings still resolve locally
  but the duplicated name is excluded from cross-module lookup. The `Prelude`
  module name is reserved.
- **D38 Required signatures & visibility:** visibility lives on the **signature**
  (a marker on a binding that has a signature is `FAI2009`). A `public` binding
  without a signature is `FAI3003`; a signature that disagrees with the inferred
  body is `FAI3004` (the signature is checked, not trusted). One signature pairs
  with one binding (orphan/duplicate signatures and duplicate bindings are
  `FAI2005`/`FAI2006`/`FAI2004`).
- **D39 The firewall:** `module_exports`/inference of a binding depends on its
  callees' **declared signatures**, never their bodies. Cross-module signature
  lookup goes through a tracked `signature_scheme` query whose body-edit-stable
  value gives early cutoff, so editing a private body never re-checks another
  module. Proven by the incremental verifier + event-log tests.
- **D40 SCC granularity:** within a module, a signature **cuts** a dependency
  edge, so only signature-less bindings can form a cycle; such cycles are always
  intra-module, so SCCs are computed **per file** (`module_sccs`). An SCC is the
  inference cache unit; recursion inside a signature-less SCC is monomorphic,
  then generalized.
- **D41 Type representation:** an immutable, structural, span-free `Ty` (`Arc`
  tree) reified after solving; the mutable union-find solver is local to one
  inference call. Constrained type-variable flavors **Numeric** (Int/Float),
  **Eq** (non-function), and **Ord** (Int/Float/String/Char) stand in for type
  classes (deferred). Numeric defaults to `Int`; `=`/`<>` on a function type is
  `FAI3006`; no implicit Int/Float coercion (`FAI3001`).
- **D42 Operators:** `++` is **String-only** (lists use the prelude `append`);
  `::` is cons; `|>`/`>>` are pipe/compose; comparison is `Ord`, equality is
  `Eq`, arithmetic is `Numeric`. Constraint generalization is **lenient for Eq**
  (generalizes to `'a`, function misuse caught at concrete sites) and **strict
  for Numeric/Ord** (a constrained var that would generalize without a signature
  is ambiguous) — because M2 has no constrained schemes to carry the constraint.
  *Deviation (to revisit):* the current build *defaults* an escaping Numeric var
  to `Int` rather than reporting the strict ambiguity; sound and predictable, to
  be tightened when constrained schemes land. *(Amended by D75: operators become
  symbolic identifiers with F#-style precedence; the overloaded ones become std
  `Num`/`Eq`/`Ord` interface methods at M5.)*
- **D43 Prelude:** **hybrid, type-only in M2** — primitives are a Rust
  `name → Scheme` table (no bodies; codegen is M3), and a derived `.fai` prelude
  is embedded (`include_str!`) and loaded as a synthetic high-durability
  `SourceFile`; it is reachable unqualified everywhere (the one exception),
  excluded from default `symbols`/`check`, and shadowing a prelude name warns
  (`FAI2010`). *(Amended by D73/D74: the embedded library is now a curated,
  multi-file `std/`, and the Rust intrinsics are prelude-private `Prim.*`.)*
- **D44 Code intelligence:** `fai-ide` returns typed serde envelopes (one per
  command) with `schemaVersion`; targets address by `Module.name`, bare-unique
  name, or `file:line:col`. `refs`/`dependents` assemble reverse indices on
  demand from each file's cached resolution, keyed by `ExprId` with spans
  resolved late (firewall-safe). Results are deterministically sorted and
  best-effort under errors.

Resolved while implementing **M3** (the native thin slice):

- **D45 Capability shape (temporary):** the thin slice predates records (M4) and
  interfaces (M5), so `Runtime` is an **opaque built-in type constructor**
  threaded through `main` (`main : Runtime -> Unit`), and `Console.writeLine :
  Runtime -> String -> Unit` is a **qualified builtin** resolved through the
  existing prelude/qualified-name path (a `Console` builtin module). This honors
  "capabilities flow from `main`" without the record/interface machinery; M5
  replaces it with the real record form (`runtime.console.writeLine`). The
  sample `Hello.fai` is written in this form for now.
- **D46 `fai run` worker:** `fai run` JIT-compiles and executes in an **isolated
  worker subprocess** (a hidden `__run-worker` subcommand that opens its own
  session); stdio is inherited and the worker's exit code is returned. Timeouts,
  resource limits, and daemon-survival are deferred to M3.5 (R15).
- **D47 Object cache = salsa query:** `object_code(Def)` is a tracked query
  producing one relocatable object per definition; salsa's dependency graph *is*
  the content-addressed cache, and the per-function cache hit is asserted via the
  query event log. Symbols and arities feeding it are derived from
  **body-edit-stable** information, so the codegen layer keeps the M2 firewall.
  On-disk persistence is M3.5.
- **D48 Value representation:** a uniform 64-bit **LSB-tagged** word — immediate
  `payload<<1|1` (Int/Bool/Unit/Runtime), boxed = 8-aligned pointer (tag 0).
  `dup`/`drop` are tag-checked, so polymorphic code reference-counts correctly
  with no type information and immediates are RC no-ops.
- **D49 Int range under tagging:** the full **64-bit `Int` is preserved** via
  boxed overflow — immediate when it fits 63 bits, a heap `i64` object otherwise.
- **D50 Heap layout:** a descriptor-pointer header `{ rc, descriptor, size }`;
  static per-type descriptors carry a children-scan used at drop. Extensible to
  ADTs/records (M4).
- **D51 Function model:** closures `{ code, arity, env… }` with a uniform
  `apply_n` eval/apply handling exact, partial (a PAP object), and
  over-application. Top-level functions are static **immortal** closures (a
  zero-arity binding — a value, not a function — is forced on reference).
  Primitives lower to runtime calls. Every operation **consumes** its operands,
  so RC insertion reduces to dup-at-use + one drop per owned binding (no reuse;
  reuse is M6).
- **D52 Typed Core IR:** `fai-core` carries a `Ty` on every node, from a new
  `body_types` query, so M4 (record field offsets) need not retrofit types —
  even though M3 codegen leans on tagging and uses the types lightly.
- **D53 Entry & scope:** the entry file must define `public main : Runtime ->
  Unit`; the backend compiles only the transitive closure reachable from `main`
  (over the lowered `Global` references, so prelude helpers are included).
- **D54 Runtime embedding:** `fai-runtime` is **std-only**, so the driver's build
  script compiles it to a static archive with a single `$RUSTC` invocation and
  embeds it (`include_bytes!`); produced executables are self-contained. Host
  target only (cross-compilation is future). The runtime is also linked as an
  `rlib` so the JIT can resolve its symbols by address.
- **D55 Backend error range & runtime ABI:** the **`FAI7xxx`** range is owned by
  the backend (`fai-core`/`fai-codegen`/`fai-runtime`): `FAI7001` "construct not
  supported by the native backend yet" (e.g. `Float`, tuples, lists), reported
  only for *reachable* definitions. The runtime ABI (tagged values, the
  `fn(env, args) -> i64` calling convention, the `fai_*` symbols) is the contract
  shared by codegen and the runtime.

Resolved while implementing **M3.5** (the daemon, persistence, and protocol):

- **D56 Persistent object cache:** the on-disk cache lives in a **non-salsa
  wrapper** (`fai-driver`'s `load_or_build_object`) around the pure `object_code`
  query, so the query stays side-effect-free: a disk hit skips code generation; a
  miss generates then writes atomically (temp file + rename). The content key is
  **blake3** over a portable `fingerprint_def` (`fai-core`) — which renders every
  `Global` through the backend namer (module-qualified symbol + arity, never a
  process-local `DefId`/`SourceId`) and includes canonical node types — stamped
  with the target triple, compiler version, and a codegen-config tag. Derived
  `Hash` is unusable (interner ids and file indices are process-local). The cache
  is global per-user (`$FAI_CACHE_DIR` → platform cache dir; a process-global
  override for embedding/tests), unbounded for now (GC is future work), and
  benefits **AOT `build`** only (the JIT can't consume objects). Determinism of
  `object_code` (already verified) makes it sound.
- **D57 Daemon concurrency (serialized):** the daemon serves per-connection
  threads but serializes **all** database access through one `Mutex<Session>`
  (true serialization, sidestepping salsa's concurrent-read/cancellation
  machinery). Control messages and (later) `run` supervision stay off-lock.
  Concurrent reads + cancel-on-input-change are deferred to the performance
  milestone; the acceptance bar (warm speedup) needs only the warm DB.
- **D58 Transport:** the client↔daemon link uses the **`interprocess`** crate
  (sync) for one safe cross-platform code path — Unix-domain sockets on POSIX,
  named pipes on Windows — with our `u32`-LE + MessagePack framing layered on top
  and the Unix socket created `0600`. The endpoint name embeds a blake3 of the
  canonicalized root and the compiler version; binding is the spawn-race lock, and
  a stale socket from a crash is reclaimed (probe-connect → unlink → rebind).
  Windows is compiled but, given the Linux-only CI, **untested** (a Windows CI job
  is future work).
- **D59 Result exchange (rendered bytes):** because the thin client has **no
  database** (so it cannot resolve spans), the daemon runs the command and returns
  the already-rendered `{stdout, stderr, exit}`; the client passes its resolved
  `message_format`/`color` and writes the bytes verbatim. A single
  `fai_driver::run_command` powers both the daemon and the `--no-daemon` path, so
  warm output equals one-shot output by construction. This deviates from CLI.md
  §7.6's structured per-method results (a documented simplification); `$/output`
  for `run` is the streaming exception, and `$/diagnostic`/`$/progress` are
  deferred.
- **D60 Daemon detachment & lifecycle:** the client spawns a detached
  `__daemon-serve` (null stdio; on Windows the safe `DETACHED_PROCESS`/
  `CREATE_NEW_PROCESS_GROUP` flags) and the daemon calls **`nix::setsid()`** at
  startup on Unix so a terminal hangup can't kill it (no hand-written unsafe; the
  same `nix` crate later covers the worker's kill/`setrlimit`). The daemon shuts
  down on an explicit `Shutdown` or after an idle period
  (`FAI_DAEMON_IDLE_TIMEOUT`, default 600s), unlinking its socket on the way out.
- **D61 File-state sync:** before each request the daemon re-scans the workspace,
  **stat-gated** (mtime/size) and **hash-confirmed** (blake3), updating a salsa
  input only when content truly changed (so a `touch` doesn't break early cutoff).
  New files are added; deleted files are dropped from a live set (their input
  lingers harmlessly). A client dirty-set (`{path, hash|content}`) is honored as a
  scan-skip fast path; the CLI does not populate it (it is for an editor/LSP
  client), and unwritten overlays remain deferred.
- **D62 Routing & graceful fallback:** the routing layer sits **above**
  `fai_cli::run` (which stays the pure in-process executor, so the existing suite
  is unchanged); the daemon server calls `fai_driver` directly. `fmt`/`build` I/O
  is performed by whoever runs the command — the daemon writes the formatted files
  and links the artifact (client sends absolute paths), the `--no-daemon` path
  does it locally (a documented relaxation of "the CLI does I/O" for the daemon
  path). When the daemon is unreachable, the client warns (`FAI0005`) and runs
  in-process, so a daemon problem never breaks a command. New tooling codes:
  `FAI0005` daemon-unavailable (warning), `FAI0006` run-timeout.
- **D63 `run` via a warm IR bundle:** rather than re-deriving in a cold worker or
  shipping a JIT image (impossible across processes), the warm daemon front end
  lowers the closure reachable from `main`, serializes it as a portable
  **`WireBundle`** (`fai-core`'s `wire`), and hands it to the worker, which
  reconstructs `LoweredDef`s and JITs them with **no database** — so the warm DB
  accelerates `run`, not just `check`/`query`. The wire form drops node **types**
  (codegen ignores them) and renders every `Global` as a module-qualified
  `{module, name}` (the worker re-mangles via the same pure `mangle` the backend
  uses, assigning a synthetic `SourceId` per module label). The bundle travels via
  a temp file; the worker is unified — both the daemon and the `--no-daemon` path
  build a bundle and spawn the same `__run-worker`. (Transferring the warm bundle
  is the realistic best-latency option; the alternatives — cold re-derive, or AOT
  re-link per edit — are slower or contradict JIT-for-run.)
- **D64 Run supervision:** the daemon spawns the worker with piped stdio, streams
  it back as `$/output`, and enforces a wall-clock timeout (`FAI_RUN_TIMEOUT_MS`,
  default 300s) via a `wait-timeout` waiter that kills the worker on expiry
  (exit `124`); a crashing/runaway worker is a separate process, so the daemon
  always survives. The `--no-daemon` path runs the same worker with inherited
  stdio and no limits.
- **D65 Worker resource limits:** the worker self-imposes `RLIMIT_CPU` (seconds,
  from `FAI_RUN_CPU_SECS` set by the daemon) at startup via `nix` — robust
  runaway-CPU protection that doesn't interfere with JIT; `RLIMIT_AS`
  (`FAI_RUN_AS_BYTES`) is opt-in because a low cap can break compilation. Windows
  Job-Object limits are future work. No hand-written unsafe (the safe `nix`
  wrappers), and limits apply only under daemon supervision.

- **D66 ADT type & value representation:** a declared union is a nullary type
  head `Ty::Adt(AdtRef)` applied to its arguments through the existing `App`
  machinery (so `Option 'a` reuses ordinary type application); `List` keeps its
  dedicated `Con::List`. At runtime a **nullary constructor is an immediate**
  `(tag << 1) | 1` (no allocation); a constructor with fields is a boxed
  composite `{ rc, descriptor, size, tag, fields… }` sharing the tuple/record
  runtime. Constructors are ordinary functions (curried) typed by a
  `constructor_scheme` query.
- **D67 Resolution identity:** constructors, ADTs, and value defs get distinct
  newtyped ids (`CtorRef`, `AdtRef`, `DefId`); name resolution adds `Res::Ctor`
  so a capitalized head in an expression or pattern resolves to a constructor,
  with `FAI2012` for an unbound one and `FAI3011` for constructor arity misuse.
- **D68 Rows via a parallel union-find:** `InferCtx` carries a second union-find
  for **row variables** alongside the type one. A record type is
  `Ty::Record(RecordRow { fields, tail })` where `fields` are **sorted by label
  text** and `tail` is `Closed` or `Open(RowVarId)`; `Scheme` gained `row_vars`
  (and `row_names` for rendering). Sorting fields by label text **everywhere**
  (inference, layout, fingerprint) is what makes the monomorphic memory layout
  and the content-addressed cache key sound. Duplicate labels are `FAI3010`.
- **D69 Match & records lower to four Core primitives:** desugaring introduces
  `MakeData`/`DataTag`/`DataField` (plus a `Lit::Float`); `match` becomes a
  decision tree emitted as `DataTag` tests in `if`-chains with `DataField`
  projections, and records reuse the same nodes with **tag 0**. `DataTag` and
  `DataField` **consume** their base operand, which keeps them reference-count
  correct under the existing dup-at-use discipline.
- **D70 Structural ordering is lenient, like equality (amends D42):** `<`, `<=`,
  `>`, `>=` are admitted on **any non-function type** and lowered to a single
  runtime `fai_compare` (constructor tags order by declaration order, records by
  sorted label, recursively). Because ordering needs no dictionary, the generic
  `compare`/`sort`/`sortBy` and the `Dict`/`Set` BSTs are plain prelude code.
  *(Amended by D75: `< <= > >=`/`= <>` become `Ord`/`Eq` interface methods at
  M5 that specialize to this single runtime compare/equal on concrete types.)*
- **D71 The prelude is a real compiled module, not magic:** `Option`/`Result`,
  the `List` combinators, `compare`/`sort`, `Dict`/`Set`, and the string helpers
  live in an embedded `Prelude.fai` whose public values, types, and constructors
  are visible unqualified in every module. Only genuinely primitive operations
  stay in Rust as a small `INTRINSICS` set. `Float` is always boxed; the
  arithmetic/comparison primitive is selected from the operand type during Core
  lowering. *(Amended by D73/D74: split into a curated, multi-file `std/`; only a
  small `Prelude` module is auto-imported and the rest is reached qualified, and
  the `INTRINSICS` are prelude-private `Prim.*` re-exported under clean names.)*
- **D72 Row-polymorphic field-access codegen is staged with M5:** a **monomorphic
  closed** record compiles field access/update to a **constant offset**; a field
  access or `{ r with … }` on a **row-polymorphic** record that is *reachable
  from `main`* reports `FAI7002` ("row-polymorphic field access needs runtime
  offset evidence"), pending the offset-evidence/dictionary work in the
  interfaces milestone — the type system already infers the fully general
  signatures (e.g. `getX : { x : 'a | _ } -> 'a`). New M4 diagnostics:
  `FAI3012` (type-constructor arity), `FAI3013` (recursive alias),
  `FAI4001`/`FAI4002` (non-exhaustive / unreachable `match`). The unused
  `FAI3009` is retired (the catalog test allows the `FAI4xxx` range in
  `fai-types`).
- **D73 The standard library is a curated, multi-file `std/` (amends D43, D71;
  the "no qualified-type syntax" clause is superseded by D88):**
  the embedded library moves from a single `crates/fai-types/src/Prelude.fai` to
  real `.fai` modules under a top-level **`std/`**, embedded at build time by
  `crates/fai-types/build.rs` (a generated `include_str!` table) and loaded as
  synthetic high-durability inputs under the `<std>/` path namespace
  (`fai_db::is_std_path`, shared so name resolution can classify a file without
  depending on the loader). Auto-import becomes **curated, Elm-style**: a single
  module **`Prelude`** is visible unqualified everywhere — forced by the grammar,
  since there is no qualified-type syntax and no opaque types, so every
  user-facing type and its constructors must be auto-imported. `Prelude` owns
  `Option`/`Result`/`Dict`/`Set` (+ constructors) and the free functions
  `identity`/`const`/`not`/`compare`; **every other operation is reached
  qualified** through a per-type module (`List.map`, `Option.withDefault`,
  `Dict.insert`, `String.split`, `Int.toString`, `Float.sqrt`, …). So
  `Prelude`/`List`/`Option`/`Result`/`Dict`/`Set`/`String`/`Int`/`Float` are
  reserved module names; `Dict`/`Set` still expose their node constructors (no
  opaque types yet — noted as future work). Auto-import is a pure tracked
  `prelude_exports` (the merged interface of the auto-imported set, keyed on the
  public **name set** for early cutoff: a Prelude *body* edit recomputes nothing
  downstream) shared by resolution and the type-name fallback; the `Prelude`
  module is located **among `std/` files only**, so a stray user `module Prelude`
  cannot hijack or collapse auto-import. The whole sample/fixture/test corpus is
  rewritten to the qualified form (a hard cutover; no compatibility aliases).
- **D74 Intrinsics are prelude-private (`Prim.*`) (amends D71):** the Rust
  intrinsics are no longer bare names anywhere. They are reached only as
  `Prim.<name>`, and only from inside `std/` modules (`FAI2014` otherwise); the
  standard library re-exports the user-facing ones under clean qualified names
  (`Int.toString` wraps `Prim.intToString`, `String.split` wraps `Prim.split`,
  `Prelude.not` wraps `Prim.not`, …), adding one call of indirection per
  intrinsic until an inliner exists. New resolution diagnostics: **`FAI2013`** (a
  name exported by more than one auto-imported module — contributor-facing,
  detected by the auto-import merge so it stays unit-testable even while the
  auto-imported set is a single module) and **`FAI2014`** (`Prim` referenced
  outside `std/`). The `INTRINSICS` name list moves to `fai_resolve::intrinsics`;
  the loader and built-in `Scheme` table move to `fai_types::std_lib`
  (`load_std`/`builtin_scheme`).
- **D75 Operators are symbolic identifiers with F#-style precedence; the
  overloaded ones are std interface methods; user-defined operators are allowed
  (amends D11, D42, D70; delivered with M5):**
  - An operator is a **symbolic identifier** (a maximal run of operator
    characters), written infix and named in value position as `(op)` — e.g.
    `let (+++) a b = …`, `List.foldl (+++) z xs`. The lexer becomes a generic
    operator-character lexer (maximal munch); the symbols that are *syntax* rather
    than operators stay reserved (`=` binder, `|`, `->`, `.`, and the list-cons
    `::`).
  - **Precedence/associativity are F#-style — a pure function of the operator's
    leading symbol(s)** (a fixed symbol-class → precedence/associativity table
    seeded by today's `binding_power`). **No fixity declarations**, so `parse`
    stays self-contained (precedence needs no name resolution or imports) and the
    incremental firewall is preserved.
  - **Resolution:** an operator resolves like any value name — local →
    this-module top-level → auto-imported `Prelude`. Built-in operators live in
    `Prelude`. A user operator is usable infix **within its defining module**;
    there is no qualified-infix form, so cross-module sharing means defining it in
    `Prelude` or accepting module scope (consistent with D36).
  - **Overloading via interfaces (M5):** `+ - * / %` become methods of a std
    **`Num`**, `= <>` of **`Eq`**, `< <= > >=` of **`Ord`**, with `Int`/`Float`/
    structural instances in `std/`. The M2 constraint flavors
    (`Numeric`/`Eq`/`Ord`) are replaced by these interface constraints; `Num`
    keeps the `Int`-defaulting rule. **Monomorphic uses still lower to the direct
    primitive** (e.g. `IntAdd`), so concrete-type operators pay no dictionary
    cost.
  - **Stays built-in regardless:** `&&`/`||` remain short-circuit sugar over `if`
    (a strict function cannot short-circuit); `::` stays the built-in `List`
    constructor. `|>`/`>>` may be redefined as ordinary `Prelude` operators (they
    are plain higher-order functions), inlined when monomorphic.
  - **Sequencing:** the lexer/precedence/user-operator half may precede M5 but
    lands unified with the interfaces work so built-in and user operators share
    one mechanism (no throwaway hybrid).

Resolved while implementing **M6** (reuse & in-place update):

- **D76 Precise reference counting is the foundation; reuse layers on it.** The
  scope-end dup-at-every-use scheme cannot reuse a matched cell: `Drop{x; body}`
  releases `x` *after* `body` runs, so the cell is freed after any reconstruction
  inside `body`. `fai-rc` is rewritten to precise, ownership-based counting so a
  dead cell is released exactly where reuse will recycle it. The pieces:
  - **A-normal form (in `fai-rc`).** Each function is normalized so every
    operation operand is an atom; compound operands bind to fresh `let`s. This
    makes sequence points explicit (so the dup/drop rules are exact) and makes
    every projection base a local — including a projection off a forced
    zero-arity global, which **must** be bound so the value it allocates is
    released. Done in `fai-rc` rather than lowering (same effect, all
    reference-counting normalization in one place; observable semantics
    identical). Chosen over a narrower "bind only temporary projection bases."
  - **Borrowing projections.** `fai_data_field`/`fai_data_tag` no longer drop
    their base; they read through it (the field is duplicated out). The base is an
    ordinary owned local that reference counting drops once at its last use — the
    drop that reuse will turn into a reset.
  - **Drop-early, dup-only-when-live.** A consuming use is preceded by `Dup` only
    when the value is still needed afterward; the last consuming use transfers
    ownership. An owned binding whose last use is a borrow (or which is unused) is
    dropped right after that use. Per-branch drops handle `if`.
  - **`MakeClosure` consumes its captures.** The capture duplication moves from
    code generation into the IR (explicit `Dup` nodes), so "operations consume
    their operands" holds uniformly; `make_closure` stores the values directly.
  - **Borrow-signature seam.** The consume-vs-borrow classification of call
    arguments and primitive operands flows from a provider that currently reports
    every argument owned (matching prior behavior); inferred argument borrowing
    fills it in a later stage.
  - **Soundness guard.** An abstract reference-count interpreter walks the
    reference-counted IR on every path (owned consumed-or-dropped once; no
    use-after-release or double drop; captures never dropped; consistent branches)
    over a corpus and a whole reachable program.

- **D77 Reuse recycles a dead cell into a same-size construction.** On the precise
  reference-counted IR, `fai-rc` rewrites the release of a dead, data-typed cell
  into reuse:
  - **IR.** A new `Reset { value, token, body }` releases the cell and binds a
    reuse `token`; `MakeData` gains an optional `reuse` slot. The token is a raw
    value — never duplicated or dropped by ordinary reference counting — consumed
    by exactly one construction per path. Both flow through the daemon-run wire
    form.
  - **Runtime.** `fai_drop_reuse(v)` decrements `v`; if it was unique it runs the
    cell's child scan and returns the cell's raw memory as the token (without
    freeing or untracking it), otherwise the null token. `fai_reuse(token, …)`
    builds into the token's memory in place when it is non-null and exactly the
    right size, else allocates fresh (freeing a wrong-sized token). A cumulative
    `ALLOCATIONS` counter (incremented only on real allocation) makes reuse
    observable.
  - **Reset at the death point, not the construction.** The reset is placed where
    the cell dies — at its last projection, *before* any recursive call — so the
    cell's now-released fields (e.g. a list tail) are unique for that call and can
    themselves be reused; the token is threaded forward to the construction. (A
    reset placed just before the construction would only recycle the outermost
    cell, since the parent's tail projection keeps the tail shared.)
  - **Branches.** Where an `if` precedes the construction, the release is pushed
    into the branches: a branch that reconstructs resets-and-reuses; one that does
    not drops — so no path leaks an unconsumed token and no separate
    free-token operation is needed.
  - **Same size, checked at runtime.** Pairing is greedy (the first construction
    on a path); the runtime size check makes any pairing correct, recycling in
    place only when the sizes match and otherwise falling back to allocation. Only
    data-typed cells (records, tuples, ADTs, lists, interface dictionaries) are
    reset, recognized from `let`-binding types.
  - **Acceptance.** `map`/`filter`/`inc` over a unique list allocate zero fresh
    cells; a shared list copies (the rc==1 guard). A differential allocation-count
    test pins both, and the soundness interpreter is extended to reset/reuse
    (a token created once, consumed once per path).

- **D78 In-place `{ r with … }` and immediate drop specialization.**
  - **Row-polymorphic update** (`fai_record_update`): when the record is the
    unique owner it overwrites the field in place (releasing the old one) and
    returns the same object; only a shared record is copied.
  - **Monomorphic update** rides the reuse mechanism (it lowers to a record
    construction). For that to recognize the record as unique, lowering reads the
    *unchanged* fields from a single base local rather than binding an alias
    (`let s = base`): an alias split the base's reference count, because the
    new-value expressions read the original. So a unique monomorphic `{ r with … }`
    is rebuilt in place; a shared one copies. A differential allocation count pins
    both the row-polymorphic and monomorphic cases.
  - **Two-stage normalization detail that this needs:** A-normal form **flattens**
    sub-operand bindings into one straight-line sequence (rather than nesting them
    in a `let` value), so a value's last use — and thus its drop/reset — sits at
    the outer level where the following construction can recycle it. Without
    flattening, a record's last field read (and its drop) nests inside a `let`
    value while the construction sits outside, out of the reuse pass's reach.
  - **Drop specialization (scoped):** code generation carries a `local → type` map
    (from `let` value types) and **omits** dup/drop of statically-immediate values
    (`Bool`/`Unit`/`Char`), whose reference-count operations are no-ops. Deeper
    specialization — inlining a known data cell's child drops and free to skip the
    descriptor dispatch — is **deferred**: after reuse, data cells are rarely
    dropped on hot paths, and inlining the reference-count/free logic (complicated
    by immediate-versus-boxed constructors) carries memory-safety risk
    disproportionate to the gain. Revisit with the performance milestone.

- **D79 Argument borrowing — sound by construction, self-contained inference,
  two-entry-point ABI.** Borrowing lends a parameter (the caller keeps ownership)
  instead of transferring it, cutting dup/drop churn for inspectors.
  - **Always sound.** A borrowed parameter is treated exactly like a capture
    (duplicated on a consuming use, never dropped), and the caller releases it at
    its own last use. So the inference can never cause a leak or double free — it
    is purely a performance choice.
  - **Direct calls (prerequisite).** Code generation gained a direct-call path: a
    saturated application of a top-level function calls its code symbol directly
    (null environment — top-level functions capture nothing), skipping the generic
    `apply_n` and the static closure. Borrowing is exploited only at such direct
    calls; partial/over/indirect applications stay all-owned.
  - **Self-contained inference.** `borrow_signature(def)` inspects one function's
    body: it lends a parameter that only flows to inspecting positions (projection
    bases, primitive operands, borrowed self-call arguments), and owns one that
    *escapes* (returned, stored in a constructor/closure, or passed to a function)
    or is *matched-and-reconstructed* (so the matched cell is reused in place — a
    field-transformed rebuild like `(x + 1) :: …` still owns). Self-recursion is a
    local monotone fixpoint; every other call's arguments are treated as consumed.
    The query never reads another function's signature, so it is **acyclic** and
    the cross-module firewall holds (a caller depends on a callee's small
    signature, computed at the call site). Row-polymorphic functions (curried
    through evidence) stay all-owned. Cross-module borrowing (an inter-procedural
    fixpoint) is a future refinement.
  - **Two-entry-point ABI (escape without whole-program analysis).** A function
    that borrows a parameter is compiled with a borrowed entry (used by direct
    callers) and an owned-ABI wrapper that calls the entry and then releases the
    borrowed arguments; the static closure (the first-class value form reached via
    `apply_n`) points at the wrapper. So passing a borrowing function as a value
    is sound with no escape analysis. Chosen over the planned "escaping functions
    forced all-owned," which on implementation needs a whole-program escape
    analysis that does not fit the per-definition incremental model. The borrow
    signature travels with the lowered definition (`entry_borrowed`), through the
    cache fingerprint and the daemon-run wire form.
  - **Primitive borrowing** (inspect-only `=`/`compare`/string reads) is left as a
    refinement: on the hot path (a `match` tag test) it would add a no-op drop, so
    it is not worth it without a per-operand-type guard.

Resolved while implementing **M7** (contracts: examples & properties):

- **D80 The property-testing framework is a dogfooded standard-library module.**
  Because Fai has **no implicit instance resolution** (interfaces are explicit
  values, D4/D13), a QuickCheck-style library cannot pick a generator by type on
  its own — so the type→generator mapping must live in the compiler regardless of
  where the generator *code* runs. Given that, the generators/shrinkers/renderers
  are written **in Fai** (`std/Test.fai`) for dogfooding and user extensibility,
  and the compiler composes them. `std/Test.fai` defines `type Gen 'a = Size ->
  Seed -> ('a * Seed)` (a pure splitmix64 over the seed — deterministic, no
  `Random` capability), `type Arbitrary 'a = { gen, shrink, show }` (a closed
  record — constant-offset access, no row evidence), `type TestResult = Passed |
  Failed String`, the combinators, and the `checkExample`/`checkForall` driver
  (the trial loop + greedy shrink run in Fai). Rejected: a Rust-side generator
  (not dogfooded; loses user-extensibility) and a generic `TypeRep`+`Dyn` Fai
  interpreter (a `Dyn` universal value cannot be coerced to a binder's real static
  type without dependent types).
- **D81 `forall` binders are patterns; residual type variables default to `Int`.**
  `Forall { binders }` carries `PatId` (`PatKind::Var`) rather than bare `Symbol`s,
  so binders flow through resolution (`pat_locals`), inference (`bind_param` →
  `pat_type`, which closes the prior "binders type as `Error`" gap), and lowering
  (`param_local`) exactly like function parameters. A new `contract_body_types`
  query infers a contract body with the binders bound and then **monomorphizes**:
  every residual unconstrained type variable becomes `Int`, so the harness lowers
  to monomorphic code and the generators know each binder's shape. `Int` is the
  standard QuickCheck witness; for parametric functions parametricity makes the
  choice irrelevant, and structural `=`/`compare` work at `Int`.
- **D82 Synthesis: dedicated, plain (non-tracked), in `fai-contracts`.** Contracts
  are lowered by parallel pieces that leave the normal-def queries untouched (so
  the cross-module firewall and perf guards are unaffected): `contract_body_types`
  (`fai-types`), the exposed `lower_params_body` (`fai-core`), and `rc_lowered`
  (`fai-rc`). `fai-contracts::synthesize` (a plain function — JIT execution needs
  no `object_code`, so no salsa key is required) builds, per contract, a *property*
  def (`contract#k$prop`: `fun binders -> body`, or a single tuple projected back
  out for ≥2 binders) and an *entry* def (`contract#k : Seed -> Int -> Size ->
  TestResult`) that calls the `Test` driver with an `Arbitrary` composed from the
  combinators for the binder types. Synthesis (and the `Test`/`Arb` name
  knowledge) lives in `fai-contracts`, not `fai-core`, which stays
  testing-agnostic.
- **D83 In-process JIT execution; one module per run; leak-guarded.** `fai test`
  runs in-process (matching the existing CLI wiring). The driver builds **one**
  `JitProgram` (`fai-codegen`) from all runnable contracts' synthesized defs plus
  the deduped reachable callees, fetches each contract's static-closure pointer,
  and applies it via the runtime's safe `apply` wrapper, decoding the returned
  `TestResult` (`Passed`/`Failed counterexample`). After the run it asserts the
  runtime's global live-object count returned to its baseline (an RC soundness
  guard). A contract whose reachable closure fails to compile, or whose lowered
  body contains an error placeholder, is reported rather than run. Isolated-worker
  execution + daemon `$/testEvent` streaming are deferred follow-ups.
- **D84 Diagnostics & output.** `FAI6001` (`CONTRACT_FAILED`, error) for a failing
  `example`/`forall`, located at the contract span, with the shrunk counterexample
  in its help (binder names + the Fai-rendered value); `FAI6002`
  (`CONTRACT_NOT_RUNNABLE`, **error** — an untestable contract fails the run) for a
  binder with no generator. Each contract associates with the nearest preceding
  top-level binding (its "subject"), which powers the `Contract` lists in
  `fai query docs`/`api` and the nullable `symbol` in the `TestOutput` JSON
  envelope (`{ total, passed, notRun, diagnostics, ok }`).
- **D85 Bitwise `Int` intrinsics + float bit-reinterpretation.** splitmix64 needs
  bitwise ops, which Fai lacked. They are **functions** in the `Int` module
  (`and`/`or`/`xor`/`complement`/`shiftLeft`/`shiftRight`/`shiftRightLogical`),
  not operators (symbolic forms collide with `>>` compose, `|` union/pattern, and
  `&&`/`||`); shift amounts are masked to `0..63`, and there are two right shifts
  (arithmetic `shiftRight`, logical `shiftRightLogical`). Full-domain float
  generation (incl. NaN/inf) needs bit reinterpretation, added as
  `Float.fromBits`/`Float.toBits`. Both are ordinary `Prim.*` intrinsics
  re-exported under clean names, mirroring the existing intrinsic wiring.
- **D86 Generation policy (Stage 1).** Deterministic by default (a fixed seed; a
  `--seed` flag overrides), 100 trials, size ramping `0..maxSize` with `Int` drawn
  from `[-size, size]` and `List` length ≤ size — bounded so `abs`/`clamp`-style
  laws hold (no `i64::MIN`/overflow surprises). Generators cover the primitives
  and built-in constructors via the std combinators (which the compiler composes);
   **`Char` is omitted** (the native backend does not support it yet, so a `Char`
   binder is `FAI6002`).
- **D87 Per-type `Arbitrary` synthesis for records and ADTs (Stage 2).** A user
  record or ADT has no generic combinator, so the compiler synthesizes a
  top-level `Arbitrary` *definition* per type, referenced as a `Global`. Two
  properties make this tractable without a by-hand closure-conversion pass:
  (1) because each type's arbitrary is a top-level def, composing them is just
  `Global` references, and a **recursive type is a self-reference** (the def's
  generator refers to its own `Global`) guarded by the size budget — at size 0
  only non-recursive constructors are chosen, and recursive fields are generated
  at `size - 1` (so no `Arb.fix` combinator is needed); (2) every synthesized
  function is **capture-free** — a value it would otherwise close over (the record
  being shrunk, the constructor being rebuilt) becomes a **leading parameter
  supplied by partial application**, so the runtime forms the closure (e.g.
  `List.map (setField r) …`). A record's generator builds the record literal and
  threads the seed through field generators; its shrinker shrinks each field via a
  partially-applied setter; its renderer prints `{ l = … }`. An ADT's generator
  picks a constructor with a (private) `Test.choose` and builds it; its
  shrinker dispatches on the tag, shrinking fields (rebuilt via per-constructor
  setters) and yielding recursive subterms; its renderer dispatches on the tag and
  parenthesizes a constructor argument only when it actually renders with a space
  (`Test.parenIfSpaced`). Field types come from `constructor_scheme` instantiated
  against the binder's concrete type arguments. Mutually-recursive ADTs and
  recursion only reachable through a collection field (e.g. `Rose (List Rose)`)
  are not size-guarded yet; a true fuel parameter is future work.

Resolved while implementing **nested modules & qualified-type syntax** (surface
completeness):

- **D88 Nested modules group declarations under a qualified path; qualified-type
  syntax is introduced (amends D73's "no qualified-type syntax").**
  - **Representation & identity.** A nested `module Name = <indented items>` is an
    `ItemKind::Module { name, body }` whose children are entries in the file's one
    shared item arena (so every item, nested or not, keeps a single-index
    `ItemId`); `Module.roots` lists the top-level items. A nested member is keyed
    by a **qualified `Symbol`** (`Internal.pi`, `Outer.Inner.Shape`), so
    `DefId`/`AdtRef`/`CtorRef`/`InterfaceRef` stay `(SourceId, Symbol)` and `Copy`,
    the content-addressed cache and the daemon wire form need no change (the
    backend namer already escapes `.`), and a top-level name keeps its bare
    spelling (qualified == bare). Chosen over a structured path id (which collapses
    to this once the path is interned).
  - **Scoping.** Transparent lexical nesting: a bare name resolves locals → this
    scope → enclosing scopes → the auto-imported `Prelude`, inner shadowing outer;
    the enclosing file sees *every* nested member (no privacy edge inside a file),
    while another file sees only `public` members. A qualified field/con chain
    resolves by a greedy path walk — leading segments that name a visible module
    (same-file nested first, then a workspace file module, then nested modules
    within it) form the module path, the next segment is the member, and any
    further segments are record-field accesses; same-file access is ungated,
    cross-file requires the member `public`. Mutual recursion and SCCs stay
    per-file over qualified `DefId`s. A nested module takes no visibility marker
    (`public module` is rejected). New diagnostics: `FAI2016` (a nested module's
    name collides with a module/type/interface/constructor in the same scope) and
    `FAI2017` (a module name used where a value/type is expected).
  - **Qualified-type syntax.** A dotted upper-case path in type position is one
    `TypeKind::Con` with an interned dotted name, resolved the same way as values
    (lexically when bare, by path walk when dotted); this also enables cross-file
    `File.Type` for top-level types, which D73 had ruled out. A constructor
    application is identified by the type's **resolved canonical** qualified name,
    so `T` (inside its module), `Inner.T` (enclosing), and `File.Inner.T` (another
    file) all unify. Constructors in patterns parse a dotted head (`Inner.MyCtor`).

- **D89 As-patterns `p as name`.** A new reserved keyword `as` introduces the
  alias pattern `PatKind::As { pat, name }`, which matches `pat` and also binds
  the whole matched value to `name`. It binds **looser than every other pattern
  form** (parsed at the top of the pattern grammar), so `(A | B) as w` and
  `(x :: xs) as w` alias the whole alternative/cons. The alias name is keyed by
  the as-pattern node for binding and typing (it has the scrutinee's type); the
  inner pattern is checked/compiled against the same value; exhaustiveness treats
  `p as name` exactly as `p`. As-patterns are allowed wherever a pattern is
  (match arms, `let`, parameters); `forall` binders stay plain variables.
  (Reserving `as` is safe — it appeared only in comments across the corpus.)

- **D90 Advanced code-intelligence queries (`callers`/`callees`/`caps`/`search`,
  `dependents --transitive`).** All build on the resolution reference graph
  (`deps_by_def`) and the type queries; no new compiler phase.
  - **Call hierarchy & transitive dependents.** `dependents --transitive` is a
    breadth-first walk of the reverse reference graph. `callers`/`callees` return
    edges with per-edge *sites*: `callees` walks the target's body collecting
    referencing expressions; `callers` finds referencing definitions (reverse
    graph) and walks each for its sites. The graph is the raw reference graph
    (every reference), not the signature-firewalled SCC graph, so the hierarchy is
    complete.
  - **`caps`** reads a function's directly-requested capabilities from its
    signature — a bare interface parameter, or a record parameter's
    interface-typed fields (so `{ console : Console | _ }` and a `Runtime` both
    surface their capabilities) — then adds those of its (transitive) callees over
    the forward call graph, tagged with the callees they come through. Because
    capabilities are explicit values, a well-typed function's signature already
    captures its footprint; the transitive pass covers capabilities a callee
    requests that the caller only constructs locally.
  - **`search`** matches a type pattern structurally against each definition's
    type, **without lowering the pattern through the type checker** (which would
    emit diagnostics outside a tracked query): the pattern is parsed as a
    signature and both sides are normalized to a shape tree. A pattern type
    variable is a hole binding consistently to a candidate subtree; an open
    pattern record admits extra candidate fields (row polymorphism); names match
    by qualified or local segment. An alpha-equivalent match scores highest; a
    hole-to-concrete or loose-name match scores lower. Search spans the whole
    workspace, the standard library included.
- **D91 Language server (`fai-lsp`).** A standard LSP server, speaking JSON-RPC
  over stdio, reusing the warm workspace `Session` and the `fai-ide` engine so its
  answers match the `fai query`/`fai check` ones; `fai lsp` runs it (the server
  owns its own stdio loop, bypassing the CLI's command envelopes). Editors use it;
  agents use `fai query`.
  - **Transport.** The `lsp-server` (synchronous framing/connection) and
    `lsp-types` crates. `lsp-types` is pinned at `0.95` because `0.97` replaced
    `Url` (with `to_file_path`/`from_file_path`) with a `Uri` type lacking
    filesystem-path helpers.
  - **Surface (v1).** Full-document `textDocument` sync, `publishDiagnostics`,
    `hover`, `definition`, and `formatting`. Open buffers are overlaid into the
    database as in-memory edits, so analysis tracks unsaved changes; diagnostics
    reuse `fai check` and formatting reuses `fai fmt`.
  - **Position-addressed queries.** Hover and go-to-definition are offset-keyed
    (an editor addresses a byte position, not a name), so `fai-ide` gains
    `hover_at`/`definition_at`: find the innermost expression containing the
    offset (walking outward when it carries no resolution), then report its
    inferred type or jump to what its reference resolves to — a definition, a
    constructor variant, or a local's binding pattern.
  - **Positions.** LSP positions are `(0-based line, 0-based UTF-16 unit)` while
    Fai spans are UTF-8 byte offsets; a per-document line map converts both ways
    (exact across non-BMP characters), clamping an out-of-range column to the
    line's content rather than spilling onto the next line.

Resolved while implementing the **inference-tuning** and **primitive-borrowing**
performance work (measurement-driven; correctness-neutral — inferred types,
diagnostics, and program output are unchanged, guarded by the full type/golden
suite):

- **D92 Solver representation & read walks: `Rc`-shared types, borrowing, and
  memoization.** The mutable solver's `SolveTy` represented application/arrow
  children with `Box`, so `resolve_shallow` deep-cloned the whole structure on
  every call — quadratic when unifying large types, cubic for the occurs check
  over a growing curried type (the dominant cost the benches surfaced). The fix:
  application/arrow children are now `Rc`, so resolving/cloning a representative
  is O(1); the read-only walks (occurs, free-variable collection) **borrow**
  (no clone) and **memoize** bound representatives, so a variable shared across a
  type (a DAG, e.g. `(p, p)` repeated) is expanded once. Unification also
  **path-compresses** the variable→variable chain it walks (only variable links
  are rewritten, never structures), keeping the repeated resolution of long
  result-variable chains (left-nested arithmetic / if-else) linear. Always-on
  thread-local **work counters** (`fai-types/src/perf.rs`) make this observable
  and let `perf_guards.rs` gate the complexity deterministically. Rejected as
  measured-not-worthwhile: a structural "variable-free" cache to skip ground
  subtrees in the occurs walk (a residual O(n²) over long *ground* application
  chains) — once `Rc` removed the clone cost the residual walk is microseconds,
  dominated by fixed overhead, and the cache only added a per-node lookup to every
  unification (it regressed the common path), so it was dropped.

- **D93 Local-`let` generalization by binding level (rank-based).** Generalizing a
  local `let` recomputed the environment's free type variables by walking every
  in-scope local — O(n²) in block size. Replaced with the standard rank/level
  scheme: each free variable records the binding depth at which it was created, a
  generalizable `let`'s right-hand side is inferred one level deeper, unifying a
  variable with an outer one lowers its level (fused into the occurs walk, which
  now also lowers as it goes), and the `let` quantifies exactly the variables
  whose level is deeper than the enclosing scope — equivalent to "free in the
  value but not the environment", computed in time proportional to the value's
  type, not the environment's size. Top-level/SCC generalization is unchanged (it
  has no enclosing environment, so it still quantifies every remaining free
  variable). The value restriction, the constrained-variable exclusion (so a
  `Numeric` local still defaults to `Int`), and the "locals do not generalize row
  variables" behavior are all preserved exactly.

- **D94 Primitive borrowing for inspect-only operations (two-variant ABI).** `=`,
  structural `compare`, and the `String` readers only inspect their operands, yet
  the runtime consumed (dropped) them, forcing a caller to duplicate a value it
  still needed (and, by sharing it, defeating in-place reuse). They now have
  **non-consuming runtime variants** (`fai_equal_borrowed`, …), and the operands
  are **borrowed when boxed**. One predicate — `Prim::borrows_operand`, on the
  operand type, in `fai-core` — drives reference counting, the RC soundness
  interpreter, and code generation's choice of runtime symbol, so the caller's
  drop and the runtime's (non-)consumption always agree by construction.
  **Immediate operands keep the consuming variant** (the hot `match` tag-test path
  is unchanged), so borrowing only applies where it removes real dup/drop churn.
  Chosen over a single uniformly-non-consuming variant, which would have pushed
  drops and let-bindings onto the immediate operand path.

- **D95 Intra-build parallelism via per-worker database clones.** salsa databases
  are `Send` but not `Sync`, so a `&dyn Db` cannot be shared across threads;
  parallelism instead gives each worker its own database handle (a cheap clone
  sharing the underlying storage and memoization, with salsa coordinating
  concurrent query execution). To keep the `&dyn Db` seam, the `Db` trait gains a
  `clone_box(&self) -> Box<dyn Db>` and `Box<dyn Db>` implements `Clone` (via
  `clone_box`; sound by the orphan rule because `Box` is `#[fundamental]` and
  `dyn Db` is local), so it is `Clone + Send` and works as a rayon `map_with`
  seed (cloned per worker). The per-definition AOT object emission and the
  lower/reference-count gathers for the run paths (`jit_run_program`,
  `build_run_bundle`) run across the rayon pool; **order is preserved** (indexed
  `collect`), so the linker input and the run bundle stay deterministic. This is
  **intra-command** parallelism only — it does not change the daemon's
  per-command serialization (D57); concurrent *requests* remain future work. (The
  JIT compile's per-function code generation is parallelized too — see D96.)
  Chosen over a generic `build_native<DB: Db + Clone>` (which would push the
  concrete database type through the driver seam) and over sharing a `&dyn Db`
  (impossible: not `Sync`).

- **D96 Parallel per-function JIT code generation (compile/define split).** The
  JIT compiles all definitions into one shared `JITModule`, which a worker cannot
  mutate concurrently, so the naive loop is serial. Split the work the way
  `Module::define_function` does internally: build each function's Cranelift IR
  serially (it must mutate the module — declaring callees, runtime imports, and
  string data), then run `Context::compile` (the expensive
  legalize/register-allocate/encode step) **in parallel** across a rayon pool —
  it needs only the module's read-only, `Sync` ISA — and finally register the
  compiled machine code serially via `define_function_bytes`. Code generation is
  factored into `build_def`/`build_fn` (build only, returning an uncompiled
  `Context`) shared by both back ends: the AOT path defines each context
  serially (it is already parallel across whole per-definition modules, D95), the
  JIT path compiles the collected contexts in parallel. The remaining serial
  parts — IR building and `finalize_definitions` (linking/relocating the shared
  image) — bound the speedup (~1.4× on a 200-tiny-function program; more as
  per-function code grows). Parallelizing IR building too would require
  pre-declaring every symbol so building never mutates the module — a larger
  refactor, deferred.

To change a locked decision: update this log **and** the table in `AGENTS.md`,
and note the migration in the affected milestones.

---

## Deferred to v2 (explicitly out of scope for now)

- Type-level **effect rows** (`a -> b / {Console, Net}`) layered over the
  capability-as-values model.
- Record **extension/restriction** operators (`{ r | z = ... }`, field removal)
  and scoped/duplicate labels.
- A **package manager** / multi-package builds and a project manifest beyond a
  single entry file.
- Additional backends and self-hosting.
