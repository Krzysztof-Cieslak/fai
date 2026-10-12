//! Reader-facing benchmark names, source links and measurement units.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::Snapshot;

pub(super) fn name(id: &str) -> &str {
    match id {
        "Fib" => "Recursive Fibonacci",
        "Collatz" => "Collatz sequences",
        "MapSum" => "Map and sum",
        "MergeSort" => "Array merge sort",
        "BinaryTrees" => "Binary-tree construction",
        "Pi" => "Pi approximation",
        "DictHistogram" => "Histogram",
        "WordCount" => "Word lengths",
        "MapSumShared" => "Two passes over a shared array",
        "SetDedup" => "Set deduplication",
        "FoldPipeline" => "Composed arithmetic pipeline",
        "InterfaceDispatch" => "Interface dispatch",
        "Particles" => "Particle simulation",
        "VecMat" => "Vector and matrix arithmetic",
        "NQueens" => "N-queens search",
        "MatrixMultiply" => "Integer matrix multiplication",
        "FloatMatrixMultiply" => "Float matrix multiplication",
        "Levenshtein" => "Edit distance",
        "GameOfLife" => "Game of Life",
        "SpectralNorm" => "Spectral norm",
        "Mandelbrot" => "Mandelbrot calculation",
        "Ackermann" => "Ackermann recursion",
        "PrngXorshift" => "Xorshift random sequence",
        "ExprEval" => "Expression parsing and evaluation",
        "GraphBFS" => "Breadth-first graph search",
        "CoinChange" => "Coin-change calculation",
        "FibMemo" => "Memoized Fibonacci",
        "QuickSort" => "Array quicksort",
        "Sieve" => "Prime sieve",
        "NBody" => "N-body simulation",
        "Fannkuch" => "Permutation flipping",
        "UnionFind" => "Union-find",
        "JsonSerialize" => "JSON serialization",
        "StringBuild" => "String building",
        "StringSlice" => "Retained string prefixes",
        "OptionEval" => "Optional-result evaluation",
        "IntEval" => "Integer-result evaluation",
        "OptionPath" => "Optional path lookup",
        "OptionTreeFind" => "Binary-tree lookup",
        "ListSort" => "Linked-list merge sort",
        "TailQuickSort" => "Quicksort detail",
        "TailBuildAscii" => "ASCII string building",
        "TailBuildUnicode" => "Unicode string building",
        "TailLengthAscii" => "Repeated ASCII length",
        "TailLengthUnicode" => "Repeated Unicode length",
        "TailViewsAscii" => "Retained ASCII prefixes",
        "TailViewsUnicode" => "Retained Unicode prefixes",
        "MemoryArrayInt" => "Integer array storage",
        "MemoryArrayFloat" => "Float array storage",
        "MemorySharedArrays" => "Shared array snapshots",
        "MemoryList" => "Linked-list storage",
        "MemoryTree" => "Binary-tree storage",
        "MemoryUtf8" => "Unicode buffer storage",
        "MemoryHistogram" => "Growing histogram",
        "MemorySet" => "Growing set",
        "MemoryGraph" => "Graph storage and traversal",
        "MemoryBodies" => "Growing body collection",
        _ => id,
    }
}

pub(super) fn benchmark(id: &str, compact: bool) -> String {
    if compact { format!("[{id}]") } else { format!("[{}][{id}]", name(id)) }
}

pub(super) fn number(value: impl ToString) -> String {
    let value = value.to_string();
    let (sign, digits) = value.strip_prefix('-').map_or(("", value.as_str()), |v| ("-", v));
    let mut out = sign.to_owned();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn decimal(value: f64) -> String {
    format!("{value:.2}").trim_end_matches('0').trim_end_matches('.').to_owned()
}

pub(super) fn duration(ns: f64) -> String {
    let (value, unit) = if ns >= 1_000_000_000.0 {
        (ns / 1_000_000_000.0, "s")
    } else if ns >= 1_000_000.0 {
        (ns / 1_000_000.0, "ms")
    } else if ns >= 1_000.0 {
        (ns / 1_000.0, "µs")
    } else {
        (ns, "ns")
    };
    if value > 0.0 && value < 0.005 {
        "<0.01 ns".into()
    } else {
        format!("{} {unit}", decimal(value))
    }
}

pub(super) fn bytes(bytes: u64) -> String {
    let (value, unit) = if bytes >= 1024 * 1024 * 1024 {
        (bytes as f64 / (1024.0 * 1024.0 * 1024.0), "GiB")
    } else if bytes >= 1024 * 1024 {
        (bytes as f64 / (1024.0 * 1024.0), "MiB")
    } else if bytes >= 1024 {
        (bytes as f64 / 1024.0, "KiB")
    } else {
        (bytes as f64, "B")
    };
    format!("{} {unit}", decimal(value))
}

pub(super) fn memory(kib: u64) -> String {
    format!("{} MiB", decimal(kib as f64 / 1024.0))
}

pub(super) fn ratio(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{value:.3}×"))
}

pub(super) fn overhead(floor: f64, time: f64) -> String {
    let percent = 100.0 * floor / time;
    if percent > 0.0 && percent < 0.05 {
        "<0.1%".into()
    } else if (9.95..10.0).contains(&percent) {
        "<10%".into()
    } else {
        format!("{percent:.1}%")
    }
}

pub(super) fn language(side: &str) -> &str {
    match side {
        "fai" => "Fai",
        "rust" => "Rust",
        "ocaml" => "OCaml",
        _ => side,
    }
}

fn source_path(id: &str) -> String {
    let memory = match id {
        "MemoryArrayInt" => Some("array_int"),
        "MemoryArrayFloat" => Some("array_float"),
        "MemorySharedArrays" => Some("shared_arrays"),
        "MemoryList" => Some("list"),
        "MemoryTree" => Some("tree"),
        "MemoryUtf8" => Some("utf8"),
        "MemoryHistogram" => Some("histogram"),
        "MemorySet" => Some("set"),
        "MemoryGraph" => Some("graph"),
        "MemoryBodies" => Some("bodies"),
        _ => None,
    };
    if let Some(file) = memory {
        format!("crates/fai-bench-workloads/src/memory_workloads/{file}.fai")
    } else if id == "TailQuickSort" {
        "samples/algorithms/QuickSort.fai".into()
    } else if id.starts_with("Tail") {
        "crates/fai-bench-workloads/src/tail_components/strings.fai".into()
    } else {
        format!("samples/algorithms/{id}.fai")
    }
}

pub(super) fn sources<'a>(
    snapshot: &Snapshot,
    ids: impl Iterator<Item = &'a str>,
    text: &mut String,
) {
    text.push('\n');
    for id in ids.collect::<BTreeSet<_>>() {
        let _ = writeln!(
            text,
            "[{id}]: https://github.com/{}/blob/{}/{} \"{id}\"",
            snapshot.manifest.repository,
            snapshot.manifest.measured_revision,
            source_path(id)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_width_negative_counts_keep_their_sign() {
        assert_eq!(number(i64::MIN), "-9,223,372,036,854,775,808");
    }

    #[test]
    fn short_counts_have_no_leading_separator() {
        assert_eq!([number(0), number(123), number(1234)], ["0", "123", "1,234"]);
    }

    #[test]
    fn millisecond_durations_are_readable() {
        assert_eq!(duration(18_700_000.0), "18.7 ms");
    }

    #[test]
    fn subnanosecond_durations_do_not_round_to_zero() {
        assert_eq!(duration(0.001), "<0.01 ns");
    }

    #[test]
    fn small_durations_keep_nanoseconds() {
        assert_eq!(duration(8.0), "8 ns");
    }

    #[test]
    fn resident_memory_converts_kib_to_mib() {
        assert_eq!(memory(52_940), "51.7 MiB");
    }

    #[test]
    fn zero_allocations_remain_zero_bytes() {
        assert_eq!(bytes(0), "0 B");
    }

    #[test]
    fn qualifying_overhead_does_not_round_up_to_the_limit() {
        assert_eq!(overhead(9999.0, 100000.0), "<10%");
    }

    #[test]
    fn overhead_at_the_limit_is_displayed_at_the_limit() {
        assert_eq!(overhead(10.0, 100.0), "10.0%");
    }

    #[test]
    fn every_reviewed_benchmark_has_a_reader_name() {
        let missing: Vec<_> = crate::memory_workloads::algorithms()
            .chain(crate::tail_components::COMPONENTS)
            .filter(|case| name(case.module) == case.module)
            .map(|case| case.module)
            .collect();
        assert!(missing.is_empty(), "missing names: {missing:?}");
    }
}
