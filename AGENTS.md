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
> `--no-daemon` runs in-process. Read commands are served **concurrently** on
> cloned database snapshots (the lock is held only to sync inputs and clone a
> snapshot, not to run the command), and an input change **cancels** in-flight
> reads, which retry on the new revision. The data layer (M4) is built:
> **discriminated
> unions and transparent type aliases, `match` with exhaustiveness/redundancy
> checking, structural records with row polymorphism, a native `Float`, and
> structural ordering** — all compiling to native code (monomorphic records use
> constant-offset projections; *row-polymorphic* field access and `{ r with … }`
> update compile via **offset-evidence passing** — integer field offsets threaded
> in as leading arguments, like dictionaries). **Opaque types** are built: a
> **`public opaque type`** exports a type's name but not its definition (a union's
> constructors, an alias's representation), **file-scoped** — transparent within
> its declaring file, abstract elsewhere (named, held, passed, and compared
> structurally, but not constructed, deconstructed, or seen through) — so `Dict`
> and `Set` now hide their node constructors (declared `opaque` in their own
> modules, with `Prelude` re-exporting the names via transparent aliases).
> Interfaces & capabilities (M5) are
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
> modules (`List`, `Array`, `Option`, `Result`, `Dict`, `Set`, `String`, `Int`,
> `Float`, `Char` — e.g. `List.map`, `Array.map`, `Int.toString`). The few Rust
> intrinsics are prelude-private, reached only as `Prim.*` inside `std/`; a
> **saturated call to one of these clean re-exports is inlined to its primitive**
> (the intrinsic inliner, run before reference counting), so the wrapper adds no
> call of indirection at a use site. A
> contiguous, growable **`Array 'a`** (Vector-style: O(1) index, in-place update
> when uniquely owned via Perceus, an unstable in-place quicksort) complements the
> linked `List`, built on five array intrinsics with the rest pure Fai and written
> with `[| 1, 2, 3 |]` literals; **`Array Float` stores its elements as raw, inline
> `f64`s** (the buffer self-tags on the first float `push`, so generic construction
> needs no evidence), so a concrete index loop reads and writes the raw slots with
 > no per-element box and drops in O(1) — a generic (type-variable element) access
> re-boxes at the boundary (the no-monomorphization ceiling). A monomorphic,
> **fixed-shape float aggregate** (a `(Float, Float)` tuple, or a closed record of
> all-`Float` fields up to eight wide, e.g. `Vec2 = { x : Float, y : Float }`) that
> does not escape is **scalar-replaced**: held as its `f64` components in registers
> and returned via a multi-result signature (no heap cell), reassembled into the
> in-cell `f64`-slot layout only where it crosses a uniform/generic/first-class
> boundary — so vector/matrix algebra and `(Float, Float)`-returning helpers compute
> allocation-free, while an escaping or heap-resident aggregate keeps the boxed
> cell (loop-carried aggregate state and nested float aggregates stay boxed for
> now). Reuse & in-place update (M6) are built:
> reference counting is **precise and ownership-based** (A-normal form, drop at
> last use, borrowing projections), a dead data cell is **reset and reused** in
> place for a same-size construction (so `map`/`filter` over a unique list
> allocate zero fresh cells, falling back to copying when shared). The reset is
> placed at the cell's death point — **before** a recursive call bound in a `let`
> when the reconstruction lives in a following branch — so a "recurse, then
> rebalance" function (a balanced-tree `insert`/`remove`) rebuilds a uniquely-owned
> search path in place (an O(n) build, not O(n log n)); the reuse token is threaded
> to each branch's construction and **freed** on a branch that builds nothing. So
> that factored code reuses too, a **general helper inliner** (`helper_inlined`,
> layered on the intrinsic prim inliner) folds **small, non-recursive, intra-file
> helpers** into their callers before reference counting (transitively, binding each
> argument so representation is coerced as at a call boundary; non-recursive callees
> are found via a full intra-file reference-graph SCC analysis, so the inlined graph
> stays a cycle-free DAG) — so `Dict`/`Set` `insert`/`remove`, written through a
> `bin` smart constructor (and `singleton`/all-`bin` `balance`) rather than
> hand-inlined nodes, still recycle a unique tree's cells in place, while the larger
> rotating `balance` stays a shared call. **Inter-procedural reuse-token passing**
> then lets a freed cell cross that call: a function records, per definition, the
> size-classed reuse tokens it can consume (its `reuse_signature`, a monotone
> fixpoint over the call graph with early cutoff, like borrow inference) and gets a
> **token-taking specialized entry** `{base}__reuse`; a caller forwards a cell it
> freed but could not recycle locally into such a call (passing the token in a
> leading register, with a runtime null/wrong-size fallback). So `insert`/`remove`
> hand the matched search-path node they free to `balance`, which recycles it — a
> rotation-heavy unique build now allocates one cell per entry rather than ~3. The
> specialized entry is a separate object linked only where a reachable caller
> forwards to it, so the per-definition primary object (the cache firewall) is
> unchanged. `{ r with … }`
 > updates a unique record in place, and **argument borrowing** lends
> inspect-only parameters at direct calls (with an owned-ABI wrapper for the
> first-class value form). Borrow inference is **inter-procedural**: a parameter
> only forwarded to another function's borrowing parameter is itself borrowed
> (a borrow fixpoint over the call graph, across modules). Direct calls use a
> **register calling convention**: a direct-callable definition
> (non-row-polymorphic, ≥1 parameter) has a register-passing entry
> `fn(env, a0, …, aN)` (a scalar `Float` in an `f64` register), a saturated call
> passes its arguments in registers, an over-application direct-calls the saturated
> prefix and `apply_n`s the rest, and a `let g = f` function alias is
> copy-propagated to a direct call; the first-class form keeps the uniform
> spilled-array ABI via the wrapper, and row-polymorphic/nullary entries stay
> uniform (proper tail calls are future work). **Combinator pipelines are
> deforested**: a maximal chain of *directly-nested* standard combinators — a
> producer (`range`/`Array.init`/`Array.repeat`/a list-or-array literal),
> transformers (`map`/`filter`), and a consumer (`foldl`/`foldr`/`sum`/`length`/
> `all`/`any`/`find`/`member` or a terminal `map`/`filter` builder), for `List`
> and `Array` — is recognized **by resolved symbol identity** (just before
> reference counting, so editing a combinator's body never changes what fuses) and
> rewritten to **one synthesized self-tail-recursive loop** that materializes no
> intermediate sequence (a small literal is **unrolled** to straight-line code
> instead). The loop is reference-counted and tail-flattened by the ordinary back
> end (so a unique producer is still recycled and it runs in constant stack), takes
> the raw-scalar register ABI, and **inlines a literal element lambda** (so
> `Array.sum (Array.map (fun x -> x*2) (Array.range 0 n))` becomes a zero-alloc,
> zero-dispatch register loop); a non-literal element function (e.g. a composed
> `transform`) is still applied via `apply_n` (the residual dispatch is separate
> work). It is **behavior-preserving** — only directly-nested intermediates fuse
> (a shared/`let`-bound sequence stays materialized, walked by the loop) and a
> stage fuses only when its element function is **pure** (an effectful stage is a
> barrier), so cross-stage reordering is unobservable. The synthesized loops are
> emitted like the mutual-recursion combined loop (the driver gathers and
> code-generates them across the AOT/JIT/bundle/contract paths); fusion is skipped
> inside the standard library itself (so the combinators stay tested by their own
> contracts). **Composed and partially-applied closures are confined** by a local
> reduction pass run just before reference counting: a point-free value built from
> `>>`/`|>`/`identity`/`const` and partial application — including one bound at a
> top-level value (a *constant applicative form* like
> `transform = (fun x -> x + 1) >> shift 3`, which, being unmemoized, would
> otherwise be *rebuilt* at every use) — is reduced by inlining the value, reducing
> the compositions (`(f >> g) x → g (f x)`, recognized **by resolved identity**, so
> a body edit or a shadowed operator never changes what reduces), flattening curried
> partial applications into saturated direct calls, and beta-reducing applied
> lambdas. The reordered operands of a composition must be **pure** (so an effectful
> composition is left intact), CAF inlining is intra-file, and the pass is skipped
> in the standard library — so the residual same-file helpers fold via the inliner
> and a now-arithmetic pipeline deforests into a register loop (the
> closure-construction-and-first-class-call workload becomes a zero-allocation
> loop). Contracts (M7) are
> built: **`fai test` runs the first-class `example`/`forall` declarations**, and
> **`fai check` eagerly evaluates the closed `example`s** (reporting a failing one
> as the located `FAI6001` without a separate `fai test`, in the same isolated
> worker; `--no-examples` opts out, and the language server does it on save). The
> property-testing framework is
> **dogfooded in the standard library** (`std/Test.fai`: a pure splitmix64 `Gen`,
> an `Arbitrary 'a` bundle of generator/shrinker/renderer, type-directed
> combinators, and the `checkExample`/`checkForall` driver with shrinking); the
 > compiler synthesizes, per contract, a harness that composes those combinators
> for the binders' (monomorphized) types and **checks it in a supervised isolated
> worker** — the same machinery as `fai run` — so a generated input that drives a
> body into a runtime trap (e.g. division by zero) fails *that* contract as a
> located **`FAI6003`** and the run resumes after it instead of aborting; the
> **daemon serves `fai test`**, streaming per-contract `$/testEvent`s (warm output
> equals `--no-daemon`). A failure is a located **`FAI6001`** with a shrunk
> counterexample (an ungeneratable binder is **`FAI6002`**). User **records and
> ADTs** (including
> recursive ones like `Dict`/`Set`/`Tree`) generate too, via a synthesized
> top-level `Arbitrary` definition per type (a recursive type is a self-reference;
> every synthesized function is capture-free, closing over values by partial
> application). The **size budget is consumed as node fuel** — split across a
> constructor's recursive fields (a recursive list splits again across its
> elements) — so **mutually-recursive types and recursion through a collection
> field** (e.g. `Rose (List Rose)`) generate without blowing up, the base case is
> the **minimal-rank** constructor, a type with no finite value is reported
> **`FAI6005`**, and a user-supplied **`Arbitrary T`** in the contract's module
> **overrides** the synthesized generator for a user record/ADT (an ambiguous
> override is **`FAI6006`**). (Splitmix needs **bitwise `Int`
> intrinsics** — `Int.and/or/xor/complement/shiftLeft/shiftRight/shiftRightLogical`
> — and float bit-reinterpretation needs `Float.fromBits`/`toBits`, both added
> as part of this work. The default `Float` generator is **finite and
> size-bounded** (never NaN/inf); an opt-in `Test.floatAll` covers the full
> IEEE-754 domain.) A standard **language server** (`fai lsp`, the `fai-lsp`
> crate) is built: it reuses the warm session and the `fai-ide` engine to serve
> diagnostics, hover (type, `///` docs, and contracts), go-to-definition,
> completion (whose chosen item resolves its `///` docs and contracts lazily via
> `completionItem/resolve`), signature help, document & workspace symbols,
> find-references,
> rename, quick fixes (apply a diagnostic's suggested edit, add a missing public
> signature, qualify an unbound name), inlay hints (inferred binder types),
> semantic tokens, and document formatting over stdio, backed by offset-addressed
> code-intelligence queries (`hover`/`definition`, context-aware `completion`,
> signature help, the `references` reverse lookup that also drives rename, code
> actions, inlay hints, semantic tokens, and the symbol outline). Editing is
> incremental (range sync + `didSave`), with client position-encoding negotiation
> (UTF-8/UTF-16), range and on-type formatting (a newline reformats the construct
> just completed), and diagnostics re-published for every open file so a
> cross-module edit refreshes its dependents. **Type-level effect rows** over the
> capability model are built: every arrow carries an effect row of the
> host-capability interfaces applying it *uses* (`a -> b / { Console | 'e }`; a
> bare arrow is pure), inferred and **required on every public signature** (a
> binding that performs an undeclared capability — or declares an unused one — is
> **`FAI5001`**), so a function's reach is visible in its type. Effects close the
> closure-laundering hole (a captured capability rides the closure's arrow),
> propagate through the standard combinators (`List.map`, `>>`, …), and are erased
 > at runtime. Effect subsumption is **deep**: at a function application an
> argument is related to its parameter by a directional `⊆` that recurses with
> variance — **covariant** under arrow results, tuples, records, list elements,
> and an interface's effect argument, and **contravariant** under arrow parameters
> — so a less-effectful function is accepted where a more-effectful one is expected
> at any depth, and arguments flowing into one shared effect variable **union**
> their effects (point-free composition of differently-effecting functions,
> `consoleFn >> clockFn : … / { Console, Clock }`). The non-effect structure still
> unifies, and every *non-argument* position unifies strictly (mixed `if`/`match`
> branches are not laundered; an effectful function is still rejected where a pure
> one is required). **Effect-parameterized interfaces** are built: an interface
> parameter used after `/` is an **effect** parameter (`interface Logger 'e = log :
> String -> Unit / 'e`), inferred from use (a parameter used as both a type and an
> effect is **`FAI3019`**); an instance forwards its body's effect into that
> parameter (a console-backed logger has type `Logger { Console }`, written with
> effect-row braces, the polymorphic form `Logger 'e` needing no new syntax),
> dispatch incurs the value's effect argument, and an instance method that
> performs an effect its declaration does not admit is rejected (closing the
> dictionary-laundering hole). **Hash-based associative containers** are built:
> alongside the ordered `Dict`/`Set` BSTs, an unordered **`HashDict`/`HashSet`**
> give O(1)-average lookup/insert/membership over a **structural hash primitive**
> (`Prim.hash` → the runtime's `fai_hash`, the peer of `Prim.compare`, agreeing
> with structural equality so equal values hash equally). They are flat
> **open-addressing** tables (a power-of-two `Array` of slots, linear probing,
> backward-shift deletion) that keep value semantics like `Array` — copy-on-share
> with in-place update when uniquely owned, so a threaded build mutates the backing
> array in place and allocates only the per-entry cell — with unspecified (but
> deterministic) iteration order; the ordered `Dict`/`Set` remain for sorted
> iteration and range use. `Prelude` re-exports the names so signatures use them
> unqualified, and the associative-container benchmarks (`fib_memo`,
> `coin_change`, `set_dedup`, `dict_histogram`, `graph_bfs`, `union_find`,
> `option_path`, `game_of_life`) run on them. Later
> milestones (performance tuning at scale, …) define the *intended* interface we
> build toward. The design is locked (see the decision table below).

This document is the orientation guide for anyone — human or AI agent — working
on the Fai compiler. Read it first. For the design rationale (locked decisions
and standing risks) see `docs/MEMORY.md`; remaining and proposed work lives in
the issue tracker; for the language by example see the `samples/` directory.

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
table **and** the decision log in `docs/MEMORY.md`).

| Area | Decision |
|---|---|
| Family | Strict, **pure**, statically typed functional (ML/F#/Elm) |
| OOP | None, except **interfaces** (sets of function signatures); **interface instances** `{ Name with ... }` are the only constructor (→ existentials) |
| Modules | One top-level module per file; **nested modules** (`module Name = …`) group declarations and are addressed by a qualified path. Within a file the enclosing module sees *every* nested member (`Inner.name`); across files only `public` members are visible (`Outer.Inner.name`). Bare names resolve outward lexically (inner shadows outer) |
| Qualified names | A cross-module/nested member is reached by a dotted path: a **value/constructor** as `Module.name` / `Outer.Inner.name` (a field-access chain), and a **type/interface** as the same dotted form in type position (`Module.Type`, `Outer.Inner.Type`). Identifiers cannot contain `.`, so a qualified name is one interned dotted symbol |
| Public API | Every `public` binding **requires an explicit type signature** (Haskell-style, on its own line above the definition) |
| Recursion | Module-level bindings are **mutually recursive** (no `rec` keyword) |
| Layout | **Indentation-significant** (offside rule); `fai fmt` pins exactly one canonical layout (2-space indent, no tabs) |
| Type variables | F#-style leading tick: `'a`, `'k 'v` |
| Equality | `=` (equal) / `<>` (not equal), structural; undefined on function-typed values (→ an `Eq` operator method at M5; see Operators). An operand that is or *may be* an immediate at runtime — an immediate/`Int` type, or a type variable / nullary-bearing union / `List` — compiles to an **inline** word compare guarding the runtime `fai_equal` fallback. A **monomorphic, fixed-shape tuple or closed record of immediate/`Int` fields** (no direct `Float` field) compiles to an **inline, short-circuiting field-wise** compare in layout order, rather than one `fai_equal` over the boxed aggregate (each `Int` field keeps the immediate guard; a boxed/nested field uses the borrowing structural call); only a `Float`-bearing or otherwise-not-fixed-shape always-boxed operand calls unconditionally |
| Ordering | `< <= > >=` are **structural** over any non-function type (a runtime `compare`; constructor tags order by declaration, records by sorted label); undefined on functions. Generalizes like equality (→ an `Ord` operator method at M5; see Operators). Same inline fast paths as equality — a polymorphic comparison whose runtime value is an immediate (e.g. an `Int` key in a generic `Dict`) is an inline compare, and a fixed-shape tuple/record of immediate/`Int` fields is an inline lexicographic field compare, not a call. The structural **hash** (`Prim.hash`, backing the hash containers) has the same inline fast path: an immediate/`Int`/`Float` (or possibly-immediate) operand hashes inline (the splitmix64 finalizer on the payload), so an `Int` key hashes without a call |
| Arithmetic | `+ - * /` **overloaded over `Int`/`Float`** (F#-style); unconstrained numeric type **defaults to `Int`**; **no implicit `Int`/`Float` coercion** (use `Int.toFloat`/`Float.toInt`) (→ `Num` operator methods at M5; see Operators) |
| Operators | **Symbolic identifiers** with **F#-style precedence** (derived from the operator's symbols; no fixity declarations); written infix, named as `(op)`. Built-in operators are **std interface methods** — `Num` (`+ - * / %`), `Eq` (`= <>`), `Ord` (`< <= > >=`) — defined in `Prelude`; **user-defined operators** resolve like names (module-local + `Prelude`). `&&`/`\|\|` stay short-circuit sugar; `::` is the built-in `List` constructor. *(Built at M5.)* |
| Comments | `//` line, `(* ... *)` block, `///` doc |
| Misc syntax | `[1, 2, 3]` lists, `::` cons, `List 'a`; `[\| 1, 2, 3 \|]` array literals (`Array 'a`; expression-only, no array patterns); `\|>`, `>>`, `++`; `true`/`false`; `if/then/else`; 64-bit `Int`/`Float` |
| Sequences | Two sequence types: the linked **`List`** (head/tail recursion, pattern matching, `::`/`[…]`) and the contiguous, growable **`Array`** (`Array 'a`, O(1) index, in-place update when unique — Vector-style semantics, an array name per the ML family, contiguous + Perceus rather than Haskell's ST/freeze or Elm's RRB tree). `Array` is a built-in `Con` (global like `List`, no import); its API mirrors `List` (collection-last) with safe total `get`/`set : … -> Option` plus partial `unsafeGet`/`unsafeSet` (out-of-bounds aborts like `/`). Built on five Rust intrinsics (`Prim.array{WithCapacity,Length,Get,Set,Push}`), the rest pure Fai. A boxed element is one uniform slot word; **`Array Float` stores its elements as raw, inline `f64`s** (self-tagged at runtime, so generic construction needs no evidence) — concrete index loops read/write the raw slots with no per-element box, while a generic (type-variable element) access re-boxes at the boundary (the no-monomorphization ceiling). An unboxed `Array Int` is unnecessary (a small `Int` is already an inline immediate) |
| Algebraic types | Discriminated unions (`type T = \| A \| B 'a`; the leading `\|` is optional — `type T = A \| B` is the same union, and `fai fmt` adds it — but a single nullary variant still needs it, else `type T = A` is an alias); transparent type aliases (`type Id = …`, acyclic). A `public opaque type` exports the name but not the definition (see Opacity) |
| Tuples | **Structural**; values `(a, b)`, type `'a * 'b` (`*` binds tighter than `->`) |
| Records | **Structural with row polymorphism**; no duplicate labels (lacks constraints); `{ x = 1.0, y = 2.0 }`; dot access; `{ r with ... }` update; field punning in patterns; `type Point = { ... }` is a **transparent alias** (unless `opaque`); **closed by default** `{ x : T }`, anonymous-open `{ x : T \| _ }`, named-open `{ x : T \| 'r }` (named only to thread the tail to the result); **patterns mirror this** — `{ ... }` closed (names all fields), `{ ... \| _ }` open (ignore rest; required for row-poly scrutinees); extension/restriction (incl. binding a pattern tail) is future work (tracked as a proposal) |
| Opacity | A **`public opaque type`** exports the type's name but **not** its definition — a union's constructors / an alias's underlying type. **File-scoped**: transparent within its declaring file (build, match, project freely), abstract everywhere else (named, held, passed, compared structurally, but not constructed, deconstructed, or seen through). `opaque` requires `public`. Cross-file use of a hidden constructor is **`FAI2018`**; of a hidden representation (field access, record construction, `{ r with … }`) is **`FAI3018`**. `Dict`/`Set`/`HashDict`/`HashSet` use this to hide their node constructors |
| Inference | Hindley–Milner + let-generalization + **rows / row unification / lacks constraints** + **effect rows** (a parallel effect-row union-find over arrows, strict by default with the signature-vs-body check lenient); exhaustiveness checking for `match` |
| Generics | **Uniform boxed representation + dictionary passing** (no monomorphization by default) |
| Interfaces | Compiled to **dictionaries**; instances (`{ Name with ... }`) are existential values. Parameters are **type or effect** (kind inferred from use): an effect parameter (`Logger 'e`) is supplied with an effect row (`Logger { Console }`) and threads the methods' effects (see Effects) |
| Effects | **Capabilities as explicit values** (interface instances flowing from `main`); **row-polymorphic capability records give least authority**. **Type-level effect rows** layer over this: every arrow carries an effect row of the host-capability **interfaces it uses** (`a -> b / { Console, FileSystem \| 'e }`; a bare arrow is pure), inferred and **required on every public signature** — a binding that performs a capability it does not declare (or declares one it never uses) is **`FAI5001`**, so a function's reach is visible in its type. Effects are **used, not held** (calling a method incurs its interface's effect; holding/forwarding a capability is pure), close the closure-laundering hole (a captured capability rides the closure's arrow), propagate through the standard combinators (`List.map : ('a -> 'b / 'e) -> List 'a -> List 'b / 'e`, …), and are **erased at runtime**. Effect subsumption is **deep**: at a function application an argument is related to its parameter by a directional `⊆` recursing with variance — **covariant** under arrow results, tuples, records, list elements, and an interface's effect argument; **contravariant** under arrow parameters — so a less-effectful function is accepted where a more-effectful one is expected at any depth, and arguments sharing one effect variable **union** their effects (point-free composition, `consoleFn >> clockFn`). The non-effect structure unifies (general ADT arguments and leaves invariant), and every *non-argument* position unifies strictly (mixed `if`/`match` branches are not laundered; an effectful function is rejected where a pure one is required). **Effect-parameterized interfaces** are built: a parameter used after `/` in a method is an effect parameter (`interface Logger 'e = log : String -> Unit / 'e`, inferred from use; both type and effect is **`FAI3019`**); the effect argument is written with effect-row braces (`Logger { Console }`, `Logger 'e`, `Logger {}`; wrong kind is **`FAI3020`**); an instance forwards its body's effect into the parameter (so a console logger is `Logger { Console }`) and a method body performing an undeclared effect is rejected; dispatch incurs the value's effect argument |
| Contracts | **First-class `example` / `forall` declarations** (`example: e` / `forall xs: e`; peers of `let`/`type`), resolved in module scope, type-checked to `Bool`, run by `fai test` (closed `example`s are also evaluated eagerly by `fai check`, reported as `FAI6001`); `///` is human prose only |
| Backend | **Cranelift** native code generation |
| Memory | **Perceus-style reference counting** (pure + strict ⇒ acyclic heaps ⇒ no cycle collector); reuse analysis enables in-place updates incl. `{ r with ... }`, and resets a matched cell before a `let`-bound recursion so a "recurse-then-rebalance" `insert`/`remove` rebuilds a unique search path in place (O(n), freeing the reuse token on branches that build nothing). **Closure escape analysis** picks each `fun`-literal's cell: a non-capturing lambda shares one immortal **static** closure, a capturing lambda that provably does not outlive its frame is **stack**-allocated (its captures still released by reference counting, the cell never freed), and one that may escape stays a **heap** cell |
| Representation | Uniform 64-bit boxed/immediate values, **except a monomorphic scalar `Float`, which is an unboxed `f64`, and a monomorphic scalar `Int`, which is a raw untagged `i64`** (both in registers/locals, in direct-call parameters and results, and across tail loops) — tagged/boxed only where the value crosses a uniform slot (a data field, a closure environment, an `apply_n`/first-class argument or result, a generic position); the first-class form bridges via a wrapper. A raw `Int` lets the hot integer ops compile to bare native instructions (no tag guard, fit check, or boxing) and carries the full 64 bits without per-step boxing; raw-ness (both `i64`) is tracked by codegen, not the Cranelift type, and is part of the object-cache key. A concrete `Float` **field** of a record/tuple/constructor is an unboxed raw `f64` **slot** in its heap cell (a per-shape scalar bitmap; generic readers consult it), and a **fixed-shape float aggregate** that does not escape — a tuple of all-`Float`, or a closed record of all-`Float`, with up to eight fields — is **scalar-replaced**: held as its component `f64`s in registers and returned via a Cranelift multi-result signature (no heap cell), reassembled into the slot layout only where it crosses a uniform/boxed boundary (a generic/`apply_n`/first-class position, a field of a larger cell, a structural `=`/`compare`/`hash`), with that ABI signature-derived (register entries only). Scalarizing `Int` inside aggregates, nested float aggregates, and loop-carried float-aggregate state remain future work (an **`Array Float`**'s elements are already unboxed — see below; an unboxed `Array Int` is unnecessary, a small `Int` being an inline immediate). An **`Array`** is a contiguous heap buffer (header + length + inline element slots; capacity derived from the allocation size), always boxed; `set`/`push` mutate in place when the array is uniquely owned and copy when shared (the `{ r with … }` model), so values stay immutable. The element-access operations compile **inline** rather than as runtime calls: `length` is a field load, `unsafeGet` an inline bounds-checked slot load (a raw `i64` for an `Int` element, and — since **`Array Float` stores raw inline `f64`s** — the slot word reinterpreted as an `f64` for a `Float`, both with no per-access dup/drop or box; any other concrete element is the slot word with an inline tag-checked dup), and `unsafeSet`/`push` an inline in-place store on a uniquely-owned array (a raw `f64` for a `Float`, self-tagging the buffer's descriptor; releasing the overwritten boxed element inline), with a runtime fallback for the shared-copy case; an out-of-bounds index still aborts (a located fault, like `/`). This holds for a monomorphic *and* a generic (type-variable) element — so the std combinators (`Array.foldl`/`map`/`sort`) inline too; a generic element additionally branches on the array's runtime self-tag (a float array re-boxes on read, stores raw on write — the no-monomorphization ceiling for generic float-array traversal) — and the access ops are part of the object-cache key. Array **construction** (`withCapacity`, and the builders that bottom out in it — `empty`/`singleton`/`init`/`map`/`fromList`/`repeat`/…, plus the array literals and the deforested builder loops) and the `push` **grow** path likewise inline the allocator's **pooled fast path** rather than calling it: the cell's size class is computed (a constant for a literal/known capacity), its thread-local free list is popped (load the head, store the next-free pointer back) and the object header written inline, and for the grow path the elements are moved into a fresh, larger pooled buffer (a plain word move — ownership transfers) and the old buffer is pushed back onto its free list inline; the thread's free-list base is fetched once per function (via `fai_pool_heads`, loop-invariant since execution is single-threaded), and the runtime allocator (`fai_alloc_array`) is the fallback for an unpooled (large) size or an empty free list. The inlined alloc/free keep the debug leak/allocation counters balanced (a debug-only counter bump; a release build's fast path is call-free past the once-per-function base fetch). A **`String`** is likewise a contiguous heap buffer (header + byte length + inline bytes; capacity derived from the allocation size); `++` **appends into a uniquely-owned left operand in place** — extending its spare capacity, growing by doubling when full, and forking a fresh copy only when the operand is shared — so building a string by repeated concatenation is amortized O(total length) rather than O(n²), and a (right-associative) `++` chain is **left-reassociated** before reference counting so the chain feeds that in-place append (a behavior-preserving rewrite — concatenation is pure and associative and operands still evaluate left to right). A `String` is also, transparently, either that inline buffer or a **borrowing slice view** (a separate heap kind: base pointer + byte offset + length, sharing the base's bytes): char-indexed `take`/`drop`/`substring`/`split` return a view for a large piece and an owned copy for a small one (a view iff `len ≥ 32 && len*4 ≥ base`, bounding retention; small pieces of a big base copy), so slicing large regions avoids the per-piece byte copy. The two representations are indistinguishable to user code (uniform length offset, one byte reader, equality/ordering compare by content across both, the drop scan releases a slice's base — so `String` is not a child-free leaf at drop). Canonical record field layout (sorted by label text); monomorphic field access is a **constant offset**; *row-polymorphic* field access and `{ r with … }` update use **offset-evidence passing** — per row lacks-constraint, an integer offset threaded in as a leading argument (like a dictionary), composing through call chains and baked into partial applications for first-class use; dictionaries for interfaces/generics. A **monomorphic `Option` is "niche"-encoded without a `Some` cell**: when the payload is always boxed, `None` is an immediate and `Some x` is the payload pointer (a boxed payload is never an immediate, so the two never collide); when the payload may itself be an immediate (`Int`, `Bool`, `List`, a nullary-bearing union, …), `Some x` is `x` unchanged and `None` is a single immortal shared sentinel object. So `map`/`filter`-style code over a monomorphic `Option` allocates no wrapper, and a niche `Option` parameter is passed **owned** (no borrowing conversion at a call). The scheme is decided at lowering and carried on the IR's data nodes (so it survives the object cache's wire form), and is **erased where the value crosses a uniform slot** (a generic position, a closure environment, an `apply_n`/first-class boundary); equality and ordering convert to the standard form first. Nesting is not niche-encoded (an `Option (Option …)` payload uses the standard representation), and `Result` keeps its standard two-cell representation (with fused get-or-default accessors instead) |
| Calling convention | A **direct-callable** definition (non-row-polymorphic, ≥1 parameter) has a **register-passing entry** `fn(env, a0, …, aN) -> ret` — a saturated direct call passes its value arguments in registers (a scalar `Float` as an `f64` register, a monomorphic `Int` as a raw untagged `i64`), an over-application direct-calls the saturated prefix and `apply_n`s the rest. Row-polymorphic and nullary entries (reached only via `apply_n`) keep the uniform spilled-array `fn(env, args) -> i64` and keep ints tagged; the first-class value form always uses the uniform ABI, bridged by a wrapper (the static closure's code). Proper tail calls (`return_call`) are future work. |
| Determinism | Clock / random / env / IO are reachable only via capabilities |
| Standard library | Real compiled `.fai` modules under **`std/`**, embedded at build time. One **auto-imported** module, `Prelude`, owns the core types `Option`/`Result` (with their constructors), re-exports the opaque `Dict`/`Set`/`HashDict`/`HashSet` type names, and provides the free functions `identity`/`const`/`not`/`compare`; all other operations are **qualified** under per-type modules (`List.map`, `Array.map`, `Option.withDefault`, `Int.toString`, …). `Prelude`/`List`/`Array`/`Option`/`Result`/`Dict`/`Set`/`HashDict`/`HashSet`/`String`/`Int`/`Float`/`Char` are reserved module names. The few Rust **intrinsics** are prelude-private, reached only as `Prim.*` from inside `std/` (`FAI2014` elsewhere) and re-exported under clean names; a **saturated call to such a re-export is inlined to its primitive** (the intrinsic inliner), so the wrapper adds no indirection at a use site. **Two associative-container families:** the ordered `Dict`/`Set` (weight-balanced BSTs, O(log n), sorted iteration/range use) and the unordered `HashDict`/`HashSet` (flat open-addressing hash tables over `Array`, O(1) average, unspecified-but-deterministic iteration), all **`opaque`** types declared in their own modules with their node constructors hidden and re-exported by `Prelude` via transparent aliases. The hash containers build on a structural-hash primitive (`Prim.hash`, the peer of `Prim.compare`) that agrees with structural equality; they keep value semantics like `Array` (copy-on-share, in-place update when uniquely owned). The collection and `Option`/`Result` lookups offer **fused get-or-default accessors** (`Dict.getOr`/`getOrElse`, `Dict.member`, `HashDict.getOr`/`getOrElse`/`member`, `Option.getOrElse`, `Result.withDefault`/`getOrElse`/`toOption`) that return the contained value (or a default) directly, never materializing the intermediate `Option`. |
| Compilation model | **Demand-driven (salsa) query engine**; per-workspace **daemon** holds the DB hot, thin CLI client; **content-addressed on-disk cache**; **JIT** for `run`/`test`, **AOT** for `build`; incremental at definition/SCC granularity |
| Tooling | `fai build/run/check/fmt/test/lsp` + read-only `fai query …` (code intelligence); per-workspace daemon (MessagePack JSON-RPC); global `--message-format=json`; stable error codes `FAInnnn`. Full reference: **`docs/CLI.md`** |

## 4. Language at a glance

```fai
module Hello

// `main` declares the capability it uses; a bare arrow would be pure.
public main : Runtime -> Unit / { Console }
let main runtime =
  runtime.console.writeLine "Hello, Fai!"
```

```fai
module Collections

/// Apply f to every element. The effect variable `'e` forwards whatever effect
/// `f` performs, so `map` is pure on a pure function and effectful on an
/// effectful one.
public map : ('a -> 'b / 'e) -> List 'a -> List 'b / 'e
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

A single Cargo workspace. Each crate owns one compiler phase or tool.

```
fai/
├── AGENTS.md            # this file
├── samples/             # language by example (canonical, tested .fai tour)
├── std/                 # standard library: real .fai modules, embedded at build time
├── editors/
│   └── vscode/          # VS Code extension: thin `fai lsp` client + TextMate grammar (TypeScript/JSON; own npm tooling + CI, outside the Cargo workspace)
├── docs/
│   ├── MEMORY.md        # design memory: standing risks + locked decisions
│   ├── CLI.md           # CLI + daemon-protocol reference
│   └── BENCHMARK.md     # benchmarking: perf guards, suites, Fai-vs-Rust comparison
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
│   ├── fai-corpus/      # synthetic workspace generator + real-world bench fixtures
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
  primitives, and only with a `// SAFETY:` comment justifying each block. The sole
  other hand-written `unsafe` is the **Windows process-control FFI**: the
  daemon-spawn handle-inheritance fix in `fai-server` and the worker's Job-Object
  resource limits in `fai-driver` call `windows-sys` directly (the peers of the
  safe `nix` calls used on Unix, for which no safe wrapper exists), each in a
  scoped `#[allow(unsafe_code)]` with a `// SAFETY:` comment. Crates
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
  - **the roadmap / issue tracker as "the plan"** — write "noted as future work", not "see the plan".

  This holds **even when the change implements a milestone or a logged
  decision**: describe the behavior, not the roadmap step that produced it. So:
  - write "Add the native runtime", **not** "Implement the M3 runtime";
  - write "no reuse analysis yet", **not** "reuse is deferred to M6";
  - write "record the design decisions in the design memory", **not** "see D45–D55
    in `docs/MEMORY.md`".

  Pointers to the durable specs (`docs/CLI.md`, `docs/MEMORY.md`, `AGENTS.md`) are
  fine when they document a real contract (e.g. a wire schema or a naming
  convention). A commit
  whose subject or body names a milestone, phase, or decision id **must be
  reworded before it merges** (reword local history with `git rebase`). Describe
  *what changed and why*, not the step in a roadmap that produced it.
- **Work on feature branches — never directly on `main`.** Every change lands on a
  short-lived feature branch (`git switch -c <topic>`) and merges into `main`
  through a pull request; `main` is never committed or pushed to directly. Keep
  the branch focused, rebase on `main` before opening the PR, and let the CI gates
  (§12) pass before merging.

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
and serves a thin CLI client over MessagePack JSON-RPC (see `docs/CLI.md`).
Read commands run **concurrently** on cloned database snapshots (the lock is held
only to sync inputs and clone a snapshot); an input change **cancels** in-flight
reads, which retry on the new revision. With LRU eviction to bound memory.
`fai-ide` exposes code-intelligence queries to both the CLI (`fai query`) and the
LSP.

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
  embarrassingly parallel per function). Per-definition AOT object emission and
  the run-path lower/reference-count gathers already do this: each worker takes
  its own database-handle clone (`Db::clone_box`; salsa databases are `Send` not
  `Sync`, so handles are cloned, not shared), order-preserving so builds stay
  deterministic. The JIT compile generates each function's machine code in
  parallel too (`Context::compile` across the pool, needing only the shared ISA),
  building the IR and linking the shared module serially.
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
- **Wall-clock benches** ([divan]) are for local profiling: `cargo bench`.
  `inference.rs` covers cold check vs warm incremental edits over a synthetic
  corpus; `contracts.rs` covers the `fai test` edit→test loop (synthesize → JIT →
  run), and `fai-cli`'s `test_loop.rs` benches the same loop end-to-end through
  the real binary and its daemon (client → daemon → worker subprocess);
  `lsp.rs` covers the language-server features (hover, go-to-definition,
  diagnostics, completion, signature help, references, rename, document symbols)
  both as warm analysis and as full round trips through the real server, over the
  synthetic corpus *and* a hand-written multi-module application (under
  `samples/`, via [`fai-corpus`](crates/fai-corpus)'s `realworld` fixtures) whose
  rows link to the exact source line each probes; `micro.rs` covers the inference
  primitives; `stress.rs` covers pathological scenarios. The deterministic
  [`fai-corpus`](crates/fai-corpus) generator backs the corpus benches and the
  guards.
- These benches' **timings are not a CI gate** (shared runners are noisy). The
  `CI` workflow still **compiles** them (`build --all-targets`) to prevent bitrot,
  and a separate **`Benchmarks` workflow** (`.github/workflows/bench.yml`) **runs**
  every benchmark on **every pull request**, on `main`, and on demand, publishing
  an **informational** report — a Markdown summary on the run page plus the raw and
  parsed (`bench-results.json`) results as artifacts — rendered by the
  `bench-summary` tool (`crates/fai-tests/src/bench_summary.rs`). A pull-request
  run uses a short per-benchmark settle time (`DIVAN_MAX_TIME=1`), so the whole
  suite runs in roughly the test job's time; it still **executes** every
  benchmark, so it fails the build only when a benchmark **crashes or its
  Fai-vs-Rust result diverges from its oracle** (a bug, not a perf regression),
  never on a timing. The `main`/on-demand run keeps the long settle time for steady
  medians. It never fails the build on timings; the deterministic guards remain the
  sole performance gate. The Fai-vs-Rust
   comparison spans **runtime** (the `algorithms_jit`/`algorithms_aot` benches) and
   **peak memory** (the `algorithms_mem` bench: each delivered binary self-reports
   its peak resident set size, rendered as a "Fai vs Rust (peak RSS)" table). The
   benchmarked algorithms (the `ALGORITHMS` registry in
   `crates/fai-tests/src/algorithms.rs`, each a Rust oracle paired with a
   `samples/algorithms/` module) deliberately span a wide range of runtime shapes —
   arithmetic/recursion, lists, persistent `Dict`/`Set` (including tuple keys),
   strings and ADTs, records with `Float` fields, dynamic programming, closures,
   bitwise intrinsics, and interface dispatch — so a performance change is measured
   broadly rather than against a few cases; the `registry_is_fully_covered` test
   keeps the benches and validation in sync with the registry. The full
   benchmarking guide — every suite, the CI report, and the comparison methodology
   (including why the JIT and AOT benches' Rust baselines are not comparable) — is
   **`docs/BENCHMARK.md`**.

The inference solver carries always-on thread-local **work counters** (variable
resolution clones, occurs-check node visits, free-variable visits — see
`fai-types/src/perf.rs`) so its asymptotic complexity is gated deterministically
by `crates/fai-tests/tests/perf_guards.rs`, not just wall-clock benches. The
super-linear hot spots the benches once surfaced have been addressed: solver
types share their children via `Rc` (so `resolve_shallow` is O(1), not a deep
clone) and the occurs/free-variable walks **borrow** and **memoize** shared
representatives; **local-`let` generalization** uses rank/level-based
quantification (no per-binding environment free-variable recomputation); and
unification **path-compresses** variable chains. The remaining super-linear case
is the occurs *walk* over a long ground application chain (O(n²) node visits, but
microseconds in practice once the dominant clone cost is gone — a structural
"variable-free" cache was measured to add per-node overhead for no real gain, so
it was dropped).

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
- **Error codes are an API.** Allocate codes by phase, documented in the
  error-code catalog **`docs/ERROR_CODES.md`**: `FAI0xxx` tooling/CLI/driver,
  `FAI1xxx` lex/parse, `FAI2xxx` resolve/visibility, `FAI3xxx` types/rows,
  `FAI4xxx` exhaustiveness/patterns, `FAI5xxx` capabilities, `FAI6xxx` contracts,
  `FAI7xxx` backend (Core lowering / codegen / runtime). Each phase crate owns its
  codes as a `pub const CODES: &[CodeInfo]` slice (code, title, default severity,
  and a prose `explanation`). The `fai-tests` catalog test aggregates them to
  enforce format, uniqueness, and that every code is documented; it also
  **renders `docs/ERROR_CODES.md` from those tables** (regenerate with
  `UPDATE_ERROR_CODES=1 cargo test -p fai-tests --test catalog`) and asserts every
  code constant defined in the sources is catalogued. Never renumber a shipped
  code.
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
`with`, `if`, `then`, `else`, `fun`, `public`, `opaque` (the opaque-type marker,
only before a `public` `type`), `as` (the as-pattern binder), and
the contract-declaration keywords **`example`** and **`forall`**. Contracts are ordinary
declarations (peers of `let`), not comment text, so the symbols inside them
resolve through normal name resolution and are fully type-checked.

Every language-surface change must update **all three** docs and add tests
(parser snapshot, type golden, and/or e2e) in the same change.

## 12. Definition of Done / CI

A change is done when:

1. `cargo build` is clean and `cargo clippy --all-targets -- -D warnings` passes.
2. `cargo fmt --all -- --check` passes (Rust side).
3. The tests pass, including golden/snapshot and e2e tests. **Do not run the whole
   `cargo test` workspace suite locally as a finalization gate — it is too slow
   (the AOT/native, algorithms, daemon, and LSP e2e suites alone take many
   minutes).** Locally, run only the **affected** crates and integration targets
   (e.g. `cargo test -p fai-core`, `cargo test -p fai-tests --test reuse`) plus any
   suite a change plausibly touches; the **full suite is CI's job** (the
   `.github/workflows/` gates run it). Prefer fast, targeted feedback over a local
   full run.
4. New behavior has tests at the appropriate levels (see §13); new diagnostics
   have codes + catalog entries.
5. Any surface-language change is reflected in `AGENTS.md`, `docs/MEMORY.md`
   (decisions), and the `samples/` directory.
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
- **One case per `#[test]`; avoid table-driven loops.** Don't iterate a literal
  table of cases inside a single test — the first failing case aborts the rest,
  and the panic doesn't say which case failed. Write one focused `#[test]` per
  case instead, factoring the shared assertion into a `#[track_caller]` helper
  they each call (see `fai-rc/src/cases.rs`). This isolates and names failures
  and lets the cases run in parallel. Two exceptions: generative **property
  tests** (`proptest`) are the right tool when a law holds across *all* inputs;
  and when the "cases" are really sub-parts of one logical fact (e.g. every field
  of a single inferred record), assert the whole structure in one comparison
  rather than looping over its pieces.
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
