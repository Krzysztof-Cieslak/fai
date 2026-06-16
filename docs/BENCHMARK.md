# Benchmarking

This document explains how performance is measured and protected in the Fai
compiler: the two layers of performance protection (the deterministic gate vs the
informational wall-clock benches), how to run the benches, how the CI report is
produced, and — in depth — how the **Fai-vs-Rust runtime comparison** benches
work. The last part answers a question the numbers invite: *why is "Rust" so much
slower in the AOT comparison than in the JIT comparison if it is the same Rust?*

## Two layers of performance protection

Performance is guarded two different ways, for two different reasons.

### 1. Deterministic guards — the gate

`crates/fai-tests/tests/perf_guards.rs` is the **regression gate**. It asserts the
*incrementality* properties the architecture promises using the query-execution
**event log** — a deterministic count of which salsa queries re-ran — rather than
wall-clock time. Because it counts query executions, it is immune to CI-runner
noise and gates in the ordinary `cargo test` run.

The headline property: the work to re-check after a localized edit is
**independent of total workspace size** (the cross-module firewall). For example,
editing one module's private body re-infers only that module's own definitions,
whether the workspace has 10 modules or 100.

If you add or change a query, cover it here (and with the incremental-vs-clean
verifier).

### 2. Wall-clock benches — informational only

The [divan] benches under `crates/fai-tests/benches/` (and
`crates/fai-cli/benches/`) measure wall-clock cost for **local profiling**. They
are **not a CI pass/fail gate** — shared runners are too noisy for that, which is
precisely why the deterministic guards above exist.

To keep them from bitrotting, the `CI` workflow still **compiles** them
(`build --all-targets`), and a separate **Benchmarks workflow** *runs* them on
**every pull request**, on `main`, and on demand to publish an informational
report (see below). It never fails the build on timings — only when a benchmark
crashes or (for the Fai-vs-Rust algorithm benches) computes a wrong result.

## Running the benches

```sh
cargo bench --workspace --benches            # everything
cargo bench -p fai-tests --bench inference    # one suite
cargo bench -p fai-cli   --bench test_loop    # the end-to-end fai test loop
```

`DIVAN_MAX_TIME=<seconds>` caps the wall time per benchmarked function. The
`main`/on-demand CI run uses `120` — generous enough that even the process-spawn
benches (`algorithms_aot`, the daemon e2e, the `fai test` loop) reach divan's full
~100-sample target for steady medians, while still bounding any pathological
function. A **pull-request** run uses `1` instead, so the whole suite finishes in
roughly the test job's time (about three and a half minutes from a cold build,
dominated by the release compile) — the report is informational and non-gating, so
rough medians are fine there.

## The CI Benchmarks workflow

`.github/workflows/bench.yml` runs on every **pull request**, every push to
`main`, and on `workflow_dispatch`. It runs `cargo bench --workspace --benches`
(at `DIVAN_MAX_TIME=1` on a pull request, `120` otherwise), then renders the
output with the `bench-summary` tool
(`crates/fai-tests/src/bench_summary.rs`):

- A **Markdown report** is appended to the run summary. divan has no
  machine-readable output, so `bench-summary` parses its Unicode tree
  (`├─`/`╰─`/`│` and the `fastest │ slowest │ median │ mean │ samples │ iters`
  columns). Parsing is best-effort and never panics — an unrecognized line is
  skipped, so a divan format change degrades to a thinner report rather than a
  failure.
- The raw output and a parsed `bench-results.json` are uploaded as the
  `benchmark-results` artifact.
- A benchmark *case* label that looks like a source location (`<path>.fai#Lnn`,
  produced by the real-world language-server benches) is **linked** to the exact
  file and line on the forge, so a report row points at the code it measured.
- For a group whose rows pair a `rust` and a `fai` leaf (the runtime-comparison
  benches), the report adds a **"Fai vs Rust" ratio table** (median `fai/rust`;
  lower is better). The pairing is **within a single group** — `algorithms_jit`
  and `algorithms_aot` are paired separately and never against each other.

To inspect a past run locally:

```sh
gh run list --workflow=bench.yml --branch=main
gh run download <run-id> -n benchmark-results -D /tmp/bench
```

## The benchmark suites

All under `crates/fai-tests/benches/` unless noted. None is a CI gate.

| Suite | Measures |
|---|---|
| `inference` | End-to-end type inference over synthetic workspaces: `cold_check` (grows with size) vs `warm_*_edit` (should stay flat — the firewall). |
| `micro` | Inference primitives: unification on large types, deep/wide expressions, large mutually-recursive groups. |
| `stress` | Pathological inference: exponential type growth, very wide/deep structures, instantiation- and constraint-heavy bodies, error-laden files. |
| `data_layer` | Inference/exhaustiveness over record- and union-heavy modules; lowering of `match`/records; structural runtime primitives (`compare`, `Float`, construction). |
| `interfaces` | Inference, lowering, and JIT execution of dictionary dispatch and offset-evidence (row-polymorphic field access / capabilities). |
| `reuse` | Reuse / in-place update / borrowing: paired *unique* (cells recycled) vs *shared* (cells copied) rebuilds, and in-place vs copying record updates. |
| `codegen` | The backend pipeline (lower → reference-count → Cranelift → JIT) plus a few runtime primitives. |
| `contracts` | The in-process `fai test` loop (collect → synthesize harness → reference-count → JIT → run) over the corpus, cold and warm. |
| `daemon` | Daemon-path pieces: content-addressed cache key, run-bundle serialization, wire framing, workspace file-state sync. |
| `lsp` | Language-server latency: warm `analysis_*` (the work to answer a request) and full `roundtrip_*` through the real server over an in-memory connection. |
| `algorithms_jit` | Runtime comparison, in-process compute: compiled Fai code vs idiomatic Rust (see below). |
| `algorithms_aot` | Runtime comparison, delivered binaries: a `fai build` executable vs a Rust release binary, end to end (see below). |
| `algorithms_mem` | Memory comparison, delivered binaries: peak resident set size of the same `fai build` vs Rust binaries (see below). |
| `test_loop` (`fai-cli`) | The supervised `edit → fai test` loop through the real `fai` binary + daemon: client → daemon → worker subprocess → JIT → run → stream back. |

## Runtime comparison: Fai vs Rust

Two benches compare Fai's runtime performance against an idiomatic Rust
reference. They are the source of the most confusing numbers, so they get the
most explanation.

The idiomatic Rust references live in `crates/fai-tests/src/algorithms.rs`, each
paired with its Fai sample under `samples/algorithms/` and two workload sizes in
the `ALGORITHMS` registry. The suite deliberately spans many runtime shapes so a
performance change is measured broadly rather than against a handful of cases:

- **arithmetic / recursion** — `fib` (wide, non-tail), `ackermann` (deep stack),
  `collatz` and `pi` (tail loops), `prng_xorshift` (bitwise `Int` intrinsics);
- **lists** — `map_sum` (reuse fast path) and `map_sum_shared` (the copying
  fallback), `merge_sort`, `quicksort`, `matrix_multiply` (nested lists),
  `fold_pipeline` (a composed/partially-applied `transform` folded over a range —
  the closure-confinement path: its compositions and CAF are reduced to a register
  loop before reference counting, so it now tracks the Rust oracle rather than
  paying per-element closure construction and first-class calls), `nqueens` and
  `fannkuch` (backtracking / permutations);
- **arrays** — `sieve` (a flat mutable `Array Bool`: in-place update of a
  uniquely-owned array, mirroring the Rust `vec![bool]` reference);
- **hash maps & sets** — `dict_histogram`, `set_dedup`, `option_path`,
  `graph_bfs` (`HashDict`+`HashSet`+`List`), `union_find`, `game_of_life`
  (`(Int*Int)` tuple keys); all over the unordered `HashDict`/`HashSet`;
- **strings & ADTs** — `word_count`, `json_serialize`, `expr_eval` (a recursive
  parser/evaluator threading `Option`);
- **records & floats** — `particles` and `nbody` (records + `{ r with … }`),
  `spectral_norm` and `mandelbrot` (float reductions);
- **dynamic programming** — `levenshtein` and `coin_change` (flat mutable `Array`
  tables, mirroring the Rust `vec` reference), `fib_memo` (`HashDict` memo);
- **interface dispatch** — `interface_dispatch`.

The associative-container workloads use the unordered `HashDict`/`HashSet`
(O(1)-average open-addressing tables), so they no longer degenerate on sorted
insertion the way the ordered BST-backed `Dict`/`Set` would; their sizes are kept
modest only for stable medians.

### "JIT" and "AOT" describe how *Fai* is compiled — not Rust

This is the key to reading these benches. Fai has two execution paths:

- **JIT** — `fai run`/`fai test` compile Core IR with Cranelift in-process and
  execute it directly, no link step.
- **AOT** — `fai build` compiles and links a native executable.

The two benches are named after the **Fai** path they exercise. **Rust is always
ahead-of-time compiled** (by Cargo/LLVM at the bench profile's optimization); it
is the *baseline*, never the thing being JIT'd. So "the JIT bench's Rust number"
just means "the Rust baseline measured alongside the JIT-compiled Fai code in the
in-process bench."

### Correctness vs timing — two separate comparisons

"Comparing results with Rust" can mean either of two things, and JIT-vs-AOT is
irrelevant to the first:

- **Correctness (values).** `algorithms_jit`'s `verify` applies the compiled Fai
  closure and asserts its value equals the Rust oracle (floats within a `1e-6`
  tolerance for Cranelift-vs-LLVM rounding), so a miscompiled benchmark cannot
  report meaningless timings. The headline backend property test
  (`crates/fai-codegen/src/proptests.rs`) does this generatively: JIT-compiled
  programs agree with a Rust reference evaluator. Whether the machine code came
  from a JIT or a linked binary makes no difference to whether `28 == 28`.
- **Timing.** The two benches below.

### `algorithms_jit` — in-process compute

`crates/fai-tests/benches/algorithms_jit.rs` compares the *execution* of compiled
code in one process:

- **Fai side**: the sample's reachable closure is JIT-compiled **once, in untimed
  setup** (`jit_compile`); the timed loop only *applies* the finished function.
- **Rust side**: the timed loop calls the idiomatic reference function directly.

Because the JIT compile is excluded from timing, this is
**native-execution vs native-execution** — not "JIT compile vs AOT binary." (The
JIT even keeps the host's native CPU features, where the portable AOT build
targets a baseline ISA.)

### `algorithms_aot` — delivered binaries, end to end

`crates/fai-tests/benches/algorithms_aot.rs` compares the *delivered artifacts*:

- **Fai side**: built once with `build_native` (untimed), then **spawned** as a
  subprocess in the timed loop.
- **Rust side**: spawns the `algo-baseline` release binary
  (`crates/fai-tests/src/bin/algo-baseline.rs`) as a subprocess.

Each timed iteration is a whole process: startup, the workload, print, exit.
(Skipped on Windows, which needs the MSVC environment for the build/link + spawn
path; it still compiles there so `--all-targets` keeps it from bitrotting, and the
workflow runs on Linux.)

### `algorithms_mem` — delivered binaries, peak memory

`crates/fai-tests/benches/algorithms_mem.rs` is the **memory** side of the same
delivered-binaries experiment: instead of timing the spawned processes, it records
each one's **peak resident set size** at the AOT workload. It is not a divan timing
loop (peak memory is not a per-iteration measurement); each binary is run a few
times and the maximum peak is kept.

Both sides are measured **identically by self-reporting**: with `FAI_REPORT_RSS`
set in the child's environment, the Fai runtime (`run_entry`) and the
`algo-baseline` binary each read their own peak RSS from `/proc/self/status`
(`VmHWM`, the high-water mark) and print a `fai-peak-rss-kib:` line to stderr. The
harness parses that and emits a `MEMSTAT\t<algorithm>\t<side>\t<kib>` line, which
`bench-summary` renders into a **"memory: Fai vs Rust (peak RSS)"** table (median
`fai/rust`; lower is better) and includes in `bench-results.json`. divan's parser
ignores the `MEMSTAT` lines, so they ride safely in the shared output stream.

Reading peak RSS:

- **Peak RSS is the whole-process footprint**, so it includes fixed overhead — the
  linked runtime/std, code pages, allocator slack — that **dominates the small-heap
  workloads** (`fib`, `collatz`, `pi` bottom out near the runtime's baseline, and
  the Fai binary can even read *lower* than Rust there). The **heap-heavy**
  workloads carry the real signal: `map_sum` builds a 1.5M-element `Array` (a large
  transient heap) where idiomatic Rust runs an allocation-free loop, so its ratio
  is large and expected; `merge_sort` sorts an `Array` vs a `Vec`; `binary_trees`
  builds a comparably large structure on both sides, so its peak is near-even. As
  with the timing benches this is a **progress metric, not a fair fight** (boxed,
  reference-counted values vs unboxed) — watch whether the gap shrinks as the
  backend improves.
- **Linux-only.** Peak RSS is read from `/proc`, so the table is populated by the
  Linux Benchmarks workflow; on other platforms (and on Windows, which also skips
  the build/link + spawn path) the bench prints a skip note and reports no rows.
  The bench still compiles everywhere so `--all-targets` keeps it from bitrotting.

### Why the Rust numbers differ so much between the two benches

The Rust *implementation* is identical, but the two benches are **different
experiments**, so their Rust baselines are not comparable. Two effects compound.

**1. Different workload sizes.** Each algorithm registers two sizes: a small
`jit_size` (for stable in-process medians) and a large `aot_size` (to amortize
process startup), from `crates/fai-tests/src/algorithms.rs` (a representative
subset; the registry is the full list):

| algorithm | `jit_size` | `aot_size` | size factor |
|---|---|---|---|
| Fib | 28 | 33 | ~11× (exponential: φ⁵) |
| Collatz | 4 000 | 60 000 | 15× |
| MapSum | 100 000 | 1 500 000 | 15× |
| MergeSort | 6 000 | 80 000 | ~13× |
| BinaryTrees | 17 | 21 | 16× |
| Pi | 45 000 | 800 000 | ~18× |

The sizes vary widely by algorithm: a few (`nqueens`, `ackermann`, `fannkuch`,
`matrix_multiply`) are tens, not thousands, because their cost grows steeply, and
the hash-container workloads are kept modest only for stable medians.

**2. Different measurement scope.** `algorithms_jit` times a **pure in-process
function call**; `algorithms_aot` **spawns a whole subprocess** (fork/exec +
dynamic linker + runtime init + print + exit), a floor of roughly 1–2 ms
*regardless of workload*.

Both effects are visible in a real run. From the `main` Benchmarks run
`27281697190` (illustrative — exact numbers drift run to run):

| algorithm | Rust in `algorithms_jit` | Rust in `algorithms_aot` | ratio |
|---|---|---|---|
| binary_trees | 6.769 ms | 235.2 ms | ~35× |
| collatz | 328.4 µs | 7.845 ms | ~24× |
| fib | 938.3 µs | 13.17 ms | ~14× |
| map_sum | 34.99 µs | 2.002 ms | ~57× |
| merge_sort | 6.684 µs | 1.262 ms | ~189× |
| pi | 70.57 µs | 2.279 ms | ~32× |

The size factor explains ~11–18×; the rest is the process-spawn floor. You can see
that floor directly: in `algorithms_aot`, `map_sum` (2.0 ms), `merge_sort`
(1.26 ms), and `pi` (2.28 ms) all bottom out near 1–2 ms even though the same
compute in-process is 7–70 µs — for `merge_sort`, sorting 80k integers is tens of
microseconds, so essentially all of that 1.26 ms *is* process spawn, which is why
its cross-bench ratio (189×) is the largest. `binary_trees` does hundreds of
milliseconds of real work, so the spawn floor is negligible and its ratio (35×)
is closest to the pure size factor.

### How to read the numbers

- A Rust row is a **baseline within its own bench**, paired against the Fai row
  measured the same way in that bench. The summary's ratio table pairs them
  per-group for exactly this reason.
- **Never compare a Rust row across benches.** `algorithms_jit` answers "how fast
  is compiled Fai *code* vs Rust *code*, in process"; `algorithms_aot` answers
  "how fast is a *delivered Fai binary* vs a *delivered Rust binary*, end to end."
- Even within a bench it is a **progress metric, not a fair fight**: Fai runs a
  uniform **boxed**, **reference-counted** representation (Cranelift), while Rust
  is unboxed and optimized by LLVM. The number to watch is whether the gap
  **shrinks** as the backend improves.
- **Match the data representation.** A benchmark should compare *like with like*,
  so the ratio reflects the compiler/runtime/std rather than a fundamental
  data-structure difference that holds in any language (a linked list pointer-chases
  where an array is contiguous — true of Rust's own `LinkedList` vs `Vec`). So each
  sample and its oracle use the **same** representation, matched in whichever
  direction fits the workload — the test being whether switching would make the
  *Fai* side faster:
  - **Contiguous** where access is index-, iterate-, or build-then-traverse-heavy
    (an `Array` is then also the better Fai structure): Fai's **`Array`** against
    Rust's `Vec`. `MapSum`/`MapSumShared` build-map-fold an `Array`; `MergeSort`
    uses the standard `Array.sort`; `QuickSort` is a hand-written in-place array
    quicksort; `MatrixMultiply`/`Levenshtein` use array-of-array and array-row DP;
    `SpectralNorm` and `FloatMatrixMultiply` use unboxed `Array Float` (raw inline
    `f64` slots); `NBody`/`Particles` hold their bodies in an
    `Array`; `WordCount` splits and joins through `Array String`
    (`String.splitArray`/`joinArray`).
  - **Persistent linked** where the workload is naturally persistent and a `List`
    is the better Fai structure — prepend-and-share backtracking, or a token stream
    a recursive-descent parser consumes head/tail, where an `Array` would copy on
    every step: Fai's **`List`** against an **`Rc`-based persistent cons-list** in
    Rust (`PList` in `algorithms.rs`), *not* `std::collections::LinkedList` (which
    is cache-hostile and would unfairly slow Rust). `NQueens` (a backtracking
    stack), `Fannkuch` (permutation generation + reversal), and `ExprEval` (a
    parser building an `Expr` tree) match this way.
  A bare `List`-vs-`Vec` mismatch is avoided — it would measure the representation
  gap, not Fai. Two further cases are noted exceptions: **`ListSort`** keeps a
  `Vec` oracle *on purpose* — it sorts a Fai `List` against the same `Vec` baseline
  as `MergeSort`'s `Array`, so the gap between the two samples' ratios isolates the
  in-Fai linked-vs-array sort cost; and **`JsonSerialize`** (2-element ADT children
  joined into a string) and **`GraphBFS`** (the cost is the `HashDict`/`HashSet`; the
  `List` is only the BFS frontier) keep a `List` whose container is immaterial
  because another structure dominates.

### Keeping the two sides in lockstep

Each `aot_size` must equal the literal the matching sample's `main` passes to
`run`/`runF`; the sample-validation tests (`crates/fai-tests/tests/algorithms.rs`)
assert this by comparing the program's output to the oracle, so the AOT bench
compares the same workload on both sides. To add an algorithm: add the Rust
reference and a registry entry in `algorithms.rs`, add the `samples/algorithms/`
module with the matching baked size, add a `validate` test in
`tests/algorithms.rs`, and list it in both `algorithm_benches!` macros. The
`algorithms_mem` bench and the `algo-baseline` binary iterate the registry
directly, so they pick up the new algorithm automatically; the
`registry_is_fully_covered` test guards the hand-maintained lists — it fails if a
registered algorithm is missing from either runtime bench or from the validation
tests. Keep `aot_size` small enough that running `main` once stays fast (the
validation test runs it under the JIT), especially for super-linear workloads.

[divan]: https://docs.rs/divan
