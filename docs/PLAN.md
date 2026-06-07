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
**Status:** in progress. Two stages are built. (1) **Precise, ownership-based
reference counting**: `fai-rc` normalizes each function to A-normal form and
inserts dup/drop precisely (duplicate only when a value is still live afterward;
drop at the last use rather than scope end; per-branch drops), with projections
(`DataField`/`DataTag`) **borrowing** their base so a matched value survives its
projections and is released once by reference counting. (2) **Reset/reuse**: a
dead data cell's release becomes a `Reset` at its death point that yields a reuse
token (its raw memory if unique, else null), threaded forward to a same-size
construction that builds into it in place (`fai_drop_reuse`/`fai_reuse`); `if`
pushes the decision into branches (reset-and-reuse where one reconstructs, drop
where not). A `map`/`filter`/`inc` over a *unique* list now allocates **zero**
fresh cells, while a shared list falls back to copying (the runtime rc==1 guard),
proven by a differential allocation count. A borrow-signature seam is in place
(every argument owned for now), and an abstract reference-count interpreter over
the IR guards soundness across a corpus and whole programs. The remaining stages —
in-place `{ r with … }`, drop specialization, and inferred argument borrowing —
are future work; see `docs/reuse-plan.md` for the staged design.

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
**Goal:** run the first-class `example`/`forall` declarations (typed in M2).

**Deliverables**
- `fai-contracts`: collect the typed `example`/`forall` declarations from each
  module (associating each with the top-level symbols its body references, for
  reporting/docs).
- `example` evaluated (constant cases may be checked as early as `fai check`);
  `forall` exercised with **type-driven generators** (QuickCheck-style),
  shrinking on failure.
- `fai test` runs all contracts and reports pass/fail with codes + JSON.

**Acceptance**
- `fai test` passes for all contracts in `samples/`; a deliberately wrong
  `example`/`forall` fails with a precise, located diagnostic (+ shrunk
  counterexample for properties).

**Crates:** `fai-contracts` (+ `fai-cli`, `fai-driver`).

---

### M8 — Surface completeness, LSP v1, advanced code intelligence, error-code catalog
**Goal:** make it pleasant and complete enough for real use.

**Deliverables**
- Nested modules; remaining pattern forms; broader standard library.
- `fai-fmt` completeness across the whole grammar; formatter conformance tests.
- `fai-lsp`: diagnostics, hover (types/docs), go-to-definition, document format —
  **reusing `fai-ide`** (the same engine behind `fai query`).
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
**Goal:** push throughput on large workspaces. (The incremental engine, daemon,
and content-addressed cache landed in M0–M3.5; this milestone is *tuning*.)

**Deliverables**
- `rayon` parallelism across independent defs/modules (parallel salsa queries;
  per-function Cranelift codegen).
- **Shared/remote artifact cache** layered on the local content-addressed cache
  (team/CI dedup; portable by construction).
- Daemon hardening: LRU eviction / memory bounds; latency profiling.
- **Opt-in monomorphization** for hot generic paths (optimization only — never a
  correctness requirement, and the one feature that *hurts* incrementality, so it
  stays opt-in).
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
- **D73 The standard library is a curated, multi-file `std/` (amends D43, D71):**
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

Resolved while implementing **M6** (reuse & in-place update; the full staged
design is in `docs/reuse-plan.md`):

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
