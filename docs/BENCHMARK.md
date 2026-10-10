# Benchmarking

This document explains how performance is measured and protected in the Fai
compiler: the two layers of performance protection (the deterministic gate vs the
informational wall-clock benches), how to run the benches, how the CI report is
produced, and how the **AOT comparisons with Rust and OCaml** work. Warm execution,
whole-process execution, memory and Fai JIT regression measurements have distinct
scopes and are reported separately.

## Primary comparison scopes

- **Warm AOT:** `algorithms_aot_warm` is the primary compute comparison. All three
  languages execute native AOT programs in persistent workers, with runtime input
  windows, identical batches and explicit harness-floor measurements.
- **End-to-end AOT:** `algorithms_aot` includes process startup, one runtime-sized
  workload, output and exit. `algorithms_mem` measures its peak process RSS.
- **Fai JIT regression:** `algorithms_jit` tracks Fai's execution route for
  `run`/contracts. Rust is an untimed correctness oracle. Historical Rust timing
  rows were real AOT function calls, not a simulated Rust JIT, and remain labelled
  as historical mixed-compilation diagnostics.

Compute-performance issues are assessed using reliable warm AOT results at the
relevant size. Startup and footprint issues use the end-to-end/RSS measurements.
Floor-limited or inconsistent results do not establish parity; neither does a
favorable result in a different scope. Preserve previous JIT measurements under
their original labels when refreshing an issue.

## What a comparison establishes

The algorithm suite is a **compiler-development microbenchmark set**. Its inputs
are known and have guided optimizations; they are not an independent holdout or
evidence that one language is faster on arbitrary applications. A result such as
"40/40" describes those executable versions, inputs, build settings, machine and
measurement scope. An equal-weight geometric mean is a summary of that selection,
not a prediction of application performance.

Record the exact sources, executable hashes, compiler versions, build commands,
optimization/CPU flags and environment with a published comparison. The normal
optimized configurations are:

| Side | Optimization and instrumentation |
|---|---|
| Rust | Cargo `release`/`bench`: LLVM `opt-level=3`, debug assertions and overflow checks off by default. LTO, codegen units and target CPU follow Cargo configuration; record overrides. |
| Fai | Both AOT and JIT use Cranelift `opt_level=speed`. AOT uses the baseline host ISA; JIT detects native CPU features. The embedded runtime is built with Cargo release optimization, with debug assertions matching the compiler's build. A release-built compiler disables its runtime counters. `fai build --release` is currently accepted but adds no optimization. |
| OCaml | `ocamlopt` produces optimized native code. Record `ocamlopt -config` and the actual flags: compiler version, Flambda configuration and GC settings matter. Native compilation alone does not establish equivalence to a current Flambda `-O3` build. |

The October 2026 development comparison used Rust 1.96.0 and OCaml 4.14.1 without
Flambda. Adding `-O3` to that particular OCaml build produced identical workload
object bytes. The current benchmark workflow pins **OCaml 5.5.1 with upstream
Flambda**, compiling all OCaml peers with **`-O3`**. Results from the old compiler
remain a separate peer version; changing the peer is not a Fai speedup or
regression.

Optimization freedom must be symmetric. Reference implementations contain no
per-element `black_box`/`Sys.opaque_identity` barriers. Whole-call input/output
barriers in the in-process harness prevent elimination of the *measurement*;
they still allow each compiler to simplify the workload. A closed-form sum or
exact fixed-point shortcut is a legitimate optimization, but then that input
measures the shortcut rather than sustained loop throughput. Sustained-throughput
claims require additional input-dependent workloads on every side.

Published results should also account for:

- **Input and representation differences.** All delivered benchmark executables
  read their size from argv. The Fai harness replaces the sample's entry point,
  retaining its workload definitions. Earlier Fai measurements baked the size
  into `main` and belong to the earlier methodology. Equal checksums establish
  result agreement, not identical intermediate work or data structures. The
  exceptions below are application comparisons, not matched kernel measurements.
- **Startup and duration.** Delivered-binary timing includes spawn, runtime
  initialization, output and exit. Several optimized workloads take less than a
  millisecond and are sensitive to that overhead. Measure a process floor and use
  longer runs or the persistent-worker/in-process suites for compute claims.
- **Memory and lifetime.** Peak RSS includes touched code, stacks, allocator
  retention and the GC heap. RC/Rust destruction and OCaml collection have
  different schedules; one short process is not a steady-state memory test.
  Fai builds one executable per workload, while each Rust/OCaml dispatcher
  contains the whole suite, another difference in the delivered image.
  Report absolute RSS and separate live-heap/allocation measurements. Do not
  describe an RSS ratio as a per-value or peak-live-heap ratio.
- **Coverage and uncertainty.** Include multiple sizes/distributions and unseen
  application workloads before generalizing. Sorting distributions, Unicode text,
  hash-key distributions, sharing, concurrency and I/O are not covered by one
  fixed input per algorithm. Repeat on a quiet host; retain raw samples and report
  close or unstable results as inconclusive rather than counting every ratio
  below 1 as a demonstrated win.

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
(`build --all-targets`). The separate **Benchmarks workflow** is disabled by
default and has a manual-only trigger. An intentional run after re-enabling it
publishes an informational report. It never fails on timings — only when a benchmark
crashes or (for the Fai-vs-Rust algorithm benches) computes a wrong result.

## Running the benches

### Pinned OCaml peer

The manual benchmark workflow uses `ocaml/setup-ocaml` with the exact packages
`ocaml-variants.5.5.1+options,ocaml-option-flambda`. It checks the version and
Flambda setting of the switch's exact executable, exports that path as
`FAI_BENCH_OCAMLOPT` for the benchmark harness, and uploads `ocaml-config.txt`, the
compile command, OCaml environment settings, and `rustc-version.txt` with the results. The
workflow remains manual-only and disabled by default.

For the same compiler locally:

```sh
opam switch create fai-bench-5.5.1 ocaml-variants.5.5.1+options ocaml-option-flambda
opam exec --switch=fai-bench-5.5.1 -- cargo bench -p fai-tests --bench algorithms_aot_warm
```

The Rust benchmark helpers use `FAI_BENCH_OCAMLOPT` when set, otherwise `ocamlopt`
from `PATH`. An absent default compiler skips optional OCaml comparisons; an
invalid explicit selection or a broken compiler fails the run. All builds use
`-O3` and retain `compiler-info.txt` beside the temporary executable, including
the compiler's `-config` output, `OCAMLPARAM` and `OCAMLRUNPARAM`. The optimized
modern compiler still needs the same source, input, measurement-scope and
representativeness checks described above.

### Benchmark commands

```sh
cargo bench --workspace --benches            # everything
cargo bench -p fai-tests --bench inference    # one suite
cargo bench -p fai-cli   --bench test_loop    # the end-to-end fai test loop
cargo bench -p fai-tests --bench algorithms_aot_warm -- --module Fib --size both
```

`DIVAN_MAX_TIME=<seconds>` caps the wall time per benchmarked function. The
manual CI recipe uses `120` — generous enough that even the process-spawn
benches (`algorithms_aot`, the daemon e2e, the `fai test` loop) reach divan's full
~100-sample target for steady medians, while still bounding any pathological
function. Use a shorter local cap such as `DIVAN_MAX_TIME=1` for a quick,
informational comparison.

The warm AOT sampler is separate from divan: it defaults to three passes of
21 paired samples after calibration and two warmups. Use `--samples N`,
`--passes N`, repeated `--module Name`, and `--size small|large|both`; `--list`
lists cases and `--test` validates them without emitting timing records.

## The CI Benchmarks workflow

`.github/workflows/bench.yml` is disabled in repository settings and retains only
`workflow_dispatch`. When explicitly enabled and dispatched, it runs
`cargo bench --workspace --benches` (at `DIVAN_MAX_TIME=120`), then renders the
output with the `bench-summary` tool
(`crates/fai-tests/src/bench_summary.rs`):

- A **Markdown report** is appended to the run summary. divan has no
  machine-readable output, so `bench-summary` parses its Unicode tree
  (`├─`/`╰─`/`│` and the `fastest │ slowest │ median │ mean │ samples │ iters`
  columns). Parsing is best-effort and never panics — an unrecognized line is
  skipped, so a divan format change degrades to a thinner report rather than a
  failure.
- The raw output, parsed `bench-results.json`, compiler metadata and retained
  warm-worker binaries/sources are uploaded as the `benchmark-results` artifact.
  Set `FAI_BENCH_ARTIFACT_DIR` locally to retain the workers in a chosen directory.
- A benchmark *case* label that looks like a source location (`<path>.fai#Lnn`,
  produced by the real-world language-server benches) is **linked** to the exact
  file and line on the forge, so a report row points at the code it measured.
- Warm `WARMSTAT` records are rendered first, with per-invocation time, batch size,
  floor fractions and reliability status. The JSON `warmAot` array retains every
  raw batch/floor duration and its pass metadata.
- Other comparison groups get a ratio table within their own scope. Historical
  mixed JIT/AOT control rows are diagnostic; current JIT regression output is
  Fai-only. Results from different scopes are never paired together.

To inspect a past run locally:

```sh
gh run list --workflow=bench.yml --branch=main
gh run download <run-id> -n benchmark-results -D /tmp/bench
```

## The benchmark suites

Compiler-source fixtures are preflighted once in a separate database by
`fai_tests::benchmark_fixture`, including their expected diagnostic codes.
This leaves each measured cold database unqueried. Successful-inference stress
sizes remain below the syntax limits; oversized inputs are measured explicitly
as `parse_rejected_nesting`. Shared backend, data-layer, and capability fixtures
also have ordinary CI tests, so a fast error path cannot masquerade as successful
compilation. Contract benchmarks assert their outcome succeeds.

All under `crates/fai-tests/benches/` unless noted. None is a CI gate.

| Suite | Measures |
|---|---|
| `inference` | End-to-end type inference over synthetic workspaces: `cold_check` (grows with size) vs `warm_*_edit` (should stay flat — the firewall). |
| `micro` | Inference primitives: unification on large types, deep/wide expressions, large mutually-recursive groups. |
| `stress` | Pathological inference: exponential type growth, very wide/deep structures, instantiation- and constraint-heavy bodies, error-laden files. Also isolates layout over long single-line arrays and application chains (lexing outside the timed loop). |
| `data_layer` | Inference/exhaustiveness over record- and union-heavy modules; lowering of `match`/records; structural runtime primitives (`compare`, `Float`, construction); standard Array sorting on equal, two-key, and skewed duplicate inputs. |
| `interfaces` | Inference, lowering, and JIT execution of dictionary dispatch and offset-evidence (row-polymorphic field access / capabilities). |
| `reuse` | Reuse / in-place update / borrowing: paired *unique* (cells recycled) vs *shared* (cells copied) rebuilds, and in-place vs copying record updates. |
| `codegen` | The backend pipeline (lower → reference-count → Cranelift → JIT) plus a few runtime primitives. |
| `contracts` | The in-process `fai test` loop (collect → synthesize harness → reference-count → JIT → run) over the corpus, cold and warm. |
| `daemon` | Daemon-path pieces: content-addressed cache key, run-bundle serialization, wire framing, workspace file-state sync. |
| `lsp` | Per-request language-server latency: warm `analysis_*` (the work to answer a request) and full `roundtrip_*` through the real server over an in-memory connection, for every editor feature (see below). |
| `lsp_scenarios` | Multi-step language-server *workflows*: a keystroke-incremental typing session, the type-a-character → diagnostics loop, cross-module change propagation, a rename refactor, and a typo → quick-fix (see below). |
| `algorithms_jit` | Fai-only JIT execution regression coverage, with Rust used for untimed answer validation. |
| `algorithms_aot` | Runtime comparison, delivered binaries: a `fai build` executable vs a Rust release binary vs an `ocamlopt`-compiled OCaml binary, end to end (see below). |
| `algorithms_aot_warm` | Primary compute comparison: checked persistent AOT workers for Fai/Rust/OCaml at both registered sizes, with common batches, paired sampling and explicit harness floors. |
| `algorithms_mem` | Memory comparison, delivered binaries: peak resident set size of the same `fai build` vs Rust vs OCaml binaries (see below). |
| `sort_patterns` | Build/sort/order-sensitive-checksum across ascending, descending, Fisher–Yates shuffled, equal, four-key, and partially sorted runs; both JIT compute (Fai/Rust) and AOT processes (Fai/Rust/OCaml), at 6,000 and 80,000 elements. |
| `tree_lookup` | Matched four-field binary nodes in Fai/Rust/OCaml, with explicit `rust_btree_map`/`ocaml_map` application alternatives. Separates build+lookup from lookup-only; validates full shapes and every hit/miss before timing. |
| `concurrency` | Runtime concurrency/networking (Fai-only, delivered binaries): task fan-out/join throughput, bounded-channel throughput, shared PRNG contention, CPU-bound **parallel speedup** (`FAI_WORKERS=1` vs the host default), and loopback TCP/UDP round-trip throughput (see below). |
| `test_loop` (`fai-cli`) | The supervised `edit → fai test` loop through the real `fai` binary + daemon: client → daemon → worker subprocess → JIT → run → stream back. |

## Runtime comparison: AOT Fai, Rust and OCaml

Warm and whole-process comparisons execute real native AOT binaries on every
side. The persistent-worker comparison excludes process startup; the delivered
comparison includes it. The memory comparison uses the same runtime-input entry
shape. Fai JIT is retained as a separate regression surface for tooling.

The idiomatic Rust references live in `crates/fai-tests/src/algorithms.rs`, each
paired with its Fai sample under `samples/algorithms/` and two workload sizes in
the `ALGORITHMS` registry. The suite deliberately spans many runtime shapes so a
performance change is measured broadly rather than against a handful of cases:

- **arithmetic / recursion** — `fib` (wide, non-tail), `ackermann` (deep stack),
  `collatz` and `pi` (tail loops), `prng_xorshift` (bitwise `Int` intrinsics);
- **lists** — `list_sort`, `nqueens` and `fannkuch` (sorting, persistent
  backtracking and permutations);
- **arrays and pipelines** — `map_sum`, `map_sum_shared`, `merge_sort`,
  `quicksort`, `matrix_multiply`, `fold_pipeline`, and `sieve`. Some pipelines
  fuse or simplify to arithmetic; a shared materialized source stays observable;
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

**Rust and OCaml comparison programs are always ahead-of-time compiled.** Old
"JIT versus Rust" tables measured a Rust AOT function alongside Fai JIT code in
process. They were actual measurements, but combined compilation route and
measurement-scope differences. Current cross-language headline results use AOT
on every side; the JIT suite reports Fai regression timings only.

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
- **Timing.** The separately labelled scopes below.

### `algorithms_jit` — Fai execution regression

`crates/fai-tests/benches/algorithms_jit.rs` JIT-compiles the reachable Fai closure
once in untimed setup. The timed loop applies that finished function. Rust is
called only for correctness validation. The suite remains useful for `fai run`
and contract execution; compilation/edit latency is measured by the dedicated
compiler and contract suites. JIT uses native CPU features, while the AOT peer
comparison uses the portable host ISA.

### `algorithms_aot_warm` — primary warm compute comparison

Every registered workload runs at both its smaller and larger registered sizes.
All three programs read an input window of 32 copies of that size after startup;
the contents are unknown to their compilers. Worker construction, input parsing,
raw-result validation and warmup are outside timing. Each timed batch cycles over
the same window and returns a checked checksum. This keeps each invocation's
input runtime-dependent without adding barriers inside a workload.

Calibration chooses **one batch count for every language** in a case. It doubles
from one until the fastest median reaches 5 ms, the slowest reaches 100 ms, or
1,048,576 invocations are reached. Sampling defaults to three interleaved passes
of 21 observations with alternating peer/floor order. Reported ns/invocation is
the median of pass medians divided by the common batch count.

The measured boundary includes the request round trip, input-window traversal,
checksum and response. A paired `floor` request measures that harness without
the workload. **Floors are not subtracted.** If a side's median floor is at least
10% of its elapsed time, ratios involving that side are suppressed and marked
floor-limited. Missing passes and mismatched batches also suppress comparisons.
The raw observations remain available. Very fast closed-form kernels may remain
floor-limited despite batching; those rows cannot prove compute parity.

Use the default baseline target CPU for Rust, the portable Fai AOT ISA, and the
recorded OCaml configuration; record any CPU/profile override as another
configuration. The sampler's `WARMMETA` and `AOTARTIFACT` records identify its
configuration and worker paths, while `WARMSTAT` carries raw batch and floor
durations. The CI artifact retains worker executables, source and compiler data.

### `algorithms_aot` — delivered binaries, end to end

`crates/fai-tests/benches/algorithms_aot.rs` compares the *delivered artifacts*:

- **Fai side**: built once with `build_native` (untimed), using an argv-reading
  wrapper around the workload, then **spawned** with the registered size as an
  argument in the timed loop.
- **Rust side**: spawns the `algo-baseline` release binary
  (`crates/fai-tests/src/bin/algo-baseline.rs`) as a subprocess.
- **OCaml side**: spawns the `ocamlopt`-compiled baseline
  (`crates/fai-tests/ocaml/baseline.ml`, compiled once in untimed setup by
  `fai_tests::ocaml::baseline`) as a subprocess. Skipped — with no row, not a
  failure — when `ocamlopt` is not on `PATH`, so the bench runs without OCaml
  installed; the Benchmarks workflow installs it.

Each timed iteration is a whole process: startup, the workload, print, exit.
(Skipped on Windows, which needs the MSVC environment for the build/link + spawn
path; it still compiles there so `--all-targets` keeps it from bitrotting, and the
workflow runs on Linux.)

Native worker fixtures use the same workload definitions with a persistent
line protocol. Their first line supplies a window of 1–64 nonnegative sizes;
after `ready`, `value i` returns the complete result for one input, `run n`
checksums `n` cyclic invocations, and `floor n` performs the same input/checksum
loop without the workload. Batch counts are bounded by 1,048,576. Integer
checksums use a canonical modulus to preserve full-width signed results across
OCaml's native `int` and `Int64` paths. Floating checksums retain left-to-right
addition. Malformed requests return an error and terminate; EOF ends the worker.
The Rust and generated Fai/OCaml worker tests compare varied inputs, full-width
and Float results, zero batches, errors and repeated requests.

### `algorithms_mem` — delivered binaries, peak memory

`crates/fai-tests/benches/algorithms_mem.rs` is the **memory** side of the same
delivered-binaries experiment: instead of timing the spawned processes, it records
each one's **peak resident set size** at the AOT workload. It is not a divan timing
loop (peak memory is not a per-iteration measurement); each binary is run a few
times and the maximum peak is kept.

All three sides are measured **identically by self-reporting**: with
`FAI_REPORT_RSS` set in the child's environment, the Fai runtime (`run_entry`),
the `algo-baseline` binary, and the OCaml baseline each read their own peak RSS
from `/proc/self/status` (`VmHWM`, the high-water mark) and print a
`fai-peak-rss-kib:` line to stderr. The harness parses that and emits a
`MEMSTAT\t<algorithm>\t<side>\t<kib>` line (`<side>` one of `fai`/`rust`/`ocaml`),
which `bench-summary` renders into a **"memory: Fai vs Rust + OCaml (peak RSS)"**
table (the `fai/rust` and `fai/ocaml` ratios; lower is better) and includes in
`bench-results.json`. divan's parser ignores the `MEMSTAT` lines, so they ride
safely in the shared output stream. (The OCaml rows are absent when `ocamlopt` is
not installed, collapsing the table back to Fai vs Rust.)

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

### The OCaml baseline

The delivered-binary benches add a third side: a single OCaml program,
`crates/fai-tests/ocaml/baseline.ml` (the OCaml twin of `algo-baseline`), that
dispatches on `baseline <module> <n>`, computes the algorithm, and prints the
result. `fai_tests::ocaml::baseline` compiles it **once** with `ocamlopt` into a
scratch directory and hands the path to both benches; the build is cached per
process, and a present-but-broken source is a loud panic rather than a silent
skip.

**Why OCaml.** It is a native, strict, statically typed ML-family language whose
`ocamlopt` emits native code with no VM and a sub-millisecond startup — the closest
apples-to-apples peer to Fai's own native, strict, ML-family model, so the
`fai/ocaml` ratio measures Fai's backend against a mature native FP compiler
rather than against a different runtime model. The `ocaml/rust` gap visible in the
table also anchors how a mature native FP compiler itself compares to Rust, which
contextualizes Fai's gap.

**Matched representations.** As with the Rust oracle, each OCaml implementation
uses the data representation its Fai sample uses, so the ratio reflects the
compiler/runtime rather than a data-structure mismatch: a contiguous `array` where
the Fai sample uses `Array` (and `Buffer` for incremental strings), a persistent
`list` where it uses the linked `List` (the backtracking and parser workloads —
`nqueens`, `fannkuch`, `expr_eval`), `Hashtbl` for the hash containers, and the
`Map`/`Set` functors for the ordered ones.

**Caveats** (the comparison stays a *progress metric, not a fair fight*):

- **63-bit `int`.** OCaml's native `int` is 63-bit, so the two workloads that
  depend on full 64-bit wrapping — `prng_xorshift` (u64 bit-twiddling) and
  `fib_memo` (i64 wrapping over thousands of Fibonacci sums) — use the boxed
  `Int64` module to reproduce the oracle's two's-complement result. Every other
  workload fits native `int`.
- **Optimization level.** The baseline is built with plain `ocamlopt` (array
  bounds checks on, matching Fai and Rust release; no flambda `-O3`), so this is
  not OCaml's peak achievable speed — just as Fai's portable AOT build targets a
  baseline ISA where the JIT does not.

**Correctness.** Every delivered Fai, Rust, and available OCaml binary is checked
against the registered Rust oracle at the AOT workload size. `algorithms_aot`
verifies an untimed first execution, then checks the exit status of every timed
execution. `algorithms_mem` checks the answer and status of every measurement.
The shared validator uses exact integer equality and the existing `1e-6` scaled
Float tolerance, rejecting malformed output and NaN/infinity. Failures report
stdout and stderr. Oracle computation stays outside timing. The
`ocaml_baseline_matches_oracle` integration test additionally re-checks each
algorithm wherever `ocamlopt` is available (skipping cleanly when it is not).

### Historical mixed JIT/AOT tables

Older reports timed the same Rust implementation in two different experiments.
The following explains those historical numbers; current warm AOT measures both
registered sizes without changing compilation route. Two effects compounded.

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
dynamic linker + runtime init + print + exit). Measure that floor on the actual
host; it varies with the runtime image, operating system and harness.

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

In that historical run, the size factor explains ~11–18×; the rest is the
process-spawn floor. You can see
that floor directly: in `algorithms_aot`, `map_sum` (2.0 ms), `merge_sort`
(1.26 ms), and `pi` (2.28 ms) all bottom out near 1–2 ms even though the same
compute in-process is 7–70 µs — for `merge_sort`, sorting 80k integers is tens of
microseconds, so essentially all of that 1.26 ms *is* process spawn, which is why
its cross-bench ratio (189×) is the largest. `binary_trees` does hundreds of
milliseconds of real work, so the spawn floor is negligible and its ratio (35×)
is closest to the pure size factor.

### How to read the numbers

- A Rust (or OCaml) row is a **baseline within its own bench**, paired against the
  Fai row measured the same way in that bench. The summary's ratio table pairs them
  per-group for exactly this reason, reporting `fai/rust` and (in the
  delivered-binary benches) `fai/ocaml`.
- Compare only equal sizes and measurement boundaries. `algorithms_aot_warm`
  reports warmed native batches; `algorithms_aot` reports delivered processes;
  `algorithms_jit` reports the separate Fai JIT regression path.
- Interpret the ratio according to its scope. Fai uses uniform values plus
  scalar-specialized representations, reference counting and Cranelift; Rust uses
  LLVM and its own representations. These are legitimate implementation choices,
  but they do not isolate a single compiler phase.
- **Identify the data representation.** For a matched kernel comparison, use the
  same logical data structures and operations. An application comparison can use
  idiomatic alternatives, with the differences stated explicitly. The suite mixes
  both kinds:
  - **Contiguous** where access is index-, iterate-, or build-then-traverse-heavy
    (an `Array` is then also the better Fai structure): Fai's **`Array`** against
    Rust's `Vec`. `MapSum`/`MapSumShared` build-map-fold an `Array`; `MergeSort`
    uses the standard `Array.sort`; `QuickSort` is a hand-written Lomuto quicksort
    on scrambled input, compared with the peers' library sorts;
    `MatrixMultiply`/`Levenshtein` use array-of-array and array-row DP;
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
  **`ListSort`** sorts a Fai/OCaml linked list against Rust's `Vec::sort`.
  `List.sortBy` uses private merge buffers for large inputs; converting to and from
  the linked representation remains timed. This is an application-level comparison.
  Comparing its ratio to `MergeSort` does not isolate representation cost because
  their input distributions and sizes differ. **`OptionTreeFind`** uses a Fai
  binary tree, Rust `BTreeMap` and OCaml `Map`; use `tree_lookup` for matched nodes.
  **`JsonSerialize`** has List versus Vec children, and **`GraphBFS`** has List
  versus Vec adjacency/frontiers. Their allocation/traversal differences remain
  part of the measurements. Hash-container implementations, hash functions,
  seeding and initial capacities also differ between languages; these are library
  comparisons rather than isolated structural-hash or probe-loop timings.

### Keeping the sides in lockstep

`MapSum`, `MapSumShared` and `FoldPipeline` previously had per-element optimization
barriers in Rust and OCaml, but none in Fai. Those barriers have been removed:
closed-form evaluation, vectorization and other behavior-preserving optimizations
are allowed on every side. Treat barrier-free measurements as a new methodology
version, retaining the old sources, binaries and raw results separately. A faster
baseline after this correction is not a Fai regression; the old ratios do not
establish unrestricted compiler parity.

`MapSumShared` materializes one contiguous source on every side and traverses it
twice: once for the sum of doubled elements, then for the original sum. The
mapped intermediate is fused into its consumer. The former Rust and OCaml
implementations instead used a single allocation-free arithmetic loop, so their
old timing and RSS rows are a different workload version; do not treat correcting
those baselines as a Fai speedup. `MapSum` remains the fully fused single-consumer
arithmetic workload.

`GraphBFS` includes construction of an adjacency hash dictionary for all nodes,
then dictionary lookups during level-by-level traversal on every side. The former
Rust and OCaml peers calculated neighbors directly and omitted that dictionary.
Their old runtime and RSS rows are a different workload version; the corrected
ratios measure matched graph construction and traversal, not a compiler speedup.

`JsonSerialize` builds the complete balanced tree, then renders it into one
growing output on every side. Fai threads an owned String accumulator, Rust uses
a mutable String, and OCaml uses Buffer. Earlier Fai/Rust versions recursively
materialized and joined child strings while OCaml already used a buffer. The
buffered traversal is a new workload version; retain those old rows separately
from compiler-only comparisons.

The historical `OptionTreeFind` row remains the application comparison of a Fai
binary tree against Rust `BTreeMap` and OCaml `Map`. Use **`tree_lookup`** to
isolate the matched binary-node kernel: all three insert the same 1,000 keys in
midpoint/left/right order, with identical four logical fields, values and shape.
It uses 5,000 and 100,000 queries (`i % 2000`) and a position-weighted checksum
that includes misses as `-1`; its timings therefore form a separate workload
version. In-process Fai/Rust rows measure build+lookup and lookup-only. Native
rows use persistent workers for all three languages: lookup-only builds before
the `ready` handshake and retains the tree across requests, while build+lookup
rebuilds per request. Those native timings include identical line-protocol IPC
and checksum work, but exclude process startup. The benchmark preflight compares
preorder/null shape, height, count and every query answer with the OCaml peer too.

`Levenshtein` uses a single in-place dynamic-programming row on all three sides,
carrying the old diagonal in a scalar before each overwrite. Earlier Fai timings
used a fresh row per left-sequence element and describe a different allocation
workload, despite computing the same edit distance.

Sorting results use position-weighted checksums, so removing the sort changes
the answer. The `MergeSort` checksum changed from a plain sum to this form;
measurements from the two workload versions must not be spliced into one trend.
`sort_patterns` keeps distribution and size in each row. Its untimed fixture tests
compare complete generated and sorted arrays, including partial final runs.
Its `parts` rows separate construction, sorting, and checksum costs. Construction
and sorting include releasing the output array; sort inputs are prepared outside
the timer, and checksum repeatedly borrows an already sorted input.

`IntEval` and `OptionEval` use the same strict fallback policy: both chains are
evaluated before choosing the first success. Rust and OCaml spell both evaluations
explicitly, matching Fai; instrumented untimed checks pin the source call count.
Optimizers may still remove provably unobservable work, so native code is the
authority for executed work. Older lazy-Int/lazy-OCaml measurements are a different
workload version.

Each `aot_size` must equal the literal the matching sample's `main` passes to
`run`/`runF`; the sample-validation tests (`crates/fai-tests/tests/algorithms.rs`)
assert this by comparing the example program's output to the oracle. The AOT
benchmark wrappers supply that registered size at runtime on every side. To add an algorithm: add the Rust
reference and a registry entry in `algorithms.rs`, add the `samples/algorithms/`
module with the matching baked size, add a match arm in `ocaml/baseline.ml`, add a
`validate` test in `tests/algorithms.rs`, list it in both `algorithm_benches!`
macros, and add its native-worker dispatch and `aot_benchmarks` case. The
`algorithms_mem` bench and the `algo-baseline` binary iterate the
registry directly, so they pick up the new algorithm automatically; the
`registry_is_fully_covered` test guards the hand-maintained lists — it fails if a
registered algorithm is missing from either runtime bench, from the OCaml
baseline's dispatch, or from the validation tests, and `ocaml_baseline_matches_oracle`
checks the OCaml result wherever `ocamlopt` is installed. Keep `aot_size` small
enough that running `main` once stays fast (the validation test runs it under the
JIT), especially for super-linear workloads.

## Concurrency & networking benchmarks

`crates/fai-tests/benches/concurrency.rs` measures the runtime's M:N scheduler and
I/O reactor. Unlike the algorithm benches it has **no cross-language baseline** — a
green-thread M:N runtime has no single "fair" peer (Rust threads, rayon, and an
async runtime each differ in kind) — so it is **Fai-only and informational**, and
renders as plain timing tables (the summary's ratio table only appears for
`rust`/`fai`/`ocaml` leaves).

Each workload is a small Fai program built once with `build_native` in untimed
setup and spawned in the timed loop (the `algorithms_aot` approach), with a large
baked size so process startup is amortized; the build also serves as the program's
verification (a build or run failure crashes the bench). The leaves:

- `spawn_await` — fan-out/join task throughput: spawn N tasks into a `scope` and
  sum the awaits.
- `channel` — bounded-channel throughput: one producer sends N items, the consumer
  drains and sums them.
- `random_single_worker` / `random_contended` — one million shared PRNG draws
  across four tasks, scheduled on one worker or four. Their sum must equal the
  serial xorshift oracle; timings show the cost of contention on the atomic state.
- `parallel_speedup_one_worker` / `parallel_speedup_all_workers` — the **parallel
  speedup**: the same CPU-bound, allocation-free fan-out (many tasks each summing a
  long range) run with `FAI_WORKERS=1` and with the host's default parallelism. The
  ratio of the two medians is the scheduler's speedup (process startup is constant,
  so it largely cancels in the ratio).
- `tcp_echo` / `udp_echo` — loopback request/response round-trip throughput: a
  server task echoes a one-byte message and the client sends/reads it back N times,
  driving the reactor's park/wake on every round-trip.

Like `algorithms_aot`, the suite is skipped on Windows (the build/link + spawn
path), still compiling there so `build --all-targets` keeps it from bitrotting.

## Language-server benchmarks

Two suites measure the `fai lsp` server, at two granularities. Both are
Fai-only and informational (plain timing tables), run over **two corpora** — the
deterministic synthetic corpus (parameterized by workspace size) and a
hand-written multi-module **store application** under `samples/store/` (a
`Catalog` hub many modules depend on, a widely-referenced `Catalog.label`, and a
layered dependency graph). Real-world rows are keyed by a `fai-corpus::realworld`
`Probe` whose label (`<path>#Lnn`) links each report row to the exact source line
it measured. Both benches drive the **real** server over an in-memory connection
through a shared client harness (`crates/fai-tests/benches/harness/`), so a
`roundtrip_*` timing includes the JSON-RPC transport and the cross-thread hop on
top of the analysis; the `analysis_*` variants call the `fai-ide`/`fai-driver`
query directly for the low-noise, size-scaling cost.

- **`lsp`** — each request in isolation on a warm server: hover, go-to-definition,
  diagnostics, completion (and its lazy `completionItem/resolve`), signature help,
  find-references, rename (and prepare-rename), document & workspace symbols,
  semantic tokens, inlay hints, formatting (whole-document, range, and on-type),
  and code actions.
- **`lsp_scenarios`** — multi-step workflows that capture costs the single-shot
  benches cannot: an **editing session** (typing a binding with keystroke-level
  incremental range edits, firing completion/signature-help/hover as it goes), the
  **keystroke → diagnostics** loop (range-edit sync, the realistic per-keystroke
  path), **cross-module propagation** (a breaking change to the hub's public
  signature with every dependent open, timed until all re-diagnose — the
  non-firewalled fan-out, in contrast to the flat firewalled private-body edit the
  `lsp` diagnostics benches measure), a **rename refactor** (`prepareRename` →
  `rename` of a workspace-wide symbol), and a **typo → quick-fix** cycle.

[divan]: https://docs.rs/divan
