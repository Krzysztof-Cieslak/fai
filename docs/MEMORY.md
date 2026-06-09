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
| R8 | Scope creep from "AI-first" features | Med | Med | Effect rows, extension/restriction, and a package manager are out of the current scope — tracked as proposals (#35, #36, #37). |
| R9 | Docs drifting from implementation | Med | Low | Self-hosted check: `samples/` files are part of the test suite (DoD #6). |
| R11 | salsa API churn / version instability | Med | Med | Pin a version; wrap behind `fai-db` so the engine is swappable; keep query definitions framework-agnostic. |
| R12 | Incremental-cache correctness (stale results → wrong diagnostics) | Med | High | Incremental-vs-clean **verifier** in CI; content-addressed keys stamped with compiler version + flags; determinism is a locked invariant. |
| R13 | Span/position instability collapses incrementality | Med | High | Position-independent item tree + spans in a side-table; edit-churn test asserts "add a comment → near-zero recompute". |
| R14 | Daemon lifecycle: stale/version-mismatch, spawn races, memory growth | Med | Med | Version handshake + auto-restart; version-stamped socket path + spawn-lock; LRU eviction + idle-timeout shutdown. `stop`/`restart` are synchronous — they block until the prior daemon's endpoint refuses connections, so `restart` spawns a genuinely fresh daemon instead of reattaching to the one still shutting down. Windows lifecycle gaps tracked in #10, #29. |
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
- **D4 Effects:** capabilities as explicit values (interface instances from
  `main`); **no** type-level effect rows for now (tracked as a proposal, #35).
  Rationale: simple, auditable, implementable now; rows can layer on later.
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
  zero-arity binding — a value, not a function — is forced on reference).
  Primitives lower to runtime calls. Every operation **consumes** its operands,
  so RC insertion reduces to dup-at-use + one drop per owned binding (no reuse;
  precise reuse layered on later, D76–D79).
- **D52 Typed Core IR:** `fai-core` carries a `Ty` on every node, from a new
  `body_types` query, so the later record-field-offset work need not retrofit
  types — even though the thin-slice codegen leans on tagging and uses the types
  lightly.
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
- **D57 Daemon concurrency (serialized):** the daemon serves per-connection
  threads but serializes **all** database access through one `Mutex<Session>`
  (true serialization, sidestepping salsa's concurrent-read/cancellation
   machinery). Control messages and (later) `run` supervision stay off-lock.
   Concurrent reads + cancel-on-input-change are deferred (tracked in issue #17);
   the acceptance bar (warm speedup) needs only the warm DB.
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
  module **`Prelude`** is visible unqualified everywhere; with no opaque types
  yet, a type's constructors travel with it, so the core types are
  auto-imported. `Prelude` owns
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
- **D74 Intrinsics are prelude-private (`Prim.*`):** the Rust
  intrinsics are no longer bare names anywhere. They are reached only as
  `Prim.<name>`, and only from inside `std/` modules (`FAI2014` otherwise); the
  standard library re-exports the user-facing ones under clean qualified names
  (`Int.toString` wraps `Prim.intToString`, `String.split` wraps `Prim.split`,
  `Prelude.not` wraps `Prim.not`, …), adding one call of indirection per
  intrinsic (an inliner is tracked as a proposal, #40). New resolution
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
    ahead of the back-edge only when it is **pure and total** (no call, no integer
    division/remainder, no capability effect), preserving observable order. The
    recursion must flow *linearly* — used exactly once at each step, carried whole
    through each cell — so two self-calls in one constructor (`Node (f l) (f r)`), a
    non-tail self-call, or any other self-reference leaves the function as ordinary
    recursion.
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
    deep end-to-end runs (JIT and AOT) confirm constant stack and a leak-free exit.
    Mutually-recursive and non-last reorder-unsafe cases are noted as future
    generalizations.

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
    decrements the live-object counter (an `unsafe extern "C"` carrying the
    precondition the inlined drop establishes).
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

- **D104 Informational CI benchmark report (non-gating).** The wall-clock benches
  now run in CI, but only to **publish a report**, never to gate. A separate
  `Benchmarks` workflow (`.github/workflows/bench.yml`) runs `cargo bench` on
  `main` and on demand (a single Linux runner, `DIVAN_MAX_TIME` bounding the heavy
  cases), renders a Markdown summary onto the run page, and uploads the raw output
  plus a parsed `bench-results.json` as artifacts. The deterministic guard tests
  remain the **sole** performance gate (shared runners are too noisy to gate on
  timings); every other CI run still merely compiles the benches to prevent
  bitrot. Trend-over-time tracking is deliberately left to the artifacts (no
  gh-pages/threshold automation) to keep the anti-flakiness stance intact.
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

To change a locked decision: update this log **and** the table in `AGENTS.md`,
and note the migration in the affected decisions.
