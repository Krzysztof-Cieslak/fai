# Generated performance trackers

Three durable issues report the current validated checkpoint:

- **[Perceus memory guarantees and allocation evidence](https://github.com/Krzysztof-Cieslak/fai/issues/745):** source-identical Fai
  controls, reference-count/space guards, live objects and allocator backing.
- **[Memory footprint](https://github.com/Krzysztof-Cieslak/fai/issues/746):** isolated Fai/Rust/OCaml executables, raw peak RSS,
  repetition counts and same-executable floors.
- **[Runtime performance](https://github.com/Krzysztof-Cieslak/fai/issues/747):** warm AOT algorithm comparisons, library workloads,
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

`performance-report` is the strict Rust validator and data exporter in `fai-tests`. It
reuses `bench-summary` parsing and comparison eligibility. It verifies:

- Stored SHA-256 and logical BLAKE3 identities, including compressed artifacts.
- The complete source-reviewed workload matrix and all three peers.
- Source, compiler and executable-set agreement within each comparison, including
  each peer's source and Rust/OCaml compiler metadata.
- Pass/sample counts and measured floors.
- Untimed boundary-input answers, tied to the exact warm-worker executable hashes.
- Instrumentation boundaries, allocation accounting domains, and final live
  storage matching the retained harness baseline.
- Successful execution of both memory-guard targets.

Snapshot schema 2 includes the versioned memory input catalog and requires
`MEMSTAT3` observations (`isolated-memory-v3`) with paired before/after current
RSS as well as peaks. Older snapshots retain their own renderer and source
identity; their peak-only observations do not acquire inferred after-RSS values.

After validation, `performance-report export SNAPSHOT OUTPUT` writes versioned
`report-data.json` and normalized `results.json`. The standalone Python package
`scripts/performance_report/` formats the three Markdown files from that data.
Ratios, omission reasons and statistics are computed in Rust; names, units,
wording and layout are Python presentation. Rendering the same data and evidence
URL is deterministic. The legacy `bench-summary` command remains a best-effort
console summary; it does not authorize publication.

Each workload's `validation.json` retains raw answers for its predeclared input
window; these run in separate processes before timed workers start. The catalog
records algorithm/materialization contracts and per-pair implementation notes.
Changing those contracts requires a new workload version and a fresh measurement;
an updated renderer cannot promote historical observations into the revised set.

A presentation-only preview needs Python alone:

```sh
python scripts/performance_report path/to/report-data.json /tmp/report-preview
python -m unittest discover -s scripts/performance_report/tests -v
```

The two renderer fixtures cover complete reports and omitted comparisons.
Formatting-only edits use Python-only CI and leave the compiler fingerprint
unchanged. Evidence/calculation changes retain their Rust checks.

`performance.py render` and `publish` revalidate with the retained exporter and
use the current Python presentation. An older snapshot can select a compatible
exporter with `--validator PATH --validator-revision FULL_SHA`; that fingerprinted
override is retained for subsequent refreshes. `render-info.json` records the
snapshot/data hashes, validator identity, Python version and presentation source
identity. Publication archives a new presentation revision while retaining the
original measurement manifest and executable. Earlier reports stay reproducible.

## Publish and migrate

```sh
python scripts/performance.py publish path/to/snapshot

# Initial setup, from a feature branch when recording issue IDs:
python scripts/performance.py publish path/to/snapshot \
  --create --migrate --record-ids
```

`scripts/performance-trackers.json` owns issue IDs, measurement settings, notes and
the explicit retirement list. Default titles live with the Python presentation;
a configuration can override them. The publisher can discover an existing tracker by
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

The runtime report has independent **Shared algorithms** and **Standard-library
workloads** sections, each with its own coverage, running-time measurements and
complete-process table. Shared-algorithm ratios require reviewed contracts and
overhead qualification. Their summary shows arithmetic and geometric means plus
nearest-rank p90 across included, unrounded case ratios. Library workloads retain
absolute timings and overhead measurements in the details; their native-library and
representation choices are described explicitly. End-to-end medians retain their
startup/shutdown scope and do not enter warm aggregates. Supplemental matched
components have an expandable section and contribute no main-summary weight.

The memory report uses the same two categories. Each owns its summary statistics,
paired coverage, peak-RSS table, post-workload residency table and input axes.
There is no blended all-case average: application ratios contribute only to the
application summary, and kernel ratios only to the kernel summary. Every
workload/input/repetition case within a category has equal weight; workloads with
more declared cases therefore receive more weight. P90 describes the distribution
of case ratios, not a percentile of individual RSS samples. These are descriptive
process-footprint summaries, not compiler-memory win counts.

The display labels correspond to the stable machine-readable `matched-kernel`
and `application` tags. Each section explains its scope once. Main tables omit
classification/status columns and use explicit **Time vs Rust/OCaml** or
**Memory vs Rust/OCaml** headings. A short note explains any omitted time ratios;
the paired overhead observations live in expandable details. The 10% overhead
exclusion applies only to timing. Both memory categories retain total-RSS ratios
when their pairs have compatible, uninstrumented observations and enough samples.
An unavailable third peer cannot suppress a valid pair.

The initial view shows readable benchmark names linked to source, grouped input
numbers and human-sized time/memory units. Its overview consistently selects the
largest registered input per benchmark; memory additionally selects the lowest
registered repetition count (one invocation in the current matrix). **Summary
statistics still cover all declared inputs and repetitions.** Expandable tables
retain every case, timing overhead, complete-process measurements, before/after
RSS, input definitions and reproducibility metadata. Detailed tables use the
source identifiers. Ratios are always calculated from unrounded observations,
independently of the display units. Perceus controls likewise show a largest-input
overview with all inputs and counters in details.

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

The two presentation fixtures live under `scripts/performance_report/tests/`.
Evidence and calculation tests live under
`crates/fai-tests/src/bench_summary/tracking/`; publisher/retention tests are in
`scripts/tests/test_performance.py` and run without GitHub credentials.
