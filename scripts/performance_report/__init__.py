"""Pure presentation of versioned, validated performance-report data."""

SCHEMA_VERSION = 1
REPORTS = ("perceus-memory.md", "language-memory.md", "language-runtime.md")
TITLES = {"perceus": "Memory guarantees and allocation evidence",
          "memory": "Memory footprint: Fai, Rust and OCaml",
          "runtime": "Runtime performance: Fai, Rust and OCaml"}
NAMES = {
    "Fib": "Recursive Fibonacci", "Collatz": "Collatz sequences", "MapSum": "Map and sum",
    "MergeSort": "Array merge sort", "BinaryTrees": "Binary-tree construction", "Pi": "Pi approximation",
    "DictHistogram": "Histogram", "WordCount": "Word lengths", "MapSumShared": "Two passes over a shared array",
    "SetDedup": "Set deduplication", "FoldPipeline": "Composed arithmetic pipeline", "InterfaceDispatch": "Interface dispatch",
    "Particles": "Particle simulation", "VecMat": "Vector and matrix arithmetic", "NQueens": "N-queens search",
    "MatrixMultiply": "Integer matrix multiplication", "FloatMatrixMultiply": "Float matrix multiplication",
    "Levenshtein": "Edit distance", "GameOfLife": "Game of Life", "SpectralNorm": "Spectral norm",
    "Mandelbrot": "Mandelbrot calculation", "Ackermann": "Ackermann recursion", "PrngXorshift": "Xorshift random sequence",
    "ExprEval": "Expression parsing and evaluation", "GraphBFS": "Breadth-first graph search", "CoinChange": "Coin-change calculation",
    "FibMemo": "Memoized Fibonacci", "QuickSort": "Array quicksort", "Sieve": "Prime sieve", "NBody": "N-body simulation",
    "Fannkuch": "Permutation flipping", "UnionFind": "Union-find", "JsonSerialize": "JSON serialization",
    "StringBuild": "String building", "StringSlice": "Retained string prefixes", "OptionEval": "Optional-result evaluation",
    "IntEval": "Integer-result evaluation", "OptionPath": "Optional path lookup", "OptionTreeFind": "Binary-tree lookup",
    "ListSort": "Linked-list merge sort", "TailQuickSort": "Quicksort detail", "TailBuildAscii": "ASCII string building",
    "TailBuildUnicode": "Unicode string building", "TailLengthAscii": "Repeated ASCII length", "TailLengthUnicode": "Repeated Unicode length",
    "TailViewsAscii": "Retained ASCII prefixes", "TailViewsUnicode": "Retained Unicode prefixes",
    "MemoryArrayInt": "Integer array storage", "MemoryArrayFloat": "Float array storage", "MemorySharedArrays": "Shared array snapshots",
    "MemoryList": "Linked-list storage", "MemoryTree": "Binary-tree storage", "MemoryUtf8": "Unicode buffer storage",
    "MemoryHistogram": "Growing histogram", "MemorySet": "Growing set", "MemoryGraph": "Graph storage and traversal",
    "MemoryBodies": "Growing body collection",
}
MEMORY_FILES = dict(zip(
    ("MemoryArrayInt", "MemoryArrayFloat", "MemorySharedArrays", "MemoryList", "MemoryTree", "MemoryUtf8", "MemoryHistogram", "MemorySet", "MemoryGraph", "MemoryBodies"),
    ("array_int", "array_float", "shared_arrays", "list", "tree", "utf8", "histogram", "set", "graph", "bodies")))
CATEGORIES = (
    ("matched-kernel", "Shared algorithms", "The same algorithms and logical data structures in all three languages. Compiler optimizations and runtime representations may differ."),
    ("application", "Standard-library workloads", "The same tasks using each language's collection and library implementations. Results include differences in library design, such as hashing, storage and growth policies."),
)
LANGUAGES = {"fai": "Fai", "rust": "Rust", "ocaml": "OCaml"}
OMISSIONS = {
    "missing-peer": "a comparison language is unavailable",
    "incomplete": "the measurements are incomplete",
    "different-algorithm": "the workload does not share a reviewed algorithm",
    "overhead": "measurement overhead reached the 10% limit",
}


class Document:
    def __init__(self):
        self.parts = []

    def write(self, text):
        self.parts.append(text)

    def line(self, text=""):
        self.write(text + "\n")

    def details(self, title):
        self.write(f"<details>\n<summary>{title}</summary>\n\n")

    def end_details(self):
        self.write("\n</details>\n\n")

    def row(self, *cells):
        self.line("| " + " | ".join(map(str, cells)) + " |")

    def text(self):
        return "".join(self.parts)


def number(value):
    return f"{value:,}"


def decimal(value):
    return f"{value:.2f}".rstrip("0").rstrip(".")


def duration(ns):
    if ns is None:
        return "—"
    for scale, unit in ((1e9, "s"), (1e6, "ms"), (1e3, "µs"), (1, "ns")):
        if ns >= scale or scale == 1:
            return "<0.01 ns" if 0 < ns < 0.005 else f"{decimal(ns / scale)} {unit}"


def byte_size(value):
    for scale, unit in ((1024**3, "GiB"), (1024**2, "MiB"), (1024, "KiB"), (1, "B")):
        if value >= scale or scale == 1:
            return f"{decimal(value / scale)} {unit}"


def memory(value, unit=True):
    return "—" if value is None else decimal(value / 1024) + (" MiB" if unit else "")


def ratio(value):
    return "—" if value is None else f"{value:.3f}×"


def overhead(value):
    if value is None:
        return "—"
    percent = 100 * value
    if 0 < percent < 0.05:
        return "<0.1%"
    if 9.95 <= percent < 10:
        return "<10%"
    return f"{percent:.1f}%"


def benchmark(name, compact=False):
    return f"[{name}]" if compact else f"[{NAMES.get(name, name)}][{name}]"


def selected(cases, category):
    return sorted((row for row in cases if row["classification"] == category),
                  key=lambda row: (row["benchmark"], row["input"], row.get("invocations", 0)))


def largest_inputs(cases):
    result = {}
    for row in cases:
        result[row["benchmark"]] = max(result.get(row["benchmark"], row["input"]), row["input"])
    return result


def header(doc, meta, key, title, evidence):
    revision = meta["measuredRevision"]
    doc.write(f"<!-- fai-performance-tracker:{key} -->\n# {title}\n\n"
              f"**Measured:** {meta['completedAt'].split('T')[0]} · **Source:** "
              f"[`{revision[:12]}`](https://github.com/{meta['repository']}/tree/{revision})")
    if evidence:
        doc.write(f" · [Data and reproducibility]({evidence})")
    doc.write("\n\n")


def introduction(doc, measure):
    doc.write(f"**Lower ratios favor Fai.** Each ratio is Fai ÷ the comparison language: "
              f"**0.80× means 20% less {measure}**; **1.20× means 20% more**.\n\n")
    doc.write("[Shared algorithms](#shared-algorithms) · [Standard-library workloads](#standard-library-workloads) · [Measurement details](#measurement-details)\n\n")


def summary(doc, values):
    doc.write("### Summary — all measured inputs\n\n| Compared with | Cases used | Geometric mean | Arithmetic mean | 90th percentile |\n|---|---:|---:|---:|---:|\n")
    for side in ("rust", "ocaml"):
        value = values[side]
        doc.row(LANGUAGES[side], f"{number(value['used'])} / {number(value['total'])}",
                ratio(value["geometricMean"]), ratio(value["arithmeticMean"]), ratio(value["p90"]))
    doc.write("\nEach included case has equal weight. The geometric mean summarizes multiplicative ratios; the arithmetic mean averages them directly. The 90th percentile is the ratio at or below which 90% of the included cases fall.\n\n")


def finish(doc, meta, key, evidence, method):
    doc.write("## Measurement details\n\n")
    doc.details("Methods, environment and reproducibility")
    doc.write(method + "\n\n")
    machine, tools = meta["machine"], meta["toolchains"]
    doc.write(f"- **Snapshot:** `{meta['runId']}`\n- **Completed:** {meta['completedAt']}\n"
              f"- **Host:** {machine.get('system', 'unspecified')} / {machine.get('machine', 'unspecified')}\n"
              f"- **CPU:** {machine.get('cpu', 'unspecified')}\n- **Rust compiler:** {tools.get('rustVersion', 'unspecified')}\n")
    if key != "perceus":
        doc.line(f"- **OCaml:** {tools.get('ocamlVersion', 'unspecified')} with Flambda and `-O3`")
    doc.line(f"- **Measurement protocol:** `{meta['memoryMethodology'] if key == 'memory' else meta['methodology']}`")
    if evidence:
        doc.write(f"\n[Raw observations, executable identities and reproduction record]({evidence})\n")
    if key != "perceus":
        doc.write("\nBenchmark names link to the measured source. Detailed tables use those source identifiers.\n")
    doc.write("\nThe evidence includes all inputs, compiler settings, workload versions and implementation notes.\n")
    doc.end_details()
    if meta["notes"].get(key):
        doc.details("Follow-up investigations")
        for note in meta["notes"][key]:
            doc.line("- " + note)
        doc.end_details()
    doc.write("---\nGenerated from validated measurements. Earlier reports are linked from the issue comments.\n")


def sources(doc, meta, names):
    doc.line()
    for name in sorted(set(names)):
        if name in MEMORY_FILES:
            path = f"crates/fai-bench-workloads/src/memory_workloads/{MEMORY_FILES[name]}.fai"
        elif name == "TailQuickSort":
            path = "samples/algorithms/QuickSort.fai"
        elif name.startswith("Tail"):
            path = "crates/fai-bench-workloads/src/tail_components/strings.fai"
        else:
            path = f"samples/algorithms/{name}.fai"
        doc.line(f"[{name}]: https://github.com/{meta['repository']}/blob/{meta['measuredRevision']}/{path} \"{name}\"")


def timing_table(doc, cases, ratios, compact):
    if ratios:
        doc.write("| Benchmark | Input size | Fai | Rust | OCaml | Time vs Rust | Time vs OCaml |\n|---|---:|---:|---:|---:|---:|---:|\n")
    else:
        doc.write("| Benchmark | Input size | Fai | Rust | OCaml |\n|---|---:|---:|---:|---:|\n")
    for row in cases:
        cells = [benchmark(row["benchmark"], compact), number(row["input"])]
        cells += [duration(row["languages"].get(side, {}).get("timeNs")) for side in LANGUAGES]
        if ratios:
            cells += [ratio(row["ratios"].get(side)) for side in ("rust", "ocaml")]
        doc.row(*cells)
    doc.line()
    if ratios:
        reasons = sorted({OMISSIONS[reason] for row in cases for reason in row["omissions"].values()})
        if reasons:
            doc.write("**Omitted ratios (—):** " + "; ".join(reasons) + ". These comparisons are excluded from the summary; overhead measurements are listed in the details.\n\n")


def timing_overhead(doc, cases):
    doc.write("### Timing overhead\n\nShare of elapsed time used by the measurement harness, measured independently. Shared-algorithm ratios require **less than 10%** on both sides.\n\n| Benchmark | Input size | Fai overhead | Rust overhead | OCaml overhead |\n|---|---:|---:|---:|---:|\n")
    for row in cases:
        doc.row(benchmark(row["benchmark"], True), number(row["input"]),
                *(overhead(row["languages"].get(side, {}).get("overheadFraction")) for side in LANGUAGES))
    doc.line()


def timing_gaps(doc, cases):
    gaps = sorted(((value, row["benchmark"], row["input"], side)
                   for row in cases for side, value in row["ratios"].items() if value is not None and value > 1),
                  key=lambda row: (-row[0], row[1], row[2], row[3]))
    if gaps:
        doc.write("### Largest timing gaps\n\nCases where Fai takes more time, across all measured inputs.\n\n| Benchmark | Input size | Compared with | Relative time |\n|---|---:|---|---:|\n")
        for value, name, size, side in gaps[:6]:
            doc.row(benchmark(name, True), number(size), LANGUAGES[side], ratio(value))
        doc.line()


def runtime_report(data, evidence):
    doc, meta, runtime = Document(), data["measurement"], data["runtime"]
    header(doc, meta, "runtime", TITLES["runtime"], evidence)
    doc.write("Time taken by native compiled programs after startup. Program startup and shutdown are reported separately below.\n\n")
    introduction(doc, "time")
    for category, title, description in CATEGORIES:
        cases = selected(runtime["cases"], category)
        largest = largest_inputs(cases)
        doc.write(f"## {title}\n\n{description}\n\n**Coverage:** {number(len(largest))} benchmarks · {number(len(cases))} input cases.\n\n")
        ratios = category == "matched-kernel"
        if ratios:
            summary(doc, runtime["summary"])
        else:
            doc.write("These timings include the selected libraries' algorithms and policies. They are presented as complete workloads.\n\n")
        doc.write("### At a glance\n\nLargest registered input for each benchmark. The summary above, where present, uses **all measured inputs**.\n\n")
        timing_table(doc, [row for row in cases if row["input"] == largest[row["benchmark"]]], ratios, False)
        doc.details("All input sizes and timing overhead")
        doc.write("### All input sizes\n\n")
        timing_table(doc, cases, ratios, True)
        timing_overhead(doc, cases)
        if ratios:
            timing_gaps(doc, cases)
        doc.end_details()
        doc.details("Including program startup and shutdown")
        doc.write("### Complete process time\n\nMedian time for one complete process, including startup, the workload, output and shutdown. Each value is the median of three pass medians, with at least 100 observations per pass.\n\n| Benchmark | Input size | Fai | Rust | OCaml |\n|---|---:|---:|---:|---:|\n")
        for row in selected(runtime["endToEnd"], category):
            doc.row(benchmark(row["benchmark"], True), number(row["input"]), *(duration(row["timesNs"].get(side)) for side in LANGUAGES))
        doc.line()
        doc.end_details()
    doc.write("## Additional algorithm measurements\n\nThese focused string and sorting measurements are separate from the main summary.\n\n")
    doc.details("String and sorting results")
    timing_table(doc, runtime["components"], True, True)
    timing_overhead(doc, runtime["components"])
    doc.end_details()
    settings = meta["settings"]
    method = (f"All three languages run native ahead-of-time compiled programs in isolated workers. Inputs are supplied at runtime. Timings are the median of {settings['warmPasses']} pass medians, each with {settings['warmSamples']} paired observations after warmup.\n\n"
              "The timing overhead is measured by running the same harness without the workload. It is never subtracted. A time ratio is omitted when either language's overhead reaches 10% of its elapsed time. Cases used in the summary are counted separately for each comparison language.\n\n"
              "Startup/shutdown timings include a fresh process, its workload, output and exit; they contribute no weight to the running-time summary.")
    finish(doc, meta, "runtime", evidence, method)
    sources(doc, meta, (row["benchmark"] for row in runtime["cases"] + runtime["components"]))
    return doc.text()


def memory_table(doc, cases, detailed):
    if detailed:
        doc.write("| Benchmark | Input size | Invocations | Fai | Rust | OCaml | Memory vs Rust | Memory vs OCaml |\n|---|---:|---:|---:|---:|---:|---:|---:|\n")
    else:
        doc.write("| Benchmark | Input size | Fai | Rust | OCaml | Memory vs Rust | Memory vs OCaml |\n|---|---:|---:|---:|---:|---:|---:|\n")
    for row in cases:
        cells = [benchmark(row["benchmark"], detailed), number(row["input"])]
        if detailed:
            cells.append(number(row["invocations"]))
        cells += [memory(row["languages"].get(side, {}).get("peakKib")) for side in LANGUAGES]
        cells += [ratio(row["ratios"].get(side)) for side in ("rust", "ocaml")]
        doc.row(*cells)
    doc.line()
    if any(row["ratios"].get(side) is None for row in cases for side in ("rust", "ocaml")):
        doc.write("**Omitted ratios (—):** the pair has missing, incompatible, instrumented or insufficient observations. It is excluded from the summary.\n\n")


def residency(doc, cases, largest):
    doc.write("### Before and after execution\n\nLargest input at each repetition count, **MiB throughout**. Arrows show the ready process → the same process after consuming its result. The baseline column shows harness-only peak → after values, in **Fai / Rust / OCaml** order. These are maximum observations; the raw records retain each paired process reading.\n\n| Benchmark | Input size | Invocations | Fai before → after | Rust before → after | OCaml before → after | Baseline peak → after (Fai / Rust / OCaml) |\n|---|---:|---:|---:|---:|---:|---|\n")
    for row in cases:
        if row["input"] != largest[row["benchmark"]]:
            continue
        def pair(side, first, second):
            values = row["languages"].get(side, {})
            return f"{memory(values.get(first), False)} → {memory(values.get(second), False)}"
        doc.row(benchmark(row["benchmark"], True), number(row["input"]), number(row["invocations"]),
                *(pair(side, "beforeKib", "afterKib") for side in LANGUAGES),
                " / ".join(pair(side, "baselinePeakKib", "baselineAfterKib") for side in LANGUAGES))
    doc.line()


def memory_report(data, evidence):
    doc, meta, values = Document(), data["measurement"], data["memory"]
    header(doc, meta, "memory", TITLES["memory"], evidence)
    doc.write("Peak RAM used by each complete process (RSS), including program code, the runtime and allocator storage. Memory after execution is available in the details.\n\n")
    introduction(doc, "memory")
    repetitions = min(meta["settings"]["memoryRepeats"])
    for category, title, description in CATEGORIES:
        cases = selected(values["cases"], category)
        largest = largest_inputs(cases)
        doc.write(f"## {title}\n\n{description}\n\n**Coverage:** {number(len(largest))} benchmarks · {number(len(cases))} input/repetition cases · {number(meta['settings']['memorySamples'])} fresh processes per language and mode.\n\n")
        summary(doc, values["summaries"][category])
        doc.write(f"### At a glance\n\nLargest registered input for each benchmark, with **{number(repetitions)} invocation{'' if repetitions == 1 else 's'} per process**. The summary above uses **all measured inputs and repetition counts**.\n\n")
        memory_table(doc, [row for row in cases if row["input"] == largest[row["benchmark"]] and row["invocations"] == repetitions], False)
        doc.details("All inputs, repetitions and memory details")
        doc.write("### All input sizes\n\nPeak process memory. Invocations are complete runs of the workload within each fresh process.\n\n")
        memory_table(doc, cases, True)
        residency(doc, cases, largest)
        doc.write("### Input definitions\n\nFixed-state cases increase work without growing their resident dataset. Storage cases increase the dataset itself.\n\n| Benchmark | Input unit | What grows | Registered sizes |\n|---|---|---|---|\n")
        for row in sorted((row for row in values["inputs"] if row["classification"] == category), key=lambda row: row["benchmark"]):
            doc.row(benchmark(row["benchmark"], True), row["unit"], row["scale"], "; ".join(map(number, row["sizes"])))
        doc.line()
        if category == "matched-kernel":
            doc.write("A MemoryTree depth of d means 2^d − 1 nonempty nodes. Each MemoryUtf8 chunk is nine bytes and four Unicode characters. Array/body inputs count elements.\n\n")
        doc.end_details()
    method = (f"Each displayed peak is the maximum across {meta['settings']['memorySamples']} fresh processes, in MiB (1 MiB = 1,048,576 bytes). Ratios and summary statistics use the unrounded measurements.\n\n"
              "The same executable also runs its harness without the workload to measure a baseline. Baselines are never subtracted. A large baseline share still represents a valid total process-footprint comparison. Memory ratios require compatible, uninstrumented measurements and at least five observations per language.\n\n"
              "A parent observes each process before the workload and after its result is consumed. Garbage collection and allocator policies run normally, without forced collection or trimming. Memory retained after execution can include unreachable values or cached storage; these RSS observations are distinct from the ownership and live-storage guarantees in the Perceus report.")
    finish(doc, meta, "memory", evidence, method)
    sources(doc, meta, (row["benchmark"] for row in values["cases"]))
    return doc.text()


def allocation_report(data, evidence):
    doc, meta, values = Document(), data["measurement"], data["allocations"]
    header(doc, meta, "perceus", TITLES["perceus"], evidence)
    doc.write("Perceus is Fai's reference-counting and storage-reuse model. These checks verify ownership, reuse and memory release within Fai.\n\n## Memory guarantees\n\n**All required memory checks passed.**\n\n| Check suite | Tests passed | What it checks |\n|---|---:|---|\n")
    for key, label, scope in (("perceus_space", "Live memory and ownership", "Reference balance, reuse, sharing and resource lifetimes"),
                              ("perceus_storage", "Retained runtime storage", "Bounded allocator storage and release of large buffers")):
        doc.row(label, number(values["guards"][key]), scope)
    names = {"Fusion": "Sequence fusion", "Scalars": "Scalar replacement", "SharedList": "Shared list", "UniqueList": "Unique list"}
    cases = sorted(values["cases"], key=lambda row: (row["benchmark"], row["input"]))
    largest = largest_inputs(cases)
    def pair(row, key):
        fmt = number if key == "requests" else byte_size
        return f"{fmt(row['disabled'][key])} → {fmt(row['enabled'][key])}"
    doc.write("\n## Effect of memory optimizations\n\nArrows mean **optimization disabled → enabled** on the same source and input. Reference counting stays active in both runs. This overview uses each benchmark's largest input.\n\n| Benchmark | Input size | Allocation requests | Extra peak live memory | Retained runtime storage |\n|---|---:|---:|---:|---:|\n")
    for row in cases:
        if row["input"] == largest[row["benchmark"]]:
            doc.row(names.get(row["benchmark"], row["benchmark"]), number(row["input"]), pair(row, "requests"), pair(row, "extraPeakLiveBytes"), pair(row, "retainedStorageBytes"))
    doc.write("\nExtra live memory is above the retained harness baseline. Runtime storage includes allocator-owned memory even after its objects are released. Every measured run finishes with its baseline live-object count and live bytes.\n\n")
    doc.details("All inputs and allocation counters")
    doc.write("| Benchmark | Input size | Disabled control | Allocation requests | Allocated memory | Extra peak live memory | Peak runtime storage | Retained runtime storage |\n|---|---:|---|---:|---:|---:|---:|---:|\n")
    for row in cases:
        doc.row(names.get(row["benchmark"], row["benchmark"]), number(row["input"]), row["disabledControl"],
                *(pair(row, key) for key in ("requests", "allocatedBytes", "extraPeakLiveBytes", "peakStorageBytes", "retainedStorageBytes")))
    doc.end_details()
    method = ("Each pair uses identical Fai source, compiler and input; only the named optimization changes. The measurements use an optimized build with allocation counters.\n\n"
              "The accounting domain is `fai-object-storage-v1`. Allocation requests include headers and pooled allocations; reset/reuse avoids a request. Live bytes exclude dead recycling cells. Runtime storage includes owned slabs, mappings and system allocations. Host-language temporaries, allocator arenas and OS page rounding are outside these counters. These figures are distinct from whole-process RSS and cross-language memory comparisons.")
    finish(doc, meta, "perceus", evidence, method)
    return doc.text()


def render(data, evidence=""):
    """Render all three documents without changing data or recalculating statistics."""
    if data.get("schemaVersion") != SCHEMA_VERSION:
        raise ValueError("unsupported report-data schema")
    if evidence and (not evidence.startswith("https://") or any(c.isspace() or c in "()<>" for c in evidence)):
        raise ValueError("evidence URL must be HTTPS without Markdown delimiters")
    reports = dict(zip(REPORTS, (allocation_report(data, evidence), memory_report(data, evidence), runtime_report(data, evidence))))
    if any(len(text.encode()) > 60_000 for text in reports.values()):
        raise ValueError("generated body exceeds the publication budget")
    return reports
