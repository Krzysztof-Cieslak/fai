//! Memory-specific size curves and materialized-storage workloads.

mod kernels;
pub use kernels::*;

use crate::algorithms::{ALGORITHMS, Algorithm, Oracle};
use crate::contracts::{Comparison, Contract};

/// External before/after-workload RSS protocol, distinct from self-reported peaks.
pub const METHODOLOGY: &str = "isolated-memory-v3";

/// A predeclared memory input axis; operation count and resident size are distinct.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// What the supplied integer measures.
    pub unit: &'static str,
    /// Whether the curve grows live data, or exercises bounded state repeatedly.
    pub scale: &'static str,
    /// Ordered input sizes, fixed independently of observed ratios.
    pub sizes: &'static [i64],
}

/// Registered application and storage fixtures in deterministic order.
pub fn algorithms() -> impl Iterator<Item = &'static Algorithm> {
    ALGORITHMS.iter().chain(FIXTURES)
}

/// Size selection is independent of the compute benchmark's JIT/AOT arguments.
pub fn profile(name: &str) -> Option<Profile> {
    let (unit, scale, sizes): (_, _, &[_]) = match name {
        "Fib" => ("recursion input", "bounded stack", &[28, 33]),
        "Collatz" => ("starts", "fixed state", &[4000, 60000]),
        "MapSum" | "FoldPipeline" => ("range elements", "fused state", &[100000, 1500000]),
        "Pi" => ("terms", "fixed state", &[45000, 800000]),
        "DictHistogram" => ("updates; 256 keys", "fixed keys", &[10000, 1000000]),
        "SetDedup" => ("inserts; 1000 keys", "fixed keys", &[5000, 1000000]),
        "InterfaceDispatch" => ("calls", "fixed state", &[20000, 400000]),
        "Particles" | "NBody" => ("steps; 5 bodies", "fixed state", &[1000, 100000]),
        "VecMat" => ("steps", "fixed state", &[100000, 2000000]),
        "NQueens" => ("board width", "backtracking", &[8, 11]),
        "MatrixMultiply" | "FloatMatrixMultiply" => ("matrix width", "live data", &[32, 128, 512]),
        "Levenshtein" => ("text width", "quadratic work", &[150, 1000]),
        "GameOfLife" => ("generations", "evolving state", &[50, 200]),
        "SpectralNorm" => ("vector width", "quadratic work", &[100, 1000]),
        "Mandelbrot" => ("grid width", "fixed state", &[60, 400]),
        "Ackermann" => ("recursion input", "bounded stack", &[6, 9]),
        "PrngXorshift" => ("steps", "fixed state", &[50000, 1000000]),
        "ExprEval" => ("expression length", "live data", &[300, 4000, 16000]),
        "GraphBFS" | "UnionFind" => ("nodes", "live data", &[2000, 30000, 120000]),
        "CoinChange" => ("table elements", "live data", &[4096, 65536, 1048576]),
        "FibMemo" => ("memo entries", "recursive work", &[500, 4000]),
        "Fannkuch" => ("permutation width", "factorial data", &[7, 8]),
        "JsonSerialize" => ("tree input", "live data", &[2000, 30000, 300000]),
        "WordCount" => ("words", "live data", &[5000, 80000, 500000]),
        "StringSlice" => ("base characters", "live data", &[2000, 20000, 100000]),
        "OptionEval" | "IntEval" => ("evaluations", "fixed state", &[5000, 100000]),
        "OptionPath" => ("path input", "live data", &[500, 4000, 30000]),
        "OptionTreeFind" => ("lookups; 1000 keys", "fixed keys", &[5000, 100000]),
        "BinaryTrees" => ("tree depth", "live data", &[12, 17, 21]),
        "Sieve" => ("Boolean slots", "live data", &[65536, 1048576, 16777216]),
        "MergeSort" | "QuickSort" | "ListSort" => {
            ("elements", "live data", &[4096, 65536, 1048576])
        }
        "MapSumShared" | "MemoryArrayInt" | "MemoryArrayFloat" | "MemorySharedArrays" => {
            ("elements", "live data", &[65536, 1048576, 8388608])
        }
        "StringBuild" => ("characters", "live data", &[65536, 1048576, 16777216]),
        "MemoryList" => ("cons cells", "live data", &[4096, 65536, 1048576]),
        "MemoryTree" => ("tree depth", "live data", &[12, 16, 20]),
        "MemoryUtf8" => ("UTF-8 chunks", "live data", &[8192, 131072, 1048576]),
        "MemoryHistogram" | "MemorySet" => ("distinct keys", "live data", &[1024, 16384, 262144]),
        "MemoryGraph" => ("nodes", "live data", &[4096, 65536, 262144]),
        "MemoryBodies" => ("bodies; 4 maps", "live data", &[1024, 16384, 262144]),
        _ => return None,
    };
    Some(Profile { unit, scale, sizes })
}

/// Extra fixtures grow resident data rather than only repeating fixed-size work.
pub const FIXTURES: &[Algorithm] = &[
    Algorithm {
        module: "MemoryArrayInt",
        entry: "run",
        jit_size: 16,
        aot_size: 65536,
        oracle: Oracle::Int(array_int),
    },
    Algorithm {
        module: "MemoryArrayFloat",
        entry: "runF",
        jit_size: 16,
        aot_size: 65536,
        oracle: Oracle::Float(array_float),
    },
    Algorithm {
        module: "MemorySharedArrays",
        entry: "run",
        jit_size: 16,
        aot_size: 65536,
        oracle: Oracle::Int(shared_arrays),
    },
    Algorithm {
        module: "MemoryList",
        entry: "run",
        jit_size: 16,
        aot_size: 4096,
        oracle: Oracle::Int(list),
    },
    Algorithm {
        module: "MemoryTree",
        entry: "run",
        jit_size: 4,
        aot_size: 12,
        oracle: Oracle::Int(tree),
    },
    Algorithm {
        module: "MemoryUtf8",
        entry: "run",
        jit_size: 16,
        aot_size: 8192,
        oracle: Oracle::Int(utf8),
    },
    Algorithm {
        module: "MemoryHistogram",
        entry: "run",
        jit_size: 16,
        aot_size: 1024,
        oracle: Oracle::Int(histogram),
    },
    Algorithm {
        module: "MemorySet",
        entry: "run",
        jit_size: 16,
        aot_size: 1024,
        oracle: Oracle::Int(set),
    },
    Algorithm {
        module: "MemoryGraph",
        entry: "run",
        jit_size: 16,
        aot_size: 4096,
        oracle: Oracle::Int(graph),
    },
    Algorithm {
        module: "MemoryBodies",
        entry: "runF",
        jit_size: 16,
        aot_size: 1024,
        oracle: Oracle::Float(bodies),
    },
];

/// Finds only memory-specific fixtures, keeping them out of compute aggregates.
pub fn by_module(name: &str) -> Option<&'static Algorithm> {
    FIXTURES.iter().find(|case| case.module == name)
}

/// Source for one isolated Fai memory fixture.
pub fn source(name: &str) -> Option<&'static str> {
    Some(match name {
        "MemoryArrayInt" => include_str!("memory_workloads/array_int.fai"),
        "MemoryArrayFloat" => include_str!("memory_workloads/array_float.fai"),
        "MemorySharedArrays" => include_str!("memory_workloads/shared_arrays.fai"),
        "MemoryList" => include_str!("memory_workloads/list.fai"),
        "MemoryTree" => include_str!("memory_workloads/tree.fai"),
        "MemoryUtf8" => include_str!("memory_workloads/utf8.fai"),
        "MemoryHistogram" => include_str!("memory_workloads/histogram.fai"),
        "MemorySet" => include_str!("memory_workloads/set.fai"),
        "MemoryGraph" => include_str!("memory_workloads/graph.fai"),
        "MemoryBodies" => include_str!("memory_workloads/bodies.fai"),
        _ => return None,
    })
}

/// Reviewed storage contracts. Consumers form one whole-structure call boundary;
/// element loops optimize normally, while materialization remains observable.
pub fn contract(name: &str) -> Option<Contract> {
    let (rust_entry, ocaml_entry, ocaml_support): (_, _, &[_]) = match name {
        "MemoryArrayInt" => {
            ("memory_workloads::array_int", "memory_array_int", &["memory_sum_int"])
        }
        "MemoryArrayFloat" => {
            ("memory_workloads::array_float", "memory_array_float", &["memory_sum_float"])
        }
        "MemorySharedArrays" => {
            ("memory_workloads::shared_arrays", "memory_shared_arrays", &["memory_sum_int"])
        }
        "MemoryList" => ("memory_workloads::list", "memory_list", &["memory_sum_list"]),
        "MemoryTree" => (
            "memory_workloads::tree",
            "memory_tree",
            &["memory_tree_value", "memory_tree_make", "memory_tree_sum"],
        ),
        "MemoryUtf8" => ("memory_workloads::utf8", "memory_utf8", &["memory_utf8_length"]),
        "MemoryHistogram" => ("memory_workloads::histogram", "memory_histogram", &[]),
        "MemorySet" => ("memory_workloads::set", "memory_set", &[]),
        "MemoryGraph" => ("memory_workloads::graph", "memory_graph", &["memory_graph_walk"]),
        "MemoryBodies" => {
            ("memory_workloads::bodies", "memory_bodies", &["memory_body", "memory_body_sum"])
        }
        _ => return None,
    };
    let (comparison, note) = match name {
        "MemoryHistogram" | "MemorySet" => (
            Comparison::Application,
            "Distinct-key cardinality grows; four insertion/update passes and endpoint reads. Standard hash functions, layouts, growth and GC policies differ.",
        ),
        "MemoryUtf8" => (
            Comparison::Application,
            "Incrementally append identical nine-byte/four-scalar UTF-8 chunks and consume the final string. Buffer growth and final copying differ.",
        ),
        "MemoryBodies" => (
            Comparison::Kernel,
            "An array of four-Float bodies, four pure map passes and a whole-array sum. Inline versus boxed record layout and reclamation remain observable.",
        ),
        "MemorySharedArrays" => (
            Comparison::Kernel,
            "Build one integer array, retain it while copying and incrementing another, then consume both. Both snapshots contribute to the checksum.",
        ),
        "MemoryGraph" => (
            Comparison::Kernel,
            "Materialized array-of-arrays adjacency with three ordered edges per node, a Boolean visited array and an array queue. Every node is reached by the ring edge.",
        ),
        "MemoryTree" => (
            Comparison::Kernel,
            "Complete binary tree of depth n, one Int and two children per node, allocation-free empty children; consume all values and node count.",
        ),
        "MemoryList" => (
            Comparison::Kernel,
            "Materialized linked Int list with one value and one successor per cell; allocation-free nil and constant-stack construction, consumption and destruction.",
        ),
        _ => (
            Comparison::Kernel,
            "One materialized numeric array, initialized in order and passed to a non-inlined whole-array sum/length consumer. No per-element optimizer barriers.",
        ),
    };
    Some(Contract {
        version: "materialized-storage-v1",
        comparison,
        note,
        rust_entry,
        ocaml_entry,
        ocaml_support,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_memory_case_has_an_independent_ordered_size_curve() {
        for case in algorithms() {
            let profile = profile(case.module).expect(case.module);
            assert!(!profile.unit.is_empty() && !profile.sizes.is_empty());
            assert!(profile.sizes.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(profile.sizes.iter().all(|&n| n > 0 && n <= crate::benchmark_aot::MAX_SIZE));
        }
    }

    #[test]
    fn cardinality_is_a_distinct_axis_from_repeating_fixed_keys() {
        assert_eq!(profile("DictHistogram").unwrap().scale, "fixed keys");
        assert_eq!(profile("MemoryHistogram").unwrap().unit, "distinct keys");
        assert_eq!(profile("MemoryBodies").unwrap().unit, "bodies; 4 maps");
    }
}
