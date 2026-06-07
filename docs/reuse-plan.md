# Reference-counting reuse & in-place update — detailed plan

This is the working implementation plan for the **reuse & in-place update**
milestone (M6 in `docs/PLAN.md`): turning the correctness-first reference
counting built with the native backend into competitive performance via
Perceus-style reuse, drop specialization, argument borrowing, and in-place
update. It records the locked design decisions, the staged build order, the core
algorithm, the file-by-file changes, and the test strategy. It is the reference
we execute against, stage by stage.

> Status: planning complete; implementation not yet started. Stages land one at a
> time, each leaving `main` green (`scripts/check.sh`), with a review gate after
> each. The first to land is Stage 1 (precise ownership RC).

---

## 1. Goal & acceptance

From the milestone definition:

- **Deliverables:** reuse analysis (reuse tokens), drop specialization, borrowing
  of arguments to avoid dup/drop churn; in-place reuse for same-size constructors
  and for `{ r with … }` when the refcount is 1.
- **Acceptance:** `map`/`filter`/`fold` over a *unique* list allocate ~zero fresh
  cells (measured); benchmarks show the expected reduction vs plain RC.
  Correctness unchanged: full suite green, zero leaks.

This milestone adds **zero surface syntax** — reuse, borrowing, and in-place
update are entirely internal to lowering, reference counting, the runtime, and
code generation. So there are **no `samples/` changes** and no new `FAInnnn`
codes (reuse cannot fail in a user-visible way). New `fai_*` runtime symbols
become part of the runtime ABI contract shared by `fai-codegen` and
`fai-runtime`.

---

## 2. Current state (the substrate)

- **`fai-rc`** implements the deliberately simple scheme: `Dup{x; x}` at *every*
  variable use, one `Drop` per owned binding (params + `let`s) at **scope end**,
  captures borrowed (dup-on-use, never dropped). Path-insensitive, self-balancing.
- **Data flow:** `core` (lower) → `rc` (dup/drop) → `object_code` (AOT cache),
  `jit_run`, and `build_run_bundle` (daemon-run path, via `wire`). Every backend
  consumer reads `rc(...)` output.
- **Runtime (`fai-runtime`):** heap header `{ rc, descriptor, size }`;
  `fai_dup`/`fai_drop` tag-checked; one `FAI_DATA_DESC` for all data with
  `data_scan` dropping fields by size; `fai_make_data` allocates; `fai_data_field`
  dups the field **and drops the base**; `fai_data_tag` reads the tag **and drops
  the base**; `fai_record_update` copies; a net `LIVE` counter powers the exit
  leak check.
- **Codegen (`fai-codegen/emit.rs`):** lowers `Dup`/`Drop`/`MakeData`/`DataField`/
  `DataTag` to runtime calls; `make_closure` dups each capture itself.
- **`wire`** mirrors the IR (incl. `Dup`/`Drop`) for the daemon-run worker and
  **drops node types** (rebuilt with placeholders).
- **Safety nets** that must stay green: RC-balance proptests at the runtime level
  (`fai-runtime/proptests.rs`), JIT level (`fai-codegen/proptests.rs`), and native
  e2e leak checks (`fai-tests/tests/native.rs`).

---

## 3. The key insight (drives the staging)

**Reuse requires precise (ownership-based) RC first.** `Drop{local, body}`
evaluates `body` then drops, and a reconstructing `MakeData` lives inside `body`,
so today's scope-end drop always frees the matched cell *after* the rebuild — too
late to reuse. The scrutinee's drop must **sink to its last use** (its field
projections), *before* the reconstruction. Hence precise RC is the foundation,
and reuse layers on top of it.

---

## 4. Locked decisions

These were resolved during design review. Changing one is a deliberate, documented
event (update this file and, as each stage lands, the decision log in
`docs/PLAN.md` — entries D76+).

1. **Full precise RC**, not targeted drop-sinking. Borrowing (Stage 4) needs
   precise ownership everywhere; a second scheme would double the correctness
   surface.
2. **Drop-early:** an owned binding's drop sinks to right after its last use (and
   before a tail call that doesn't use it); per-branch drops in `if` are
   mandatory.
3. **`MakeClosure` consumes its captures.** The capture dup moves *into the IR*
   (explicit `Dup` nodes emitted by `fai-rc`); codegen's `make_closure` stops
   dup-ing and just stores the captured values. One uniform "operations consume
   their operands" rule with no special case.
4. **Projections always borrow.** `DataField`/`DataTag` stop dropping their base
   (they still dup the field out); the base becomes an ordinary owned local that
   RC drops once at its last use. Net dup/drop behavior is preserved, just
   relocated from the projection into RC. Realised by **A-normal-form
   normalization in `fai-rc`** (see §6.1), which makes every operand atomic —
   subsuming the "ANF-bind temporary projection bases" requirement and giving the
   precise-RC rule a clean, verifiable form.
5. **Reset/reuse IR (Stage 2):** `ExprKind::Reset { value, token, body }` plus a
   `reuse: Option<LocalId>` field on `MakeData` (mirrors Koka's `Con@ru`). The
   token local is special: never dup'd or dropped by normal RC.
6. **Drop-pushing; no free-token node.** Reset only on paths that reconstruct;
   non-reconstructing branches plain-drop. A reset token is therefore *always*
   consumed by a construction, with no free-token op and no pessimization of
   non-reusing paths. Requires pushing a dead cell's handling into `if` branches.
7. **Same byte-size, any constructor** reuse: pair a reset cell with any
   `MakeData` of equal field count (statically matched; all data share
   `FAI_DATA_DESC`, and the cell carries its size). Covers map/filter (Cons→Cons)
   and same-shape ADT/record rebuilds. The reuse pass is **general** (any owned
   dead data value, including the monomorphic `{r with}` `let s = base` shape),
   greedy same-size pairing in evaluation order.
8. **Always-on `ALLOCATIONS` counter** (incremented only in `alloc_obj`, like
   `LIVE`) with accessors. Acceptance is a **differential in-process JIT test**:
   the same program over a unique list (~0 cons allocs) and over a shared list
   (~N allocs — proving the rc==1 dynamic guard makes shared data fall back to
   copy).
9. **`fai_record_update` in-place when rc==1**, else copy (no new symbol). The
   monomorphic `{r with}` gets in-place for free via the general reuse mechanism.
10. **Drop specialization (Stage 3):** a codegen-local `local→Ty` map (no IR/wire
    change), **bounded one-level** — omit drops for immediate-only types
    (`Bool`/`Unit`), unconditional decref for always-boxed leaves
    (`Float`/`String`), and a direct field-drop + free for known monomorphic
    data/records, falling back to generic `fai_drop` for composite/`Int`/
    polymorphic fields (bounds code size; safe on recursive types like `List`).
    Also specializes `Reset`'s child-scan. Degrades gracefully to generic drops on
    the type-stripped daemon-run path (still correct).
11. **Borrowing fixpoint (Stage 4):** acyclic-by-construction. Build the
    cross-module call graph (edges from `referenced_globals`), condense to SCCs
    (Tarjan, modeled on `fai-resolve/scc.rs`), and run the monotone borrow
    fixpoint within each SCC in plain Rust; `borrow_signature` is a salsa query
    keyed per-SCC, so every salsa query stays **acyclic** (preserving the repo's
    deliberate no-salsa-cycles invariant). Per-SCC early cutoff.
12. **Borrow ABI:** borrow only at direct `Global` call sites; indirect
    (`apply_n`) stays all-owned; a function that **escapes as a value** (referenced
    as a `Global` anywhere but a direct-call head) is forced **all-owned** so its
    convention is identical at every call site. `check` firewall untouched;
    `rc`/`object_code` re-run only when a callee's borrow *summary* changes (early
    cutoff).
13. **Field-sensitive deep borrowing:** a param is borrowable iff it *and every
    field projected from it* flow only to borrowed positions; owned if any
    field/the value is consumed (returned, stored in data/closure, passed to an
    owned position or `apply_n`, or reused). Lattice: start all-borrowed, demote
    to owned on a consuming use, fixpoint per SCC. "Owned-when-reused" falls out
    (reuse applies only to owned dead cells).
14. **Borrowing primitives** via a fixed **per-prim borrow table** (inspect-only:
    `Eq`, `Compare`, `stringContains`, `stringLength`, …); arithmetic/IO prims
    stay consuming. The runtime fns for borrowing prims stop dropping the borrowed
    operand; precise RC manages those operands (and eta-expanded prim wrappers
    stay consistent, since prims are never called via `apply_n`).
15. **Abstract RC interpreter** over the IR is the primary per-stage safety net
    (built incrementally: owned-only → reset/reuse → borrowing), complementing the
    runtime/JIT/native leak proptests.
16. **TRMC excluded** (tail-recursion-modulo-cons / destination-passing loop
    conversion). Reuse alone meets the allocation acceptance; TRMC is a separate,
    larger transform recorded as future work in §11.

---

## 5. Staged build order

Each stage passes `scripts/check.sh` (fmt, `clippy -D warnings`, build, test) and
adds incremental-verifier / early-cutoff coverage for any new query. Review gate
after each.

| Stage | Theme | Primary crates |
|---|---|---|
| 1 | Precise ownership RC (+ ANF, borrow seam, abstract checker) | `fai-rc`, `fai-runtime`, `fai-codegen` |
| 2 | Reset/reuse mechanism (+ alloc counter, differential test) | `fai-core`, `fai-rc`, `fai-runtime`, `fai-codegen` |
| 3 | In-place `{r with}` + drop specialization | `fai-runtime`, `fai-codegen` |
| 4 | Full borrowing inference | `fai-rc` (+ a borrow-inference module), `fai-runtime` |

---

## 6. Stage 1 — Precise ownership RC

Pure optimization: no new surface, no new IR shapes. Rewrites `fai-rc` from the
simple scheme to precise ownership-based dup/drop, makes projections borrow, and
moves capture dups into the IR. Establishes the **borrow seam** (a
`borrow_signature` provider that returns all-owned in Stage 1) so Stage 4 only
swaps in the real analysis.

### 6.1 A-normal form (in `fai-rc`)

`fai-rc` first normalizes each lowered function to ANF: every operand of an
operation (`Prim` args, `App` func+args, `MakeData` args, `DataField`/`DataTag`
base, `If` cond) becomes an **atom** (`Local`/`Lit`/`Global`), binding compound
operands to fresh `let`s. Flat expressions stay flat; only *nested* operations
gain a `let` (e.g. `f (g x)` → `let t = g x in f t`; `runtime.console.writeLine`
→ `let t1 = field1 runtime in let t2 = field0 t1 in app t2 "Hi"`).

Why ANF: with atomic operands, sequence points are explicit, so the precise-RC
rule is the clean textbook one and the borrowed-base drop placement is
unambiguous. ANF also makes projection bases `Local`s, subsuming the
"ANF-bind temporary projection bases" decision (done here rather than in lowering,
keeping all RC-enabling normalization in one place; observable semantics are
identical). Fresh locals are allocated past `first_free_local`.

### 6.2 Ownership model

- **Owned** vars: parameters (incl. leading offset-evidence params) and `let`
  bindings. Each must be **consumed exactly once or dropped exactly once on every
  path**.
- **Borrowed** vars: a lifted function's captures. Used by dup-on-use (the env
  owns them; `closure_scan` releases them at closure death); never dropped by the
  body.
- **Borrow positions** (Stage 1): `DataField`/`DataTag` base, and a `DataField`
  `Dyn { evidence }` evidence local. A borrow reads through a live reference
  without taking ownership.
- **Consume positions:** everything else (`App` func+args, `Prim` args, `MakeData`
  args, `MakeClosure` captures, a `Let` value, an `If`/function result).

### 6.3 The transform (clean rule on ANF)

For each operation `O` with atomic operands and continuation `cont`:

- **Consume of `Local(x)`** at operand position *i*: insert `dup x` before `O`
  iff `x` is needed afterward — i.e. `x` is consumed again at a later operand
  `j > i` of `O`, **or** `x` is used (consume *or* borrow) anywhere in `cont`.
  (Operations drop their consumed operands at the operation point, which is a
  sequence point before `cont`; a borrow *within the same `O`* reads before that
  drop, so it does not force a dup.)
- **Borrow of `Local(x)`** (projection base / evidence): never dup.
- **Captured `Local(x)`** in a lifted fn (borrowed var): always dup on use; never
  drop.
- **Drop placement (drop-early):** an owned var is dropped immediately after its
  last use when that last use is a *borrow* (or it is unused); when the last use
  is a *consume*, the consuming operation performs the drop (no explicit drop).
  Concretely:
  - `Let{x, value, body}`: bind `x`; if `x ∉ FV_owned(body)`, drop `x` at body
    start; drop at the body seam any owned var that `value` borrowed and that is
    dead in `body` (the match-scrutinee case — a projection whose base dies drops
    the base right after the projection, exactly where Stage 2 inserts a reset).
  - `If{cond, then, els}`: `cond` is transformed preserving the branches' needs;
    each branch drops, at its start, the owned vars live entering the branches but
    not used by that branch and not live after the `if`. A var whose last use is a
    *borrow inside a branch* is dropped within that branch at the borrow.
  - Function entry: unused params (`∉ FV_owned(body)`) are dropped up front.
- **`MakeClosure{func, captures}`:** captures are consumed into the env. Emit
  `Dup{c; …}` before the `MakeClosure` for each capture that is a borrowed var, or
  that is needed afterward (captured again later / used in `cont`); otherwise the
  capture's owned reference transfers into the env with no dup.

The **borrow seam**: the consume/borrow classification of *call arguments* and
*prim operands* comes from a `borrow_signature(callee)` provider. In Stage 1 it
returns all-owned (every argument consumed, matching today), so the only borrows
are projection bases and evidence. Stage 4 fills it in.

### 6.4 Runtime changes (`fai-runtime`)

- `fai_data_field(v, index)`: dup the field, **do not drop `v`** (borrow); return
  the field.
- `fai_data_tag(v)`: read and return the tag immediate, **do not drop `v`**.
- Update the runtime unit tests / proptests that relied on these dropping the
  base (they must drop the base themselves now).

### 6.5 Codegen changes (`fai-codegen/emit.rs`)

- `make_closure`: stop calling `fai_dup` on captures; store the (already-dup'd by
  RC) capture values directly into the env.
- `DataField`/`DataTag` emission is unchanged (the runtime fns now borrow).

### 6.6 Tests for Stage 1

- **Abstract RC interpreter** (`fai-rc`, owned-only model): walks each rc'd
  function on all paths, tracking each owned var as owned-live / consumed /
  dropped, with borrow positions non-consuming and captures never dropped.
  Asserts: every owned binding consumed-or-dropped exactly once per path; no
  use-after-consume; no double drop; captures never dropped. Run over the existing
  generators plus the corpus. Pinpoints the offending node/path on failure.
- **Rewrite the `fai-rc` snapshot tests** to the precise-RC shapes (e.g.
  `id x = x` → `%0`; `add x y = x + y` → `(+ %0 %1)`; `k x y = x` →
  `(drop %1; %0)`).
- **Replace `assert_rc_invariants`** (which encoded the plain scheme) with the
  precise-RC invariants checked by the abstract interpreter.
- Keep all runtime/JIT/native leak proptests green (live count returns to
  baseline). Update any codegen/native snapshots that change shape; verify
  behavior (stdout, exit code, zero leaks) is unchanged.

### 6.7 Stage 1 acceptance

`scripts/check.sh` green; abstract checker passes over generators + corpus; leak
proptests and native e2e unchanged in behavior; visibly less dup/drop traffic in
`benches/data_layer.rs` / `benches/codegen.rs` (local profiling).

---

## 7. Stage 2 — Reset/reuse mechanism

### 7.1 IR (`fai-core/ir.rs`, `pretty.rs`, `wire.rs`)

- Add `ExprKind::Reset { value: Box<CExpr>, token: LocalId, body: Box<CExpr> }`.
- Add `reuse: Option<LocalId>` to `ExprKind::MakeData`.
- Thread both through every exhaustive `ExprKind` match: `collect_globals` and
  `collect_free` (`ir.rs`/`lower.rs`), `pretty.rs`, `wire.rs` (the `WireExpr`
  enum + both conversions), `fai-rc` (producer), `fai-codegen/emit.rs` (consumer).
  All ~5 existing `MakeData` literals pass `reuse: None`.

### 7.2 Runtime (`fai-runtime`)

- `fai_drop_reuse(v) -> token`: immediate → `0`; else `rc -= 1`; if it hit zero,
  run the descriptor child-scan (release children) and **return the pointer
  without freeing** (no `LIVE` decrement); if still shared, return `0`.
- `fai_reuse(token, tag, nfields, fields) -> Value`: `token == 0` → `fai_make_data`
  (fresh; `LIVE++`, `ALLOCATIONS++`); else write tag + fields in place, set
  `rc = 1` (size guaranteed by the static same-size pairing; `debug_assert` it),
  return the cell (no `LIVE`/`ALLOCATIONS` change).
- Add the always-on `ALLOCATIONS` atomic in `alloc_obj` with
  `allocations()` / `reset_allocations()` accessors.
- Register the new symbols in the JIT symbol table (`fai-codegen/jit.rs`) and the
  AOT path.

### 7.3 Reuse pass (`fai-rc`, after precise RC)

Operates on the precise-RC output. For an owned data value that dies at point `P`
(a drop-early `Drop`, e.g. the match scrutinee after its last projection):

- If a same-byte-size `MakeData` follows on `P`'s path, rewrite the `Drop` into a
  `Reset` binding a token and attach the token to that `MakeData` (greedy, first
  in evaluation order). Multiple dead cells → multiple tokens.
- **Drop-pushing:** when the dead cell is followed by an `if`, push its handling
  into the branches — branches that reconstruct a same-size value reset+reuse;
  branches that don't keep the plain drop. A reset token is thus always consumed
  on its path.
- The general shape (any owned dead data value + a following same-size
  construction) also covers the monomorphic `{r with}` `let s = base in MakeData`.

### 7.4 Codegen (`fai-codegen/emit.rs`)

- `Reset` → `fai_drop_reuse`, binding the token local.
- `MakeData{reuse: Some(tok)}` → `fai_reuse(tok, …)`; `reuse: None` →
  `fai_make_data` as today.

### 7.5 Tests / acceptance

- Extend the abstract checker: a reset token is created once and consumed once on
  every path; the reset value is otherwise dead.
- **Differential allocation test** (in-process JIT): `List.map`/`filter` over a
  unique list of immediates allocate ~0 cons cells; with a second reference held,
  ~N (proving the rc==1 fallback to copy). Snapshot the reuse IR for map/filter.
- Leak proptests/native e2e remain green; `wire` round-trip (`bundle.rs`) covers
  the new nodes.

---

## 8. Stage 3 — In-place `{r with}` + drop specialization

### 8.1 In-place row-polymorphic update (`fai-runtime`)

- `fai_record_update(record, index, value)`: if `record` is unique (rc==1), drop
  the old field at the slot, write `value`, return the same cell (no alloc, no dup
  of the other fields); if shared, copy as today. No new symbol, no IR change.
- The monomorphic `{r with}` already gets in-place via Stage 2; add a test. The
  differential alloc test covers both.

### 8.2 Drop specialization (`fai-codegen`)

- Build a `local→Ty` map from the lowered `CExpr` types during translation (no
  IR/wire change).
- At a drop site of known monomorphic shape: omit for immediate-only types
  (`Bool`/`Unit`); unconditional decref for always-boxed leaves
  (`Float`/`String`); direct field-drop + free for known data/records — **one
  level**, calling generic `fai_drop` for composite/`Int`/polymorphic fields
  (bounds code size; safe on recursive types). Also specialize `Reset`'s
  child-scan.
- Falls back to generic `fai_drop` when the local's type is a placeholder (the
  daemon-run wire path) — still correct.

### 8.3 Tests / acceptance

Behavioral equivalence (stdout, exit, zero leaks) under specialization; benches
show fewer/cheaper drops; the in-place `{r with}` differential alloc test passes.

---

## 9. Stage 4 — Full borrowing inference

### 9.1 Call graph & SCCs (`fai-rc` borrow module)

- Build the cross-module call graph from `core(...).referenced_globals()` (the
  edges `reachable_defs` already follows).
- Condense to SCCs with Tarjan (modeled on `fai-resolve/scc.rs`).
- An **escape set**: a `Global` referenced anywhere but a direct-call head escapes
  as a value.

### 9.2 `borrow_signature` query

- `BorrowSig` = a per-parameter owned/borrowed bitvector (by position; leading
  evidence params are immediates → mark owned, immaterial).
- Per-SCC salsa query: monotone fixpoint in plain Rust (start all-borrowed; demote
  a param to owned on a consuming use of it or a field projected from it — field
  sensitive; using callees' `BorrowSig`s and the per-prim borrow table; an
  escaping function is forced all-owned). Acyclic salsa graph (condensation is a
  DAG) → per-SCC early cutoff.

### 9.3 Wire-in

- `rc(def)` consults `borrow_signature(def)` for its own params (borrowed params
  are not dropped by the body; their projections inspect-only) and
  `borrow_signature(callee)` / the prim table at direct call sites (a borrowed
  argument is not dup'd before the call and is dropped later at its own last use).
- Borrowing is reflected purely in where dup/drops sit in the `rc` IR, so the
  daemon-run `wire` path inherits it for free; the function ABI is unchanged.
- Borrowing prims: the inspect-only prim runtime fns stop dropping the borrowed
  operand; update their unit tests/proptests to drop operands themselves.

### 9.4 Tests / acceptance

- Abstract checker extended with borrow signatures (a borrowed arg/operand is not
  consumed).
- **Borrow-firewall guard** (incremental verifier / query-count): a callee body
  edit that preserves its borrow summary does not recompute caller `object_code`;
  a summary-changing edit recomputes exactly the callers.
- Benches show dup/drop churn reduction (e.g. `length`/`sum`/compare-heavy
  recursion borrow their structure); leak proptests/native e2e green.

---

## 10. Cross-cutting obligations

- **Determinism:** borrow inference is a monotone fixpoint over a fixed lattice
  with deterministic SCC order; reuse pairing is deterministic (evaluation order).
- **Firewall:** borrow signatures feed only `rc`/`object_code` (downstream of
  `check`); they are small, early-cutoff summaries. The `check` firewall is
  untouched.
- **Docs:** as each stage lands, update `AGENTS.md` (status header + the
  `fai-rc`/memory rows) and `docs/PLAN.md` (M6 status + decision log D76+),
  describing behavior, never milestone/decision ids (the enforced rule). New
  `fai_*` symbols are documented as part of the runtime ABI.
- **Testing standards:** every new query gets incremental-vs-clean + edit-churn
  coverage; every bug fix ships a regression test; snapshots reviewed by hand.

---

## 11. Out of scope (future work)

- **Tail-recursion-modulo-cons (TRMC)** / destination-passing loop conversion:
  flattening a self-tail-recursive constructor-returning function into an
  in-place-building loop (O(1) stack). Reuse alone meets this milestone's
  allocation acceptance (a recursive `map` over a unique list allocates zero fresh
  cells, with N reset tokens live on the stack). TRMC additionally removes the
  stack growth and improves locality; it is a substantial separate transform to
  be taken up later as its own optimization.
- Cross-call reuse tokens (tokens never cross a function boundary).
- Borrowing through indirect (`apply_n`) calls and two-entry-point wrappers for
  escaping borrowing functions.

---

## 12. Risk register (milestone-specific)

| Risk | Mitigation |
|---|---|
| RC correctness (leaks / double-free), elevated by precise RC + reuse + closures/existentials | Stage 1 lands behind the full leak-proptest suite *before* any reuse; the abstract IR checker localizes failures; reset's rc==1 check makes shared data fall back to copy. |
| Blast radius of new `ExprKind`s | Compiler exhaustiveness + a grep sweep across lower/pretty/wire/codegen/rc/collect-free. |
| Borrow inference vs the incremental firewall | `borrow_signature` is a per-SCC early-cutoff summary feeding only `rc`/codegen; a dedicated firewall guard in CI. |
| First cross-module SCC machinery | Modeled on the existing `fai-resolve/scc.rs`; condensation keeps all salsa queries acyclic. |
| Borrowing × reuse interaction (a reused param must stay owned) | Field-sensitive lattice treats reuse/consumption as forcing owned; reuse applies only to owned dead cells; reuse (Stages 2–3) lands before borrowing (Stage 4). |
| Daemon-run path strips types (no drop specialization there) | Specialization degrades to correct generic drops on the wire path; AOT/tests/benches retain types. |
