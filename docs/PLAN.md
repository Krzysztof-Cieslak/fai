# Fai — Implementation Plan

This is the tactical build plan: milestones with concrete deliverables and
acceptance criteria, the sequencing rationale, a risk register, and the decision
log. For project conventions see `Agents.md`; for the language itself see the
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

**Acceptance**
- e2e: a program that takes only the capabilities it needs, builds a derived
  capability via an interface instance (`{ Name with … }`, e.g. a prefixing
  `Console`), and runs.
- Type error when code attempts an effect without holding the capability.

**Crates:** `fai-syntax`, `fai-resolve`, `fai-types`, `fai-core`, `fai-codegen`,
`fai-runtime`.

---

### M6 — Perceus reuse & in-place update
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
  regression guards.

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
| R7 | `'a'` char vs `'a` type-var lexing | Low | Med | Single documented lexer rule; dedicated tests (`Agents.md` §11). |
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

Resolved during planning (see the locked table in `Agents.md` §3):

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
  comparison).
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
  be tightened when constrained schemes land.
- **D43 Prelude:** **hybrid, type-only in M2** — primitives are a Rust
  `name → Scheme` table (no bodies; codegen is M3), and a derived `.fai` prelude
  is embedded (`include_str!`) and loaded as a synthetic high-durability
  `SourceFile`; it is reachable unqualified everywhere (the one exception),
  excluded from default `symbols`/`check`, and shadowing a prelude name warns
  (`FAI2010`).
- **D44 Code intelligence:** `fai-ide` returns typed serde envelopes (one per
  command) with `schemaVersion`; targets address by `Module.name`, bare-unique
  name, or `file:line:col`. `refs`/`dependents` assemble reverse indices on
  demand from each file's cached resolution, keyed by `ExprId` with spans
  resolved late (firewall-safe). Results are deterministically sorted and
  best-effort under errors.

To change a locked decision: update this log **and** the table in `Agents.md`,
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
