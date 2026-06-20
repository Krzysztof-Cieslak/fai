# Fai — Design Memory

The durable design rationale for the Fai compiler: the **locked decisions** and
the **standing risks** — the "why" behind the code. Read it alongside
`AGENTS.md`, which carries the project conventions and a summary table of the
locked decisions.

- Conventions, repository layout, coding standards: `AGENTS.md`.
- CLI surface and the daemon protocol: `docs/CLI.md`.
- Error-code catalog: `docs/ERROR_CODES.md`.
- The language by example: the `samples/` directory.
- **Remaining and proposed work lives in the issue tracker**, not here.

Decision and risk IDs are stable and never reused; a gap in the numbering is an
entry that was folded into a later one (consolidated) or whose mitigation is
fully realized.

---

## Risk register

Standing and residual risks (fully-realized one-time integration risks are
retired; IDs are not reused).

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | Cranelift integration + linking harder than expected | Med | High | Driven early via a thin native slice on a tiny subset; runtime ABI kept minimal and stable. Realized; platform-specific codegen/link edge cases remain (tracked in #9, #10). |
| R2 | RC correctness (leaks / double-free), esp. with closures & existentials | Med | High | Plain RC first; a debug leak counter in every e2e test; precise reuse added only after green. Standing invariant. |
| R6 | Exhaustiveness checking bugs (rows/literals) | Med | Med | A known algorithm (Maranget-style); golden tests for false pos/neg. An unresolved/ill-typed pattern that left an arity-inconsistent matrix row once panicked the checker (#27); such rows are now lowered to a distinct unmatchable value and the matrix splits guard against short rows. |
| R8 | Scope creep from "AI-first" features | Med | Med | Effect rows are now built (D115); extension/restriction and a package manager remain out of scope — tracked as proposals (#36, #37). |
| R9 | Docs drifting from implementation | Med | Low | Self-hosted check: `samples/` files are part of the test suite (DoD #6). |
| R11 | salsa API churn / version instability | Med | Med | Pin a version; wrap behind `fai-db` so the engine is swappable; keep query definitions framework-agnostic. |
| R12 | Incremental-cache correctness (stale results → wrong diagnostics) | Med | High | Incremental-vs-clean **verifier** in CI; content-addressed keys stamped with compiler version + flags; determinism is a locked invariant. |
| R13 | Span/position instability collapses incrementality | Med | High | Position-independent item tree + spans in a side-table; edit-churn test asserts "add a comment → near-zero recompute". |
| R14 | Daemon lifecycle: stale/version-mismatch, spawn races, memory growth | Med | Med | Version handshake + auto-restart; version-stamped socket path + spawn-lock; LRU eviction + idle-timeout shutdown. `stop`/`restart` are synchronous — they block until the prior daemon's endpoint refuses connections, so `restart` spawns a genuinely fresh daemon instead of reattaching to the one still shutting down. The Windows spawn clears inheritance on the client's std handles so the detached daemon no longer holds them open; the daemon e2e suite runs on Windows CI. |
| R15 | JIT'd user code crashes/hangs the toolchain | Med | High | Run in an isolated **worker process** with timeouts/resource limits; the daemon survives worker death. `run` *and* `test` both supervise isolated workers (D63–D65, D103). |
| R16 | Large mutually-recursive SCCs reduce per-def granularity | Low | Med | SCCs computed from actual references (usually small); consider a lint for accidental large cycles. |
| R18 | The editor grammars (TextMate, tree-sitter) re-encode the lexer/parser and drift from the canonical `fai-syntax` | Med | Low | The hand-written `fai-syntax` stays the single source of truth; grammars are highlighting/structure aids only, pinned with tests over `samples/` so drift fails CI. The TextMate grammar (in `editors/vscode/`) and its samples tokenization test (no `invalid`/unscoped spans) are realized (D103); the tree-sitter grammar (no `ERROR` nodes) remains a stretch goal to bound the dual-maintenance cost. Tracked in #31, #33 (#32 done). |

---

## Decision log

Initial design decisions (summarized in the locked table in `AGENTS.md` §3):

- **D1 Backend:** Cranelift native codegen (over interpreter / bytecode VM / LLVM
  / transpile). Rationale: native speed with fast compiles; avoids LLVM build cost.
- **D2 Memory:** Perceus-style reference counting. Rationale: strict + pure ⇒
  acyclic heaps ⇒ no cycle collector; enables in-place reuse.
- **D3 Generics:** uniform boxed representation + dictionary passing (no
  monomorphization by default). Rationale: protects compile throughput; no code
  bloat. Monomorphization is an opt-in optimization (tracked as a proposal, #16).
- **D4 Effects (effect rows since added — see D115):** capabilities as explicit
  values (interface instances from `main`). Type-level effect rows were
  initially left out (simple, auditable, implementable then; rows layered on
  later) and are **now built** — every arrow carries an effect row of the
  capabilities it uses, required on public signatures (D115).
- **D5 Signatures:** Haskell-style explicit signature on its own line above each
  `public` binding; signatures are checked, not trusted.
- **D6 Layout:** indentation-significant (offside); one canonical layout pinned
  by `fai fmt` (2-space indent).
- **D7 Type variables / equality:** F#-style `'a`; `=`/`<>` (parser
  disambiguates `=` binding vs. equality by position).
- **D8 Tuples:** structural; value `(a, b)`, type `'a * 'b`.
- **D9 Records:** **structural with row polymorphism**; lacks constraints (no
  duplicate labels); `type X = { ... }` is a transparent alias; extension/
  restriction is future work (tracked as a proposal, #36). Rationale: better
  inference + row-polymorphic capability least-authority; reuses evidence-passing
  machinery.
  **Openness:** record type annotations are **closed by default** (`{ x : T }`);
  `{ x : T | _ }` is anonymous-open (common case), `{ x : T | 'r }` names the
  tail only to thread it to the result. Chosen over open-by-default (which would
  invert the default for data records/literals and still need named rows for
  updates) and over width subtyping (incompatible with principal HM inference).
  Governs written signatures only; inference is unchanged; no subtyping.
  **Patterns mirror this (P-A):** `{ ... }` closed (names all fields),
  `{ ... | _ }` open (ignore rest; required for row-poly scrutinees); binding a
  pattern tail (restriction) is future work (#36). Chosen over always-open
  patterns so `{ ... }` means the same thing in types and patterns.
- **D12 Contracts:** **first-class `example`/`forall` declarations** (peers of
  `let`), placed immediately after the binding they describe, *not* a doc-comment
  extension. Rationale: symbols inside contracts resolve via normal name
  resolution (real diagnostics, types, LSP), laws can span multiple functions,
  and it is simpler to build than embedding checked code in `///` comments.
  `///` stays human prose. Considered and rejected: contracts inside `///`
  (doctest-style) — murky scoping + lexer/formatter complexity.
  Separator is `:` (`example: e` / `forall xs: e`); `=` was rejected because
  contract bodies are usually equalities, which would put two `=` on one line.
  Contracts are **pure**: a contract has no `Runtime` in scope, so reaching a
  host capability is impossible by construction. A contract that references an
  effectful binding (whose type carries `Console`/`Clock`/`Random`/`FileSystem`/
  `Env`, or the `Runtime` bundling them) is now reported directly as `FAI6004` at
  the offending reference, rather than as a downstream type mismatch.
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
  shared/remote cache layers on later (#15). Determinism makes this sound; an
  incremental-vs-clean **verifier** runs in CI.
- **D18 Code intelligence:** a **read-only** `fai query` surface (namespaced),
  sharing the `fai-ide` engine with the LSP; addressing by name path or
  `file:line:col`; JSON by default; best-effort under errors. **No write/refactor
  commands** (no `rename`/`fix`) — agents perform edits themselves. Full command
  reference in `docs/CLI.md`.

Cross-cutting conventions (workspace, spans, diagnostics, the database seam):

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

Syntax front end (lexer, layout, parser, AST, formatter, incremental queries):

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
  local-arena lowering (for body-level cutoff) is deferred (future work).
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
  added later). The binding `=` is consumed by the declaration parser, so `=`
  in expressions is always equality. **Error nodes in every category** with
  multi-level recovery (synchronize on layout `Sep`/`Close` and item keywords).
  `public` is accepted on signature and binding items; sig↔binding association and
  the "public needs a signature" rule belong to name resolution and the type
  system. A reserved-but-unimplemented construct (`type`, records, `match`,
  `interface`, nested `module … =`) emits **`FAI1030` "not yet supported"** and
  recovers, going dormant as those constructs landed.
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
  signature types are added by the resolution/type layer. `fai-db` gains
  `Db::all_source_files` and re-exports `salsa::Update`.
- **D34 `check`/`fmt` wiring:** the driver computes, the CLI does I/O.
  `check(db, files)` parses the filtered files and reports `Diag` (`ok` = no
  error-severity diagnostics). `fmt(db, files)` returns per-file results; the CLI
  writes changed files unless `--check`; the JSON envelope is `FmtOutput
  { schemaVersion, changed, diagnostics }` (the additive `diagnostics` reports
  files skipped for parse errors). The optional `[path]` argument is resolved to a
  `SourceFile` set by the CLI. The front end is one-shot in-process (the daemon
  layered on later; see D56–D65).
- **D35 Samples as files:** the language tour lives as canonical `.fai` files in
  **`samples/`** (one self-contained module per file), replacing the former
  `Samples.md`. The test suite buckets each file by parse result: zero diagnostics
  ⇒ must round-trip under `fai fmt` and be idempotent; ≥1 `FAI1030` ⇒
  future-surface, skipped; any diagnostic without `FAI1030` ⇒ failure (a real
  syntax bug). A known-module guard asserts the implemented-surface modules stay
  clean; files auto-promote to the round-trip set as later milestones land.

Type system (name resolution, inference, code intelligence):

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
  **Eq** (non-function), and **Ord** (Int/Float/String/Char) are realized as the
  std `Num`/`Eq`/`Ord` interface constraints (D75). Numeric defaults to `Int`;
  `=`/`<>` on a function type is `FAI3006`; no implicit Int/Float coercion
  (`FAI3001`).
- **D42 Operators:** `++` is **String-only** (lists use the prelude `append`);
  `::` is cons; `|>`/`>>` are pipe/compose; comparison is `Ord`, equality is
  `Eq`, arithmetic is `Num`. The overloaded operators are std interface methods
  (D75) — a monomorphic use lowers to the direct primitive, so concrete-type
  operators pay no dictionary cost.
- **D43 Prelude visibility:** the standard library is embedded and loaded as
  synthetic high-durability inputs; the auto-imported `Prelude` is reachable
  unqualified everywhere (the one exception), excluded from default
  `symbols`/`check`, and shadowing a `Prelude` name warns (`FAI2010`). The
  curated multi-file `std/` layout and the prelude-private `Prim.*` intrinsics
  are D73/D74; primitive `Scheme`s live in a Rust table consumed by codegen.
- **D44 Code intelligence:** `fai-ide` returns typed serde envelopes (one per
  command) with `schemaVersion`; targets address by `Module.name`, bare-unique
  name, or `file:line:col`. `refs`/`dependents` assemble reverse indices on
  demand from each file's cached resolution, keyed by `ExprId` with spans
  resolved late (firewall-safe). Results are deterministically sorted and
  best-effort under errors.

Native backend (Core IR, reference counting, codegen, runtime, object cache):

- **D45 Capability shape (historical, superseded):** the initial native slice
  predated records and interfaces, so `Runtime` was an **opaque built-in type
  constructor** threaded through `main` (`main : Runtime -> Unit`), and
  `Console.writeLine : Runtime -> String -> Unit` was a **qualified builtin**
  resolved through the prelude/qualified-name path. This honored "capabilities
  flow from `main`" without the record/interface machinery; the real capability
  records/interfaces (`runtime.console.writeLine`) later replaced it.
- **D46 `fai run` worker:** `fai run` JIT-compiles and executes in an **isolated
  worker subprocess** (a hidden `__run-worker` subcommand that opens its own
  session); stdio is inherited and the worker's exit code is returned. Timeouts,
  resource limits, and daemon-survival are handled by the daemon supervision
  (D63–D65, R15).
- **D47 Object cache = salsa query:** `object_code(Def)` is a tracked query
  producing one relocatable object per definition; salsa's dependency graph *is*
  the content-addressed cache, and the per-function cache hit is asserted via the
  query event log. Symbols and arities feeding it are derived from
  **body-edit-stable** information, so the codegen layer keeps the cross-module
  firewall. On-disk persistence layered on later (D56).
- **D48 Value representation:** a uniform 64-bit **LSB-tagged** word — immediate
  `payload<<1|1` (Int/Bool/Unit/Runtime), boxed = 8-aligned pointer (tag 0).
  `dup`/`drop` are tag-checked, so polymorphic code reference-counts correctly
  with no type information and immediates are RC no-ops.
- **D49 Int range under tagging:** the full **64-bit `Int` is preserved** via
  boxed overflow — immediate when it fits 63 bits, a heap `i64` object otherwise.
- **D50 Heap layout:** a descriptor-pointer header `{ rc, descriptor, size }`;
  static per-type descriptors carry a children-scan used at drop. Extensible to
  ADTs/records (later realized).
 - **D51 Function model:** closures `{ code, arity, env… }` with a uniform
   `apply_n` eval/apply handling exact, partial (a PAP object), and
   over-application. Top-level functions are static **immortal** closures (a
   zero-arity binding — a value, not a function — is forced on reference). A
   **non-capturing lambda** shares the same treatment: with no per-activation
   environment it references one immortal static closure (emitted per lifted
   function, defined in the definition's primary object and imported by its reuse
   entry) instead of calling `fai_make_closure` at every evaluation, so a
   `fun`-literal that closes over nothing allocates no cell even in a hot loop.
   The immortal reference count makes the shared cell's `dup`/`drop` balance
   harmlessly (it is never freed); a capturing lambda heap-allocates unless escape
   analysis proves it stack-safe (D127). Primitives lower to runtime
   calls. Every operation **consumes** its operands, so RC insertion reduces to
   dup-at-use + one drop per owned binding (no reuse; precise reuse layered on
   later, D76–D79).
- **D52 Typed Core IR:** `fai-core` carries a `Ty` on every node, from a new
  `body_types` query, so the later record-field-offset work need not retrofit
  types — even though the thin-slice codegen leans on tagging and uses the types
  lightly.
- **D53 Entry & scope:** the entry file must define `public main : R -> Unit`,
  where `R` is whatever the **runtime root** produces — by default the standard
  `defaultRuntime` (`R = Runtime`), or, when the entry file defines a `runtime`
  builder, that builder's record (an *extended* bundle; see D137). The backend
  compiles only the transitive closure reachable from `main` (over the lowered
  `Global` references, so prelude helpers are included), plus the runtime root as a
  second root (the trampoline injects it; it is not referenced from `main`'s body).
  *(Amended by D137: the root was the fixed `defaultRuntime` before.)*
- **D54 Runtime embedding (amended by D139):** the driver's build script compiles
  `fai-runtime` to a static archive and embeds it (`include_bytes!`); produced
  executables are self-contained. Host target only (cross-compilation is future).
  The runtime is also linked as an `rlib` so the JIT can resolve its symbols by
  address. *Originally* `fai-runtime` was **std-only** so the archive came from a
  single `$RUSTC` invocation; the concurrency runtime (D139) needs native crates
  (stackful coroutines, work-stealing deques, an IO reactor), so the build script
  now produces the archive with a **nested `cargo`** build into a private target
  directory (a `staticlib` bundles those dependencies into the one archive). See
  D139.
- **D55 Backend error range & runtime ABI:** the **`FAI7xxx`** range is owned by
  the backend (`fai-core`/`fai-codegen`/`fai-runtime`): `FAI7001` "construct not
  supported by the native backend yet" (e.g. `Float`, tuples, lists), reported
  only for *reachable* definitions. The runtime ABI (tagged values, the
  `fn(env, args) -> i64` calling convention, the `fai_*` symbols) is the contract
  shared by codegen and the runtime.

Daemon, persistence & protocol:

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
- **D57 Daemon concurrency (serialized — superseded by D112):** the daemon
  initially served per-connection threads but serialized **all** database access
  through one `Mutex<Session>` (true serialization, sidestepping salsa's
  concurrent-read/cancellation machinery), with control messages and `run`
  supervision off-lock. **D112 lifts the serialization:** read commands now run
  off-lock on cloned snapshots (concurrent reads), and an input change cancels and
  retries in-flight reads. The mutex remains (the brief sync/snapshot section is
  exclusive, and `Session` is not `Sync`), but it no longer guards command
  execution.
- **D58 Transport:** the client↔daemon link uses the **`interprocess`** crate
  (sync) for one safe cross-platform code path — Unix-domain sockets on POSIX,
  named pipes on Windows — with our `u32`-LE + MessagePack framing layered on top
  and the Unix socket created `0600`. The endpoint name embeds a blake3 of the
  canonicalized root and the compiler version; binding is the spawn-race lock, and
  a stale socket from a crash is reclaimed (probe-connect → unlink → rebind).
  Both platforms run in CI; the Windows named-pipe path is exercised by the daemon
  end-to-end suite on the `windows-latest` job.
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
  `__daemon-serve` (null stdio; on Windows the `DETACHED_PROCESS`/
  `CREATE_NEW_PROCESS_GROUP` flags) and the daemon calls **`nix::setsid()`** at
  startup on Unix so a terminal hangup can't kill it. On Windows the stable
  `Command` always spawns with `bInheritHandles = TRUE` and no handle-list
  restriction, so before spawning, the client clears the inheritable flag on its
  own standard handles (`SetHandleInformation`); otherwise the detached daemon
  inherits and holds the client's stdout/stderr pipes, and a client whose output
  is captured blocks until the daemon's idle timeout instead of returning
  promptly. There is no safe std API to control per-handle inheritance, so this is
  a small scoped `unsafe` block — the Windows peer of the safe `nix` Unix calls
  (see AGENTS.md §8). The daemon shuts down on an explicit `Shutdown` or after an
  idle period (`FAI_DAEMON_IDLE_TIMEOUT`, default 600s), unlinking its socket on
  the way out.
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
- **D65 Worker resource limits:** the worker self-imposes a CPU-time limit
  (seconds, from `FAI_RUN_CPU_SECS` set by the daemon) at startup — robust
  runaway-CPU protection that doesn't interfere with JIT; a memory cap
  (`FAI_RUN_AS_BYTES`) is opt-in because a low cap can break compilation. On Unix
  these are `RLIMIT_CPU`/`RLIMIT_AS` via the safe `nix` wrappers. On Windows the
  worker assigns its own process to a **Job Object** carrying a
  `PerProcessUserTimeLimit` and a `ProcessMemoryLimit` (the committed-memory peer of
  `RLIMIT_AS`), which the OS enforces by terminating the process; the job handle is
  left open so the limits hold for the process's whole life. `win32job`'s safe API
  exposes only a working-set (not committed-memory) limit and no CPU-time limit, so
  the job is configured through `windows-sys` in a small scoped `unsafe` block (see
  AGENTS.md §8). Limits apply only under daemon supervision; either way the daemon's
  wall-clock reaper (kill on `FAI_RUN_TIMEOUT_MS`) is the cross-platform backstop.

Data layer (ADTs, pattern matching, structural records with rows):

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
  `< <= > >=`/`= <>` are `Ord`/`Eq` interface methods (D75) that specialize to
  this single runtime compare/equal on concrete types.
- **D71 The standard library is real compiled code, not magic:** `Option`/
  `Result`, the `List` combinators, `compare`/`sort`, `Dict`/`Set`, and the
  string helpers are ordinary compiled `.fai` modules; only genuinely primitive
  operations stay in Rust (the `Prim.*` intrinsics, D74). `Float` is always
  boxed; the arithmetic/comparison primitive is selected from the operand type
  during Core lowering. The curated multi-file `std/` layout and auto-import are
  D73.
- **D72 Field-access codegen:** a **monomorphic closed** record compiles field
  access/update to a **constant offset**; a **row-polymorphic** access or
  `{ r with … }` update compiles via **offset-evidence passing** — per-row
  lacks-constraint integer offsets threaded in as leading arguments, like
  dictionaries (D75) — and the type system infers the fully general signatures
  (e.g. `getX : { x : 'a | _ } -> 'a`). A residual row-polymorphic case with no
  available offset evidence still reports `FAI7002` (help: give the value a
  closed record type). Diagnostics: `FAI3012` (type-constructor arity),
  `FAI3013` (recursive alias), `FAI4001`/`FAI4002` (non-exhaustive / unreachable
  `match`); the unused `FAI3009` is retired (the catalog test allows the
  `FAI4xxx` range in `fai-types`).

Standard library & operators:

- **D73 The standard library is a curated, multi-file `std/`:**
  the embedded library is real `.fai` modules under a top-level **`std/`**,
  embedded at build time by
  `crates/fai-types/build.rs` (a generated `include_str!` table) and loaded as
  synthetic high-durability inputs under the `<std>/` path namespace
  (`fai_db::is_std_path`, shared so name resolution can classify a file without
  depending on the loader). Auto-import is **curated, Elm-style**: a single
  module **`Prelude`** is visible unqualified everywhere; a public type's
  constructors travel with it (except an **opaque** type, which exports its name
  only — see D113), so the core types are auto-imported. `Prelude` owns
  `Option`/`Result` (+ constructors), re-exports the opaque `Dict`/`Set` type
  names, and provides the free functions
  `identity`/`const`/`not`/`compare`; **every other operation is reached
  qualified** through a per-type module (`List.map`, `Option.withDefault`,
  `Dict.insert`, `String.split`, `Int.toString`, `Float.sqrt`, …). So
  `Prelude`/`List`/`Option`/`Result`/`Dict`/`Set`/`String`/`Int`/`Float` are
  reserved module names; `Dict`/`Set` are **opaque** types declared in their own
  modules, with their node constructors hidden (D113). Auto-import is a pure tracked
  `prelude_exports` (the merged interface of the auto-imported set, keyed on the
  public **name set** for early cutoff: a Prelude *body* edit recomputes nothing
  downstream) shared by resolution and the type-name fallback; the `Prelude`
  module is located **among `std/` files only**, so a stray user `module Prelude`
  cannot hijack or collapse auto-import. The whole sample/fixture/test corpus is
  rewritten to the qualified form (a hard cutover; no compatibility aliases).
- **D74 Intrinsics are prelude-private (`Prim.*`):** the Rust
  intrinsics are no longer bare names anywhere. They are reached only as
  `Prim.<name>`, and only from inside `std/` modules (`FAI2014` otherwise); the
  standard library re-exports the user-facing ones under clean qualified names
  (`Int.toString` wraps `Prim.intToString`, `String.split` wraps `Prim.split`,
  `Prelude.not` wraps `Prim.not`, …). A saturated call to such a wrapper is
  collapsed back to the primitive by the intrinsic inliner (D121), so the
  re-export adds no call of indirection at a use site. New resolution
  diagnostics: **`FAI2013`** (a
  name exported by more than one auto-imported module — contributor-facing,
  detected by the auto-import merge so it stays unit-testable even while the
  auto-imported set is a single module) and **`FAI2014`** (`Prim` referenced
  outside `std/`). The `INTRINSICS` name list moves to `fai_resolve::intrinsics`;
  the loader and built-in `Scheme` table move to `fai_types::std_lib`
  (`load_std`/`builtin_scheme`).
- **D75 Operators are symbolic identifiers with F#-style precedence; the
  overloaded ones are std interface methods; user-defined operators are allowed
  (supersedes the earlier solver constraint-flavor handling; see D41, D42, D70):**
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
  - **Overloading via interfaces:** `+ - * / %` are methods of a std **`Num`**,
    `= <>` of **`Eq`**, `< <= > >=` of **`Ord`**, with `Int`/`Float`/structural
    instances in `std/`. The earlier solver constraint flavors
    (`Numeric`/`Eq`/`Ord`) are realized as these interface constraints; `Num`
    keeps the `Int`-defaulting rule. **Monomorphic uses still lower to the direct
    primitive** (e.g. `IntAdd`), so concrete-type operators pay no dictionary
    cost.
  - **Stays built-in regardless:** `&&`/`||` remain short-circuit sugar over `if`
    (a strict function cannot short-circuit); `::` stays the built-in `List`
    constructor. `|>`/`>>` may be redefined as ordinary `Prelude` operators (they
    are plain higher-order functions), inlined when monomorphic.
  - **Mechanism:** the lexer/precedence/user-operator half is unified with the
    interfaces work, so built-in and user operators share one mechanism (no
    throwaway hybrid).

Reuse & in-place update:

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
    `ALLOCATIONS` counter (incremented only on real allocation, and compiled in
    only under `debug_assertions` — see D110) makes reuse observable.
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
    specialization — inlining a known data cell's child drops and free instead of
    the runtime release path — was originally deferred (after reuse, data cells are
    rarely dropped on hot paths, and the inlined release carries memory-safety risk
    disproportionate to the gain), but is **now implemented for monomorphic records
    and tuples — see D101**.

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
    Originally the query never read another function's signature, so it was
    **acyclic** (a caller depended on a callee's small signature, computed at the
    call site). Row-polymorphic functions (curried through evidence) stay
    all-owned. Cross-module borrowing — the inter-procedural fixpoint that lets a
    forwarded parameter be borrowed — is **now implemented (see D100)**, which
    supersedes that self-contained/acyclic property.
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

Contracts (examples & properties):

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
  body contains an error placeholder, is reported rather than run. (This
  in-process path is retained as the `fai_driver::test` library entry point and
  for the corpus tests; the CLI/daemon now check in an isolated worker — see
  D103.)
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
  (arithmetic `shiftRight`, logical `shiftRightLogical`). Float bit
  reinterpretation, added as `Float.fromBits`/`Float.toBits`, backs the splitmix
  fraction and the full-domain `Test.floatAll` generator (the default `Test.float`
  is finite — see D110). Both are ordinary `Prim.*` intrinsics re-exported under
  clean names, mirroring the existing intrinsic wiring.
- **D86 Generation policy (Stage 1).** Deterministic by default (a fixed seed; a
  `--seed` flag overrides), 100 trials, size ramping `0..maxSize` with `Int` drawn
  from `[-size, size]` and `List` length ≤ size — bounded so `abs`/`clamp`-style
  laws hold (no `i64::MIN`/overflow surprises). Generators cover the primitives
  and built-in constructors via the std combinators (which the compiler composes).
  (`Char` was initially omitted while the native backend lacked it; it is now a
  native type with a generator, so a `Char` binder is runnable — see D107.)
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
  against the binder's concrete type arguments. The recursion budget and
  user-overridable generators are refined in D109.

Nested modules, qualified-type syntax, advanced code intelligence & the language
server:

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
  - **Surface.** `textDocument` sync (incremental; see below),
    `publishDiagnostics`, `hover`, `definition`, and `formatting`, since grown
    with the features in the following notes. Open buffers are overlaid into the
    database as in-memory edits, so analysis tracks unsaved changes; diagnostics
    reuse `fai check` and formatting reuses `fai fmt`.
  - **Position-addressed queries.** Hover and go-to-definition are offset-keyed
    (an editor addresses a byte position, not a name), so `fai-ide` gains
    `hover_at`/`definition_at`: find the innermost expression containing the
    offset (walking outward when it carries no resolution), then report its
    inferred type or jump to what its reference resolves to — a definition, a
    constructor variant, or a local's binding pattern.
  - **Positions.** LSP positions are `(0-based line, 0-based column)` while Fai
    spans are UTF-8 byte offsets; a per-document line map converts both ways,
    clamping an out-of-range column to the line's content rather than spilling
    onto the next line. The column unit is the **negotiated encoding**: the split
    initialize handshake reads the client's `general.positionEncodings` and picks
    UTF-8 when offered (Fai's native byte offsets — no re-encoding) else the LSP
    default UTF-16, and advertises the choice; the line map measures columns
    accordingly (exact across non-BMP characters either way).
  - **Editing fidelity & dependent diagnostics.** Sync is **incremental**: each
    change's range is applied to the open buffer in order (a range-less change is
    a full replacement), and `didSave` re-checks. On any change the server
    re-publishes diagnostics for **every open file**, not just the edited one, so
    a cross-module edit refreshes its open dependents (salsa's early cutoff keeps
    the unaffected files cheap). Range formatting reuses the whole-file formatter
    and line-diffs its output against the original, keeping only the changed hunks
    whose lines overlap the requested range, so "format selection" rewrites just
    the selection. On-type formatting shares that machinery: a newline trigger
    scopes the same line-diffed edits to the line just completed and the cursor's
    line, so typing reformats the current construct and nothing else. Because the
    whole-file formatter skips a file with parse errors, a mid-edit buffer that
    does not yet parse simply yields no edits rather than disturbing the typing.
  - **Navigation & structure.** `documentSymbol` and `workspace/symbol` reuse the
    outline/symbol queries (nested-module aware; `documentSymbol` is keyed by file
    and `outline` delegates to it, so the two never drift). `references` first
    resolves what the cursor names — a definition, a constructor, or a local —
    then collects every occurrence across the workspace (uses in expressions and
    patterns), adding the declaration when the client asks for it. Each reported
    range is the bare name: a qualified use `A.inc` reports only the trailing
    `inc`, and a constructor pattern reports only its head. A definition's own
    name is itself a reference site, so find-references and rename work when
    invoked on the declaration, not just a use.
  - **Rename.** `prepareRename` returns the bare-name range under the cursor (the
    editor's placeholder) and rejects what cannot be renamed: builtins, and
    standard-library symbols (the embedded std is read-only). `rename` is
    find-references with the declaration always included, replacing each
    occurrence with the new name — so the same bare-name precision applies (a
    qualified `A.inc` edits only `inc`, a constructor pattern only its head). The
    new name must be a plain identifier in the symbol's casing namespace (a
    constructor stays upper-case, a value or local lower-case), so a rename can
    never move a symbol between namespaces; an invalid name yields no edit.
  - **Completion.** The candidate set is chosen by the context immediately before
    the cursor, determined lexically so a half-typed buffer with a trailing `.`
    still works (the parser recovers a `Field` with an empty member): after
    `Module.` the module's members (cross-file public exports, or a same-file
    nested module's members), after `value.` the fields of the value's record
    type, and otherwise the names in scope — locals visible at the cursor
    (reconstructed by a scope walk down to the offset, innermost binding winning),
    this module's scope-visible definitions, the visible constructors, and the
    auto-imported prelude values. Each item carries a kind and a rendered type;
    the editor filters by the typed prefix. Lazy doc resolution
    (`completionItem/resolve`) waits on `///` doc extraction, so detail is the
    type only for now.
  - **Docs, richer hover & signature help.** `///` doc prose is extracted by
    attaching the leading doc trivia to a definition (its signature when present,
    else its binding) and stripping the markers — filling the previously-empty
    `doc` of the `docs`/`api` queries and enriching hover, which now reports the
    referenced definition's type, doc prose, and attached `example`/`forall`
    contracts. Signature help finds the enclosing application (or a function name
    followed by whitespace), takes the head's inferred function type, and splits
    its arrow chain into parameters (a function-typed parameter is parenthesized);
    the active parameter is the number of arguments lying strictly before the
    cursor, so a separating space — not mere adjacency — advances it.
  - **Code actions / quick fixes.** Two sources feed `codeAction`: the
    machine-applicable `Suggestion`s a diagnostic already carries become a
    one-edit quick fix, and an unbound/ambiguous bare name (`FAI2001`/`FAI2002`)
    becomes a "qualify as `Module.name`" fix per module that publicly exports that
    name (the standard library included, the prelude-private `Prim` excluded) —
    the qualified form Fai requires for cross-module access. The missing
    public-signature diagnostic (`FAI3003`) now carries such a suggestion: it
    moves `public` onto a freshly inserted signature line (the inferred type),
    matching the binding's indentation and the member's bare name. The engine
    re-derives the file's diagnostics from the salsa accumulators, so the
    suggestions exactly match `fai check`'s.
  - **Inlay hints & semantic tokens.** Inlay hints annotate every variable binder
    (parameters, lambda binders, local `let`s, match binders — Fai binders carry
    no inline annotation) with its inferred type, read from the per-body pattern
    types. Semantic tokens classify the lexer's token stream: keywords, literals,
    operators, and comments syntactically; identifiers by resolution (a function
    vs. value definition, a constructor, a local, a builtin) where a name
    reference resolves, the qualifier of a `Module.member` as a namespace, and
    otherwise by casing (a lower name is a variable, an upper one a type). The
    engine yields byte-range tokens; the server splits any multi-line token and
    delta-encodes them in UTF-16 against the advertised legend.

Inference tuning, primitive borrowing & intra-build parallelism
(measurement-driven; correctness-neutral — inferred types, diagnostics, and
program output are unchanged, guarded by the full type/golden suite):

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
  structural `compare`, and the `String` operations that only read their inputs —
  the readers (`length`, `contains`) and the read-and-rebuild builders
  (`toUpper`/`toLower`/`trim`/`split`/`++`/`join`) — were consumed (dropped) by
  the runtime, forcing a caller to duplicate a value it still needed (and, by
  sharing it, defeating in-place reuse). They now have **non-consuming runtime
  variants** (`fai_equal_borrowed`, `fai_to_upper_borrowed`, …), and the operands
  are **borrowed when boxed**. (`++` was later removed from this borrowing set: it
  **owns** both operands so it can append into a uniquely-owned left buffer in
  place — see D124.) One predicate — `Prim::borrows_operand`, on the
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

- **D97 Daemon hardening: a bounded native-object cache and per-command latency.**
  The warm daemon keeps one salsa database for its lifetime. Salsa stores one
  memo per query *key* (replaced, not accumulated, across edits), so the warm
  front-end memos track workspace size — that is the daemon's intended working
  set and is left unbounded. The one explicitly growable, large payload is
  `object_code` (a relocatable object per definition, also backed by the on-disk
  cache), so it is made LRU-capable (`#[salsa::tracked(lru = 0)]` — `0` is
  unbounded, so the one-shot CLI and tests are unaffected) and the daemon caps it
  at startup via `Session::set_object_cache_capacity` (`FAI_DAEMON_OBJECT_CACHE`,
  default 1024); over-capacity blobs are evicted at the next revision (salsa's
  per-revision hook, driven by the daemon's disk-sync) and re-read from the
  on-disk cache on demand, so eviction only trades memory for a lookup. For
  latency profiling, the daemon times each served `Command` (the check/query/fmt/
  build path; `run` is excluded — it is dominated by the worker's own execution)
  and reports the count, total, and slowest in `fai daemon status`. Chosen over
  bounding the front-end queries (which would defeat the daemon's whole purpose)
  and over a coarse RSS-watchdog Session rebuild (unnecessary given memory tracks
  workspace size; deferred as a safety valve if a pathological case appears).
  Cross-request concurrency remains future work (D57).

- **D98 Iterative drop: release a dead structure with an explicit worklist.**
  Releasing a dead heap object recursed through a per-kind child scan that called
  `fai_drop` on each child, so freeing a deep structure (e.g. a long list)
  recursed once per element and could overflow the native stack. `fai_drop` now
  decrements and, on reaching zero, drains the object's reference-counted children
  with an explicit worklist (a fixed inline buffer with a heap spill), enqueuing
  grandchildren as cells die; `fai_drop_reuse` releases a reset cell's children
  through the same path. The descriptor no longer carries a child-scan function
  pointer — an object's child layout is recovered from its kind (its descriptor
  address). So no structure overflows the stack when it is freed, regardless of
  depth, while the common case (decrementing a still-shared value) touches no
  worklist. The worklist's working set is constant for an immediate-headed list
  and bounded by the structure's branching otherwise (always heap, never native
  stack).

- **D99 Flatten self-tail-recursion into a loop (tail-call/TRMC).** A transform in
  `fai-rc`, run after dup/drop and reuse, rewrites a self-tail-recursive entry
  function into a loop.
  - **Eligibility (all-or-nothing).** Every reference to the function itself is a
    **saturated self-call in tail position** — either plain (a tail-call loop) or
    threaded through a **chain of one or more tail constructors** (the "modulo cons"
    case, e.g. `x :: f r` or `x :: x :: f r`). The recursive call may sit at any
    field index; a constructor argument after it (in evaluation order) is hoisted
    ahead of the back-edge only when it is **pure and total**, preserving observable
    effect/abort order. Purity/totality is decided by the `is_pure_total` analysis:
    no capability effect, no integer division/remainder that could abort (a non-zero
    *literal* divisor cannot), and no call except a saturated/partial application of
    a statically known top-level function that is itself pure and total. Since Fai
    has no loops, an acyclic call graph implies termination, so **recursion is
    conservatively excluded** (a function reachable from itself is treated as
    not-total — proving its termination is undecidable); this falls out of a salsa
    cycle whose members resolve to "not pure-total". The recursion must flow
    *linearly* — used exactly once at each step, carried whole through each cell — so
    two self-calls in one constructor (`Node (f l) (f r)`), a non-tail self-call, or
    any other self-reference leaves the function as ordinary recursion.
  - **Row-polymorphic functions flatten too.** A function carrying leading
    offset-evidence parameters calls itself *curried* — lowering partially applies
    it to its evidence and then to the real arguments
    (`App { App { self, [ev…] }, [args] }`). A fusion pre-pass **before** reference
    counting normalizes that nested application back to a single saturated
    `self ev… args`; the flattened loop then carries the evidence as ordinary
    loop-carried parameters, passed unchanged on every back-edge (Fai has no
    polymorphic recursion, so a self-call always threads its own evidence). Fusing
    before reference counting is essential: done afterward, a plain tail call's
    evidence — consumed early by the partial application and so `dup`/`drop`-balanced
    around it — would strand that `drop` once the call became a back-edge. As a side
    effect every saturated row-polymorphic self-call (even in a non-flattened
    function) becomes a direct call rather than building a partial-application
    closure.
  - **Mutual recursion flattens too (intra-module, plain-tail first cut).** A group
    of functions that tail-call one another in a cycle (e.g. `isEven`/`isOdd`) is
    reduced to *self*-recursion: a per-file analysis (`mutual_groups`) finds the
    plain-saturated-tail-call SCCs of size ≥ 2 whose every member is monomorphic,
    lambda-free, and references group members only through plain tail calls, then a
    synthetic **combined** function is built whose body dispatches on a leading tag
    parameter (`if tag == 0 then <member 0> else …`) with every group-internal tail
    call rewritten to a saturated *self*-call of the combined function carrying the
    target's tag. Ordinary reference counting and this very transform then turn it
    into one `Join`/`Recur` loop — so no new IR or code generation. Each member
    becomes a thin wrapper (`f args = combined(tag, args, padding)`). The combined
    function is not source-backed, so (like a contract harness) it is
    reference-counted in memory and assembled at build time — emitted as an extra
    object/def per group alongside the `fai_main` trampoline — leaving the cached
    per-definition `object_code` path untouched; reachability still finds members
    (and their callees) through their original bodies. Cross-module groups and
    constructor-wrapped ("modulo cons") mutual calls are left as ordinary recursion.
  - **Borrowing yields to it (amends D79).** A parameter that flows into a
    saturated tail self-call is **owned, not borrowed**: a lent argument must be
    dropped *after* the call, which would push the call out of tail position. So an
    accumulator fold (`sum`/`length`/`find`) is owned, runs in constant stack, and
    frees its input cell-by-cell. Non-tail self-calls (`1 + f r`) are unaffected.
  - **IR.** A generic loop (`Join`/`Recur`) plus, for a constructor-wrapped
    recursion, **destination passing**: a non-reference-counted "hole" token (the
    same shape as a reuse token) threads through the loop; each iteration builds its
    cell with a placeholder recursive field, links it into the spine (`HoleFill`),
    and advances, and the base case fills the final hole (`HoleClose`). A recursion
    nested several constructors deep links a **chain** of `HoleFill`s (one per cell)
    — no new node: the cells are built in their original (reference-count) order and
    then linked outermost-first, so the outer cell goes at the loop hole and each
    inner cell into its parent's recursive field. The per-iteration reuse token is
    consumed by one cell build **before** the back-edge, so a unique list still
    rebuilds with zero fresh allocations for that cell. The nodes flow through the
    pretty-printer, the content fingerprint, and the daemon wire form.
  - **Code generation.** A Cranelift loop: the header carries the loop locals as
    variables (sealed after its `Recur` back-edges), the holes lower to inline
    pointer stores into a per-frame result slot, and a tail-position translator
    routes `Recur` to the header and the base/`HoleClose` to the loop exit.
  - **Soundness.** The abstract reference-count interpreter models the new nodes
    (the hole as a linear token; loop balance via the existing per-path consistency
    check), so the corpus and whole-program oracles cover the transformed output;
    the differential allocation tests confirm a unique list still recycles its
    spine (for monomorphic, row-polymorphic, *and* nested-constructor rebuilds), and
    deep end-to-end runs (JIT and AOT) confirm constant stack and a leak-free exit
    (including a deep mutual `isEven`/`isOdd`). The reorder-safety of hoisting a
    later argument is **not** covered by the reference-count oracle (it does not
    model effect ordering), so the `is_pure_total` analysis is the guarantee and
    carries its own conservative test matrix; likewise the combined function for a
    mutual group is reference-counted and checked like any other definition. The
    remaining noted future generalizations are **cross-module** mutual groups and
    **constructor-wrapped ("modulo cons") mutual** calls (the test harness also does
    not flatten mutual recursion, which is harmless on its small generated inputs).

- **D100 Inter-procedural argument borrowing (amends D79).** Borrow inference now
  consults callees' signatures, so a parameter only *forwarded* to another
  function's borrowing parameter is itself borrowed. This implements the
  "future refinement" D79 named, superseding its self-contained/acyclic property.
  - **The exploit.** In `call_arg_borrows`, a saturated direct call to another
    function reads that function's `borrow_signature` (gated by `exploitable_at`,
    the same saturation test code generation uses for a direct call), so the args
    it lends mirror the callee's borrowed parameters. A self-call still uses the
    in-progress signature from the function's own local fixpoint (so self-recursion
    — the common case — never enters a salsa cycle).
  - **Sound regardless of precision.** Borrowing is sound by construction (a
    borrowed parameter is a capture: duplicated on a consuming use, never dropped;
    the caller releases it), so over- or under-borrowing only adds or removes a
    dup — never a leak or double free. The same `borrow_signature` feeds the RC
    pass (`arg_borrows`), code generation (the two-entry-point ABI), and the
    soundness interpreter, so the caller-side drops always match the assumed sig.
  - **Cycles: the first deliberate salsa cycle.** An acyclic call graph resolves
    as ordinary query dependencies. Cross-module mutual recursion (which the call
    graph permits — unlike the type-SCC graph, signatures do **not** cut a real
    call, so a borrow SCC can span files) forms a salsa cycle resolved by a
    **monotone fixpoint** (`cycle_fn`/`cycle_initial`, `CycleRecoveryStrategy::
    Fixpoint`). It starts **optimistic** — every parameter borrowed (the top of the
    lattice) — so it converges to the *greatest*, most precise, sound signature; an
    all-owned start would be a trivial fixpoint that never borrows across a cycle.
    The step is monotone (a more-borrowed callee can only make a forwarding caller
    more-borrowed) over a finite lattice, so it converges in a few rounds; a
    high-iteration **fallback to all-owned** keeps the query total for a
    pathologically large recursion cluster, well below salsa's own iteration cap.
    This is the project's first use of salsa cycle recovery — chosen over a manual
    cross-module call-SCC + joint fixpoint because it is far smaller and keeps the
    per-definition incremental model (no whole-call-graph SCC query).
  - **Firewall.** `borrow_signature(A)` now depends on its saturated callees'
    `borrow_signature`. Early cutoff on the small `BorrowSig` value bounds the
    ripple: editing a callee body re-runs a forwarding caller's
    `borrow_signature`/`rc`/`object_code` only when the callee's borrow signature
    actually changes — analogous to a public-signature change, and confirmed
    workspace-size-independent by the perf guards.

- **D101 Inline drops of monomorphic data cells (extends D78).** Code generation
  now releases a dropped local whose static type is a fixed-shape data cell — a
  non-empty **closed record** or a **tuple** — with inlined IR instead of a
  `fai_drop` call.
  - **What it emits.** Decrement the cell's reference count in place; branch on
    zero; on the dead path load each **boxed** field at its constant offset and
    release it with `fai_drop` (immediate fields — `Bool`/`Unit`/`Char` — are
    skipped at compile time), then reclaim the cell's memory with a new `fai_free`
    runtime export. The common still-shared case is the bare decrement-and-branch.
  - **What it actually saves (premise corrected).** There is no indirect "scan"
    call to remove: `fai_drop` recovers a dead object's children by **comparing its
    descriptor address** against the known kinds (data is compared first), not by an
    indirect call through a function pointer. The inlining saves the `fai_drop`
    call, that descriptor load and comparison, the field-count-from-size
    arithmetic, and the per-immediate-field `is_boxed` checks of the runtime's
    field loop. The win is small (after reuse, hot-path cells are recycled, not
    dropped); it is taken because it is correctness-neutral and immediate fields
    then drop for free.
  - **Scope (sound by construction).** Only **closed** records (exact field count)
    and tuples qualify: a value of such a type is always a boxed cell with exactly
    those fields at the canonical layout offsets (records sorted by label, tuples
    positional), with no constructor-tag variation. Excluded — discriminated unions
    and `List` (field count varies by tag), open/row-polymorphic records (unknown
    count), the empty record (a tagged immediate), and anything reached only as a
    **parameter** (parameter types are absent from code generation's `let`-value
    type map, so they take the runtime path). Children are released through
    `fai_drop` rather than recursing the inlining, so deep structures stay
    iterative and the emitted code stays small; the cell is freed **last** — the
    heap is acyclic, so a child drop can never reach the parent, and the field
    pointers are read before the free.
  - **Width cap.** A cell with more than eight **boxed** fields takes the runtime
    path, bounding generated-code growth (immediate fields are free to skip and do
    not count toward the cap, so a wide mostly-immediate record still inlines).
  - **The IR is unchanged.** This is purely a code-generation lowering of the
    existing `Drop` node, so the reference-count soundness interpreter is
    unaffected. `fai_free(v)` reclaims a dead, child-released cell's memory and
    decrements the live-object counter (debug-only; see D110) — an
    `unsafe extern "C"` carrying the precondition the inlined drop establishes.
  - **Acceptance.** A classifier unit-test matrix pins which static types
    specialize; an IR-inspection test pins that a specialized drop emits a
    reference-count branch (`brif`) while a `List` drop does not; and a behavioral
    matrix (record with a boxed child, tuple, all-immediate record, nested record,
    tail-position loop drop, shared `rc > 1` drop) exits leak-free with the expected
    output.

- **D102 A leading `|` on a union is optional (refines D-era union syntax).** A
  discriminated union may be written without the leading pipe — `type T = A | B`,
  the same union as the canonical `| A | B`. The parser reads the type body as a
  type expression and, if a `|` follows, reinterprets it as the first variant
  (its application spine `Con atom…` is the constructor name and its field types)
  and parses the remaining `| …` variants.
  - **Why it is unambiguous.** A union is signalled by the presence of a `|`; no
    transparent alias has a top-level `|` (record-row `|` lives inside `{ … }`,
    and there is no structural-union type). The previous behavior silently parsed
    `type T = A | B` as an alias to `A` and dropped `| B`, which was a latent
    bug, not a competing meaning.
  - **Spellings.** Because `|` is a layout *continuation* token, the single-line,
    inline-first-variant (`type T = A` then `  | B`), and
    indented-without-leading-pipe forms all parse to one union. `fai fmt`
    normalizes every spelling back to the canonical leading-pipe form, so the
    canonical layout is unchanged.
  - **Limit.** A lone nullary variant still needs the pipe: `type T = A` (no `|`)
    stays a transparent alias, since nothing distinguishes it from one. A
    qualified or non-constructor head before the `|` is a recoverable syntax
    error.
  - **Exhaustiveness robustness (fixes #27).** Independently, the usefulness
    checker no longer panics on an arity-inconsistent pattern matrix. An
    unresolved constructor (whose tag/arity are unknown) is lowered to a unique,
    unmatchable value rather than to tag 0 — so it cannot collide with a real
    first constructor and leave a matrix row shorter than its column — and the
    matrix split/first-column reads guard against short rows. The unbound-name
    error is reported as before; the bogus arm is no longer also flagged
    unreachable.
- **D103 Isolated-worker contract execution + daemon `test`, resume-on-crash
  (supersedes the in-process part of D83; reuses D63–D65).** `fai test` no longer
  checks contracts in-process. The warm front end builds a portable
  **`TestWireBundle`** — the synthesized harness/property/`Arbitrary` defs plus
  the reachable callees, and the list of contract entries with their generator
  configuration — and a supervisor ships it to the same isolated worker `fai run`
  uses (a hidden `__test-worker` subcommand) under a wall-clock timeout
  (`FAI_TEST_TIMEOUT_MS`) and the self-imposed `RLIMIT_CPU`. The worker JIT-compiles
  the bundle once and applies each contract from a start index, streaming one
  newline-delimited result frame per contract (position, pass/fail, raw
  counterexample, live-object delta) **after** fully dropping each result.
  - **Resume on crash.** A generated input that drives a body into a runtime trap
    (e.g. integer division by zero, which the runtime turns into a process abort)
    kills *the worker*, not the run: the supervisor takes the first un-acked
    contract as the culprit, records it as **`FAI6003`** (aborted; a timeout is the
    same code), and re-spawns a worker to resume after it. Each spawn advances past
    at least one contract, so the loop terminates in at most *n* spawns. The
    happy path is a single worker, a single JIT. The resume state machine is pure
    over a spawn closure (unit-tested with a mock spawner).
  - **One execution path.** The worker, the in-process `fai_driver::test` library
    entry point (retained for the corpus tests; no isolation, for known-safe
    inputs), and the daemon all share one `run_contracts` over the reconstructed
    bundle, so behavior is identical; only the spawn/resume wrapper is worker-only.
  - **Daemon + streaming.** The daemon serves `test` as a dedicated streaming
    request (like `run`): it builds the plan under the session lock, supervises the
    worker(s) **off-lock** streaming each contract as a `$/testEvent`, then renders
    the report under the lock as the terminal result. The CLI prints live
    per-contract lines (human mode) from the same shared formatter in both paths
    and the same final report, so warm output is byte-identical to `--no-daemon`.
  - **Per-contract config + richer output.** Each contract carries its own
    `seed`/`trials`/`max_size` in the bundle (uniform — the global flags — for now;
    the structure admits a future per-property source override) and the JSON
    `TestOutput` gains a top-level `seed` and an `events` array (one `TestEvent`
    per contract: ordinal, subject symbol, kind, status, counterexample, and the
    config it ran with). The per-contract live-object soundness check moves into
    the worker (a nonzero delta is a located internal error).
  - **Wire types fix (corrects D63).** Codegen does *not* fully ignore node types:
    it reads the first operand's type to pick the borrowed vs owned runtime variant
    of an inspect-only primitive (`=`, `compare`, the `String` ops), the same
    decision reference counting made when it inserted the matching drops. The wire
    form drops types, so that decision was being re-derived from a placeholder type
    and silently flipped — a latent double-free for any boxed structural `=`/
    `compare`/`String` op shipped to a worker (it never bit `run` because such
    programs were rare, but property bodies hit it constantly). The bundle now
    carries the borrow decision per primitive and restores it as a boxed-type
    marker on the first operand; the other type uses in codegen
    (immediate/fixed-shape drop) are optimizations that safely fall back when the
    type is a placeholder.

Editor integration:

- **D103 VS Code extension (`editors/vscode/`).** An official editor integration:
  a thin `vscode-languageclient` for the `fai lsp` server, a `fai` language
  contribution + `language-configuration.json`, and a TextMate grammar
  (`source.fai`) for highlighting. It lives outside the Cargo workspace, with its
  own TypeScript/esbuild tooling and a separate CI workflow.
  - **Multi-root is client-side.** `fai lsp` is single-root by construction (it
    opens one warm `Session` for the root it is handed and does **not** read the
    LSP `rootUri`/`workspaceFolders`), matching the per-workspace-root
    session/daemon/cache model. So the client launches **one server per workspace
    folder**, passing the root as `fai lsp --project <folder>` (plus `cwd`), and
    confines each client to its folder with a document-selector glob; servers
    start/stop as folders are added/removed. A `.fai` file outside every folder
    gets highlighting but no language features. This needs no compiler change; a
    single server multiplexing roots was rejected as a large `fai-lsp` refactor at
    the wrong layer.
  - **Shipped as CommonJS, authored as ESM.** VS Code cannot load an ESM
    extension entry point (microsoft/vscode#130367 is open/backlog; the 1.94 ESM
    migration was core-only and explicitly excluded extensions), so esbuild emits
    a CommonJS `dist/extension.cjs` even though the source and the test are
    authored as ES modules. Only `vscode` is external; everything else
    (including `vscode-languageclient`) is bundled, so the package ships no
    `node_modules`.
  - **The grammar is a highlighting aid, pinned against drift.** `fai-syntax`
    stays the single source of truth (R18). The grammar mirrors the lexer's
    dispatch order (the `'a'` char-literal vs `'a` type-variable rule, the three
    comment forms, `_`-separated/hex/oct/bin/float numerics, maximal-munch
    operators with the reserved `-> :: = | :` carved out, upper-vs-lower idents).
    A Node test tokenizes every `samples/*.fai` and fails on any `invalid` scope
    or any non-whitespace span left with only the root `source.fai` scope, so a
    lexer change that the grammar does not track breaks CI. Stronger golden token
    snapshots were rejected to keep maintenance low.

- **D104 Informational CI benchmark report (non-gating; runs on every pull
  request).** The wall-clock benches run in CI to **publish a report**, never to
  gate on timings. A separate `Benchmarks` workflow (`.github/workflows/bench.yml`)
  runs `cargo bench` on **every pull request**, on `main`, and on demand (a single
  Linux runner, `DIVAN_MAX_TIME` bounding the heavy cases), renders a Markdown
  summary onto the run page, and uploads the raw output plus a parsed
  `bench-results.json` as artifacts. The deterministic guard tests remain the
  **sole** performance gate (shared runners are too noisy to gate on timings);
  every other CI run still merely compiles the benches to prevent bitrot.
  Trend-over-time tracking is deliberately left to the artifacts (no
  gh-pages/threshold automation) to keep the anti-flakiness stance intact.
  - **Pull-request runs use a short settle time, so running them is cheap.** A
    pull-request run sets `DIVAN_MAX_TIME=1` (vs `120` on `main`/on demand), so the
    whole suite finishes in roughly the test job's wall time — measured at about
    three and a half minutes from a cold build, dominated by the release compile —
    rather than the long settle the steady-median main report needs. A PR run still
    **executes** every benchmark, so it fails (via `pipefail`) when a benchmark
    crashes, fails to build-and-run, or — for the Fai-vs-Rust algorithm benches,
    which assert the compiled result against the Rust oracle in untimed setup —
    produces a wrong result; it never fails on a timing. A push to a PR supersedes
    its in-flight run (`cancel-in-progress` on the `pull_request` event only), and
    a tighter `timeout-minutes` fails a hung PR bench fast; the `main`/on-demand
    run is never cancelled so its long report always completes. This makes a perf
    change visible on the PR that introduced it (and catches bench bitrot before
    merge) while keeping timings strictly non-gating.
  - **Parsing divan, not a new harness.** divan has no machine-readable output,
    so a small in-tree tool (`fai-tests`' `bench_summary` module + `bench-summary`
    bin, unit-tested over a captured fixture) parses its Unicode-tree text. Writing
    the parser in Rust (rather than a shell/Python script) keeps it covered by the
    normal test suite; it degrades gracefully (an unrecognized line is skipped)
    so a divan format change thins the report instead of failing the job.
  - **`edit→test` measured at two levels.** The contract loop is benched both
    in-process (`fai-tests`' `contracts` bench — front end + synthesize + JIT +
    run, scaling to large workspaces) and end-to-end through the real binary and
    its daemon (`fai-cli`'s `test_loop` bench — adding the client/daemon round trip
    and the worker subprocess + IPC). No new deterministic guard was added: the
    incremental front end it exercises is already guarded, and synthesis + JIT are
    per-run by design and so not deterministically countable.
  - **Language-server benches over real code, linked to source.** Beyond the
    synthetic corpus, the `lsp` bench probes a hand-written multi-module
    application (`fai-corpus`'s `realworld` fixtures, living under `samples/` so the
    sample suite keeps them green). Each probe's divan argument label is its
    `<path>.fai#Lnn` source location, so the rendered report links every
    real-world row to the exact line it measured. The corpus generator itself moved
    into the standalone `fai-corpus` crate so both `fai-tests` and `fai-cli`'s
    benches can share it without a dependency cycle.

- **D105 Daemon traffic tap (realizes the D15 "JSON tap").** `fai daemon tap`
  observes a workspace daemon's live traffic for debugging. A `Tap` request turns
  its connection into a passive subscriber; the daemon then **broadcasts** a JSON
  decode of every frame read or written on every *other* connection (requests,
  responses, streamed `$/output`, `$/testEvent`) as a `TapFrame { conn, direction,
  json }`, which the client prints one per line. This is the cross-connection
  surface D15 anticipated when it kept binary framing "a JSON tap keeps
  debuggability".
  - **One read/write choke point.** Each served connection runs through a `Conn`
    wrapper whose `read`/`send` mirror the frame to subscribers, so the tap feed
    is complete without each call site remembering to broadcast. The cost is gated
    on a relaxed atomic: with no tap attached (the common case) a broadcast is one
    load and the frame is never serialized, so the warm `run`/`test` streaming
    path is unaffected.
  - **Best-effort, bounded delivery.** Subscribers have a bounded buffer; a tap
    that falls behind drops the surplus rather than throttling the connection
    producing it (a debug observer must never affect real work), and a
    disconnected tap is pruned on the next broadcast. The subscription is
    acknowledged (`Ok`) *before* streaming, so a client that waits for the ack
    observes every later frame with no startup race. `tap` auto-spawns a daemon
    like `start`. Rejected: an unbounded buffer (a forgotten tap could grow daemon
    memory without bound).
- **D106 `fai check` evaluates closed `example` contracts (extends D12/D84;
  reuses D103).** A closed `example` has no binders, so it can be evaluated
  eagerly without generation. `fai check` now does so: after the selection
  type-checks clean, each closed `example` in the selected files is run and a
  failure is reported as the located **`FAI6001`** — the same diagnostic `fai
  test` produces — so a wrong example is caught in the fast inner loop, not only
  by `fai test`.
  - **Reuse the isolated worker, not in-process evaluation.** An example can trap
    (e.g. division by zero) or loop, which would crash or hang the checker — fatal
    in the warm daemon. So check reuses the **same `__test-worker` subprocess**
    `fai test` uses (D103): it builds an example-only `TestPlan`
    (`build_example_plan`, `forall`s excluded) and runs it under a **shorter
    wall-clock limit** (`FAI_CHECK_TIMEOUT_MS`, default 10s) than the test limit,
    because the daemon serves check **under the session lock** (the generic
    command path, not a dedicated off-lock handler — a deliberate
    simplicity/responsiveness trade, since the happy path is one fast JIT and a
    runaway example is bounded by the short limit). Off-lock execution and
    memoizing results by the reachable rc-hash are noted as future work.
  - **Definite failures only.** Check reports `FAI6001` and nothing else: an
    example that aborts/times out (`FAI6003`), one that cannot be compiled
    (`FAI6002`), and a live-object leak are all dropped here, leaving `fai test`
    authoritative for them and for every `forall`. A type error skips example
    evaluation entirely (the body could not be compiled soundly), and a file with
    no example pays nothing (no plan, no worker).
  - **Opt-out.** `fai check --no-examples` restores a pure type-check (an
    `examples` flag on the `Check` command spec, flowing to the daemon). The
    front-end `check` query stays pure — example evaluation lives in the command
    path — so the LSP's per-keystroke diagnostics and the incremental firewall are
    unchanged.
  - **Editor: on save, not per keystroke.** The language server evaluates the
    saved file's examples on `didSave` (in the worker) and caches the failures per
    file, re-attaching them to that file's published diagnostics until the file is
    edited (which clears them, to be recomputed on the next save) or closed.
    Running on save — not on every change — keeps typing responsive; the
    `examples` initialization option (surfaced as the `fai.examples` VS Code
    setting) disables it.
- **D107 Native `Char` (supersedes the `Char` omission in D86).** `Char` is a
  first-class native type, not just a lexer distinction. The lexer/parser/types
  already handled it; this makes it compile, run, and generate.
  - **Representation: an immediate, like `Int`/`Bool`.** A `Char` is a tagged
    Unicode scalar value, `(codepoint << 1) | 1`. A code point fits the 63-bit
    immediate payload, so there is no heap descriptor and no boxing (unlike
    `Float`, which is boxed because it needs all 64 bits). Because it is an
    immediate that shares the `Int` encoding, structural equality and ordering
    work through the existing immediate paths with no new `fai_equal`/`fai_compare`
    branch, and reference counting treats it as a no-op. Codegen already
    classified `Con::Char` as immediate; lowering now emits `Lit::Char`.
  - **Four prelude-private intrinsics.** `charToString` (a one-character
    `String`), `charToCode`/`charFromCode` (the Char/Int conversions — typed
    bitcasts, implemented as the identity at runtime since the encodings
    coincide), and `isValidCharCode` (range/surrogate check). `std/Char.fai`
    exposes `toString`, `toCode`, and a total `fromCode : Int -> Option Char`
    written as `if isValidCharCode n then Some (charFromCode n) else None` — the
    `Option` is built in Fai, keeping the runtime ADT-agnostic (the same split as
    the `FileSystem`/`Env` hosts in D-era capability wiring). Naming follows the
    representation-conversion precedent (`Float.toBits`/`fromBits`), not a
    numeric-cast spelling.
  - **Generator.** `Test.char` draws across the whole valid range (an invalid
    surrogate/out-of-range draw falls back to `'a'`) and shrinks toward `'a'`. Its
    renderer prints a valid Fai char literal — printable ASCII verbatim, the quote
    and backslash and the common control characters as named escapes, and anything
    else as `\u{hex}` (hex built in Fai from the bitwise `Int` intrinsics) — so a
    counterexample is unambiguous and always renderable. The contract harness maps
    a `Char` binder to it, so a `forall` over a `Char` is runnable (no longer
    `FAI6002`).

- **D108 Inline integer/boolean primitives with an immediate fast path (extends
  D70/D71; companion to the `opt=speed` codegen).** The hot integer primitives —
  `+ - *`, the `< <= > >=` comparisons, the bitwise `and/or/xor`, the shifts,
  unary `complement`, boolean `not`, and structural `=`/`compare` **on
  immediate-representable operands** — compile to inline machine code instead of an
  out-of-line runtime call per operation. Division and remainder were left as calls
  here (they fault on a zero divisor) but are inlined by a later refinement (see
  D117); the `Float` operations stay calls (a boxed `Float` would add a heap box and
  operand drops, so inlining waits on unboxing them).
  - **What it emits.** The fast path mirrors the runtime (`unbox_int` / operate /
    `fai_box_int`): a both-operands-immediate guard (`(a & b) & 1`), then untag
    (`sshr` by one), the native operation, and re-tag (`value << 1 | 1`). For the
    operations whose result can exceed the 63-bit immediate (`+ - *` and the
    shifts) the re-tag is guarded by `sadd_overflow(r, r)` — `r + r` overflows i64
    signed exactly when `r`'s top two bits differ, i.e. exactly when `r` no longer
    fits the immediate, which is the precise `fai_box_int` boundary; its result is
    the `r << 1` we need. `and/or/xor/complement` of immediates always fit (the
    operands' top two bits agree), so they skip the fit check. Comparisons build a
    `Bool` immediate (`false`=1, `true`=3) from the `icmp`; `compare` builds
    `-1/0/1` as `(a > b) - (a < b)` (no overflowing subtraction). `not` is
    `x ^ 2` (no guard — its operand is always an immediate `Bool`).
  - **The fallback is today's call, and it is always correct.** Whenever an
    operand is boxed (a large `Int`) or the result overflowed the immediate,
    control branches to the same runtime symbol the operation used before, which
    unboxes, operates, boxes, and consumes both operands. Because the slow path is
    unchanged and valid for *any* operands, the fast path is a pure optimization.
  - **Reference counting is unaffected.** Operands are consumed exactly as before:
    in the fast path both are immediates, so the runtime drops the operation would
    perform are no-ops and are correctly omitted; a boxed operand always takes the
    fallback, which consumes it. The IR (the `Prim` node) is unchanged — this is
    purely a code-generation lowering — so the reference-count soundness
    interpreter is untouched. Operands are evaluated once, up front, in source
    order; the fast and fallback paths reuse those values.
  - **Equality/ordering are type-directed.** `Bool`/`Char`/`Unit` are never boxed,
    so `=`/`compare` on them inline with no guard and no fallback (a small
    immediate never equals a boxed `Int`, so the mixed case the guard excludes is
    already handled correctly by the runtime). `Int` adds the guard and the
    `fai_equal`/`fai_compare` fallback. Every other operand type (strings, floats,
    records, ADTs, type variables) keeps the structural runtime path **including its
    operand borrowing**, so only the listed primitives change.
  - **Cache invalidation.** The change alters the generated object for identical
    IR/target/compiler-version, so the object cache's codegen-config stamp gains an
    `int-prims-inlined` token; a cache warmed before the change can never serve a
    pre-inlining object.
  - **Acceptance.** IR-inspection tests pin the inline op + guard `brif` + a lone
    fallback `call` for `+`/`<`/`=`, the bare (no-guard, no-call) shape for `Char`
    `=`, and that `/` stays a plain call; a boundary matrix exercises the
    immediate maximum/minimum, overflow-to-box, `wrapping_mul`, logical-shift of a
    negative, and boxed-operand fallbacks; and the JIT-vs-Rust-reference property
    test now spans the bitwise operators over full `i64`.
- **D109 Fuel-guarded generation and custom `Arbitrary` overrides (refines
  D87).** Two gaps in the per-type `Arbitrary` synthesis are closed: recursion
  that the size budget did not guard, and the inability to supply one's own
  generator.
  - **Size is a node-fuel budget, split across recursive fields.** The earlier
    rule decremented the size only for a field of the type's *own* type, which
    left mutually-recursive types and recursion through a collection field (e.g.
    `Rose (List Rose)`) unguarded — and even a directly-recursive constructor with
    more than one recursive child could blow up super-linearly. Now a field is
    "recursive" when its type can **reach** the type being generated (a transitive
    walk of the type graph: tuple/record/`List`/`Option`/`Result` arguments and
    constructor fields, with custom-overridden types as opaque leaves), and a
    constructor or record with `k` recursive fields gives **each** recursive field
    `(size - 1) / k` — so the total number of generated nodes stays within the
    budget regardless of branching or mutual recursion. A recursive `List` field
    splits its slice again across its elements via a private `Test.recList`
    combinator (length within the slice, each element an equal share); the
    `Option`/`Result` wrappers bottom out at the floor (`None` / `Ok`), and `List`
    already does (`[]`).
  - **The base case is rank-driven.** A least-fixpoint `rank` (smallest-value
    depth; a cycle with no base ⇒ ungeneratable) is computed over the reachable
    types. At the budget floor a constructor is eligible only if it is of minimal
    rank, so floor generation strictly shrinks and always terminates — which the
    old "no self-typed field" heuristic got wrong for a type like `Rose` (its only
    constructor is "recursive" yet bottoms out through the empty list) and for
    mutually-recursive types (where a constructor's field forces another type that
    itself grounds). A binder whose type has no finite value is reported
    **`FAI6005`** (a non-groundable type) rather than diverging.
  - **`Result` grounds through its `Ok` side.** Rank/groundability of `Result X Y`
    follow `X`, and the floor grounds to `Ok`. A type whose only base case is
    reachable through the `Err` side (e.g. `type T = MkT (Result T Int)`) is
    reported `FAI6005` rather than generated — a deliberately accepted limitation
    for a rare shape.
  - **User-supplied generators override synthesis.** A top-level value in the
    contract's file whose type is `Arbitrary T` (the closed `{ gen, show, shrink }`
    record, recognized by its `show : T -> String` field) overrides the
    synthesized generator for `T`, checked at the top of `arb_for` so it applies
    wherever `T` is generated (as a binder, a tuple component, or nested in another
    type) and bypasses the groundability analysis (so a user can generate a type
    the synthesizer would reject). Overrides apply to **user records/ADTs only**,
    not the built-in generators or the `Option`/`Result` wrappers. Two matching
    definitions for one type are ambiguous, reported **`FAI6006`**. Parametric
    custom combinators (`Arbitrary 'a -> Arbitrary (T 'a)`) are out of scope (the
    discovered value must be a monomorphic `Arbitrary T`).
- **D110 Finite float generation (amends D86; supersedes the full-domain default
  of D85/D86).** The default `Float` generator emitted any 64-bit pattern via
  `Float.fromBits`, so it produced NaN, ±infinity, and astronomically large
  magnitudes. On float-arithmetic laws those are technically true counterexamples
  but rarely what a law author means (e.g. `x * 0.0` is `0.0` for every finite
  `x` but `NaN` for `x = inf`), and they overflow to infinity under further
  arithmetic. So `Test.float` now generates a **finite, size-bounded** value:
  take a word's top 53 bits as a fraction in `[0, 1)` (divide by `2^53`), scale by
  the size budget, and pick a sign from the low bit — giving a magnitude in
  `[0, size)` that grows with `size` (like `int`'s `[-size, size]`), never NaN/inf,
  and never overflowing. A zero magnitude is forced to `+0.0` (not `-0.0`). The
  shrinker drives a counterexample toward simple values: `0.0`, then the
  whole-number truncation, then half — so a genuine failure reports a clean
  counterexample (e.g. `x < 1.0` shrinks to `x = 1.0`). The full-domain generator
  is retained as **`Test.floatAll`** (the old `Float.fromBits` behavior) for
  bit-level and round-trip tests; it is a building block (reachable, like any
  generator, through a user newtype's custom `Arbitrary`), since built-in scalar
  binders are not overridable. Note that structural `=` on `Float` is **bitwise**
  (so `-0.0 <> 0.0`), so a law expected to hold should compare with the IEEE
  ordering operators (`>=`/`<=`/`<`/`>`), not `=`.

- **D110 Debug-gated leak counters (refines D77/D-era runtime).** The runtime's
  live-object and cumulative-allocation counters (`LIVE`/`ALLOCATIONS`, behind
  `live_count`/`allocations`/`reset_allocations`) exist only to detect leaks and
  to make reuse observable in tests, so they are compiled in only under
  `debug_assertions`. A release build performs no per-alloc/free atomics (three
  relaxed atomics per allocation pair were pure hot-path overhead on every
  allocating program); with the counters absent, `live_count`/`allocations`
  report zero and `run_entry`'s end-of-run leak check and the per-contract
  `live_delta` are no-ops.
  - **Why this is safe to gate.** All heap allocation already flows through
    `alloc_obj`/`free_obj` (every constructor, box, closure, and reuse-miss calls
    them), which is why the counter was accurate; routing the increments through
    `note_alloc`/`note_free` (no-ops in release) keeps that invariant while
    centralizing the gate. The accessors stay `pub` and return zero in release so
    callers in other crates compile unchanged.
  - **The toolchain has counters iff it was built with debug assertions.** The
    in-process JIT runtime inherits `debug_assertions` from the cargo profile (on
    for `cargo test`, off for release/bench). The embedded AOT runtime archive is
    built by the driver's build script with a matching `-C debug-assertions`
    (read from `CARGO_CFG_DEBUG_ASSERTIONS`), still optimized — so the native
    end-to-end tests keep their leak check under `cargo test`, while a shipped
    `fai build` and the benchmarks link a counter-free runtime. An optimized build
    can opt the counters back in with `[profile.release] debug-assertions = true`.
  - **Tests are debug-only by nature.** The counter-asserting tests (the runtime
    unit/property/reuse tests, the codegen JIT tests, and the end-to-end reuse
    allocation-count tests) are meaningful only in a debug build, which CI always
    uses; a `--release` test run would see zero and is not supported.

- **D111 Size-class recycling allocator.** `alloc_obj`/`free_obj` no longer go to
  the system allocator per object. A freed cell is kept on a free list and handed
  back to the next allocation of the same size, so the common allocate/free pair
  (cons cells, boxes, small records) becomes a few pointer moves instead of a
  `malloc`/`free`. Sizes above `MAX_POOLED_SIZE` (512 B — rare: large strings,
  wide records) bypass the pool and use the system allocator directly.
  - **Exact 8-byte classes.** Every heap object is 8-aligned and a multiple of 8
    bytes, so the class is `size.div_ceil(8)` and a class's cells have capacity
    equal to the request — no internal fragmentation, and the dominant shapes
    (cons 48 B, `Int`/`Float` box 32 B) recycle perfectly among themselves. A
    pool miss takes a fresh block at the class capacity, so all cells of a class
    are interchangeable and a cell's class (hence its deallocation layout) is
    stable across reuse.
  - **Thread-local, no synchronization.** The free lists are thread-local. This
    is sound because Fai execution is single-threaded and a cell is always
    allocated and freed on the same thread, so a list is only ever touched by its
    owning thread — no atomics or locks on the hot path (the point of the change
    was to *remove* allocation overhead, not relocate it). The list is intrusive:
    a dead cell's first word (its now-unused reference-count slot) holds the
    next-free pointer, so pooling allocates nothing itself, and `alloc_obj`'s
    header rewrite repurposes a recycled cell (descriptor, size, and that next
    pointer are all overwritten).
  - **Recycled until thread exit.** Blocks are retained for reuse and returned to
    the system allocator only when the owning thread exits (`Pool`'s drop), so
    retention is bounded by a thread's working set. Pooled blocks are never
    `dealloc`'d while recycling, so a freed-then-reused cell is never seen as
    freed by memory tooling.
  - **Orthogonal to reuse analysis and the counters.** The pool sits *below*
    `alloc_obj`/`free_obj`, so it is invisible to the cumulative `ALLOCATIONS`
    counter (which counts logical `alloc_obj` calls): the differential
    allocation-count tests that pin Perceus in-place reuse (D77/D78) produce
    identical numbers. The live counter still balances (every `alloc_obj`
    increments and every `free_obj` decrements, pooled or not), so the leak check
    is unaffected. This is a pure runtime optimization — generated code is
    unchanged (allocation is always a runtime call) and the object cache key is
    untouched.
  - **Fuzzed and property-tested.** A custom allocator is memory-safety-sensitive,
    so one harness (`run_ops`, behind `cfg(any(test, feature = "fuzzing"))`) drives
    a decoded alloc/free sequence over sizes spanning the pooled classes and the
    large fallback, checking no-aliasing, payload integrity, and alignment after
    every step (all independent of the debug counters). It backs three drivers: a
    proptest, deterministic fixed-seed stress tests, and an out-of-workspace
    cargo-fuzz target (its own workspace, nightly-only, run by a non-gating `fuzz`
    workflow — never on the stable merge path).

- **D112 Cross-request daemon concurrency (concurrent reads + cancel-on-edit;
  supersedes D57's serialization).** The daemon no longer serializes database
  access through one mutex. A read command now runs on a **cloned database
  snapshot off-lock**, so distinct requests are served concurrently; the mutex is
  held only briefly — to sync inputs to disk and clone the snapshot — not for the
  command itself.
  - **Cloned handles, not shared borrows.** A salsa database is `Send` but not
    `Sync`, so a `&Session` cannot be shared across threads; an `RwLock<Session>`
    therefore cannot be `Sync` either. Concurrency comes from each request taking
    its own snapshot (`Session::snapshot`: a database clone sharing salsa's storage
    and memoization, plus a copy of the live-file set), exactly as intra-build
    parallelism clones handles (D95). The lock stays a `Mutex` (the brief
    sync/snapshot critical section is exclusive); the win is off-lock execution.
  - **Cancel-and-retry on input change (the policy).** salsa's input setters
    unconditionally set a cancellation flag and then block until every other handle
    is dropped (`cancel_others` waits for the clone count to reach one). So a sync
    that actually changes an input cancels in-flight reads — they unwind at the next
    query boundary and are caught (`catch_cancellation`) and retried on the new
    revision. A no-op sync touches no setter, so concurrent reads of an unchanged
    workspace never cancel each other. This policy is therefore forced by the
    engine, not chosen; it matches what CLI.md already documented. Retries do not
    re-sync (the cancelling edit is already in shared storage), which also keeps a
    side-effecting command (`fmt`/`build` writing to disk) from observing its own
    writes on a retry.
  - **A held snapshot blocks writes, not reads.** Because `cancel_others` waits for
    all clones to drop, a snapshot held across a long, non-salsa operation would
    stall edits. So `run`/`test` build their bundle/plan on a snapshot and **drop it
    before supervising the worker** (which runs up to minutes); only the brief,
    bounded eager-example worker of `fai check` keeps its snapshot (its 10 s cap
    already exists for this reason), and even then concurrent reads are unaffected —
    only an interleaved edit waits, normally for milliseconds.
  - **Registry shareable for O(1) snapshots.** The non-salsa file registry
    (`files`, `ids_by_path`) is wrapped in `Arc`, mutated copy-on-write
    (`Arc::make_mut`), so a snapshot shares it instead of copying every path
    string; copy-on-write only fires while a snapshot is alive.
  - **Observability + tests.** The daemon tracks peak concurrent reads and reports
    it in `daemon status` (`max_concurrency`), which a test drives over a threshold
    deterministically via a test-only per-read hold (`FAI_DAEMON_TEST_HOLD_MS`).
    Coverage: concurrent snapshot reads agree; an edit cancels an in-flight read;
    reads survive a storm of edits without deadlock; and end-to-end, concurrent
    `check`/`query` equal `--no-daemon`, peak concurrency exceeds one, and reads
    interleaved with edits stay well-formed.
  - **Still future work.** Client-initiated `$/cancelRequest`, client-disconnect
    cancellation, and a debounced/background sync cadence (today every request
    syncs under the brief lock).
- **D113 Unboxed monomorphic `Float` (amends the "uniform representation" of
  D-era backend; companion to D108's inline primitives).** A value whose static
  type is exactly `Float` is represented as an **unboxed `f64`** in a register,
  not a heap cell — the first representation specialization away from the uniform
  64-bit word. The boundary rule is purely type-directed (so it round-trips
  through the wire form, which carries each node's projected type): a `Float`-typed
  node yields an `f64`; it is **boxed** only where it crosses a *uniform slot* — a
  data/record/tuple field, a closure environment, an `apply_n`/first-class
  argument or result, or a generic (type-variable) position — and **unboxed** when
  read back out (a field read is a borrowing load of the box's bits; an
  `apply_n`/generic-call result or a forced `Float` global is read and released).
  - **Calling convention.** A definition's *entry* uses an unboxed-float ABI —
    scalar `Float` parameters and a scalar `Float` result travel as raw `f64`
    bits — derived from its stable type signature (a tracked `float_abi` query, so
    a caller's object depends on a callee's *signature*, not its body; the
    firewall holds). Saturated direct calls between monomorphic float functions
    thus allocate nothing — a tail-recursive float accumulator loops allocation
    free (gated by an iteration-independent allocation-count test). The
    first-class form keeps the uniform boxed representation (`apply_n`/PAP slots
    are `i64`): the static closure points at a wrapper that unboxes incoming boxed
    float arguments (releasing the boxes), drops borrowed arguments, and boxes a
    raw float result — generalizing the borrow-only owned wrapper. Both the entry
    ABI and each referenced callee's ABI are part of the object-cache key (the
    instantiated call-site type cannot distinguish a monomorphic-`Float` callee
    from a generic one instantiated at `Float`).
  - **Primitives & reference counting.** Float arithmetic, comparison, `sqrt`, the
    `Int`/`Float` conversions, bit reinterpretation, and float `=`/ordering are
    inline machine instructions on `f64` (float `=` compares raw bits, matching
    the boxed runtime's bit equality; ordering uses a no-alloc
    `fai_float_compare_bits`). An unboxed float carries no reference count, so its
    `dup`/`drop` are no-ops; a `Float` *field* inside a cell stays a boxed child.
    The boxed runtime float operations are retained as the **uniform fallback**
    for a boxed float operand — reached when a value's representation is forced
    uniform, e.g. the mutual-recursion combined function, whose shared, padded
    positional slots cannot carry an unboxed `f64`, so it erases `Float` from its
    body. A local is unboxed only when *every* observation of its type is `Float`
    (a contract binder projected by an untyped synthesized access stays boxed).
  - **Scope.** Only a *scalar* `Float` is unboxed; floats nested in
    records/tuples/lists/ADTs stay boxed heap fields. Scalarizing those (record
    SROA / multi-value returns) is future work.
  - **Cross-block representation follows the value, not the static type.** A value
    flowing across a block edge — a branch merge or a tail-loop (`Join`) exit —
    carries an `f64` for an unboxed scalar `Float`, else the uniform word. The edge
    type must be read from the **actual** value being passed, because a desugared
    `match` wraps its arms in `If` nodes typed `Error`, so the node's *static* type
    is unreliable. The branch merge and the loop exit therefore both fix their
    parameter type from the first value that reaches them (later values coerce to
    it). A `Float`-returning tail-recursive fold built from a `match` (e.g.
    summing a `List Float`) relies on this: taking the loop-exit type from the
    static type instead made the `f64` result a uniform word that the float-return
    path then unboxed as a pointer — a wild dereference. (`samples/algorithms/`'s
    `SpectralNorm` and the float folds in the runtime benchmarks cover it; before
    them only `if`-based float loops like `Pi` were exercised.)

- **D114 Opaque types (`public opaque type`; resolves the D73/D74 note that
  `Dict`/`Set` had to expose their node constructors).** A `public opaque type`
  exports a type's **name** but not its **definition** — a union's constructors,
  an alias's underlying type. Opacity is **file-scoped**: transparent within the
  declaring file (constructors, field access, and pattern matching all work
  there), abstract in every other file (the type may be named, held, passed, and
  compared structurally, but not constructed, deconstructed, or seen through). It
  requires `public` (a private type already hides everything); a lone `opaque`
  is reported and recovered as `public opaque`.
  - **Surface & representation.** A single `opaque: bool` on the `Type` item
    (only meaningful with `Public`); the formatter prints `public opaque type`.
    No new `Ty`: a *union* is already a nominal `Ty::Adt`, so opacity only hides
    its constructors; an *opaque alias* is lowered to a nominal `Ty::Adt` (its
    own canonical name) **from another file**, suppressing expansion, while the
    declaring file keeps expanding it transparently (the `decl_file == use_file`
    test the alias expander already had). Chosen over a distinct representation
    because reuse needs **zero** changes in Core/codegen/rc: values stay uniform
    64-bit boxed words and drop is **header-driven** (a dead cell scans its own
    descriptor's children), so an opaque value crosses a module boundary as an
    opaque pointer with no layout knowledge required.
  - **Enforcement.** The hiding is one rule: `module_interface` keeps an opaque
    type's name in `types` but **omits its constructors** from `ctors`, which
    cascades through `prelude_exports`/auto-import (the constructors leave the
    unqualified set) and the cross-file path resolver (a hidden constructor is the
    new **`FAI2018`**, distinct from the plain-private `FAI2003`). Treating an
    opaque type structurally from another file — field access, record
    construction, or `{ r with … }` — is **`FAI3018`**, detected where a record
    shape fails to unify with an opaque `Ty::Adt` (and at the body-vs-signature
    check, so a construction in a binder's body reports `FAI3018`, not the bare
    `FAI3004`). An opaque type's constructor-field / alias-body types are no
    longer a public surface, so the `FAI2015` privacy-leak check skips them.
  - **Structural operations stay.** `= <> < <= > >=` / `compare` remain available
    on an opaque type cross-file (they are universal structural runtime ops keyed
    on the cell header, not the static type); opacity hides construction and
    projection, not identity — consistent with the language's structural model
    and needed so `Dict`/`Set` (and sets of them) stay comparable.
  - **Contracts.** A property-test generator may **peek past opacity** to build
    values (the synthesized `Arbitrary` is compiler-generated, not user code), so
    a cross-file `forall` over an opaque type still runs. `arb` expands an opaque
    alias to its representation (`expand_alias_ty`) wherever a type enters its
    analysis (binders, constructor fields, generation), honoring a user-supplied
    `Arbitrary` override as a leaf. Opaque *unions* already generated, since `arb`
    reads `type_decls` directly.
  - **Standard library (amends D73/D74).** `Dict`/`Set` move from `Prelude` into
    their own `Dict`/`Set` modules as `public opaque type`s (so their operations,
    same-file, still build and match the nodes), and `Prelude` keeps the names
    auto-imported by **re-exporting** them as transparent aliases
    (`public type Dict 'k 'v = Dict.Dict 'k 'v`), which uses the qualified-type
    syntax of D88. No re-export mechanism or auto-import special-casing was
    needed; the alias re-export was validated against the embedded-std typecheck.

- **D115 Register calling convention for direct calls (amends the D-era uniform
  `fn(env, args)` ABI; companion to D113's unboxed `Float`).** A saturated direct
  call to a known top-level function previously spilled its arguments to a stack
  array and round-tripped them through the uniform entry `fn(env, args) -> i64`.
  Now a **direct-callable** definition uses a register-passing entry
  `fn(env, a0, …, aN) -> ret` — the value arguments flow in registers — and direct
  callers pass them directly, the dominant remaining cost for the call-bound
  algorithms (`fib`, `collatz`).
  - **Who.** Direct-callable = **non-row-polymorphic** (no offset evidence) with at
    least one parameter — exactly the definitions a saturated application reaches as
    a bare `Global` head. Row-polymorphic functions bake their evidence into a
    partial application (an `App` head, so always `apply_n`), and nullary bindings
    are values; both are reached *only* through `apply_n`, so a register entry would
    add a wrapper hop for no benefit. They keep the uniform array entry (and D113's
    raw-bits `Float` slots) **unchanged** — so capability / least-authority code,
    which is row-polymorphic, does not regress. A new `FnAbi::register_abi` flag
    (from the stable signature) records this; it is part of the object fingerprint,
    and a `reg-direct-call` code-generation cache token retires old-convention
    objects. The synthetic mutual-recursion combined loop is direct-called by its
    member wrappers, so it carries the flag too.
  - **`Float` in registers.** A scalar `Float` argument/result of a register entry
    is an **`f64` register** (not raw bits in an `i64` slot), matching the native FP
    ABI; the first-class wrapper unboxes a boxed float argument to `f64` and boxes an
    `f64` result. The raw-bits representation remains for the uniform (row-poly /
    nullary) entries.
  - **First-class form unchanged.** `fai_apply_n` calls a closure through the fixed
    uniform `fn(env, args)` pointer, so a register entry's static closure points at a
    **bridging wrapper** that loads the boxed argument array into registers (unboxing
    a float, releasing borrowed arguments) — generalizing the borrow-only / float-only
    owned wrapper.
  - **Two widenings of the direct path.** (1) A `let g = f` binding a non-row-poly
    top-level function is **copy-propagated** in lowering (every `g` becomes
    `Global f`, the binding dropped), so `g x` is a direct call and `g`-as-a-value is
    `f`'s closure — transitive, and capture-eliminating; a nullary value (non-arrow
    type) or row-polymorphic function (an `App`, not a bare `Global`) is excluded.
    (2) An **over-application** direct-calls the saturated prefix and `apply_n`s the
    surplus, which widens the borrow saturation test from `==` to `>=` in lockstep
    across the borrow inference, the reference-count pass, and code generation (the
    leading parameters follow the callee's borrow signature, the surplus are owned),
    so a borrowed prefix argument is lent and dropped by the caller, not leaked.
  - **Deferred.** Proper tail calls (`return_call`) are future work: Cranelift's
    `return_call` requires the not-ABI-stable `tail` convention on every
    participating entry, and a tail call past a pending drop (a borrowing tail call
    on an owned value) would be a use-after-free, so it needs its own design and
    test scrutiny — and the hot tail recursion is already compiled to loops.

- **D116 `Dict`/`Set` are weight-balanced search trees with a fuller API.** The
  ordered map and set were plain (unbalanced) binary search trees, so inserting
  keys in sorted order — what most map/set code does — built a linear chain and
  made every operation O(n) (the benchmark `Dict`/`Set` workloads ran O(n²)). They
  are now **weight-balanced** (bounded-balance) trees: each node caches its
  subtree `size`, balance is restored by rotations under the verified parameters
  `delta = 3`, `ratio = 2` (correct under deletion), so all operations are O(log n)
  regardless of insertion order and `size` is O(1). The change is internal — the
  types stay `opaque`, so the public surface (and every caller) is unaffected — and
  it removes a latent stack overflow (a sorted build no longer recurses O(n) deep).
  The API grew to a full ordered-collection surface: `singleton`/`isEmpty`/
  `remove`/`update`/`keys`/`values`/`map`/`filter`/`foldl`/`foldr`/`union`/
  `intersection`/`difference` for `Dict` (plus `isSubset` for `Set`), the bulk
  set operations built on `join`/`split`.
  - **Reuse status (what is and isn't in-place).** `Dict.map` preserves the tree
    shape — its recursion is *embedded in the constructor* (`DictNode s (map f l) k
    (f k v) (map f r)`), so the reuse pass (D77) resets the matched cell **before**
    the recursive call and a uniquely-owned `map` recycles every node in place
    (pinned by a differential allocation-count test). `insert`/`remove` originally
    did **not**: they bind the recursion first (`let l2 = insert … l` then `if rotate
    … else build`), so the reset landed **after** the recursive call, the
    recursed-into child was still shared, and a unique build path-copied at
    **O(n log n) allocations**. That reset-placement limitation is now resolved (see
    D120), so a uniquely-owned `insert`/`remove` build is O(n); the time win
    (O(n²)→O(n log n)) was always independent of it.
  - **Two constraints the rewrite had to respect.** (1) The contract generator
    only honors a *monomorphic* `Arbitrary T` override, not a parametric combinator,
    so a synthesized `Dict`/`Set` value carries a meaningless cached `size` (a law
    reading it would fail). The `forall` laws therefore build trees through the
    public API from generated key *lists* and observe via `toList`/`get`/`member`/
    `size`. (2) A balanced tree's structural equality is shape-sensitive (two maps
    with the same entries built differently are unequal), so laws compare via
    `toList`, never `=` on a map/set.

- **D117 Inline integer division and remainder (extends D108).** `/` and `%` on
  `Int` were the last arithmetic primitives compiled as out-of-line runtime calls
  (`fai_int_div`/`fai_int_rem`); D108 left them out because they fault on a zero
  divisor. They now compile inline, which removes the per-operation call from
  integer code — the bottleneck for `collatz` (`if n % 2 = 0 then n / 2 else
  3 * n + 1`) once register direct calls (D115) erased the call overhead. The
  runtime fallbacks are retained unchanged for the cases the fast paths exclude, so
  the fault behavior and `wrapping_div`/`wrapping_rem` overflow semantics are
  preserved exactly.
  - **Shape chosen from the divisor.** A literal divisor is always non-negative —
    a negation lowers to `0 - n`, never a negative literal — so a literal `d` is
    `0` or a positive constant, and three constant shapes plus a general path
    cover everything:
    - **`d == 0`** keeps the plain runtime call, so a literal `10 / 0` still aborts
      with the located message (a native `sdiv`/`srem` by zero is a raw hardware
      trap with no message).
    - **`d` a positive power of two `2^k`** (immediate) **strength-reduces to a
      shift**: `q = (x + bias) >> k` with `bias = (x >> 63) >>u (64 - k)` (the
      signed-truncation correction — an arithmetic shift alone floors), and the
      remainder is `x - (q << k)`. No zero or overflow guard is needed (the divisor
      is a known nonzero power of two and the result always fits the immediate).
    - **any other in-range positive constant** divides with the native
      `sdiv`/`srem` and **no zero guard and no fit check** — the divisor is
      statically nonzero and never `-1`, and with `|d| >= 1` the result always fits
      (the backend further strength-reduces a constant divide to a multiply).
    - **a variable divisor** (or a constant too large to be immediate) takes the
      **general path**: a both-operands-immediate guard, then a zero-divisor branch
      to the runtime fault, the native op, and — for division only — the immediate
      fit check.
  - **The fit check is division-only and lives only in the general path.** With two
    immediate operands the hardware `INT_MIN / -1` overflow trap is unreachable
    (an immediate untags to `[-2^62, 2^62-1]`, never `i64::MIN`), so only a zero
    divisor must be guarded before the native op. The one quotient that overflows
    the *immediate* is `(-2^62) / -1 = 2^62`; the existing `sadd_overflow(r, r)`
    fit check (shared with `inline_arith`) routes that lone case to the fallback,
    which boxes it. A remainder is always strictly smaller in magnitude than the
    divisor, so it never needs a fit check.
  - **Reference counting and the IR are unchanged.** As with D108 this is purely a
    code-generation lowering — the Core `Prim` node is untouched, so the
    reference-count soundness interpreter and the reorder-safety analysis (which
    already treats a non-literal/zero divisor as fault-capable) are unaffected. In
    every fast path the dividend is an immediate, so its drop is a no-op and is
    correctly omitted; a boxed dividend takes the fallback, which consumes the
    operands. The constant paths gate on the divisor fitting the immediate so an
    unused-in-the-fast-path divisor is never a heap box that could leak.
  - **Cache invalidation.** The generated object changes for identical
    IR/target/compiler-version, so the codegen-config stamp gains a
    `divrem-inlined` token; a cache warmed before the change cannot serve a
    pre-inlining object.
  - **Acceptance.** IR-inspection tests pin each shape — the general `sdiv`+fit
    check, the `srem` with no fit check, the power-of-two strength reduction (no
    native op, the sign-bias `ushr`), the guard-elided constant divide, and the
    bare `x / 0` call; behavior tests cover truncation and remainder sign for both
    operand signs across the paths, the overflow-to-box, and boxed-operand
    fallbacks; two JIT-vs-Rust property tests (variable and constant divisors)
    check agreement with `wrapping_div`/`wrapping_rem` over arbitrary operands; and
    end-to-end tests confirm a variable zero divisor and a remainder by zero still
    abort with the located fault.

- **D118 Type-level effect rows over the capability model (realizes #35;
  amends D4 and the no-subtyping stance of D75/D9 for the effect dimension).**
  Every arrow carries an **effect row** — the host-capability interfaces that
  applying it *uses* — so a function's reach is visible in its type. A bare arrow
  is pure; `a -> b / { Console | 'e }` uses the console plus a forwarded tail.
  - **Representation.** `Ty::Arrow` gains an `EffectRow` (sorted `InterfaceRef`
    atoms + a `Closed`/`Open` tail, mirroring `RecordRow`); `Scheme` quantifies
    effect-row variables; the solver carries a **third parallel union-find** for
    effect rows alongside the type and record-row ones. Effects are **erased**
    after the front end (Core/codegen/the daemon wire form are unchanged), so
    only inference and rendering touch them.
  - **Used, not held (D8-style precision).** An effect is incurred where a
    capability *method is applied* (directly, or transitively through a function
    or a captured closure), not where a capability value is held. So a pure
    projector (`getConsole rt = rt.console`) and a closure *builder* are pure; the
    effect rides the closure's own arrow, closing the laundering hole that the
    value-flow capability model alone leaves. An interface method's declared
    arrow effect is its incurred effect; the host capabilities self-label
    (`Console.writeLine : String -> Unit / { Console }`).
  - **Inferred, coupled, required on public.** A body's latent effect is the
    union of what it applies; it lands on the function's saturating arrow (so
    higher-order functions are effect-polymorphic — `List.map : ('a -> 'b / 'e)
    -> List 'a -> List 'b / 'e`). Required on every signatured binding: the
    declared effect must equal the inferred one, reported as **`FAI5001`** (a
    capability used but undeclared, or declared but unused). The whole standard
    library, samples, and test corpus carry accurate effects; the std combinators
    forward `'e`, so mapping/folding a capability-using closure reflects it.
  - **Deep subsumption at arguments; strict elsewhere (sound, not complete).** At a
    function *application* each argument is related to its parameter by a directional
    `subsume_types` (`sub ⊆ sup`) rather than unified. It walks both types in
    lockstep — **unifying** the non-effect structure — and relates effect rows by
    `⊆` with **variance**: covariant under arrow results, tuple elements, record
    fields, list elements (`List` is immutable), and an interface's effect argument;
    **contravariant** under arrow parameters (the relation flips). A general
    type-constructor (ADT) argument and a leaf are invariant (unified), their
    variance not being tracked. So a less-effectful function is accepted where a
    more-effectful one is expected at *any* depth (a maker returning a pure function
    where one returning a `{ Console }` function is expected), and several arguments
    flowing into one shared effect variable *union* their effects — point-free
    composition (`consoleFn >> clockFn : … / { Console, Clock }`), including the
    tuple/list cases. User-defined operator application takes the **same path** as
    function application. A residual open tail left by subsuming a concrete effect
    into a variable that appears only *covariantly* in a definition's type is
    **closed to pure** at finalization (variables in a parameter position stay
    polymorphic), so `let f = inc >> inc` is pure while `let g = List.map` stays
    effect-polymorphic. Everywhere *other* than an argument, effect unification
    stays strict: differently-effecting `if`/`match` branches do not silently unify
    (no laundering — joining them is separate, deferred work), and passing an
    effectful function where a pure one is required is still a type mismatch.
    Lenient only at the signature-vs-body check (so a disagreement is the single
    `FAI5001`, not a generic mismatch); interface instance methods are checked by
    subsumption against the declared method effect (see *Effect-parameterized
    interfaces* below), not leniently.
  - **Surface & tooling.** Syntax mirrors the record row — `/ { A, B | 'e }`,
    lone-`'e` sugar, bare = pure — bound to the innermost arrow; `fai fmt` sorts
    the atoms and drops `/ {}`. Interface effect arguments and `fai query caps`
    re-derivation are wired through the same machinery.
  - **Effect-parameterized interfaces.** An interface parameter used after `/` in a
    method is an *effect* parameter (`interface Logger 'e = log : String -> Unit /
    'e`); its kind is inferred from use across the methods (type position → a type
    parameter, an effect-row tail → an effect parameter, *both* → the ill-kinded
    **FAI3019**, unused → a type parameter as before). The effect argument is
    written with effect-row braces in argument position — `Logger { Console }`,
    `Logger 'e`, `Logger {}` — told apart from a record by its upper-case leading
    atom, and carried as an erased **`EffectArg`** child of the interface
    application (the spine keeps type and effect arguments in declaration order; a
    type supplied for an effect parameter, or vice versa, is **FAI3020**). An
    instance shares its parameters across methods by kind, and each method body's
    effect is checked against the declared method effect by **subsumption**
    (`body ⊆ declared`): performing fewer is fine (the declared effect is an upper
    bound), performing more than a *concrete* declared effect is **FAI5001** (this
    closes the dictionary-laundering hole the old lenient check left open), and an
    effect *parameter* grows to admit the body's effect — so a console-backed
    instance is `Logger { Console }`, **forwarding** the effect rather than masking
    it. At dispatch the value's effect argument is unified into the method's effect
    parameter, so calling the method incurs it. Effects are erased before codegen,
    so a parameterized interface compiles to the same dictionary as any other.
  - **Future work.** Subsumption is applied at *argument* positions only; *joining*
    differently-effecting `if`/`match` branches to their union (a least-upper-bound
    at merge points) is a separate, deferred feature (#107), as is subsumption
    through a general ADT's argument (which would need variance inference, #108).
    Deep subsumption through arrows (with variance), tuples, records, lists, and
     interface effect arguments, and effect-parameterized interfaces, are done — see
     above.
  - **Extended to user data types (see D144).** An effect parameter may now also
    ride a user `type`/alias parameter, not just an interface's, so a plain data
    type can carry an effectful suspension in its type (the basis for the lazy
    `Stream`).

- **D119 Array-backed sequence type (`Array 'a`; closes the linked-`List`
  representation gap).** A contiguous, growable sequence alongside the linked
  `List`, so indexed/contiguous/sorted code has cache-friendly O(1) storage and the
  sequence benchmarks compare a *matched* representation against Rust's `Vec`
  rather than a linked-vs-array fundamental.
  - **Lineage.** Semantics follow Haskell's `Data.Vector` (0-based `Int` index,
    length not `Ix`/bounds, contiguous), with the name `Array` per the ML/F#/OCaml
    family (and to avoid colliding with the `Vec2`/`Vec3` math records in
    `samples/`). The distinguishing choice: where Haskell needs `ST`/`freeze`/`thaw`
    for in-place mutation and Elm uses an RRB tree (no uniqueness to exploit), Fai's
    Perceus reference counting gives **in-place update when unique** inside a pure
    interface — contiguous *and* mutable-when-unique.
  - **Representation.** A built-in `Con` (recognized globally by `con_or_unit` like
    `List`, no import; `Array` is a reserved module name). The runtime object is a
    header + `length` + inline element slots; **capacity is derived from the
    allocation size** (not stored), so a unique array grows into its spare capacity.
    **Always boxed** (even empty allocates a header — simpler/faster dup/drop than a
    `Nil`-style immediate; `is_always_boxed_ty` includes it). Only slots
    `0..length` are live; the child scan, equality, and ordering touch only those.
    Capacity is invisible to semantics (equal elements ⇒ equal regardless of
    capacity). Equality is length + elementwise; ordering is **lexicographic,
    shorter-prefix-first** (matching `List`/`String`), so arrays sort and serve as
     `Dict`/`Set` keys. A boxed element is one uniform slot word; **`Array Float`
    stores its elements as raw, inline `f64`s** (D133, the array peer of D113/#86 —
    an unboxed `Array Int` is unnecessary, a small `Int` being an inline immediate).
  - **Five intrinsics, the rest pure Fai.** `Prim.array{WithCapacity,Length,Get,
    Set,Push}` (the first *polymorphic* builtins, quantified over the element type
    and instantiated per use); `withCapacity`/`length`/`get` borrow, `set`/`push`
    mutate in place when unique and copy when shared (the `fai_record_update`
    model), growing by doubling. Everything else — `empty`/`init`/`map`/`filter`/
    `foldl`/`foldr`/`reverse`/`append`/`zip`/`sort`/… — is `std/Array.fai` in Fai,
    collection-last like `List`. Unknown-size combinators (`filter`/`partition`/…)
    are single-pass `push`-builders (one predicate call per element, effect-correct).
  - **Access is total via `Option`, with partial fast paths.** `get : Int -> Array
    'a -> Option 'a` and `set : Int -> 'a -> Array 'a -> Option (Array 'a)` are
    total; `unsafeGet`/`unsafeSet` return the bare value/array and **abort on
    out-of-bounds** (the division-by-zero model, surfaced by contracts as
    `FAI6003`) for in-bounds-by-construction hot loops. The intrinsics themselves
    bounds-check-and-abort for memory safety, so a std bug can never read past the
    buffer.
  - **Sort is an in-place median-of-three quicksort, unstable.** `sortBy` recurses
    into the smaller side and tail-calls the larger (logarithmic depth); the
    median-of-three pivot keeps sorted/reverse-sorted input O(n log n). It diverges
    from `List.sortBy`'s stability, but only observably for `sortBy` with a
    *custom partial-key* comparator — `sort = sortBy compare` is unaffected (equal
    values are structurally identical). A faster *List* sort would not come from
    quicksort (a linked list has no O(1) swap); it is a bottom-up merge sort (#104),
    out of scope here.
  - **Literals `[| a, b, c |]`** (empty `[||]`), expression-only (no array
    patterns). The lexer takes `[|`/`|]` by maximal munch (no conflict with
    `|>`/`||`/`|`); lowering emits a pre-sized `withCapacity` + in-place `push`
    chain (one buffer, no transient `List`).
  - **`map` builds a fresh buffer** (one allocation), not yet recycling a unique
    input the way `List.map` recycles a unique spine — generic in-place-when-unique
    `map` (which a type-changing `'a -> 'b` cannot express in well-typed Fai without
    a reuse pass or a type-reinterpreting intrinsic) is future work, pinned by a
    test.
  - **Benchmarks use a matched representation, in both directions.** A sample and
    its Rust oracle use the same data structure, so the ratio reflects the
    runtime/codegen rather than a linked-vs-array fundamental; the test is whether
    switching would make the *Fai* side faster. Where contiguous access wins for
    Fai too, the sample uses `Array` against Rust's `Vec`: `MapSum`/`MapSumShared`,
    `MergeSort` (std sort), the hand-written in-place `QuickSort`,
    `MatrixMultiply`/`Levenshtein` (array-of-array / array-row DP), `SpectralNorm`
    (`Array Float`), `NBody`/`Particles` (record simulations), and `WordCount`
    (split/join through `Array String` via the `String.splitArray`/`joinArray`
    intrinsics). Where a persistent `List` is the better Fai structure —
    prepend-and-share backtracking, or a parser consuming a token stream head/tail,
    where an `Array` would copy on every step — the *Rust oracle* is matched to an
    `Rc` persistent cons-list (`PList` in `algorithms.rs`), not
    `std::collections::LinkedList` (cache-hostile, would unfairly slow Rust):
    `NQueens`, `Fannkuch`, `ExprEval`. `ListSort` keeps a `Vec` oracle on purpose
    (the shared baseline that, against `MergeSort`'s `Array`, isolates the in-Fai
    linked-vs-array sort cost); `JsonSerialize` and `GraphBFS` keep a `List` whose
    container is immaterial (the cost is the string / `Dict` / `Set`). A bare
    `List`-vs-`Vec` mismatch is still avoided.

- **D119 Unboxed monomorphic `Int` (untagged `i64`; the `Int` peer of D113).** A
  value whose static type is exactly `Int` is carried as a **raw, untagged
  `i64`** in registers and locals — not the 63-bit low-bit-tagged immediate — so
  the hot integer primitives compile to **bare** native ops (`iadd`/`isub`/`imul`/
  shifts/bitwise/`icmp`) with no immediate guard, no 63-bit fit check, and no
  boxing, and a value beyond the immediate range flows untagged rather than
  heap-boxing every step. This removes both costs the tagged representation paid in
  integer-heavy loops: the per-op guard tax (`fib`/`collatz`/`ackermann`) and the
  64-bit per-step boxing (`prng_xorshift`). A raw `Int` is tagged (or boxed on
  >63-bit overflow) only where it crosses a *uniform slot* — a data/record/tuple
  field, a closure environment, an `apply_n`/first-class argument or result, or a
  generic (type-variable) position — and untagged/unboxed when read back out, the
  same boundary rule as D113.
  - **Representation is a side-set, not the Cranelift type.** D113 distinguishes a
    boxed `Float` (an `i64` pointer) from an unboxed one by the Cranelift value
    type (`F64` vs `I64`). A raw `Int` and a tagged immediate are **both `I64`**, so
    raw-ness cannot live in the type: codegen tracks it explicitly (a set of the
    Cranelift values known to be raw, the analogue of the free `is_f64` test) and,
    per local, a `every observation is Int` classification (the analogue of the
    unboxed-float local rule). Representation **follows the value, not the node's
    static type** at branch/loop merges (a desugared `match` types its arms
    `Error`), and `define_var` is the single coercion point reconciling a value
    with its target local's representation.
  - **Calling convention — register ABI only.** `Int` parameters and results are
    untagged only on the **register (direct-call) ABI**, where a direct caller
    receives them raw and skips the round-trip. Uniform (row-polymorphic / nullary)
    entries are reached only through `apply_n`, which boxes everything, so they keep
    ints **tagged** — a tagged immediate is already a valid uniform word (unlike a
    `Float`, which must unbox on both ABIs), so unboxing them would be a pure
    wrapper round-trip with no direct-call beneficiary. The first-class form keeps
    the uniform representation, bridged by the same wrapper as D113 (which now also
    untags incoming int arguments and re-tags/boxes a raw int result). Because the
    Cranelift parameter type is `i64` either way, raw-ness is **conventional**, so
    it is part of the object-cache key (the fingerprint records the int ABI, just as
    a monomorphic-`Float` callee is distinguished from a generic one instantiated at
    `Float`). Offset-evidence parameters stay tagged immediates.
  - **Reference counting & safety.** A raw `Int` carries no reference count, so a
    raw-int local's `Dup`/`Drop` are **no-ops** — gated on the raw classification,
    **not** the `Int` type, because a tagged int local genuinely may be a heap box.
    This is safety-critical: a raw int with an even value (low bit clear) would pass
    a tag-check as "boxed" and dereference the integer as a cell. `fai-rc` stays
    representation-agnostic (it inserts the dup/drop; codegen discards them),
    mirroring `Float`.
  - **Wrapping/fault semantics preserved.** Native `iadd`/`imul`/shifts wrap like
    the runtime's `wrapping_*` (Cranelift masks a dynamic shift modulo 64). Raw
    division/remainder, whose operands are now full 64-bit, add a `b == -1` branch
    yielding `0 - a` / `0` to match `wrapping_div`/`wrapping_rem` and dodge
    Cranelift `sdiv`/`srem`'s `i64::MIN / -1` hardware trap (the tagged path never
    saw `i64::MIN`), and a zero divisor still routes to the located runtime fault.
  - **Mutual recursion stays tagged.** The combined loop of a mutual-recursion
    group shares padded uniform slots, so it **erases `Int`** (as it already erased
    `Float`) to keep ints tagged; its integer arithmetic takes the tagged guarded
    path with the runtime fallback retained — the surviving use of that path.
  - **Boundary-box release at borrowed scalar arguments (a fix spanning D113).** A
    scalar (raw `Int` or unboxed `Float`) boxed at a direct-call boundary for a
    **borrowed** parameter is a caller-owned temporary the callee inspects but does
    not consume; it is not a named local whose drop would free it (and a raw scalar
    local's drop is a no-op), so the caller now releases it after the call, using
    the callee's borrow signature. This also fixes a latent `Float` leak from D113.
  - **Scope.** Only a *scalar* `Int` is unboxed; an `Int` nested in a
    record/tuple/list/ADT stays a tagged/boxed field (the `Int` peer of the
    scalarization deferred for `Float`).

- **D120 Reset before a `let`-bound recursion, so "recurse-then-rebalance" rebuilds
  in place (resolves the D116 reuse gap).** The reuse pass (D77) recycled a matched
  cell only when a construction sat on a single straight-line path after the cell's
  death, so it reset the cell **before** the recursion only for a constructor that
  *embeds* the recursion (`Dict.map`). A balanced-tree `insert`/`remove` instead
  binds the recursion in a `let` and reconstructs in a *following branch* (`let l2 =
  insert … l` then `if … then balance l2 … else DictNode … l2 …`), so the reset was
  pushed past the recursion into the branches; the recursed-into child stayed
  shared, the recursion path-copied, and a unique build cost O(n log n)
  allocations.
  - **Fix.** The reuse pass now recognizes a construction reachable through the
    body's straight-line bindings **and** `if` branches (`reaches_construction`,
    generalizing the former straight-line predicate), resets the dead cell at its
    death point — *before* the `let`-bound recursion — and threads the reuse token
    to the construction on each path (`thread_or_free`). The matched children were
    duplicated when projected (`fai_data_field`), so resetting the cell early only
    releases the cell's own references: the recursion's copy of the child becomes
    uniquely owned and is rebuilt in place, cascading down the search path. A
    uniquely-owned `Dict`/`Set` build is now O(n) (a build-linearity allocation test
    guards it; the per-element cost is flat where it previously rose ~log n).
  - **Freeing an unconsumed token.** Resetting before a *branch* means a branch that
    builds nothing into the token (a rebalance that tail-calls `balance`) must still
    consume it exactly once. A new Core node `FreeReuse { token, body }` (lowered to
    the runtime `fai_free_reuse`, a guarded `free_obj` that no-ops the null token)
    reclaims such a token; `thread_or_free` emits it on every non-constructing leaf.
    The soundness interpreter treats it as consuming the token, so an `if` whose one
    arm reuses and whose other frees leaves a consistent reference state. It is
    **transparent to the tail-call transform** (handled like `Reset`/`Dup`/`Drop`),
    so a token-free wrapping a tail self-call does not defeat flattening.
  - **`Set.insert`'s equal branch rebuilds the node.** Returning the matched value
    unchanged (`else s`) forces it to be shared (duplicated) on every insert, so its
    reset yields a null token and the path cannot be recycled. `Set.insert` now
    rebuilds an identical node (`SetNode n l y r`, mirroring `Dict.insert`'s equal
    branch), keeping the matched node uniquely owned. The equal branch is off the
    hot build path (distinct keys never take it), so this only changes a
    duplicate-key insert on a shared set (one extra copy, already the shared case).
  - **Blast radius.** Generalizing the predicate/threader (rather than adding a
    narrow special case) also moves the reset earlier for any function with a
    top-level `if` reaching a construction (e.g. list `filter`'s
    `if p then x :: rest else rest`); the emitted IR changes but the allocation
    counts do not (the existing reuse differentials are unchanged).
- **D121 Intrinsic inliner: a saturated call to a primitive re-export wrapper
  becomes the primitive (removes the per-`Prim.*` indirection of D74).** Each
  standard-library re-export is a one-line eta-expansion of an intrinsic
  (`let toString n = Prim.intToString n`, `let push x xs = Prim.arrayPush xs x`),
  so a use site was two calls deep (caller → wrapper → runtime primitive). A new
  `fai-core` pass removes the wrapper hop:
  - **Recognizer (`prim_wrapper`, a per-definition query).** Reports a definition
    that is exactly `fun p0 … pk -> Prim.op <a permutation of p0 … pk>`: a single
    function (no lifted lambdas), no captures, a body that is one primitive whose
    operands are a *bijection* of the parameters, with `op.arity()` equal to the
    parameter count. This is shape-based, so a user one-liner that eta-expands an
    operator (which lowers to a primitive, e.g. `let add a b = a + b`) is
    recognized too; it excludes nullary constant wrappers (`Array.empty`, a
    literal operand), nested bodies (`Array.isEmpty`), and row-polymorphic
    definitions (leading offset-evidence parameters are not operands). The result
    is a tiny value, so editing a wrapper's body ripples to its callers only when
    the recognized primitive or permutation actually changes (salsa early cutoff).
  - **Inliner (`core_inlined`, a per-definition query).** Rewrites every
    *saturated* `App` of a recognized wrapper to the primitive. An identity
    permutation splices the arguments straight into the operands; a non-identity
    one (`Array.push`/`unsafeGet`/`unsafeSet`) binds the arguments to fresh locals
    in source order and references them through the permutation, so evaluation
    (hence trap) order is preserved. A wrapper used first-class (not in a saturated
    call) keeps its `Global` reference and stays compiled; one reached only through
    now-inlined calls is dead-code-eliminated.
  - **Runs before reference counting.** Reference counting balances the resulting
    primitive directly: a borrowing primitive (`stringLength`, `arrayGet`) borrows
    its operand exactly as the wrapper's borrow signature did, so an owned argument
    is still dropped at the call site (a codegen-only rewrite would have leaked it).
    Inlining is correctness-neutral and always on; the raw `core` query is
    unchanged for inspection. Purity/totality reasoning still reads `core` — a
    wrapper call and its primitive are equally pure, and the tail-call transform
    already judges a primitive's reorder safety, so the decision is unaffected.
  - **Codegen.** A primitive that yields a scalar `Float` through the uniform
    runtime ABI (`arrayGet` of an `Array Float` element) now unboxes its boxed
    result to the `f64` an unboxed-`Float` context expects, mirroring the result
    coercion a direct call already applied to a generic callee's boxed `Float`.
    (Before inlining, such a primitive only ever appeared at a generic `'a` result
    type inside the wrapper, so no coercion was needed.)

- **General helper inlining (`helper_inlined`, layered on `core_inlined`).** A
  second `fai-core` pass folds **small, non-recursive, intra-file helpers** into
  their callers before reference counting, so factored code (smart constructors,
  a shared `balance`) still gets Perceus reuse — a construction that lived inside a
  *called* helper can now recycle the caller's freed cell. `rc`, `borrow_signature`,
  the mutual-recursion combined loop, and reachability read `helper_inlined` (which
  itself reads `core_inlined`), so it is the back end's view of Core.
  - **Eligibility (`inline_summary`).** A callee is inlined when it is
    **non-recursive** (excluded via `fai_resolve::recursive_defs`, a full intra-file
    reference-graph SCC analysis — *not* the inference `module_sccs`, which cuts
    signatured edges and so would miss a signatured self-recursion), a single
    function with no captures (so no lifted lambda to renumber), non-row-polymorphic
    (no offset evidence), with ≥1 parameter, and whose prim-folded body is at most a
    node budget (currently 64 — admits `bin`/`singleton`, excludes the larger
    `balance`). Inlining is **transitive** (a caller splices the callee's own
    `helper_inlined` body, already folded) and cycle-free precisely because the
    eligible-callee graph over non-recursive nodes is a DAG; the recursion check is
    done **before** reading `helper_inlined`, so a self-call resolves to "not
    inlinable" without forming a query cycle.
  - **Substitution.** A saturated (or over-applied) direct call binds **every**
    argument to a fresh local and remaps the callee's locals to fresh slots; binding
    (not splicing) routes each argument through code generation's single
    representation-coercion point (`define_var`), so a raw scalar flowing into a
    generic position is tagged exactly as the call boundary would have, and the
    callee body's own types are kept verbatim (no instantiation needed under the
    uniform representation). An over-application applies the surplus arguments to the
    inlined result.
  - **Intra-file only.** Only same-file callees are inlined, which keeps the
    cross-module firewall intact (a body edit never crosses a module boundary, and a
    caller's object never depends on another module's body) and keeps an opaque
    type transparent at the splice site. Cross-module general inlining is future work.
  - **Incrementality.** `inline_summary` returns a tiny value (the arity, or
    "not inlinable"), so a callee body edit ripples to its callers only when its
    *eligibility* changes (early cutoff) — for the common non-helper callee it stays
    "not inlinable" and the caller is cut off. An eligible helper's body edit ripples
    only to its same-file callers, never across modules.
  - **The standard library is written with helpers.** `Dict`/`Set` `insert`/`remove`
    rebuild through the `bin` smart constructor (and `singleton` on the empty branch,
    and an all-`bin` `balance`) rather than hand-inlined `DictNode`/`SetNode`; the
    inliner folds `bin` back in, so the non-rotating spine still recycles a unique
    tree's cells in place. The rotating `balance` stays a shared call (over budget),
    so rebalancing allocates fresh — the cost model the reuse differential pins.

- **D122 A dead value's drop is emitted before its continuation (codegen
  ordering; restores in-place mutation through a recursive destructure).** Code
  generation lowered a non-tail `Drop { local, body }` (D101 governs *what* the
  drop emits; this is *when*) by emitting `body` first and the release after it.
  Reference counting only ever drops a value that is **dead in `body`** — the
  abstract soundness interpreter (D76) enforces exactly this, and the tail path
  and `Reset` already release before their continuation — so the late release was
  sound but kept the value alive across the continuation needlessly. Where the
  value is a **matched cell** whose boxed field was projected out and is mutated
  in the continuation, that defeats in-place update: the cell still references the
  field, so the field is shared (`rc > 1`) during the recursion. The headline
  victim was an `Array` threaded through a tuple-returning recursive sort
  (`match partition … with | (p, a2) -> … qsort … a2`): the result tuple was
  dropped only *after* the recursive sort, so the array was copied once per
  recursion frame — O(n) full-buffer copies, i.e. O(n²) work, making the
  contiguous `Array` sorts slower than the linked-`List` sorts they replaced.
  - **Fix.** Emit the release **before** `body` (`drop_local` then the
    continuation), matching the tail path (`expr_tail`), `Reset` (which already
    calls `fai_drop_reuse` before its body), and the soundness interpreter's
    consume-then-body model. The matched cell is freed at its death point, so a
    field projected from it (duplicated when projected) becomes uniquely owned for
    the recursion and is mutated in place. `Array.sort` and a hand-written
    in-place quicksort now copy the buffer **zero** times on a uniquely-owned
    array (an allocation-/copy-count test guards it via a debug-only array-copy
    counter that, unlike the allocation count, makes a per-element copy visible —
    each copy is one allocation but O(length) work).
  - **Soundness.** Purely a code-generation reordering of the existing `Drop`
    node; the reference-counted IR (and so the object-cache fingerprint) is
    unchanged. Releasing a dead value earlier is unobservable in a pure
    reference-counted language (no finalizers; free order is invisible) and can
    only improve allocator reuse. The cache key carries an `early-drop` token in
    its codegen-config stamp so a cache warmed before this change cannot serve a
    stale drop-after object (the fingerprint alone would not distinguish them).
  - **Blast radius.** Global (every non-tail drop releases earlier), but the
    existing reuse differentials and allocation-independence tests are unchanged
    (they share genuinely, so the timing does not make a value unique), and the
    codegen IR tests assert only instruction *presence*, not order. New coverage:
    the array copy-count tests, a random-input sort property test (both
    `Array.sort` and a hand-written quicksort: exactly sorted vs a Rust oracle,
     leak-free, zero copies), and a generated-tree destructure-and-recurse property
     test (folds exercise the drop path, maps the reset path) asserting result and
     leak-freedom.

- **D123 Inter-procedural reuse-token passing (forwarding a freed cell into an
  accepting callee).** Reuse (D77) is intra-function: a freed cell can only be
  recycled by a construction in the same body, so a rebuild that happens in a
  *called* function (a non-inlined helper) cannot reuse the caller's freed cell.
  The headline victim was the weight-balanced `Dict`/`Set` rebalancer: `insert`/
  `remove` free the matched search-path node, but the rotation branch calls the
  large, non-inlined `balance`, so on a rotation the node was freed and `balance`
  allocated fresh — a rotation-heavy unique build cost ~3 cells per entry instead
  of one. Reuse tokens are already plain `i64` values with a runtime null/wrong-
  size fallback, so a token can cross a call boundary.
  - **Two sides.** *Source:* a freed reuse token the intra-function pass could not
    home locally (it would `FreeReuse` it) is **forwarded** into a saturated direct
    call on its path whose callee accepts a token, recorded in the call's reuse
    list (`App.reuse`). *Sink:* a function that can consume incoming tokens gets a
    **token-taking specialized entry** (`{base}__reuse`) whose leading parameters
    are reuse tokens, threaded into its leftover sinks (a construction its own
    resets did not fill, or another forwardable call). Both reuse the intra-
    function first-sink-per-path threading, generalized so a *sink* is a
    construction **or** a forwardable call.
  - **Reuse signature (the seam, modeled on borrow inference D79/D100).**
    `reuse_signature(def)` is the size-classed (field-count) token slots a
    function accepts: the sinks reachable by threading, **net of the function's own
    dying cells**, per-class max across paths. It is inter-procedural via
    forward-through (a forward sink contributes the callee's full capacity), so it
    is a monotone salsa fixpoint — acyclic call graphs resolve as ordinary
    dependencies, a mutual-builder cycle resolves through cycle recovery (start
    empty, grow to the least fixpoint, capped). Early cutoff on the small signature
    bounds a callee edit's ripple to callers that forward to it; the entry **arity**
    needed to test saturation is read from the callee's *borrow signature* (also a
    firewall-stable small value), never its full lowering, so a body edit that
    leaves the arity unchanged does not ripple.
  - **Calling convention & ABI.** `App.reuse` is a `Vec<Option<LocalId>>`, one
    entry per callee slot — a forwarded token (`Some`) or a null-token pad
    (`None`) — so the slot count travels in the IR (and the cache fingerprint),
    needing no extra cross-crate plumbing. A forwarded call passes one leading
    `i64` register per slot (the token, or the null token `0`) ahead of the value
    arguments, to `{base}__reuse`. Tokens are raw `i64`s, never reference-counted.
    The specialized entry is emitted as a **separate object**, so the per-
    definition primary object stays a pure function of the definition (the cache
    firewall); the driver links it only for definitions a reachable caller actually
    forwards to (AOT, JIT, and the daemon-run wire bundle alike), so an
    accepting-but-never-forwarded-to definition costs no extra code generation.
  - **Soundness.** The reference-count oracle (D76) models the new edges: a
    forwarded token is consumed by the call (exactly like a `MakeData` reuse
    token), and a specialized entry's leading token parameters are linear (born at
    entry, consumed once per path). The runtime null/wrong-size check (D77) makes
    any pairing correct, so the analysis is a pure performance choice. A self-tail-
    call is **never** a forward target — the tail-call transform (D99) owns per-
    iteration loop reuse via its destination hole, keeping the two token mechanisms
    disjoint.
  - **Result.** A rotation-heavy unique `Dict`/`Set` build now allocates exactly
    one cell per entry (every rebuilt path, including the `balance` rotations,
    recycled in place); a unique `remove` recycles its rebalanced path. Guarded by
    allocation differentials and a reuse-signature firewall guard (a
    signature-preserving callee edit re-runs only the callee; a signature-changing
    edit recompiles the callee and the one forwarding caller — both independent of
    workspace size). Forwarding currently fires where the freed token reaches a
    *tail* accepting call (the `insert`/`remove` → `balance` shape); extending it to
    the bulk operations (`union`/`intersection`/`difference` → `join`/`split`,
    whose freed operand is plain-dropped rather than freed-for-reuse) is noted as
    future work, as is class-precise multi-slot caller marshalling.

- **D123 Specialize structural `=`/`compare` for known-immediate operands
  (extends D108).** Polymorphic structural comparison — `=`/`<>` and `< <= > >=`/
  `compare` on a *type variable* — previously always called the runtime
  `fai_equal`/`fai_compare`, even when the value at runtime is an immediate (an
  `Int`/`Char`/`Bool` or a nullary constructor), which is the common case for the
  keys of a generic `Dict`/`Set` and the elements of a generic sort. Three changes
  make that common case an inline word compare with no monomorphization.
  - **Inline immediate fast path for maybe-immediate operands (codegen).** D108
    inlined `=`/`compare` only on statically immediate or scalar (`Int`/`Float`)
    operands. Code generation now also emits the immediate guard — both operands
    immediate ⇒ inline word equality / raw three-way, else the structural runtime
    call — for an operand whose type *may* be an immediate at runtime but is not
    statically known: a **type variable**, or a discriminated union / `List` /
    empty record (whose nullary constructors are immediates). The always-boxed
    types (`String`, tuples, non-empty records, arrays, interfaces, functions)
    keep the direct structural call (the guard would always miss). The fallback
    honours the borrow decision reference counting made for the operand type — the
    *owned* `fai_compare`/`fai_equal` for a type variable (`is_boxed_rc` is false),
    the *borrowed* variant for a reference-counted union/`List` — so the emitted
    (non-)consumption agrees with the runtime variant; the immediate fast arm drops
    nothing (immediates), sound in both modes. Purely a code-generation lowering of
    the unchanged `Prim::Eq`/`Prim::Compare` node, so reference counting and the
    object-cache fingerprint are untouched; the codegen-config stamp gains a
    `poly-cmp-inlined` token so a cache warmed before the change cannot serve a
    pre-inlining object. The fallback remains exactly the prior behavior, so the
    fast path is a pure optimization.
  - **`compare` is a primitive wrapper.** `Prelude.compare` was ordinary Fai
    (`if a < b … else if a > b …`, two structural comparisons). It is now the
    one-line wrapper `let compare a b = Prim.compare a b` over a new prelude-private
    `Prim.compare` intrinsic (the structural three-way primitive, the first
    polymorphic non-array intrinsic, typed `'a -> 'a -> Int`). The intrinsic
    inliner folds a saturated `compare x y` to the bare primitive (one inline
    immediate compare), and `compare`'s own first-class body drops from two
    comparisons to one. `Dict`/`Set` now compute `let c = compare key k` once per
    node and branch on `c` (the search-tree three-way is one comparison, not the
    two of `key < k` then `key > k`) — halving both the per-node comparison work
    and, for boxed keys, the per-node key duplications, while preserving the
    in-place reuse of `insert`/`remove`.
  - **Default-ordering sorts compare directly.** `List.sort`/`Array.sort` went
    through `sortBy compare`, so every comparison was an `apply_n` into a passed
    comparator. Each now has a comparator-free recursion (`List.merge`; `Array`'s
    `qsortOrd` family) that compares with the structural `<`/`>`/`<=` directly — an
    inline immediate compare with no `apply_n`. `sortBy`/`mergeBy` keep the
    comparator path for a caller-supplied order (whose `apply_n` is inherent to
    passing a function); the `Array` quicksort body is duplicated for the default
    sort, the deliberate cost of removing the indirection without monomorphization.
  - **Ceiling.** A *boxed* monomorphic key (`String`/record/tuple — e.g. a
    tuple-keyed dictionary) still takes the structural runtime call (the operand is
    always boxed, so the guard misses), and a custom `sortBy` comparator still pays
    its `apply_n`. Inlining those needs specialization at a concrete instantiation
    (opt-in monomorphization), noted as future work.

- **D124 In-place amortized `String` append, with `++` chains left-reassociated
  (closes the concatenation half of #101).** `String` was an immutable contiguous
  byte buffer whose concatenation always allocated a fresh buffer and copied *both*
  operands, so building a string by repeated `++` was O(n²) copying. Two changes
  make incremental construction amortized O(total length), with no surface, type,
  or representation change (the on-disk string layout is unchanged).
  - **`fai_string_concat` appends in place into a unique left operand.** It now
    **owns both** operands (the `Array.push` model — `Prim::StrConcat` dropped from
    the inspect-only borrow set, so reference counting consumes both and an operand
    reused after the concat is duplicated first). Capacity is derived from the
    `size` header (`string_cap = size − STRING_BYTES_OFFSET`), exactly as `Array`
    derives element capacity, so no field is added and the codegen static-literal
    emission is untouched. When the left operand is uniquely owned (`rc == 1`) it
    appends `b`'s bytes into the spare capacity and bumps the length; when unique
    but full it grows into a **doubled** buffer and reclaims the old memory; when
    **shared** it forks a fresh **tight** buffer (a counted uniqueness-loss copy).
    Concatenating the empty string returns the other operand without copying. Leaf
    constructors (`make_string`, `Int.toString`, …) allocate tight: only the
    unique-but-full grow path over-allocates, so a one-shot `a ++ b` wastes nothing
    while a builder amortizes. A new `string_copies()` debug counter (the peer of
    `array_copies()`) records uniqueness-loss forks; a unique builder forks zero
    times.
  - **`++` chains are left-reassociated before reference counting.** `++` is
    right-associative, so a source chain `a ++ b ++ c` is a right-leaning tree of
    `StrConcat` nodes (after the prim/helper inliners), whose left operand at each
    step is a fresh small piece — the in-place append never fires, and a long chain
    re-copies the growing suffix (O(n²)). A `fai-core` transform
    (`reassociate_concat`) rewrites a maximal `StrConcat` tree to **left-nested**
    form so the growing prefix is the unique left accumulator. It runs in
    `rc_lowered` (so it also normalizes synthesized harnesses), reads the
    pre-reference-counting body, and is **behavior-preserving**: concatenation is
    pure and associative, and code generation evaluates a primitive's operands left
    to right, so flattening the tree to its ordered leaves and rebuilding it
    left-nested keeps operand evaluation (hence effect) order. Borrow signatures and
    purity read the un-reassociated inlined body (the rewrite changes neither), so
    only reference counting and codegen observe it.
  - **Proven structurally, not by the named bellwethers.** The acceptance signal is
    a deterministic gate — a `string_build` registry algorithm (a recursive
    literal-append builder) whose allocations stay sub-linear (only the O(log n)
    doublings) and whose `string_copies()` is zero — plus a folded `++` property and
    a first-class `List.foldl (++)` build that likewise never forks. The issue's two
    named cases stay roughly flat in this change: `json_serialize` is dominated by
    `String.join` and the recursive re-copy of subtree strings (already single-pass;
    not a `++` chain or a single accumulator), and `word_count` by `split`'s
    per-piece allocation and list/closure overhead — both addressed elsewhere
    (borrowing slice views for `split`; the array-backed sequence work). The win
    here is structural: any heavy `++`/accumulator builder is now O(n).

- **D125 Borrowing `String` slice views, large-piece-only (closes the slicing half
  of #101).** Substring/`split` previously materialized an owned copy per piece. A
  new heap kind, **`KIND_STRING_SLICE`** (header + byte length + base pointer + byte
  offset, 48 B), is a borrowing view sharing an inline base's bytes; `String.take`,
  `String.drop`, `String.substring`, and `String.split` return one when the piece is
  large, an owned copy when small.
  - **Char-indexed, view-large / copy-small.** The new ops index by **character**
    (consistent with `String.length`; a byte index could split a code point), so a
    slice scans O(position) to the byte boundary then makes an O(1) view. A piece is
    a view iff `byte_len >= 32 && byte_len*4 >= base_byte_len` — the absolute floor
    skips pieces too small to be worth a 48 B header, and the quarter-of-the-base
    ratio bounds retention (a view pins at most ~4× its own bytes), so a small piece
    of a large base is copied rather than pinning it. The ratio is measured against
    the **ultimate inline base** (slicing a slice flattens to that base and adds the
    offsets, so a slice's base is always inline and the byte reader never recurses).
    `split` into a few large pieces views them; many small pieces (words) are copied
    — so `word_count` is unchanged (its per-piece *allocation* is the cost, not the
    byte copy; addressed by the array-backed sequence work).
  - **Transparent to the rest of the language.** A `String` value is inline or a
    slice at runtime; the distinction is invisible to types and to user code. The
    byte length lives at the same offset for both, so length reads are uniform;
    every `String` reader goes through one byte accessor that branches on the kind;
    **equality and ordering treat the two kinds as one "string-like" category** (a
    sliced `"abc"` equals and orders identically to an inline `"abc"`, e.g. as a
    `Dict` key); and the drop child-scan releases a slice's base. Because a `String`
    value may now own a child, **code generation no longer treats `String` as a
    child-free leaf on drop** (only boxed `Int`/`Float` are): a dead string is
    released through the child-scanning runtime drop — a direct free for the inline
    case, a base release for a slice. `++` still appends in place only into a unique
    *inline* left operand (a slice owns no extensible buffer, so it forks).
  - **Proven by a view counter, not by `word_count`.** A `string_views()` debug
    counter (the peer of `string_copies()`) records the zero-copy slice path —
    allocation *count* cannot tell a view from a copy (both are one allocation). The
    deterministic gates assert a large slice is a view (`string_views` increments)
    and a small one a copy, that a view exits leak-free (the shared base released
    when the views die), and that the standard library's own `take`/`drop`/
    `substring` examples run. A `StringSlice` registry algorithm (200 large
    substrings of a base) is the informational Fai-vs-Rust bench (runtime and peak
    memory). Char-indexing means the win is *no byte copy* — asymptotic for `drop`
    (the kept suffix is neither scanned nor copied), a constant factor for the
    scanning ops.
- **D126 Niche (wrapper-free) representation for a monomorphic `Option`, plus
  fused get-or-default accessors (closes #102).** `Some x` previously always
  allocated a one-field cell. A **monomorphic** `Option` is now encoded without it,
  by one of two schemes decided from the payload type at lowering:
  - **Scheme A — always-boxed payload** (`String`, a tuple, a record, another
    boxed ADT): `None` is the immediate `1` and `Some x` is the payload pointer
    unchanged. A boxed payload is never an immediate, so the two never collide.
  - **Scheme B — the payload may itself be an immediate** (`Int`, `Bool`, `Char`,
    `List`, a nullary-bearing union, …): `Some x` is `x` in its uniform
    representation and `None` is a single **immortal, shared sentinel object** of a
    new `KIND_NONE` heap kind (so all `None`s are pointer-equal). The sentinel is a
    **process-static** (`FAI_NONE_VALUE`) that generated code references by its
    **relocatable address** (a `symbol_value`, as for the static descriptors) rather
    than fetching through a `fai_none_value` runtime call — so a tight `Option Int`
    loop pays no per-operation call for its tag tests and `None` builds. It is also
    **never reference-counted**: a `None` is the bare address (no `dup`), niche
    Scheme-B dup/drop skip both an immediate payload and the sentinel (counting only
    a boxed `Some` payload), and the niche/standard conversions leave its count
    alone — so the loop pays no per-`None` count write either. (The static is an
    `UnsafeCell` header — writable — only so it *could* be touched; its count is in
    fact never written, so it is effectively a constant and not a normal allocation,
    and it never trips the leak counter.) The `fai_none_value` accessor remains for
    the niche/standard conversions.

  The scheme is carried on the IR's data nodes (`MakeData`/`DataTag`/`DataField`
  gain a `niche` field), so it **survives the object cache's wire form** and is
  part of the content-addressed fingerprint (a niche and a standard def must not
  share an object). It is **erased where the value crosses a uniform slot** — a
  generic position, a closure environment, an `apply_n`/first-class argument or
  result — via runtime conversions (`fai_niche_{a,b}_to_std` /
  `fai_std_to_niche_{a,b}`); the first-class wrapper bridges the ABI. *Within* a
  function the niche encoding is **kept wrapper-free across every local, branch
  merge, and loop carry**: codegen classifies a local niche when any value reaching
  it (an alias, an `if`/`match` arm, a `Recur` argument, an entry parameter) is
  niche, propagated to a fixpoint, so conversions happen only at the true erasure
  boundaries above. Without this, a niche value flowing through a reference-count
  wrapper or a branch merge reverted to the standard form — heap-allocating a
  `Some` cell and converting straight back — so an `Option` threaded through a loop
  allocated once per iteration.
  - **Owned, not borrowed.** A niche `Option` parameter is always passed **owned**
    (the borrow signature never borrows one): a borrowed niche argument would force
    the caller to convert a *duplicate* (the conversion consumes its input), so
    keeping it owned lets the callee drop it cheaply — an immediate `Some`, or the
    immortal sentinel. A niche value flowing into a *generic* function's borrowed
    parameter still converts a duplicate.
  - **Comparison converts first.** A niche value and a standard value of the same
    type have incomparable encodings, so `=`/`compare` convert a niche operand to
    the standard form before comparing; `KIND_NONE` also gets runtime equality and
    ordering cases (`None = None`; `None` sorts before any `Some`, matching the
    declaration order). The reuse pass excludes a niche `Some` (it is wrapper-free,
    so there is no cell to reset).
  - **Scope.** **Nesting is not niche-encoded** (an `Option (Option …)` payload
    uses the standard representation) — which is why a single global sentinel
    suffices rather than per-type ones. **`Result` keeps its standard two-cell
    representation**; instead, the lookup-and-default idioms get **fused
    get-or-default accessors** that never materialize the intermediate `Option`:
    `Dict.getOr`/`getOrElse`/`member`, `Option.getOrElse`, and
    `Result.withDefault`/`getOrElse`/`toOption`. (A `getOrCompute` taking a thunk
    was considered and dropped: a cross-file callback overflowed the stack on deep
    memoization; `FibMemo` uses `member` + `getOr` instead.)
  - **Measured.** Three Option-heavy registry algorithms — `OptionEval` (a safe
    evaluator threading `Option Int` through binds), `OptionPath` (association-list
    next-pointer walks), and `OptionTreeFind` (binary-search-tree lookups) — bench
    the Scheme-B path against Rust.
- **D127 Stack-allocate non-escaping closures (closure escape analysis, the
  `MakeClosure` facet of #103).** Every `fun … ->` previously heap-allocated a
  reference-counted cell, even a non-capturing one and even a lambda that is merely
  handed to `List.map`/`foldl` and dropped at the end of the call. A `MakeClosure`
  now carries a **`ClosureAlloc`** (`Static` / `Stack` / `Heap`) choosing its cell.
  - **`Static` (no captures).** A non-capturing lambda has no per-activation
    environment, so it references one immortal static closure (D51) — no allocation,
    set at lowering, always sound (it captures nothing and is never freed).
  - **`Stack` (captures, non-escaping).** A capturing lambda that provably does not
    outlive its creating frame is built in a stack cell, laid out exactly like a
    heap closure but tagged **`KIND_STACK_CLOSURE`**: `apply_n`, `dup`, and the env
    child-scan are unchanged, but when its reference count reaches zero the runtime
    releases its captures yet does **not** free the cell (the frame reclaims the slot
    on return). Reference counting is therefore *identical* to the heap case — only
    the cell's storage and the elided free differ — so the single new soundness
    obligation is that the closure never escapes the frame. **Partial applications
    get the same treatment:** an under-application of a known function that does not
    escape carries `Stack` on its `App` node and builds its cell — the target's
    static closure plus the stored arguments — in a stack slot tagged
    **`KIND_STACK_PAP`** (the `KIND_PAP` peer), again released-not-freed at death.
  - **`Heap` (may escape).** The conservative default, the prior behavior.
  - **Escape analysis** (`fai-rc/escape.rs`) establishes non-escape. A value escapes
    when it is returned, stored in a constructor/record or a storing primitive,
    captured into another closure, or passed to a callee parameter that itself
    escapes; crucially **applying** a closure (the callee position) does *not* escape
    it (the runtime calls and drops it), which is the precision a borrow view lacks
    and what lets a combinator's lambda stack-allocate. A per-parameter
    **`escape_signature`** (consulted at a saturated direct call to relate a closure
    argument to the callee's parameter) is an inter-procedural monotone fixpoint —
    optimistic (nothing escapes), a self-call uses the in-progress signature, a
    cross-function call reads the callee's signature, mutual recursion a salsa cycle
    — mirroring `borrow_signature`; a row-polymorphic definition (only ever called
    curried) is conservatively all-escape. A context-aware marking pass then restamps
    each non-escaping capturing `MakeClosure` `Stack`, deciding an inline closure by
    the position it occupies and a `let`-bound one by whether its local reaches an
    escaping sink. Unknown (first-class) callees, primitive operands, and captures
    are conservatively escaping.
  - **Direct-calling a known closure.** A saturated application of a local bound to
    a `MakeClosure` (a `let f = fun … -> …` applied in scope, which the inliner keeps
    when `f` is used more than once) is code-generated as a **direct call** to the
    lifted function — its environment read from the closure cell, its arguments in
    the uniform slot array — then the closure dropped, exactly the machine call
    `apply_n` makes for a saturated closure, minus the dispatch (descriptor/arity
    checks and the indirect code pointer). Reference counting is unchanged (the
    closure is still *consumed* at the call, the drop replacing `apply_n`'s), so this
    needed no borrow analysis; it works uniformly for a static, stack, or heap
    closure local.
  - **Validated** by `closure_allocations()`/`pap_allocations()` debug counters (the
    peers of `string_copies()`/`string_views()`: a non-capturing or non-escaping
    lambda — or partial application — built per loop iteration adds zero heap cells,
    an escaping one adds one each), by the runtime leak check (a stack cell's
    children are released and the cell is never freed), and by the closure-heavy
    algorithm benches running correctly. A closure *returned from a CAF* (e.g.
    `FoldPipeline`'s `transform`) escapes its definition, so escape analysis cannot
    confine it directly; that case is instead reduced away before reference counting
    by the simplify pass (see D132).

- **D128 Deforest `List`/`Array` combinator pipelines (a Core-level fusion pass):**
  a pipeline of directly-nested standard combinators — a producer, transformers,
  then a consumer (`Array.sum (Array.map f (Array.range 0 n))`) — builds one
  intermediate sequence per stage only to walk it once. A new pass (`fai-core`'s
  `fuse_def`) recognizes such a chain and rewrites it to a single synthesized loop
  that materializes no intermediate, eliminating the buffers/spines between stages.
  - **Where/how it recognizes.** Run just before reference counting (the
    `reassociate_concat` slot), on the `helper_inlined` body, where a recursive
    combinator call is still a `Call` to its resolved std symbol. Recognition is by
    **resolved `DefId`** (a `fusion_defs` resolver reads only module headers), never
    by reading a combinator's body — so editing a combinator's body never changes
    what fuses (the cross-module firewall; guarded by a `perf_guards` test).
  - **Menu.** Producers `range`/`Array.init`/`Array.repeat` and a syntactic
    list/array literal; transformers `map`/`filter`; consumers
    `foldl`/`foldr`/`sum`/`length`/`all`/`any`/`find`/`member` and a terminal
    `map`/`filter` builder — for both `List` and `Array`, and over any
    `List`/`Array`-typed *value* source (a Local, a call result, or a fusion
    barrier's output like `reverse`).
  - **What it emits.** One synthesized self-tail-recursive top-level loop (a
    `fuse#…` name, sharing no name with a source binding) — a numeric index loop for
    a range/`init`/`repeat`, an indexed loop for an `Array` value, a spine walk for a
    `List` value, a `foldr` driving downward, a `List` builder via
    tail-recursion-modulo-cons. The loop is emitted exactly like the
    mutual-recursion combined loop: the consuming definition's `fuse_def` result
    carries both the rewritten body and the loops; the driver reference-counts them
    in memory (`rc_owned`) and code-generates them at assembly across every path
    (AOT, JIT, run-bundle, `jit_compile`, the contract harness). The loop takes the
    **raw-scalar register ABI** (untagged `Int` index/accumulator), so a pure
    arithmetic pipeline (`map_sum`) becomes a register loop. A **small literal** is
    instead **unrolled** to straight-line code under a node budget (no loop, the
    literal's cells gone), falling back to walking it as a value source when over
    budget. A **literal element lambda is inlined** into the loop (lifting its
    captures to loop parameters), so `map_sum` has no per-element dispatch; a
    non-literal element function (e.g. `FoldPipeline`'s composed `transform`) is
    passed and applied via `apply_n` (the residual dispatch is #94/#103, not this
    pass). `find` builds a niche-correct `Option`. Dead lifted lambdas left after
    inlining are pruned (renumbering `FnId`s).
  - **Behavior-preserving.** Only directly-nested applications fuse, so every
    intermediate is an unnamed temporary consumed exactly once (a shared/`let`-bound
    value is the loop's *source*, never fused away — `map_sum_shared`); and a stage
    fuses only when its element function is **pure**, so reordering element
    applications (including which element a trap falls on) is unobservable. Purity is
    decided structurally (a lambda is impure if it performs a capability `Prim` or
    makes an indirect call; a named function is pure iff its *scheme*'s arrows are
    pure) because the lowered Core's types erase a polymorphic effect variable to
    pure — the body types cannot be trusted for an effect-polymorphic stage.
  - **Carve-outs.** `foldr` over a non-reversible `List` value is left unfused (a
    single tail loop is impossible without a reverse pass or unsafe deep recursion,
    which std itself avoids); a pipeline inside a **mutual-recursion group member**
    is not fused (the combined loop is built from the pre-fusion body); fusion is
    skipped entirely inside the standard library (so the combinators stay tested by
    their own contracts). Deferred (per #116): `zip`/`concat`/`concatMap`,
    `take`/`drop`/`takeWhile`/`dropWhile`, the `toList`/`fromList` bridge, and
    fusing across an effectful stage.
  - **Rationale (over alternatives).** A bounded, symbol-keyed Core rewrite wins the
    target shapes now, fits the salsa firewall, and has a direct precedent
    (`reassociate_concat` + the mutual-recursion machinery). foldr/build and stream
    fusion both rely on inlining/eliding per-element step closures, which the
    uniform-boxed, `apply_n`-dispatched runtime does not yet do (#94/#103), so they
    would pessimize. The synthesized-loop form (over rewriting to existing
    combinators + a composed closure) has the higher performance ceiling — a fully
    inlined literal-lambda pipeline reaches a zero-allocation, zero-dispatch register
    loop — at the cost of threading synthetic definitions through the driver, as the
    mutual-recursion combined loops already do.
  - **Validated** by allocation tests (`map_sum` over a range allocates the same as
    a pure-arithmetic baseline — zero buffers; a `List` pipeline's allocation count
    is independent of `n`), IR-shape unit tests (which chains fuse, which are barred
    — the effect barrier, the shared-source materialization), AOT/JIT e2e tests
    across the menu, the `algorithms` oracle suite (`MapSum`/`MapSumShared`/
    `FoldPipeline`/`SpectralNorm`), and the firewall/edit-churn `perf_guards` test.
    The deforestation half of #85 (the array-backed representation half was #111).
- **D129 Inline `Array` element access (get/set/length/push):** every `Array`
  element operation used to compile to an out-of-line runtime call, so a hot index
  loop over an `Array Int`/`Array Float` paid a call + index-unbox + dup/drop per
  touch where Rust emits one load/store. Codegen now compiles `Prim.array{Length,
  Get,Set,Push}` **inline** when the array operand is a statically recognized
  `App(Con::Array, elem)` (`array_prim` in `fai-codegen`'s `emit`, after the
  int/float inliners), returning `None` to keep the runtime call only for an
  unrecognized (bare `Ty::Error`) operand.
  - **Representations.** `length` is a field load yielding a raw `Int`. `unsafeGet`
    is an inline unsigned bounds check + a slot load: an `Int` element read to a raw
    `i64` and a `Float` to an unboxed `f64` (both borrow the array — no dup/drop),
    any other element (a concrete boxed type, a **type variable**, or an erased
    `App(Con::Array, Error)`) the slot word with an inline tag-checked dup, so the
    returned reference outlives the borrowed array. `unsafeSet`/`push` inline the
    unique-owner (`rc == 1`) fast path — an in-place slot store (set releasing the
    overwritten element via a uniform-representation drop; push bumping the length,
    capacity derived as `(size − ELEMS) / 8`) — and fall back to the runtime
    `fai_array_set`/`fai_array_push` for the shared-copy / grow case (set's fallback
    also serves the out-of-bounds abort).
  - **Generic too (not just monomorphic).** Inlining keys off the operand being an
    array, not a concrete element, so the generic std combinators (compiled once at a
    type variable — `Array.foldl`/`map`/`sort`/`swap`) inline as well; an immediate
    element behind the type variable skips its tag-checked dup/drop at runtime. This
    is what speeds up the std `Array.sort` (the `MergeSort` workload), which never
    sees a concrete element. The `unsafeGet` wrapper is intrinsic-inlined to
    `Prim.arrayGet` carrying the call-site operand's concrete type, so a direct
    `Array.unsafeGet i (xs : Array Int)` reaches codegen at `Int`.
  - **Out-of-bounds still aborts (a located fault, "like `/`").** The inline get's
    bounds check, on failure, calls a new `fai_array_index_panic` runtime symbol
    (`fai_panic("array index out of bounds")`) on a cold branch, preserving the
     documented abort. The bounds check was kept unconditionally (`unsafeGet`/`get`
     lower to the same `Prim`, so it can't be selectively skipped at the primitive),
     so the safe `get`/`set` paid a redundant second check — since elided by the
     difference-bound bounds-check elimination (D131).
  - **rc helpers.** The local-keyed inline reference-count emitters were refactored
    to value-keyed cores (`emit_rc_incr_value`/`emit_rc_dec_then_value`/
    `emit_inline_drop_value`) so an array element (a value with no backing local) can
    be dup'd/dropped inline; `uniform_drop_class` is `drop_class` except a slot
    `Float` is the boxed leaf it is in a slot, not the unboxed scalar a `Float` local
    is.
  - **Cache.** A codegen-only change does not move the rc'd-IR fingerprint, so the
    `CODEGEN_CONFIG` stamp gains an `array-access-inlined` token to invalidate stale
    cached objects.
  - **Standing notes.** `Array Float` now stores raw, inline `f64`s (D133), so a
    concrete Float-array write loop is allocation-free too — `unsafeSet`/`push` store
    the raw bits in place, no per-element box. The redundant safe-path bounds check is
    since elided by bounds-check elimination (D131). `withCapacity`/`split`/`join`
    stay runtime calls (their cost is the irreducible allocation/string work).
  - **Validated** by codegen IR-shape tests (the inline bounds/unique checks and slot
    load/store, not a runtime call), `arrays` e2e tests (`Int`/`Float`/boxed/generic
    correctness + leak-free, in-place preserved), the array generate-and-run oracle
    proptest, and a `native` subprocess test asserting an out-of-bounds `unsafeGet`
    aborts with the located message. Issue #138 (under #136); complements #112
    (`Array Float` storage) and #120 (aggregate SROA).

- **D129 Hash-based associative containers (`HashDict`/`HashSet`) over a structural
  hash primitive, open-addressed and `Array`-backed.** std's only map/set was the
  O(log n) weight-balanced BST (D116); the associative-container benchmarks (a
  whole cluster of the Fai-vs-Rust gap) were dominated by that algorithmic +
  representation gap against Rust's flat `HashMap`. Add an unordered O(1)-average
  family alongside the ordered one.
  - **Hashing is a structural primitive, not an interface.** A new `Prim.hash`
    (runtime `fai_hash`/`fai_hash_borrowed`) mirrors `Prim.compare`/`fai_equal`: a
    borrowing recursive walk over the uniform value representation, kind-dispatched
    on the descriptor, that **agrees with structural equality** (`a = b` ⇒
    `hash a = hash b`) — Ints by logical value, `Float`/scalar slots by raw bits,
    inline-and-slice strings by content, data cells by tag + fields, arrays by
    length + elements, the niche `None` sentinel like a standard `None`. It returns
    a non-negative immediate `Int` (a splitmix64 finalizer, masked to 62 bits so it
    never boxes). Chosen over a user-facing `Hash` interface for the same reasons
    `Dict`/`Set` use structural `compare` rather than an `Ord` dictionary:
    consistency, no per-key-type instance burden, and it works on tuple/ADT keys
    with no user code. Prelude-private (`Prim.*`, std-only), consumed by the
    containers; `borrows_operand` so hashing a key does not consume it.
  - **Open addressing, `Array`-backed, copy-on-share — "like `Array`", not a
    persistent trie.** Each container is an opaque single-constructor wrapper of a
    live count and a power-of-two `Array` of slots (`HD Int (Array (Slot 'k 'v))`),
    with a private `Slot = Empty | Full …`. Linear probing; backward-shift deletion
    (so the slot type stays two-state — no tombstones touching the hot paths).
    Index is `hash & (cap-1)`; grow (double, rehash) past load 3/4; the empty
    container holds a zero-length array and allocates on first insert. Value
    semantics come for free from `Array`: a uniquely-owned table mutates slots in
    place (a threaded build allocates only the per-entry `Full` cell), a shared one
    copies. The inspect-only `get`/`getOr`/`member` only read slots, so borrow
    inference lends the table — a `get`-then-`insert` step stays in place. A HAMT
    was rejected: worse constant factors and it does not match the Perceus in-place
    model the benchmarks reward.
  - **`Slot` (one boxed cell per entry) over parallel arrays.** `Empty` is a
    nameable nullary immediate, so the backing array is `Array.repeat cap Empty`
    with no per-empty-slot heap cell and RC-clean drops; an occupied slot is one
    `Full` cell (tag + key [+ value]) — one allocation per entry, cheaper than a
    5-field `DictNode`. The alternative (parallel `keys`/`vals`/control arrays with
    no per-entry box) needs a generic "array of a chosen immediate" intrinsic and
    delicate RC for the uninitialized slots; left as **future work** (it intersects
    the "scalarize `Array` elements" and inline-array-access items), since the
    boxed-`Slot` form already turns O(log n)-with-`compare` into O(1)-with-`hash`.
  - **Iteration order is unspecified but deterministic** (slot/probe order, a pure
    function of the operation sequence — no per-run seed or pointer addresses), so
    `HashDict`/`HashSet` omit the ordered-only operations (`foldr`, range, a sorted
    `toList`); the ordered `Dict`/`Set` remain for those. Container `=` compares
    the representation, not the contents, for both families (different layouts/balances
    of the same entries differ), so contents are compared via a canonical form
    (sorted `toList`); the laws are membership-based to avoid order.
  - **Validation.** Runtime unit tests for the hash/equality agreement across every
    kind; the containers' own `example`/`forall` laws (insert/remove/member, size,
    set ops) under property testing; a scale suite vs Rust `HashMap`/`HashSet`
    (sorted/reverse/scrambled, removal, set ops) with order-independent checksums
    and leak-free exit; an allocation test pinning the unique-build-is-in-place
    (zero copy-on-write) contract; and the nine migrated `algorithms` oracles. Adds
    `Prim.hash` across resolve/types/core/codegen/runtime, `std/HashDict.fai` and
  `std/HashSet.fai`, and the `Prelude` re-exports. The hash-container half of the
  associative-container cluster (#137).

- **D130 Inline the structural hash (`Prim.hash`) for immediate/`Int`/`Float`
  operands.** The hash containers (D129) compute a bucket on every
  `insert`/`get`/`member`/probe via `indexFor cap key = Int.and (Prim.hash key)
  (cap - 1)`, but `Prim.hash` always lowered to an out-of-line
  `fai_hash`/`fai_hash_borrowed` call — one call per operation whose body is just a
  splitmix64 finalizer. Codegen now compiles `Prim.hash` of an immediate-shaped
  operand **inline** (`inline_hash` in `fai-codegen`'s `emit`, the peer of
  `inline_compare`/`inline_eq`), mirroring their operand ladder: a `Bool`/`Char`/
  `Unit` untags its payload and mixes it bare; an `Int` mixes a raw operand
  directly and guards a tagged immediate over the `fai_hash` fallback (a boxed,
  overflowed `Int` hashes its full 64-bit value out of line); a scalar `Float`
  reinterprets its bits and mixes them (matching the runtime's boxed-`Float` hash);
  a possibly-immediate operand (a type variable, nullary-bearing union, `List`,
  empty record — the generic container's key) guards the immediate fast path over
  the structural call, honouring the same borrow decision reference counting made;
  and an always-boxed operand (`String`, record, tuple, `Array`, interface) keeps
  the out-of-line call.
  - **Bit-identity is the contract.** The inline finalizer (`mix64`/
    `hash_payload_raw`) is emitted instruction-for-instruction equal to the
    runtime's `mix64` — the splitmix64 avalanche (logical shifts, wrapping
    multiplies) masked to 62 bits, then tagged — so the inline and out-of-line
    paths agree (a key hashed inline at insert finds the same bucket as one hashed
    out of line elsewhere). The result is a non-negative immediate `Int`.
  - **Niche carve-out (the correctness pitfall).** A niche `Option` operand is
    *never* hashed on its wrapper-free encoding: a niche `Some x` is the bare
    payload, but a key stored in a uniform `Array` slot is standardized (hashed as
    `tag + field`), so hashing the bare payload would land in a different bucket and
    never be found. The inline path converts an owned standard temporary and hashes
    that (consuming it), exactly as `inline_compare` does. Through the generic
    containers the key is a type variable (standardized at the boundary), so this
    branch is defensive parity rather than a path the containers reach.
  - **Cache.** A codegen-only change leaves the rc'd-IR fingerprint untouched, so
    the `CODEGEN_CONFIG` stamp gains a `hash-inlined` token to invalidate stale
    cached objects.
  - **Float in scope (unlike the issue's initial sketch).** A scalar `Float`
    operand inlines too (no box + call), since the bit-reinterpret machinery
    already exists for `=`/`compare`.
  - **Validated** by JIT bit-identity tests (an immediate/boxed `Int`'s inline hash
    equals the runtime `fai_hash` of the same value, bit for bit, including
    negatives), a `Float` agreement test (inline equals the boxed out-of-line
    path), a niche carve-out pin (a niche `Some "…"` hashes as its standard form,
    matching a laundered copy), codegen IR-shape tests (the inline finalizer for
    `Int`/`Float`, the runtime call kept for a `String`), and `HashDict`/`HashSet`
    round-trips over niche `Option String` keys alongside the existing
    Fai-vs-Rust scale suite. Issue #173 (under #136); complements #138 (inline
    `Array` access), the sibling per-operation call in the same loop.

- **D131 Bounds-check elimination for inlined `Array` access (a difference-bound
  analysis eliding a provably-redundant inline check).** The inline `Array`
  get/set (D124/#138) kept an *unconditional* unsigned bounds check on every
  access, so the safe `get`/`set` (which already guard `i >= 0 && i < length`) paid
  a second check and an `i`-from-`0 .. length` loop re-checked every iteration. A
  small **difference-bound fact engine** (`fai-core/src/bounds.rs`) now proves an
  index within `0 .. len`, and code generation drops the redundant check (the set
  keeps its `rc == 1` uniqueness branch).
  - **The domain.** A weighted graph of difference constraints `a <= b + c` over
    terms `{0, an int local, len(an array local)}`; an inequality is entailed when
    the shortest path's weight allows it (`i >= 0` is `0 -> i <= 0`, `i < len(a)`
    is `i -> len(a) <= -1`). Transfer functions interpret `+`/`-` (literal and
    var), `&` masks (`0 <= x & m <= m` for `m >= 0`, the hash-bucket idiom), `%`
    non-negativity, and the `Array` length-producing primitives; an `if` refines
    each branch with its dominating guard (including a disequality-from-zero bump,
    so `cap != 0` with `cap >= 0` gives `cap >= 1`). Reference-count wrappers are
    peeled. Soundness rests on the allocator invariant that a valid array's length
    is far below `i64::MAX` (so an index `< len` survives `+1` without wraparound —
    the invariant Rust's BCE relies on); a cost cap bails to no-facts on a
    pathological definition.
  - **Interprocedural, file-local.** A loop index passed a literal `0` start is
    non-negative only because of its caller, so entry facts are inferred caller-
    directed. A `private` definition's whole caller set is in its own file
    (cross-file references can only name `public` members), so `entry_bounds` is a
    **file-local** greatest fixpoint (`fai-rc/src/bounds_sig.rs`): the meet over
    every in-file call site of the facts provable for the arguments, with a
    self-call (loop back-edge) made to preserve each candidate and an
    upward-creeping bound **widened** away to converge. A `public` or
    first-class-used definition gets no entry facts (its callers are unknown). This
    keeps `object_code` a pure per-definition unit (facts depend only on the
    definition's own module), so the cross-module codegen firewall holds. So the
    hot index loops and the `HashDict`/`HashSet` bucket probes (`h & (cap - 1)`
    with `cap == length slots`) elide. The recursive in-place sorts need the
    relational extension layered on top (see D134).
  - **Cache & wire.** A codegen-only change leaves the rc'd-IR fingerprint
    untouched, so the `CODEGEN_CONFIG` stamp gains a `bounds-check-elim` token and
    the on-disk key mixes in the definition's entry facts and its callees' result
    facts; the wire bundle carries both so a daemon worker elides identically.
  - **Validated** by codegen IR-shape tests (the redundant `ult` gone for the safe
    `get`/`set`, a `0 .. length` loop, and a masked bucket index; kept for an
    unguarded or partially guarded access), a **shadow-check** soundness net (a
    test mode retains an elided check but routes its failure to a distinct abort,
    so a generate-and-run proptest over random array pipelines and quicksort turns
    any over-elision into a loud failure), the array/`HashDict`/`HashSet`/algorithm
    oracle suites, and a perf guard pinning the cross-module firewall. Issue #175
    (under #136); follows #138 (inline `Array` access).

- **D132 Confine composed/partially-applied/CAF closures by local reduction (the
  simplify pass).** A point-free value built from `>>`/`|>`/`identity`/`const` and
  partial application — especially one bound at a top-level value (a *constant
  applicative form*, e.g. `let transform = (fun x -> x + 1) >> shift 3`) — otherwise
  compiles to reference-counted heap closures called through the first-class
  `apply_n` path. Worse, a CAF is **not memoized** (every reference re-forces its
  static closure), so one referenced in a loop body is *rebuilt* per iteration: the
  `fold_pipeline` workload allocated two `>>` closures and one `shift` partial
  application per element. Escape analysis (D127) cannot help — the closure escapes
  its CAF's definition. A new Core pass (`fai-core/src/simplify.rs`, the
  `simplified` query) contracts these redexes before reference counting, so escape
  analysis and fusion then see ordinary direct code.
  - **Placement.** Layered `core_inlined → simplified → helper_inlined → fuse_def →
    rc`: the small same-file helpers a reduced composition leaves (e.g. a
    now-saturated `shift` call) are folded by helper inlining, and a pipeline whose
    element function is thereby reduced to arithmetic is then deforested into a
    register loop by fusion (D128). So `fold_pipeline`'s `transform x` becomes
    `((x + 1) * 2) + 3`, the fold becomes a raw-`i64` loop, and `transform` is
    dead-code-eliminated.
  - **Four behavior-preserving rewrites**, to a fixpoint within each definition
    (a defensive step cap keeps the query total): **CAF inlining** (a
    saturated-or-over application of a same-file, non-recursive, nullary, small value
    binding splices that binding's body in head position, relocating its lifted
    lambdas into the caller with freshened local *and* function ids; only an
    *applied* CAF, where reduction follows, never a value-position reference);
    **combinator reduction** by the resolved `Prelude` identities (`(f >> g) x →
    g (f x)`, `x |> f → f x`, `identity x → x`, `const a b → (let _ = b in a)`);
    **application flattening** (`App(App(h, xs), ys) → App(h, xs ++ ys)`, collapsing a
    curried partial application into a saturated direct call); and **beta reduction**
    of an applied literal lambda (binding arguments to fresh locals, mapping captures
    to the supplied locals).
  - **Behavior-preserving.** Recognition is by **resolved identity** (a
    `combinator_defs` resolver reads only `Prelude`'s module header, never a body),
    so editing `>>`'s body never changes what reduces and a user-shadowed operator
    (a different id) is left alone — the cross-module firewall. The reordered
    operands of `>>`/`|>` must be **pure** (a structural check mirroring fusion's
    barrier), so an effectful composition stays a heap closure; `const`'s discarded
    operand is kept in a dead binding, preserving its strict evaluation. CAF inlining
    is **intra-file** (a body edit never crosses a module boundary). Skipped entirely
    inside the standard library, so the combinators and operators stay exercised by
    their own contracts. Reference counting re-runs afterward, so it re-derives all
    dup/drop on the reduced body.
  - **Validated** by per-rule Core tests, an IR assertion that the `fold_pipeline`
    shape reduces to pure arithmetic, the `closure_allocations()`/`pap_allocations()`
    counters showing **zero** per-element heap closure/partial-application cells, an
    end-to-end allocation-scaling test (the composed-CAF fold's total is independent
    of `n`), an event-log guard (a CAF body edit re-simplifies its caller but does
    not re-run recognition), and rc-soundness property tests over random `>>` chains.
    Closes the `fold_pipeline` Fai-vs-Rust gap (#130, carved from #103).

- **D133 Unboxed `Array Float` (raw inline `f64` slots, self-tagged, no
  monomorphization).** An `Array Float` used to store each element as a pointer to a
  separate heap `Float` cell (a uniform slot word), so a float-array program
  allocated one box per element, pointer-chased on read, and freed a cell per
  element on drop. Its elements are now **raw, inline `f64`s** in the buffer (the
  slot stride is already 8 bytes, so capacity/length/growth math is unchanged — only
  the *interpretation* of the slot changes). The array peer of the unboxed scalar
  `Float` (D113); an unboxed `Array Int` is unnecessary (a small `Int` is already an
  inline immediate, and a float is self-describing where an immediate is not — a
  reason this is Float-only).
  - **Self-tagging, not evidence or monomorphization.** A second runtime descriptor
    `FAI_FLOAT_ARRAY_DESC` keeps `kind = KIND_ARRAY` (so the "same type ⇒ same kind"
    invariant the structural equality/ordering walks rely on holds for an empty vs a
    non-empty float array alike) and carries a non-zero `scalar_bitmap` as a
    whole-array "raw `f64`" flag (the array analogue of a record's per-slot scalar
    bitmap). A buffer is born plain (`FAI_ARRAY_DESC`) and **upgraded on its first
    float `push`** — the generic runtime path detects the boxed-float value's
    `KIND_FLOAT` descriptor, concretely typed codegen stamps it directly — so
    *generic* construction (`Array.init`/`repeat`/`fromList`, compiled once at a type
    variable) builds a correctly-tagged raw float array with no evidence plumbing. An
    empty `Array Float` stays plain-tagged, which is harmless: no walker's element
    loop runs over it.
  - **Concrete access is the win; generic access is the ceiling.** Codegen reads the
    element representation from a wire-preserved standalone type — the get's *result*
    type and the set/push *value* type, never the operand's `App` element (the object
    cache projects an `App`'s argument away). A statically-`Float` `unsafeGet` reads
    the slot word *as* the `f64` bits (no box deref); `unsafeSet`/`push` store the raw
    bits in place (push self-tags the buffer), the hot path allocation-free (the cold
    shared/uniqueness-loss fallback re-boxes for the runtime). A **generic
    (type-variable/erased) element** cannot know the representation statically, so it
    branches on the array's (or pushed value's) runtime descriptor: a float array
    re-boxes on read and stores-raw + self-tags on write; any other array keeps the
    uniform-word path (a single hot, header-cache-line load + compare, so non-float
    generic access — the common `Array Int` combinator case — is not regressed). The
    re-box at the generic boundary is the permanent no-`#16` ceiling: generic
    float-array *traversal* (non-fused `sort`/`toList`/`append`/… and first-class
    folds) gains no per-element compute and adds transient allocation, but the memory
    win (no persistent per-element boxes, O(1)-leaf drop, contiguous `f64`s) holds for
    *all* `Array Float`, and concrete index loops + fused user pipelines are faster.
  - **Runtime.** `scan_push` treats a float array as a leaf; `fai_array_get`/
    `get_borrowed` re-box a raw slot; `fai_array_set`/`push` store raw bits (freeing
    the transient value box) and self-tag, copying raw slots (no per-element dup) on a
    shared copy and re-stamping a grown/copied buffer; `array_equal`/`array_compare`/
    the hash arm compare/hash by `f64` bits (`total_cmp`, like a boxed `Float` and a
    scalar float field). Arrays are excluded from the reset-reuse token mechanism and
    the pool always rewrites the descriptor, so no stale tag survives recycling.
  - **Cache.** A representation change to the emitted loads/stores may leave the
    rc'd-IR fingerprint untouched, so the `CODEGEN_CONFIG` stamp gains an
    `array-float-unboxed` token to invalidate stale boxed-element objects.
  - **Validated** by runtime unit tests (raw-slot get/set/push/drop/equal/compare/
    hash, the empty-array edge), codegen IR-shape tests (a float get is a slot load +
    bit-reinterpret with no box deref; float set/push store raw bits), `arrays` e2e
    tests (a concrete build/read allocates independently of length; the keystone
    generic-build-then-concrete-read consistency; structural eq/ordering and
    `Dict`/`HashDict` keys; leak-free), a float generate-and-run oracle proptest, and
    the `SpectralNorm` + new `FloatMatrixMultiply` algorithm benches. Issue #112
    (under #136); follows D113 (#72, unboxed scalar `Float`) and D129 (#138, inline
    `Array` access); recommended after #86 (record/tuple float scalarization).

- **D134 Relational bounds-check elimination for the recursive in-place sorts
  (result facts + coinductive length preservation).** D131 elides an access proven
  from a definition's own *entry* facts, but the recursive sorts — the hand-written
  `quicksort` and the std `Array.sort`'s median-of-three quicksort — index by
  parameters bounded only *relationally*: `hi <= len(a)` must thread through the
  recursion (needing the partition pivot's bound and that the threaded array keeps
  its length), and the length relation holds *through* the recursion, so it must be
  assumed to be proved. Three pieces close this, all behind the same `result_facts`
  query (wired but empty at D131) and a no-monomorphization, file-local design.
  - **Coinductive length preservation (`length_preservation`, `fai-rc/src/length.rs`).**
    A callee-directed **greatest fixpoint** (the peer of `borrow_signature`: a salsa
    cycle, started optimistic and demoted) over `(result-component, parameter)` pairs
    deciding `len(result.component) == len(param k)`. It runs on the fused,
    pre-tail-flattening body (self-recursion still an `App`), with `arraySet` the only
    length-preserving primitive, preservation through a call read from the callee's own
    signature, and a self-call using the in-progress assumption. It is the **sole
    source** of result length *equalities* (so `partition` returns a same-length array,
    `qsort` preserves its argument's length).
  - **Coupled entry↔result fixpoint (`module_bounds_facts`, `fai-rc/src/bounds_sig.rs`).**
    Entry facts and the numeric result facts (length *inequalities* like
    `Array.init`'s `len >= n`, and integer bounds like a pivot in `[0, hi)`) are
    mutually recursive, so one file-local fixpoint computes both: an outer loop grows
    the result facts monotonically (read off each definition's return paths) while an
    inner two-phase entry-fact fixpoint runs with the current result facts applied —
    **phase 1** propagates a fact across the call graph external-only (so a fact that
    arrives only once a *caller's* facts are known is seeded), **phase 2** is the
    coinductive narrowing (assume a fact to prove it preserved by the self-recurrence;
    widen from the second round to drop a genuine creeper). `entry_bounds`/
    `result_facts` are thin projections; `result_facts` merges the length equalities
    in. The coupling is internal to one query, so entry/result form no salsa cycle
    between themselves; the only (defensive) salsa cycle is the cross-file one a cyclic
    module-call graph would create.
  - **Engine extensions (`fai-core/src/bounds.rs`).** Two additions let the
    `hi - lo <= 1` base-case guard establish `lo <= hi - 2`: a **literal-constant
    guard** (a comparison against a non-zero literal becomes a `Zero`-relative edge
    offset) and **two-variable subtraction** (a `d = a - b` binding is recorded in a
    side-table and, when a guard bounds `d` against a constant, emitted as the implied
    difference edge). The core graph stays a two-term difference-constraint system, so
    the shortest-path soundness argument is unchanged.
  - **What elides.** `quicksort`'s `partition` (the `j`/`hi-1` scan, with `swap`
    inlined into it) and `checksum`; the std sort's `partitionRangeOrd` scan. The
    median-of-three `pivotToEnd`'s `mid = lo + (hi-lo)/2` access keeps its check — that
    midpoint needs integer-division and two-variable-addition reasoning beyond a
    difference-bound domain — but it is O(log n) (the median selection), not the
    O(n log n) scan, so the dominant cost is elided.
  - **Cache & wire.** The `CODEGEN_CONFIG` stamp gains a `result-bounds` token (the
    relational extension changes emitted code for the same reference-counted IR); the
    object-cache key already mixes in a definition's entry facts and its callees'
    result facts, and the wire bundle already ships both, so daemon and cached builds
    elide identically. Entry facts now carry a bounded cross-file dependency on a
    callee's result facts (e.g. `gen` → `Array.init`), which follows the call graph
    and is early-cutoff, so the cross-module firewall for independent modules holds.
  - **Validated** by engine unit tests (the literal-constant and two-variable-guard
    cases), `fai-rc` fact tests (length preservation of `swap`/`partition`/`qsort`;
    the coupled `qsort` `hi <= len(a)`, `partition` pivot, `Array.init` `len >= n`,
    `checksum` `n <= len(a)`; a no-drift convergence test), codegen IR-shape proofs
    (no per-element `ult` in `partition`/`checksum` and the median-of-three partition
    scan; an unprovable access still checks), the **shadow-check** soundness proptest
    extended to `Array.sort` (every elided check re-verified at run time over random
    arrays), and the array/algorithm oracle suites plus the firewall perf-guard.
    Issue #182 (under #136); follows D131 (#181, difference-bound BCE).

- **D135 Scalar replacement of fixed-shape float aggregates (SROA + multi-value
  returns; the register/return half of #86, complementing D113's in-cell `Float`
  slots).** A **fixed-shape float aggregate** (FFA) — a tuple of all-`Float`, or a
  *closed* record of all-`Float`, with 1..=8 fields — that does not escape is held
  as its scalar `f64` **components in registers** rather than a heap cell, and
  crosses a direct call boundary in registers: an FFA **parameter** occupies N
  consecutive `f64` registers (arguments past the argument registers spill to the
  stack, so a parameter is register-eligible up to all 8 fields on every target),
  and an FFA **result** is returned via a Cranelift multi-result signature **when
  it fits the target's float return-register budget** — a wider result is returned
  as the boxed scalar-slot cell instead, because a multi-value return must fit
  entirely in registers (unlike arguments, returns cannot spill). The cap is the
  host's float return-register count — **AArch64 eight, x86-64 System V two,
  Windows x64 one** — a compile-time constant, since the compiler only ever targets
  the host (the JIT and the AOT object path both build for the host triple, which
  the object cache key already includes). So `Vec2`/`Mat2` vector-matrix algebra,
  `(Float, Float)`-returning helpers, and intermediate aggregates in numeric
  pipelines compute allocation-free, where D113 only unboxed a *scalar* `Float` and
  #114 (the in-cell slot layout) only stopped per-field boxing of a *heap-resident*
  aggregate.
  - **Type-directed, not escape analysis.** The representation is the exact analog
    of the scalar-`Float` rule (D113): an FFA is carried as its components and
    **materialized into the in-cell `f64`-slot layout on demand** wherever it
    crosses a *boxed boundary* — a field of a non-FFA cell, a closure capture, a
    uniform/`apply_n`/generic argument, a uniform-ABI return, a `=`/`compare`/`hash`
    operand. An FFA arriving **boxed** (a generic call result, a CAF, a boxed
    parameter/capture, a field of a larger cell) is exploded with field loads only
    where a spread boundary needs its components, and otherwise left boxed (the
    descriptor-aware reader handles its slots). No global escape pass is needed;
    soundness is the same "boxed where it crosses a uniform slot" boundary as D113,
    so the no-monomorphization ceiling holds (a generic/row-polymorphic/opaque
    position keeps the boxed cell — a tuple/record with a non-`Float` field, an open
    row, or a type variable is never an FFA).
  - **ABI is signature-derived (the firewall).** A new `Repr::Spread(components)`
    extends the per-slot ABI representation (the groundwork #114 laid). A shared
    `abi` query in `fai-core` derives each definition's calling convention from its
    *signature* (the SROA pass, reference counting, and code generation all consult
    it; the driver's `float_abi` delegates to it), so a caller's compiled object
    depends on a callee's signature, not its body. Spread is **register-ABI only**:
    a uniform entry (row-polymorphic / nullary, reached via `apply_n`) keeps the
    boxed cell, bridged by the first-class wrapper, which explodes boxed arguments
    and reassembles the spread result.
  - **SROA pass (`fai-rc`, on the A-normal-form body before reference counting).**
    Two new Core nodes — `Spread { components }` (the exploded components as a
    multi-value unit: a spread result tail, or a spread call argument) and
    `LetMany { locals, value, body }` (binding a spread-returning call's result
    components). The pass replaces an FFA construction with its component atoms, a
    projection with the component, a spread-returning call with a `LetMany`, and a
    spread-result tail with a `Spread`; it reassembles a cell at most **once per
    straight-line scope** (cache-one). A spread parameter's aggregate slot is a
    **borrowed anchor** that carries no runtime value (reference counting must not
    duplicate or drop it); its components are bound directly from the incoming
    registers, recorded in `LoweredDef::entry_spread_params`.
  - **Code generation.** Multi-value entry/return signatures (each parameter group
    keyed on the runtime arity, a spread expanded to its `f64` registers); the
    body's tail returns its components directly (an `if` returns from each branch,
    so no N-value merge is needed); spread call arguments are marshalled to
    registers; the first-class wrapper bridges `apply_n` ⇄ the spread entry. A
    `spread-aggregate` `CODEGEN_CONFIG` token retires stale cached objects.
  - **Deferred (accepted ceilings).** *Loop-carried* float-aggregate state is left
    boxed: a spread-ABI entry is **not** flattened into a tail loop (it recurses via
    direct calls), so a loop *carrying* a `Vec2` accumulator does not yet recycle —
    but a loop whose aggregates are *intermediate* (built and consumed within the
    body, the common case) is unaffected, since it carries plain scalars and
    flattens normally. Structural `=`/`compare`/`hash` on an FFA **reassembles**
    then calls the runtime (an inline component-wise compare is a follow-up).
    Nested FFAs (a record of records, e.g. a `Body` of two `Vec2`s) are not
    flattened. Per-#16, these are representation ceilings, not correctness gaps.
  - **Validated** by e2e allocation tests (a non-escaping `Vec2`/`Mat2`/`(Float,
    Float)` program allocates exactly as many cells as the equivalent scalar
    baseline — zero aggregate cells — on a target whose return-register budget fits
    the result, and is checked for correctness/leak-freedom where a narrower-budget
    target returns it boxed; an escaping aggregate still boxes; first-class spread
    closures via the wrapper are leak-free; a unit test pins the boxed-return
    reassembly directly, since it is reached only when the budget is below the
    result's width), the `algorithms` oracle suite (the `Array`-resident
    `NBody`/`Particles` float-record programs and the new register-resident `VecMat`
    vector/matrix benchmark run correctly on every target — a wide return boxes
    rather than overrunning the return registers), and the full
    backend-incremental/cache round-trip (the new IR nodes and ABI survive the wire
    form and the firewall). Issue #120 (the SROA + multi-value track of #86); D114
    delivered the in-cell-slot half and laid the `Repr`/`FnAbi` groundwork.

- **D136 Hoist a generic array's element-access self-tag out of the per-touch path
  (fixes a generic-traversal regression from D133).** Storing `Array Float`
  elements unboxed (D133) made every *generic* (type-variable element) array
  `get`/`set` branch on the array's runtime float self-tag — a descriptor load +
  compare deciding raw-`f64`-slot vs boxed word (`array_load_elem_generic`/
  `array_set_store_generic`). The standard `Array.sort`/`reverse`/… are compiled
  once at `Array 'a` (no monomorphization), so they paid that self-tag load on
  **every element touch** in their O(n log n) hot loops — where a *monomorphic*
  `Array Int` access (a hand-written quicksort) reads the slot directly with no
  self-tag. Measured on the `MergeSort` workload: the per-touch self-tag was the
  **whole** of a ~36% slowdown (≈866µs vs a ≈639µs floor with the self-tag
  removed), while the monomorphic quicksort was unaffected — the two diverged
  exactly along the generic-vs-monomorphic line.
  - **The tag is loop-invariant** within any body that re-tags no buffer: a
    descriptor word is rewritten **only** by a float `push`'s self-stamp, so a body
    that performs no `Array` allocation or `push` never changes one. Code generation
    therefore precomputes a generic array's `array_is_float` **once** — in the entry
    block (which dominates the whole body) for each generic array parameter, and at
    a `Join` (tail-loop) header for each loop-carried generic array parameter — and
    a generic element access reuses it instead of reloading the descriptor. The
    cache is keyed by the array's **pointer value**, not its local, so it hits
    through reference-counting aliases (a borrowed read keeps the parameter's own
    value, and an in-place `set` returns it unchanged) and is correct for any number
    of distinct arrays (each keyed by its own value); a value that misses (a freshly
    duplicated/copied array) falls back to the inline load. Dominance is automatic
    (the value is a parameter or loop block-parameter, computed where it dominates
    its uses), and the per-iteration loop value is one SSA block parameter, so the
    header computes it once for the whole loop.
  - **Soundness** rests only on "no buffer is re-tagged in this body": gated on the
    body containing no `Array` allocation/`push` (the existing `body_uses_array_alloc`
    predicate, the sole stamp sites). A body that *may* stamp keeps the conservative
    per-access load. The reused tag is the exact `array_is_float` of that value, so
    reuse is a pure common-subexpression — no representation assumption beyond tag
    stability.
  - **The float re-box arm is marked `cold`.** A generic array is far more often a
    boxed-element one than an (unboxed) float one, and the float arm holds an
    allocating `fai_box_float` call; laying it out of line keeps the call's clobbers
    off the hot boxed/immediate read path. This helps only *with* the hoist (once
    the per-access descriptor load is gone), the two together recovering the gap to
    ≈694µs (~77% of the regression).
  - **Residual (accepted ceiling).** The hoist removes the per-access *load*, but a
    generic access still *branches* on the (now loop-invariant) tag, and a
    duplicated/copied array value re-loads it; closing the last gap to the floor
    would need whole-loop specialization on the tag (two loop bodies), deferred as
    not worth the code-size/risk. A concrete (monomorphic) element never had a
    self-tag and is untouched.
  - **Cache.** A `array-tag-hoisted` `CODEGEN_CONFIG` token retires stale cached
    objects (the reference-counted IR fingerprint is unchanged — this is purely
    emitted-code).
  - **Validated** by codegen IR-shape tests (a generic two-read function emits
    exactly one self-tag `symbol_value`, with the re-box arm `cold`; a monomorphic
    `Array Int` emits none), the existing `arrays` e2e suite (generic/`Float`/boxed
    correctness + leak-free, in-place preserved, the constant-buffer-copy sort guard,
    the option-`Float`-not-mistaken-for-a-float-array guard), and the random sort /
    pipeline proptests and their bounds-check shadow checks. Issue #144 (under #136);
    follows D133 (`Array Float` unboxed) and D128 (inline array access).

- **D137 Generic foreign calls + a `foreign` declaration (extensible host side).**
  The host capabilities were a closed set: each of the seven host primitives
  (`Console`/`Clock`/`Random`/`FileSystem`/`Env`) was a dedicated `Prim` variant
  threaded through five synchronized tables (the resolver intrinsic allow-list, the
  type schemes, the `Prim` enum + `from_builtin`/`runtime_symbol`/`arity`, the JIT
  symbol registry, and the runtime implementations), so a new host meant editing
  the compiler. Since a capability already compiled as a generic out-of-line
  runtime call (the `Prim`-enum membership bought it nothing), the host side is now
  extensible too — the type side already was (effect rows track interface
  references, not a fixed enum).
  - **One generic Core node.** A new `ExprKind::Foreign { symbol, args }` names its
    native runtime symbol directly (an interned `Symbol`, which
    `Prim::runtime_symbol -> &'static str` cannot carry). It consumes its operands
    and returns an owned result like a primitive, but is a **host-effect barrier**
    (never reordered ahead of recursion, never fused across, never borrows). It
    serializes by symbol *name* across the run-worker wire and is part of the
    object-cache fingerprint; codegen routes it through the existing out-of-line
    path (the runtime-import cache rekeyed to an owned `String`). The seven host
    `Prim` variants are removed.
  - **A `foreign` declaration.** `foreign "native_symbol" name : Type` binds `name`
    to a native function with no Fai body. It is modeled as a definition whose
    synthesized entry is the un-wrapped eta-expansion `fn(p…) = Foreign{symbol, [p…]}`
    (arity = the type's arrow count), so a saturated call folds to a bare `Foreign`
    via the prim/foreign-wrapper inliner and a first-class reference compiles the
    entry closure — no new resolution, calling-convention, or codegen path. Its
    `DefInfo` points its `signature` and `binding` at the one foreign item, so the
    written type *is* its declared scheme. Rules: a `foreign` is **always
    module-private** (`public foreign` is **FAI2019** — a raw native function is
    exposed only through a capability interface), and its signature **must name a
    capability in its effect row** (**FAI5002**), closing the pure-laundering hole.
  - **Dogfood.** The seven hosts are now ordinary `foreign` declarations in
    `std/Prelude.fai`; the Rust name tables are deleted, leaving only the
    symbol→pointer registry (JIT) and the linked archive (AOT). `Prim.*` is no
    longer the path for host effects.
  - **Contract purity (amends D14).** The contract-purity check (`FAI6004`) is
    redefined off effect rows: an interface is a *capability* iff it declares an
    **effect-carrying method**, so a user-declared capability is rejected in a
    contract for free (the hardcoded host-name list is gone), while a plain pure
    interface stays usable.
  - **Extensible `Runtime` bundle (amends D53).** The default capability instances
    (`stdConsole`/…) and `defaultRuntime` are now `public`, so a program can compose
    them with its own (foreign-backed) capabilities into an extended record. The
    runtime root prefers a zero-arity `runtime` builder in the **entry file**,
    falling back to `defaultRuntime`; the untyped trampoline forces it and applies
    `main` to it unchanged, so `main : R -> Unit` for that `R` (a concrete record —
    constant-offset field access; row-polymorphic least authority stays an internal
    call-boundary feature). No trampoline-Rust change beyond which definition is
    chosen.
  - **Forward-target lambda linkage fix.** A definition that emits a separate
    token-taking reuse object *and* has a lifted lambda the reuse body reconstructs
    (a capturing capability-instance method built by a runtime builder is the
    natural trigger) now **exports** its `{base}__fn{i}` lambda symbols, which the
    reuse object imports — previously they were object-local, so the cross-object
    reference failed to link. The cache's codegen tag is bumped
    (`reuse-lambda-export`) so a warm cache cannot serve a pre-fix object.
  - **Marshalled user-FFI ABI.** A user (non-std) `foreign` uses a **marshalled**
    native ABI, not the raw Fai value ABI the built-in hosts use: each operand and
    the result are converted between the Fai value and a plain native type
    (`Int`/`Bool` ↔ `int64_t`, `Float` ↔ `double`, `String` argument ↔ a borrowed
    `(ptr, len)` pair, `String` result ↔ a `const char*` returned with its length
    through a trailing `int64_t* out_len` and copied into a fresh Fai `String` —
    the foreign owns its buffer; `Unit` is the empty value). So a plain C function
    is callable directly. A `marshalled` flag on `K::Foreign` (set at lowering from
    the declaration's origin) selects the ABI, carried in the wire form and the
    cache fingerprint; codegen emits the conversion glue (the `fai_marshal_*`
    runtime helpers). A foreign signature outside the marshallable subset is
    **FAI5003** (a type-level check, surfaced by `fai check`).
  - **Native linking.** A program's native dependencies are declared in a
    `fai.toml` (`[native]`: `library-dirs`/`libraries`/`objects`) at the workspace
    root — the first project-config file (breaks the single-`Main.fai` assumption,
    so `docs/CLI.md` documents it). **AOT** (`fai build`) threads the `-L`/`-l`
    flags and object files into the system linker. **JIT** (`fai run`) ships the
    resolved shared-library paths in the run bundle and the isolated worker
    `dlopen`s them (via `libloading`), installing a JIT symbol resolver that owns
    the handles for the run; objects are AOT-only. Contracts cannot call a foreign
    (its capability effect makes the contract impure), so the test worker loads
    nothing.
  - Issue #132. Builds on the type-level effect rows (D115).

Concurrency (tasks, channels, the M:N scheduler, biased reference counting):

- **D138 Biased reference counting (the foundation for sharing across tasks).**
  Perceus counts (D2) are non-atomic, which is unsound once a value is reachable
  from two tasks. Because Fai is pure, the *only* cross-task hazard on a shared
  value is its **count word** (the payload is immutable; in-place reuse fires only
  when uniquely owned), so the fix is confined to reference counting rather than the
  whole heap. Each count carries one of three states, distinguished by a single
  unsigned compare on the hot path (`rc < IMMORTAL_RC`):
  - **single-threaded** — a plain non-atomic count, the default and the common path;
  - **shared** — a high marker bit (`MT_FLAG = 1 << 63`) with the count in the low
    bits, manipulated atomically (relaxed increment; release decrement with an
    acquire fence on the last reference — the `Arc` discipline);
  - **immortal** (`≥ IMMORTAL_RC`) — a reference-counting no-op, so sharing a static
    across threads is race-free (and ThreadSanitizer-clean; previously immortals
    were still incremented).
  A value becomes shared via **`fai_mark_shared`**, which flips it and its reachable
  boxed subgraph (iteratively, reusing the drop worklist, so a deep structure never
  overflows the stack) when it crosses a task boundary — a spawned thunk's captures,
  a channel send, a task result. The acyclic heap bounds the walk; the spawn/channel
  hand-off is the happens-before edge that publishes the marks. Marking is the Lean-4
  model. A shared cell with an atomic count of 1 is still exclusively owned (no other
  task can hold a reference), so **in-place reuse/update still applies** to it via an
  atomic uniqueness check, and `fai_reuse` resets a recycled cell back to the
  single-threaded state. All runtime reference-count sites funnel through shared
  `rc_inc`/`rc_dec_is_dead` helpers, so the polymorphic and builtin paths count
  correctly regardless of a value's state. **Gating:** this is keyed (at code
  generation) on whether a program uses concurrency at all — a program with no
  `Concurrency` in any reachable effect row keeps today's exact non-atomic inline
  reference counting, so single-threaded code and the benchmarks are unaffected (the
  inlined-codegen branch and the cache tag for it land with the capability; the
  runtime helpers carry the branch already, on the cold polymorphic path).

- **D139 An M:N green-thread scheduler runs a program's tasks.** A task is a
  **stackful coroutine** (`corosensei`) whose body runs compiled Fai code; a fixed
  pool of worker OS threads runs tasks from **lock-free Chase-Lev work-stealing
  deques** (`crossbeam-deque`), so a task can migrate between workers. Awaiting a
  task or a blocked channel send/recv **parks** the task — freeing its worker — and a
  completion or freed slot **wakes** it by re-queueing, so many tasks multiplex onto
  few threads and `await` never blocks a worker. Each task's coroutine sits behind a
  mutex held for the duration of a resume, so it can never run on two workers at
  once; a parked task is referenced only by the one object it waits on. The
  coroutine's yielder pointer is re-established in worker-thread-local storage after
  every suspend, which keeps suspension correct across migration. The coroutine is
  `!Send`, so it is wrapped in a `Send` newtype justified by the task stack holding
  only `Send` data (Fai values are plain words, the runtime is thread-safe). The
  pool starts **lazily** on first use, so a program that never spawns pays nothing.
  The **worker count** is the host's available parallelism — which on Linux already
  honors the process's cgroup CPU quota and scheduler affinity, so a containerized
  or pinned run scales to its CPU budget with no extra probing — overridable by
  `FAI_WORKERS` (a positive integer; `=1` forces sequential multiplexing); the
  parallel-speedup benchmark measures the scaling. Because these crates and an IO reactor cannot be linked by a single
  `$RUSTC`, the runtime is no longer dependency-free and its archive is built by a
  nested `cargo` (amends D54). The reasoning behind choosing general concurrency, a
  capability surface, structured (nursery) scope, the M:N scheduler, an IO reactor
  with a TCP capability, and proven crates over hand-rolled coroutine/deque code is
  recorded with the work that builds the language surface on this foundation.
  - **Fai handles.** A `Task 'a`/`Channel 'a`/`Nursery` value is a reference-counted
    Fai heap cell (`KIND_TASK`/`KIND_CHANNEL`/`KIND_NURSERY`) whose slot owns a raw
    `Arc` to scheduler state; the free path drops that `Arc`, and the handle's
    `Drop` releases any Fai values it still owns (a task's stored result, a
    channel's buffered values), so the whole path is leak-free. Channels are bounded
    MPMC with backpressure and an explicit close (`recv` yields `None` once closed
    and drained); `await` is memoized (the result is duplicated out, so a handle may
    be awaited again).
  - **Capability surface.** `Concurrency` is a capability in the default `Runtime`:
    its interface (`scope`/`spawn`/`await`/`channel`/`send`/`recv`/`close`) and the
    native primitives + standard instance live in `Prelude`; the opaque
    `Task`/`Channel`/`Nursery` types live in a `Concurrency` module that depends on
    nothing (so `Prelude` re-exports and builds on them with no cycle). `scope` and
    `spawn` are **effect-polymorphic** — a method-local `'e` forwards the spawned or
    scoped body's own effect (`spawn : Nursery -> (Unit -> 'a / 'e) -> Task 'a /
    { Concurrency | 'e }`), so a function that spawns surfaces `Concurrency` without
    hiding what runs concurrently. The opaque handle types are single-constructor
    unions, so code generation never specializes their layout and drops them through
    the runtime (which frees the `Arc`).
  - **Execution gate (zero cost when unused).** Whether a program *uses* concurrency
    is a whole-program property — `Concurrency` is in `main`'s reachable effect row
    (merely holding the capability, which is in the default `Runtime`, does not
    count). When set, code generation switches to the thread-safe paths: inlined
    reference counting routes to the branchful runtime `fai_dup`/`fai_drop` (a value
    may be shared across tasks) and inlined `Array` allocation routes to the runtime
    allocator (the cached thread-local pool base would go stale across a task's
    worker migration); and `main` runs as the scheduler's **root task**
    (`fai_run_main_concurrent`, which `block_on`s it) so `scope`/`spawn`/`await` run
    inside a task. The flag rides the code-generation config — part of the
    `object_code` query key, the on-disk cache fingerprint, and the run-bundle wire
    form — so a single-threaded program keeps the fully inlined fast paths byte for
    byte (the common case pays nothing) and the two modes cache separately. Making
    the *inlined* reference counting itself branch per-object (so a concurrent
    program keeps inline RC rather than calling out) is a future refinement.

- **D140 A blocking-work thread pool keeps blocking host calls off the workers.**
  A host operation that blocks the OS thread (file I/O; later, DNS resolution) must
  not run on a scheduler worker, or it would stall every task multiplexed onto it.
  The runtime grows a separate pool of OS threads (lazily — a new thread only when
  every existing one is busy, up to a cap, `FAI_BLOCKING_THREADS`, default 512), and
  `run_blocking` offloads a closure to it while the calling task **parks**, waking
  the task (`schedule`) when the work completes. The offloaded closure yields a plain
  Rust value, so no Fai heap allocation happens off-worker — the parked task builds
  the Fai values back on its own worker after it resumes, which sidesteps any
  cross-thread allocator/reference-count question. `FileSystem.readFile`/`writeFile`
  use it **when called inside a task** and run inline otherwise (a program without
  concurrency has no scheduler to park on), dispatched on whether the caller is a
  worker. The pool wake may race ahead of the park; that is safe, because the task is
  queued exactly once and resumes only after it yields (its coroutine lock serializes
  the resume against the running worker).
- **D141 A built-in `Bytes` type — an immutable binary byte buffer.** Network (and
  future binary) payloads need a byte sequence distinct from the UTF-8 `String`.
  `Bytes` is its own built-in `Con` (global, no import), **not** `Array Byte`:
  modeling it as an array would force introducing a `Byte` scalar type (its own
  representation, literals, arithmetic, `Int` conversions) and a packed array
  representation — large scope for no gain, and against the existing choice not to
  pack element-typed arrays. `String` is the precedent (a distinct buffer type, not
  `Array Char`). At runtime `Bytes` reuses the inline `String` buffer layout (length
  then inline bytes) but carries a **distinct kind** (`KIND_BYTES`), so a binary
  buffer is never treated as a UTF-8 `String` and the two never compare equal in a
  generic position; equality/ordering/hash compare it by byte content. It has no
  borrowing-slice form (slicing copies). Its elements are bytes exposed to Fai as
  `Int`s (0–255); the `Bytes` std module wraps byte-oriented `Prim.bytes*` intrinsics
  (`length`/`get`/`unsafeGet`, `concat`, `slice`, `fromList`/`toList`,
  `fromString`/`toString`). The runtime primitives stay simple (they return
  `Int`/`Bool`/`Bytes`/`List`/`String`); the `Option` results (`get`, `toString`) are
  built in Fai, so the niche `Option` representation is unaffected. Conversions copy:
  `fromString` always succeeds; `toString` is guarded by a UTF-8 check (so invalid
  bytes can never masquerade as a `String`).

- **D142 A readiness-based network reactor and the `Net` (TCP/UDP) capability.** The
  host network surface is built on a **readiness** I/O model (a single reactor
  thread running `mio`'s Poll — epoll/kqueue/IOCP) rather than a completion/callback
  loop, because it fits the M:N work-stealing scheduler: worker threads register
  their own sockets (the registry is `Send + Sync`) and perform the read/write
  syscalls themselves, and the reactor only reports "this socket is readable/
  writable" and wakes the waiting task. So the worker that owns a task does its I/O
  and only readiness wakeups cross to the reactor thread — no socket data or
  operation is marshalled between threads (the decisive reason **`mio` was chosen
  over `libuv`**: libuv's single-loop, handle-affine, callback model would funnel
  every operation through the loop thread and marshal buffers across threads, a poor
  fit for work-stealing, and would add a C/cmake/libclang toolchain dependency for
  capabilities — DNS, file I/O — the blocking pool already covers). A per-direction
  readiness latch closes the lost-wake race (a readiness edge that arrives between a
  failed syscall and the park is recorded, so the task retries rather than parking
  forever). The reactor starts lazily on first use. **`Net` is a capability** in the
  default `Runtime` — TCP `listen`/`localPort`/`accept`/`connect`/`send`/`recv`/
  `close` and connectionless UDP `udpBind`/`udpLocalPort`/`udpSend`/`udpRecv`/
  `udpClose` (each datagram addressed by host/port, `udpRecv` reporting the sender as
  a `(Bytes * String * Int)` tuple) — surfaced exactly like `Concurrency`: the
  interface, foreign primitives, and standard instance live in `Prelude`; the opaque
  `Listener`/`Connection`/`UdpSocket` types live in a dependency-free `Net` module
  that `Prelude` re-exports. Payloads are `Bytes`; fallible operations return
  `Result _ String` (built by the runtime, the standard two-cell representation).
  Each operation runs on its task and parks on the reactor at every would-block; a
  hostname (in `connect`/`udpSend`) is resolved on the blocking pool (D140) while an
  IP literal is parsed inline (no park). The whole-program **execution gate** (D139) triggers on
  `Concurrency` **or** `Net`, since a networking program must run `main` as the
  scheduler root task so its socket operations can park — so a `Net` program runs on
  the scheduler (and pays the biased-RC cost) just as a concurrent one does;
  decoupling scheduler-execution from atomic reference counting for a `Net`-only,
  spawn-free program is a possible future refinement.

- **D143 An `Async` combinator library — ergonomic concurrency over the structured
  primitives.** The low-level `Concurrency` capability (D139) exposes
  `scope`/`spawn`/`await` and channels, which are powerful but verbose used
  directly — a nursery threaded through, a per-task `spawn`, a matching `await`.
  `std/Async.fai` adds a high-level layer — `parallel`/`parallel2`/`parallel3`,
  `mapConcurrent`/`iterConcurrent`, and the channel helpers
  `collect`/`sendAll`/`produceList`/`pipe` — so the common fan-out, concurrent-map,
  and producer/consumer patterns are a single call. It is **pure Fai over the
  existing primitives** (no compiler or runtime change): each combinator takes the
  `Concurrency` capability as its first argument and opens its own structured
  `scope`, spawning every task and *then* awaiting them (the two-phase split is what
  makes the work run concurrently rather than serialize), so the structured guarantee
  is preserved — no task outlives the call — and the effect row still surfaces
  `Concurrency` (forwarding the supplied work's own effect through `'e`).
  - **Direct style, not a monad.** Fai's concurrency is direct-style: a deferred unit
    of work is an ordinary thunk `Unit -> 'a`, `await` returns `'a`, and a blocking
    host call parks transparently (D139/D140). So the reason F#/.NET needs a cold
    `Async<'T>` value and `async { let! … }` computation-expression sugar — an explicit
    continuation to sequence — does not apply here. `Async` is therefore an ordinary
    library of functions over thunks, **not** a new type or block syntax, which keeps
    the small, regular grammar. The one residual difference from an ambient-runtime
    async is the explicit `Concurrency` argument; that is the capability model (a
    function's reach stays visible in its type), reduced to one occurrence per
    high-level call.
  - **No contracts; tested end-to-end.** A contract may not reference a capability
    (FAI6004), and every combinator requires the `Concurrency` capability, so they
    carry no `example`/`forall`. The std example-coverage guard is widened to exempt
    any public function whose signature names a capability type (keyed off the same
    rule as FAI6004: an interface that carries an effect is a capability). The library
    is covered instead by end-to-end tests that JIT-run real programs and assert
    deterministic results and a leak-free exit.
  - **Cancellation-dependent combinators deferred.** `race`/`choice`/`timeout` (and
    cancel-siblings-on-error) are **not** provided: there is no task cancellation, and
    `scope` joins every child unconditionally, so a race would return the winner yet
    still block on the loser running to completion. These need a cooperative
  cancellation primitive in the runtime (a nursery cancel plus a cancel check at the
  park points), noted as future work.

- **D144 Effect-kinded parameters on user data types (extends D118).** A `type`/alias
  parameter used only in effect position (an arrow's `/ 'e` tail, or the effect slot
  of another type/interface it is applied to) is now an **effect** parameter — the
  same rule interfaces already used (D118), generalized from `interface` to `type`.
  This lets a plain data type *store an effectful suspension in its type* — the basis
  for the lazy `Stream` (D145), and for any deferred-effect value (a thunk, a parser
  combinator) that previously had to be smuggled through an interface existential.
  Without it the effect annotation inside a constructor field decoupled from the type
  parameter and **laundered to pure** (the field's `/ 'e` and the type's `'e` were
  unrelated variables), which is unsound; this closes that gap.
  - **Kind inference is a per-file monotone fixpoint.** Classifying a parameter as
    type- vs effect-kinded must handle recursion: in `type Stream 'a 'e = MkStream
    (Unit -> Step 'a 'e / 'e)` the recursive `Stream 'a 'e` makes `'e` *look*
    type-used, but the `/ 'e` tail is an unambiguous effect anchor. So every type in
    a file is classified together by a fixpoint that routes a recursive reference's
    arguments by the referenced type's *current* kinds, and **defers** an immediate
    variable in a not-yet-settled slot rather than counting it as a type use. This
    classifies **mutually-recursive** types that both thread an effect (`Stream`
    references `Step` and vice versa) correctly. A cross-file type cycle breaks
    conservatively (every parameter a type).
  - **Reuses the rest of the machinery.** The constructor scheme seeds an effect
    parameter as an effect-row variable shared with its fields' arrows (so projecting
    a field yields the scrutinee's effect, not a fresh one), and a use lowers the
    effect argument to the same `Ty::EffectArg` interfaces produce. Unification, deep
    subsumption (covariant under an effect argument), instantiation, pattern
    projection, generalization, and the whole back end (effects are erased, D118) are
    unchanged — they were already generic over `Ty::EffectArg`. An **alias** that
    threads an effect parameter (the `Prelude` re-export `type Stream 'a 'e =
    Stream.Stream 'a 'e`) substitutes effect rows in expansion, not just types.
  - **Diagnostics.** A parameter used as *both* a type and an effect is `FAI3019`
    (generalized from interfaces); a wrong-kind argument is `FAI3020`. The change is
    purely additive — a parameter flips to effect-kinded only when used *solely* in
    effect position, so no existing type is reclassified.

- **D145 A lazy `Stream` and progressive file/stdio I/O.** A `Stream 'a 'e` is the
  composable abstraction for streaming data — generated sequences and progressive
  file/stdio reading — without materializing it. It is a **pure data type** (an
  effect-carrying ADT, D144), not an interface or a channel: `type Stream 'a 'e =
  MkStream (Unit -> Step 'a 'e / 'e)` with `Step = Done | Yield 'a (Stream 'a 'e) |
  Fail String`. A suspension yields one step when forced; the effect rides inside it,
  so **building** a stream is pure and only **consuming** it (forcing steps) performs
  `'e`. This fits a strict, pure language: it is the strict-ML `Seq` shape with an
  effect row, so it needs no laziness primitive beyond the explicit thunk.
  - **API shape.** `Stream` is **fully opaque** (its `MkStream` constructor, the
    `Step` observation type, and the `next` step function are all internal, so the
    representation stays swappable) — consumption is through the terminal consumers
    (`fold` is universal, so no public eliminator is needed; a public `Step` would
    also be unexampleable, since it carries a function-bearing `Stream`). A
    construction kit (`empty`/`cons`/`defer`/`failed`/`unfold`) builds custom sources;
    producers (`fromList`/`range`/`iterate`/…), transformers (`map`/`filter`/`take`/
    `flatMap`/`zip`/`scan`/…), and consumers (`fold`/`toList`/`forEach`/…) round it
    out. Transformers are **pure** (`Stream → Stream`) and thread `Fail` through
    unchanged; terminal consumers return **`Result _ String`**, short-circuiting to
    `Err` on the first `Fail`. Consumers loop self-tail-recursively, so consumption is
    constant-stack and constant-memory (consumed nodes are freed by reference
    counting).
  - **Errors as values (not traps).** Fai library code cannot abort with a message
    (only the runtime can, and only with fixed messages), and every host I/O op
    already threads errors as values — so a mid-stream failure is the structural
    `Fail`, surfaced by consumers as `Err`. I/O sources are **lazy**: `fileLines`/
    `ofReader`/`fileBytes` are pure to build, the file opens on first consumption, and
    both open and read errors become `Fail`. The handle closes when the stream value
    is dropped (reference counting) — full, partial, or zero consumption all release
    it promptly, with no `bracket`.
  - **The progressive I/O primitives.** `FileSystem` gains `openRead`/`openWrite`/
    `openAppend` (returning opaque `Reader`/`Writer` handles, declared in `std/Io.fai`
    like the `Net` handles), chunked `readChunk` (an empty `Bytes` is EOF) and
    `writeChunk`, and `closeReader`/`closeWriter` (the latter flushes). `Console` gains
    `write` (stdout, no newline), `writeError` (stderr), and `readLine` (stdin, an
    `(Int * String)` status pair the standard instance wraps into `Result (Option
    String) String`). Each blocking call runs on the blocking pool inside a task (the
    task parks) and inline otherwise — the same dispatch `readFile` uses (D140), so
    streaming I/O is transparently async on the scheduler with no awaitable type
    (direct style, D143). A new `KIND_FILE` handle cell owns the `Arc` to the OS-side
    `BufReader`/`BufWriter`, released (closing the file) by `free_obj` when the cell
    dies, mirroring `Net` (D142). `decodeUtf8Lines` turns a byte stream into UTF-8
    lines, buffering across chunk boundaries so a multi-byte character split between
    chunks decodes whole.
  - **Single-consumer, not deforested.** A stream is consumed linearly (each step
    forced once; re-forcing an effectful node would repeat its effect). Concurrent
    fan-out uses `Async`/channels (a `fromChannel`/`toChannel` bridge is future work).
    The List/Array deforestation pass does not apply (a different mechanism); a stream
    wins by *not materializing* and constant memory, not by zero-alloc-per-element.

- **D146 A NodaTime-style date & time library (`std/datetime/`).** A set of distinct
  value types in the ML/NodaTime tradition, each its own module with the type name
  re-exported by `Prelude`: `Instant` (a UTC timeline point), `Duration` (a fixed
  elapsed length), `LocalDate`/`LocalTime`/`LocalDateTime` (calendar values with no
  zone), `Offset` (a fixed UTC offset), `OffsetDateTime` (a local date-time pinned by
  an offset), `Period` (a calendar-aware length), and the `DayOfWeek`/`Month` enums.
  Operations are subject-last so they pipe (`date |> LocalDate.plusDays 3`),
  mirroring `List.map`/`Dict.insert`.
  - **Nanosecond precision, two-field representation.** `Instant` and `Duration` are a
    whole-day count plus a nanosecond-of-day in `[0, 86_400_000_000_000)`; `LocalDate`
    is an epoch-day count; `LocalTime` a nanosecond-of-day; `LocalDateTime` the two
    counts; `Offset` whole seconds. A single `i64` of nanoseconds would cap the
    calendar at ~1678–2262, so the split keeps the range vast while arithmetic stays
    exact integer work. The calendar uses the standard civil↔days conversion (Howard
    Hinnant's algorithm), which divides only non-negative operands, so the truncating
    `/` is the floored division it needs; a shared `Int.floorDiv`/`Int.floorMod` (also
    added) handles the genuinely-signed normalization of nanosecond-of-day and offset.
  - **Opaque, validated value types.** Every type is `opaque` with a module-private
    constructor; smart constructors validate and return `Option` (`LocalDate.of`,
    `LocalTime.of`, `Offset.ofHours`), so an out-of-range date/time is unrepresentable.
    Structural `=`/`<`/`compare` still work across files (opacity permits structural
    comparison), and the components are laid out chronologically so the built-in order
    is the chronological one; each type also offers explicit `compare`/`isBefore`/…
  - **ISO-8601 everywhere + a custom pattern engine.** Each type renders and reads its
    canonical ISO form (`toString`/`parse`, pure). `DateTimeFormat` adds a custom
    pattern mini-language over `LocalDateTime` (`yyyy`/`yy`, `M`…`MMMM`, `dd`/`d`,
    `HH`/`hh`, `mm`, `ss`, `fff`/`ffffff`/`fffffffff`, `tt`, `EEE`/`EEEE`, and quoted
    literals): a tokenizer feeds a format emitter and an inverse parse consumer.
  - **Offset-based; IANA time zones deferred.** The library is pure except
    `Instant.now`/`OffsetDateTime.now`, which take the `Clock` capability. To read
    *local* wall-clock time (not just UTC) the `Clock` interface gains
    `localOffset : Unit -> Int / { Clock }` — the system UTC offset in seconds — backed
    by a new `fai_clock_local_offset` runtime primitive that asks the C library
    (`localtime` + `timegm`/`_mkgmtime`, declared directly so the runtime keeps no crate
    dependencies). Full IANA `DateTimeZone`/`ZonedDateTime` with daylight-saving rules
    is **deferred**: it needs an embedded, periodically-updated tz database; the
    offset-based model covers fixed-offset wall-clock use without that maintenance
    burden.
  - **Subfolder embedding.** The modules live under `std/datetime/`; the std-embedding
    build step and the std-scanning gates now recurse subdirectories and name each
    embedded module by its path relative to `std/` (e.g. `datetime/Instant.fai`).
    Module resolution is unchanged (it keys off the `module` header, not the path), and
    `is_std_path` is a prefix check, so users still reach everything qualified
    (`Instant.now`) with no import.

- **D147 An `internal` visibility tier (same-origin visibility).** Visibility gains a
  middle tier between `public` and module-private: `public > internal > private`.
  `internal` exports a binding/type/interface across files **only within the same
  *origin***, where an origin is today the standard library vs. user code (the
  existing `fai_db::is_std_path` `<std>/` prefix check) and becomes a package id when
  a Fai package system lands. The motivating problem is standard-library API hygiene:
  cooperating std modules could previously share a helper only by marking it `public`,
  which leaked it into the user-facing API (e.g. the `datetime` modules' raw
  `fromEpochDayAndNanoOfDay`/`fromNanoOfDay` constructors). `internal` lets std share
  such seams among its modules while hiding them from user programs.
  - **Same-origin rule, reused machinery.** A cross-file reference to an `internal`
    member is allowed iff `is_std_path(referrer) == is_std_path(definer)`. The value
    gate is in `walk_cross_file` (the referrer's origin is the resolver's existing
    `is_std` flag); cross-file *type*/*interface* resolution gates the same way in
    `fai-types`'s `lower.rs` (`lookup_type`/`lookup_interface`). A cross-origin
    `internal` reference is **`FAI2020`** (one code for value/constructor/type/
    interface, with an origin-accurate message). In user code there is a single
    origin, so `internal` is currently observably identical to `public` — a legal,
    forward-compatible no-op that gains teeth when packages arrive.
  - **Purely name-level; orthogonal to `opaque`.** Because resolution hides an
    `internal` *name* cross-origin, and the leak rule forbids a public surface from
    naming an `internal` type, cross-origin code can never name, hold, or infer a value
    of an `internal` type — so there is **no new types-layer (`FAI3xxx`) enforcement**,
    unlike `opaque` (which hides a *representation* and needs `FAI3018`). The two axes
    compose: `internal opaque type` is allowed and reuses the existing
    visibility-independent opaque machinery (the constructor/representation hiding,
    `FAI2018`/`FAI3018`); the cross-file gate checks **origin before opacity** (a
    cross-origin sibling sees `FAI2020`, a same-origin sibling sees the opaque
    `FAI2018`). `internal` is rejected on `foreign` (always module-private, **`FAI2019`**
    generalized) and on nested modules (which carry no visibility marker, like the
    absence of `public module`).
  - **Required signature + visibility monotonicity.** An `internal` binding requires an
    explicit signature, like `public` (**`FAI3003`**, generalized; its quick-fix moves
    the keyword to a new signature line). The leak check (**`FAI2015`**, generalized from
    "private type in a public signature") now enforces the full ordering: an exported
    surface may not name a type of lower rank — a `public` surface naming an `internal`
    or `private` type, or an `internal` surface naming a `private` type — and it resolves
    *cross-module* type references (a public std surface naming a same-origin `internal`
    type from another std file still leaks), flagging only types that are nameable here
    (a cross-origin type is the unresolved/`FAI2020` case, not a leak).
  - **Firewall kept public-only; tooling gets its own.** `module_interface` (the
    cross-module incremental firewall) stays **public-only**, so all existing
    early-cutoff guarantees and perf guards are untouched; `internal` value resolution
    rides the body-independent `module_defs`/`type_decls` queries it already used, so an
    `internal` edit invalidates dependents exactly like a `public` one and never a
    cross-origin importer. A new `module_internal_interface` query (the `Internal`-tier
    peer) feeds **origin-aware tooling** only: cross-module completion, the
    qualify-an-unbound-name code action, and `fai query api` offer/list `internal`
    members when the request shares the target's origin (for `fai query`, that means a
    user module's own `internal` members, never a std module's).
  - **Codegen/RC treat `internal` like `public` (conservatively).** `internal` is *not*
    a whole-module-private optimization assumption: borrow-signature entry-fact
    eligibility stays `private`-only (an `internal` member is callable from sibling
    same-origin modules, so its call sites are not all known here). The `jit_compile`
    image makes the entry file's exported API fetchable by name with `!= Private` (the
    entry file is one origin, so its `internal` bindings are as fetchable as `public`
    ones); the minimal AOT path stays main-only.
  - **Standard-library cleanup.** The first user is `LocalDateTime`: a zoneless local
    date-time has no meaningful public "epoch day", so its raw epoch-day/
    nanosecond-of-day storage bridge (`fromEpochDayAndNanoOfDay` and the `epochDay`/
    `nanoOfDay` accessors) — previously `public` only so the sibling date/time modules
    could convert representations — is now `internal`, and a clean `public epoch`
    replaces the idiom of `fromEpochDayAndNanoOfDay 0 0` (which a sample had reached
    for). `Instant` (epoch-relative) and `LocalTime` (nanosecond-of-day is a first-class
    concept) keep their decompositions public. A sweep of the other multi-module
    clusters (`Stream`/`Io`, `Concurrency`/`Async`, `Net`, `Dict`/`Set`/`HashDict`/
    `HashSet`) found no further leaks: they already encapsulate via opaque handle types
    and capabilities and use each other's intended public APIs. `samples/Visibility.fai`
    demonstrates the three tiers.

- **D148 Timers, cancellation, TLS, and an HTTP stack.** Added in dependency order
  on the existing scheduler/reactor and `Net`:
  - **Timer & cancellation (runtime).** The `mio` reactor gains a deadline min-heap
    and a `Waker`; `Clock.sleep` parks a task on it (or sleeps the thread when run
    inline, so a non-scheduler program still works). Task cancellation is
    **cooperative and sticky**: `cancel` sets a per-task flag and unparks the task;
    every park point (sockets, channels, the blocking pool, sleep, await/join)
    re-checks it and returns a cancellation `Err`, so the task unwinds and frees its
    resources by reference counting. It is **structured**: cancellation propagates
    down the task tree (a task spawned under a cancelled parent is cancelled too), so
    a `timeout`/`race` (in `Async`) or a server shutdown tears down the whole subtree
    while `scope` still joins. No exceptions: a task ends only by returning, so
    cancellation surfaces as a value, never a forced unwind (which would skip RC
    cleanup). Considered and rejected: trap-based forced termination (leaks the
    task's live handles/memory).
  - **TLS (native, thin).** A `Tls` capability wraps a **sans-I/O rustls** engine
    with the **ring** provider — chosen over reimplementing TLS in Fai (impossible to
    keep constant-time/side-channel-safe in a boxed, reference-counted language, and
    it would need native entropy/trust-store anyway) and over `aws-lc-rs` (adds a
    cmake/NASM build step; ring needs only the C toolchain the compiler already
    requires via blake3). The engine steps the handshake/record layer over in-memory
    buffers (`feedIncoming`/`takeOutgoing`/`readPlaintext`/`writePlaintext`), so Fai
    keeps **all** the networking — it drives the handshake and shuttles ciphertext
    over the existing async `Net`. A `KIND_TLS` handle owns the `rustls` connection;
    trust is the bundled `webpki-roots` plus an explicit `clientWithRoots` (no
    insecure "accept any cert" mode).
  - **HTTP (pure Fai).** `std/Http.fai` is an HTTP/1.1 client and server over `Net`,
    with an opaque validated `Url` (`std/Url.fai`) and a case-insensitive, order- and
    duplicate-preserving `Headers` (`std/Headers.fai`). `Url` is an opaque **union**
    wrapping its component record, not an opaque record: records are structural, so an
    opaque record leaks its fields across files, whereas a single hidden constructor
    keeps the type nominal (the date & time value-type shape). Framing is written
    against an abstract `Transport` (recv/send/close), so it is socket-independent: a
    `plainTransport` runs over a TCP `Connection`, and a `tlsTransport` drives the
    rustls handshake and shuttles ciphertext over the same `Connection` (so HTTPS is
    pure-Fai orchestration of the `Tls` engine over `Net`). The client picks
    plain-vs-TLS by the URL scheme at runtime, so its whole surface is uniformly
    `/ { Net, Tls }`: `plainTransport` is typed `Transport { Net, Tls }` and
    over-declares the `Tls` effect it never performs, so the two transports unify in
    the scheme `if` (whose branches must agree — a user data type's effect argument is
    invariant). `get`/`getWith`/`post`/`postForm`/`request`/`requestWith`/`requestOnce` (client,
    the `With` forms taking extra trusted roots) and `serve`/`serveTls`/`serveListener`/
    `serveListenerTls` (server) follow. Bodies are `Stream Bytes`. A **received** body
    is a **lazy** stream decoded on demand per its framing — chunked transfer-encoding,
    Content-Length, or read-to-EOF — so a client streams a large response without
    buffering it (the body owns the connection, dropped to close it); a server request
    body is drained in full so the transport is free for the response. A **sent** body
    is drained to a `Content-Length` by default, or streamed **chunked** without
    buffering when the headers select `Transfer-Encoding: chunked` (`chunkedResponse`,
    or the header on a request). Chunked sending is driven by a new low-level
    **`Stream.uncons`** (`Stream 'a 'e -> Result (Option ('a * Stream 'a 'e)) String /
    'e`): the sender's own recursive loop unions the body's effect `'b` with the
    transport's `{ Net, Tls }` into the row `{ Net, Tls | 'b }` — a generic dual-effect
    *consumer* (`fold`/`forEach`) cannot, since it ties the element action and the
    stream to one effect, and `{ 'e | 'f }` (two effect *variables* unioned) is not an
    expressible row, but concrete atoms plus one tail var is. The client **follows
    redirects** automatically up to a hop limit: a 301/302/303 becomes a `GET` and a
    307/308 is followed only for a bodyless method (the already-consumed body cannot be
    replayed), a relative `Location` is resolved against the request URL, and
    `Authorization` is dropped on a cross-origin hop (along with the hop-specific
    `Host`/`Content-Length`/`Transfer-Encoding`); each 3xx response is dropped before
    re-requesting (closing its connection), and `requestOnce` is the single-shot,
    no-follow path. **Auth and form helpers** round out the request side: `basicAuth`/
    `bearer` build an `Authorization` value over a `base64Encode`, and `formBody`/
    `postForm` build an `application/x-www-form-urlencoded` body. A connection-pooling
    client remains follow-up.
  - **A codegen fix surfaced by this work:** a definition that both has a string
    literal and a token-taking reuse entry emitted its entry body twice in the single
    in-process JIT module (the primary and the reuse entry shared a `{base}__fn0__strN`
    local-data prefix), panicking with a `DuplicateDefinition`; the reuse entry now
    names its local data under a distinct `{base}__reuse` prefix (AOT was already fine
    — separate objects, local symbols).

To change a locked decision: update this log **and** the table in `AGENTS.md`,
and note the migration in the affected decisions.
