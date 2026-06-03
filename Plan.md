# Fai — Implementation Plan

This is the tactical build plan: milestones with concrete deliverables and
acceptance criteria, the sequencing rationale, a risk register, and the decision
log. For project conventions see `Agents.md`; for the language itself see
`Samples.md`.

---

## Strategy & sequencing rationale

1. **De-risk the backend early.** Native codegen via Cranelift + a
   reference-counted runtime + linking is the highest-uncertainty part of the
   project. We therefore drive a **thin vertical slice all the way to a running
   native binary (M3)** on a tiny language subset, *before* widening the
   language. Integration risk is paid down first, not last.
2. **Front-end before types, types before data.** Get a forgiving parser and the
   formatter working (M1), then Hindley–Milner for the functional core (M2),
   then the slice (M3), then the data layer — ADTs + pattern matching +
   structural records with rows (M4).
3. **Capabilities follow interfaces.** Interfaces compile to dictionaries;
   capabilities are just interface instances threaded from `main`. So M5
   delivers both at once.
4. **Optimize only once it runs and is correct.** Perceus reuse (M6) and
   parallel/incremental compilation (M9) come after correctness.
5. **Docs are tested.** Every `.fai` snippet in `Samples.md` is checked by the
   test suite from M1 onward, so documentation cannot drift.

Milestones are vertical where possible: each should leave `main` building,
linting clean, and green.

---

## Milestones

### M0 — Workspace scaffolding & toolchain
**Goal:** an empty but coherent Cargo workspace with the diagnostics/span
foundation and a test harness, so every later milestone plugs in cleanly.

**Deliverables**
- `Cargo.toml` workspace; `rust-toolchain.toml` pinning a stable Rust.
- Crates created as stubs: `fai-span`, `fai-diagnostics`, `fai-cli`, `fai-driver`.
- `fai-span`: `SourceId`, `Span` (byte offsets), `SourceMap`, line/column mapping.
- `fai-diagnostics`: `Diagnostic` model (code, severity, primary/secondary spans,
  labels, help, suggestions); human renderer + `--message-format=json` renderer;
  a stable, versioned JSON schema.
- `fai-cli`: argument parsing; subcommand stubs (`build/run/check/fmt/test/lsp`)
  that return "not implemented" diagnostics; global `--message-format`.
- `tests/`: golden/snapshot harness (e.g. `insta`) wired into `cargo test`.
- CI script running build + `clippy -D warnings` + `fmt --check` + `cargo test`.

**Acceptance**
- `cargo build`, `cargo clippy -D warnings`, `cargo fmt --check`, `cargo test`
  all pass.
- `fai --help` and `fai check --message-format=json` emit well-formed output.

**Crates:** `fai-span`, `fai-diagnostics`, `fai-cli`, `fai-driver`.

---

### M1 — Lexer, parser, AST, formatter (no types)
**Goal:** parse the core surface syntax with error recovery and format it
canonically.

**Deliverables**
- `fai-syntax`: hand-written lexer (incl. the `'a'` char vs `'a` type-variable
  rule), tokens with spans.
- Recursive-descent parser with a Pratt expression sub-parser; **error
  recovery** (synchronize on layout/keywords; report multiple errors).
- AST in arenas, referenced by newtyped ids; spans on every node.
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
- All non-type-dependent snippets in `Samples.md` parse and round-trip through
  `fai fmt` unchanged.

**Crates:** `fai-syntax`, `fai-fmt`, (+`fai-resolve` skeleton).

---

### M2 — Hindley–Milner inference for the functional core
**Goal:** type the pure functional core; enforce that every `public` binding has
an explicit signature.

**Deliverables**
- `fai-resolve`: single top-level module per file; name resolution; visibility
  (`public`/private); **dependency analysis / SCCs** so module-level bindings are
  mutually recursive and generalized correctly.
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

**Acceptance**
- Golden type tests (expected type or expected diagnostic) over a corpus.
- `fai check` reports precise, well-located type errors in both formats.
- Every public function in `Samples.md` typechecks against its written signature.

**Crates:** `fai-resolve`, `fai-types`.

---

### M3 — End-to-end native thin slice ⚠️ (highest-risk milestone)
**Goal:** compile a tiny program to a **native executable that runs**, exercising
the whole backend toolchain on the smallest possible language.

**Subset:** `Int`/`Bool`/`String`, functions, `let`, `if`, arithmetic, and a
single built-in capability (`Console.writeLine`) reached via `main`.

**Deliverables**
- `fai-core`: typed, desugared Core IR (the canonical lowered form).
- `fai-rc`: **plain** dup/drop insertion (no reuse analysis yet) over Core IR.
- `fai-codegen`: Core IR → Cranelift IR → object file; calling convention;
  boxed/immediate value representation; string constants.
- `fai-runtime` (Rust static lib): allocator, RC primitives (`dup`/`drop`/free),
  boxed value layout, `String` builtins, the `Console` capability host, and the
  C-ABI entry shim that constructs `Runtime` and calls `main`.
- `fai-driver`: orchestrate compile → emit object → link with `fai-runtime` →
  executable; `fai build` and `fai run`.

**Acceptance**
- `fai run hello.fai` prints via the Console capability and exits 0.
- A handful of e2e programs (arithmetic, string concat, conditional) produce
  correct stdout under `cargo test`.
- No leaks: a debug allocator/counter reports zero live objects at exit.

**Crates:** `fai-core`, `fai-rc`, `fai-codegen`, `fai-runtime`, `fai-driver`.

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
- `fai test` passes for all contracts in `Samples.md`; a deliberately wrong
  `example`/`forall` fails with a precise, located diagnostic (+ shrunk
  counterexample for properties).

**Crates:** `fai-contracts` (+ `fai-cli`, `fai-driver`).

---

### M8 — Surface completeness, LSP v1, error-code catalog
**Goal:** make it pleasant and complete enough for real use.

**Deliverables**
- Nested modules; remaining pattern forms; broader standard library.
- `fai-fmt` completeness across the whole grammar; formatter conformance tests.
- `fai-lsp`: diagnostics, hover (types/docs), go-to-definition, document format.
- **Error-code catalog** documenting every `FAInnnn` (the JSON schema + codes
  are a public, versioned API).

**Acceptance**
- LSP serves diagnostics/hover/go-to-def on a sample project over stdio.
- Catalog covers every emitted code; a test asserts no undocumented code ships.

**Crates:** `fai-lsp`, `fai-fmt`, `fai-resolve`, `fai-diagnostics`.

---

### M9 — Performance & incrementality
**Goal:** fast compiles at scale.

**Deliverables**
- `rayon` parallelism across independent modules in the module graph.
- Incremental recompilation (salsa or equivalent) powering the LSP.
- **Opt-in monomorphization** for hot generic paths (optimization only — never a
  correctness requirement).
- Compile-throughput benchmarks + regression guard in CI.

**Acceptance**
- Documented throughput target on a large synthetic corpus; no compile-time
  regressions beyond a set threshold in CI.

**Crates:** `fai-driver`, `fai-types`, `fai-codegen`, (+ most front-end crates).

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
| R9 | Docs drifting from implementation | Med | Low | Self-hosted check: `Samples.md` snippets are part of the test suite (DoD #6). |
| R10 | Overloaded arithmetic adds inference complexity / "ambiguous numeric type" noise | Low | Med | Restrict overloading to the built-in numeric set (`+ - * /`) with a simple `Int`-defaulting rule; clear help text steering to annotation or `intToFloat`/`floatToInt`. |

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
