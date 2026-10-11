# Generated performance trackers

Three durable issues report the current validated checkpoint:

- **[Perceus memory guarantees and allocation evidence](https://github.com/Krzysztof-Cieslak/fai/issues/745):** source-identical Fai
  controls, reference-count/space guards, live objects and allocator backing.
- **[Native process memory](https://github.com/Krzysztof-Cieslak/fai/issues/746):** isolated Fai/Rust/OCaml executables, raw peak RSS,
  repetition counts and same-executable floors.
- **[Native runtime](https://github.com/Krzysztof-Cieslak/fai/issues/747):** warm AOT kernel comparisons, application observations,
  and separate end-to-end medians.

The issue bodies contain one current snapshot. Previous snapshots live in
versioned evidence archives linked from refresh comments. Concrete bugs and
implementation proposals remain ordinary issues linked from the trackers.

## Run a fresh main checkpoint

```sh
python scripts/performance.py run --ocaml /path/to/ocamlopt
```

The runner fetches `origin/main` once, resolves its immutable commit, and creates
a detached measurement worktree. Compiler builds, fixtures and peers all come
from that worktree. Moving `main` during a run does not change its inputs.
The report generator must therefore already be merged before this command is used.

The complete run includes:

1. Three warm-AOT passes of 21 paired run/floor observations at both registered
   sizes, plus the matched supplemental components.
2. Three end-to-end passes with at least 100 observations per language/workload.
3. Independent memory-size curves and 1/16 complete invocations, using five fresh
   processes per side and mode. Each process is observed before the workload and
   after its result is consumed, with current and peak RSS kept distinct.
4. Optimized `allocation-stats` source-identical controls.
5. The `perceus_space` and `perceus_storage` guard targets.

Builds and measurements run serially. Uninstrumented timing/footprint binaries,
instrumented allocation binaries and assertion-enabled guards have separate
identities. Executables are copied into the snapshot and losslessly compressed
after use. Identical compressed images share disk storage through hard links when
available. The runner removes only its own disposable build directory and can
release duplicate Cargo images after retaining the executable copies.

The default destination is `target/performance/<timestamp>-<revision>`; choose
another with `--output`. Linux and `taskset` are required for the complete RSS
matrix. `--cpu` selects an allowed CPU; the chosen CPU, host, toolchains and
relevant environment are recorded. Host load, memory, CPU accounting and available
frequency/governor observations bracket each suite, without correcting or filtering
its timings. The configured OCaml version and Flambda are checked before measurement.

## Resume and render

```sh
python scripts/performance.py resume path/to/snapshot
python scripts/performance.py render path/to/snapshot
```

Resumption keeps the same revision, host configuration, compiler configuration
and measurement settings. Completed steps are retained; failed step output is
archived before that step is repeated. A failure cannot turn an absent result
into zero. Fully completed snapshots are immutable once published.

`performance-report` is the strict Rust validator/renderer in `fai-tests`. It
reuses `bench-summary` parsing and comparison eligibility. It verifies:

- Stored SHA-256 and logical BLAKE3 identities, including compressed artifacts.
- The complete source-reviewed workload matrix and all three peers.
- Source, compiler and executable-set agreement within each comparison, including
  each peer's source and Rust/OCaml compiler metadata.
- Pass/sample counts and measured floors.
- Instrumentation boundaries, allocation accounting domains, and final live
  storage matching the retained harness baseline.
- Successful execution of both memory-guard targets.

Snapshot schema 2 includes the versioned memory input catalog and requires
`MEMSTAT3` observations (`isolated-memory-v3`) with paired before/after current
RSS as well as peaks. Older snapshots retain their own renderer and source
identity; their peak-only observations do not acquire inferred after-RSS values.

The three Markdown files and normalized `results.json` are emitted only after
validation succeeds. Rendering the same snapshot and evidence URL is
deterministic. The legacy `bench-summary` command remains a best-effort console
summary; it does not authorize publication.

## Publish and migrate

```sh
python scripts/performance.py publish path/to/snapshot

# Initial setup, from a feature branch when recording issue IDs:
python scripts/performance.py publish path/to/snapshot \
  --create --migrate --record-ids
```

`scripts/performance-trackers.json` owns canonical titles, issue IDs, notes and
the explicit retirement list. The publisher can discover an existing tracker by
its unique generated marker; `--create` permits initial creation. It archives raw
logs, normalized data, source/build provenance and prior issue bodies in a
versioned gist. Full compressed executable bytes remain in the local snapshot;
their identities are included in the published manifest. `archivePublic` controls
the gist visibility.
Empty or whitespace-only logs are retained exactly in `blank-files.json`, since
GitHub rejects them as individual gist files.

Each issue body is replaced, then fetched again and verified. A short refresh
comment links the immutable evidence revision. Repeating publication is
idempotent; interrupted publication resumes from `publication.json`.
`--record-ids` updates the selected configuration file so the IDs can be committed.

`--migrate` closes only the explicitly configured superseded trackers, after all
three replacement bodies have been verified. Closing comments link the new
trackers and preserve the distinction between consolidating tracking and fixing
every performance gap. Already-closed issues remain closed; concrete work items
are not part of the retirement list.

## Interpretation

Runtime ratios require reviewed matched-kernel contracts and floor qualification.
Application rows retain absolute timings and their status. Runtime aggregate
means are equally weighted geometric means; p90 is nearest-rank across eligible
case ratios. End-to-end medians retain their startup/shutdown scope and do not
enter warm aggregates.

The generated runtime report includes an **Eligibility legend** for
`matched kernel`, `application` and `floor: <languages>`, including the pairwise
effect on F/R and F/O. The memory report explains **comparison class and floor
share** separately: both matched kernels and application cases retain total-RSS
ratios when each pair has compatible, uninstrumented observations and enough
samples. Large baseline shares remain visible; they do not exclude a process
comparison, and an unavailable third peer cannot suppress a valid pair. The
10% floor exclusion applies only to timing. Both reports explain abbreviations
and withheld-ratio dashes next to their measurement tables.

RSS is process footprint, not a common live-heap measure or a compiler-memory
win count. Perceus evidence uses its own Fai object/backing accounting domain.
All allocation controls retain reference counting; disabling reuse does not
disable RC or the runtime's ordinary uniqueness rules.

The memory matrix comes from `fai-bench-workloads/src/memory_workloads.rs`, rather
than the compute benchmark sizes. It includes the application workloads and ten
storage fixtures: Int/Float arrays, shared array snapshots, a linked list, a
complete binary tree, UTF-8 construction, distinct-key maps/sets, materialized
array-adjacency BFS and variable-count Float bodies. Whole-structure consumers
keep storage materialized without per-element optimizer barriers. Each reviewed
contract records the logical shape and remaining representation/policy differences.

The harness reads `/proc/PID/status` in the parent while each child is paused:
after setup and after workload-result consumption. The child stays alive until
the parent finishes observing it. Current RSS after work exposes retained pages;
the high-water mark remains a peak and cannot establish reclamation by itself.
No forced GC, allocator trimming or baseline subtraction changes the natural
language/runtime policy. The issue shows the largest input's before/after values
at each repetition count; every size's paired observations remain in the archive.

Presentation and validation tests live under
`crates/fai-tests/src/bench_summary/tracking/`; publisher/retention tests are in
`scripts/tests/test_performance.py` and run without GitHub credentials.
