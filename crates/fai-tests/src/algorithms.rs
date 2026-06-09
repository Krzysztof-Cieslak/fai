//! Reference Rust implementations of the well-known algorithms the runtime
//! benchmarks compare against Fai, plus the registry tying each to its Fai sample
//! module and workload sizes.
//!
//! The implementations are **idiomatic Rust** (unboxed scalars, `Vec`,
//! iterators): the benchmark measures the real end-to-end gap against Fai's
//! boxed, reference-counted values, not a representation-matched one. They are
//! shared by the in-process JIT bench (the Rust side and the correctness
//! oracle), the `algo-baseline` binary (the Rust side of the subprocess AOT
//! bench), and the sample validation tests.

/// Naive recursive Fibonacci: function-call and integer overhead, no allocation.
#[must_use]
pub fn fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

/// Collatz stopping time of `n`: the number of steps to reach 1 (0 for `n <= 1`).
fn collatz_steps(mut n: i64) -> i64 {
    let mut steps = 0;
    while n > 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        steps += 1;
    }
    steps
}

/// Sum of Collatz stopping times over `1..=n`.
#[must_use]
pub fn collatz_sum(n: i64) -> i64 {
    (1..=n).map(collatz_steps).sum()
}

/// Sum of doubling every element of `[0, n)`. Allocation-free in Rust (the Fai
/// version builds and folds a `List`); `black_box` per element defeats LLVM's
/// scalar-evolution collapse of this arithmetic series to a closed form, so the
/// benchmark times a real loop rather than a constant.
#[must_use]
pub fn map_sum(n: i64) -> i64 {
    let mut acc: i64 = 0;
    for x in 0..n {
        acc += std::hint::black_box(x * 2);
    }
    acc
}

/// Sum of the descending list `[n-1, …, 0]` after sorting it ascending. Idiomatic
/// Rust uses a `Vec` and `Vec::sort` (a stable merge sort), where Fai sorts a
/// linked `List`.
#[must_use]
pub fn merge_sort_sum(n: i64) -> i64 {
    let mut v: Vec<i64> = (0..n).rev().collect();
    v.sort();
    v.iter().sum()
}

/// A full binary tree, mirroring the Fai sample's heap structure.
enum Tree {
    Leaf,
    Node(Box<Tree>, Box<Tree>),
}

fn build_tree(depth: i64) -> Tree {
    if depth <= 0 {
        Tree::Leaf
    } else {
        Tree::Node(Box::new(build_tree(depth - 1)), Box::new(build_tree(depth - 1)))
    }
}

fn count(tree: &Tree) -> i64 {
    match tree {
        Tree::Leaf => 0,
        Tree::Node(left, right) => 1 + count(left) + count(right),
    }
}

/// The number of internal nodes in a full binary tree of the given depth, built
/// then counted (an allocation-and-free burst).
#[must_use]
pub fn tree_count(depth: i64) -> i64 {
    count(&build_tree(depth))
}

/// The Leibniz approximation of pi from `terms` terms.
#[must_use]
pub fn pi(terms: i64) -> f64 {
    let mut acc = 0.0;
    let mut i = 0i64;
    while i < terms {
        let denom = 2.0 * i as f64 + 1.0;
        acc += if i % 2 == 0 { 1.0 / denom } else { -1.0 / denom };
        i += 1;
    }
    4.0 * acc
}

/// The Rust reference for an algorithm: a function of one size argument returning
/// either an integer or a floating-point result.
#[derive(Clone, Copy)]
pub enum Oracle {
    /// An `Int -> Int` workload.
    Int(fn(i64) -> i64),
    /// An `Int -> Float` workload.
    Float(fn(i64) -> f64),
}

/// One benchmarked algorithm: its Fai sample module, the benched entry binding,
/// the JIT and AOT workload sizes, and the Rust oracle.
pub struct Algorithm {
    /// The sample module name (and file stem under `samples/algorithms/`).
    pub module: &'static str,
    /// The benched top-level binding (`run` for an Int workload, `runF` for a
    /// Float one).
    pub entry: &'static str,
    /// The in-process JIT workload size, kept small for stable medians.
    pub jit_size: i64,
    /// The subprocess AOT workload size — large, to amortize process startup —
    /// equal to the literal baked into the sample's `main`.
    pub aot_size: i64,
    /// The Rust reference implementation.
    pub oracle: Oracle,
}

impl Algorithm {
    /// The Fai source of this algorithm's sample module.
    #[must_use]
    pub fn source(&self) -> &'static str {
        source(self.module)
    }
}

/// Every benchmarked algorithm. Each `aot_size` must equal the literal the
/// matching sample's `main` passes to `run`/`runF`; the sample validation tests
/// assert this by comparing the program's output to the oracle.
pub const ALGORITHMS: &[Algorithm] = &[
    Algorithm { module: "Fib", entry: "run", jit_size: 28, aot_size: 33, oracle: Oracle::Int(fib) },
    Algorithm {
        module: "Collatz",
        entry: "run",
        jit_size: 4_000,
        aot_size: 60_000,
        oracle: Oracle::Int(collatz_sum),
    },
    Algorithm {
        module: "MapSum",
        entry: "run",
        jit_size: 100_000,
        aot_size: 1_500_000,
        oracle: Oracle::Int(map_sum),
    },
    Algorithm {
        module: "MergeSort",
        entry: "run",
        jit_size: 6_000,
        aot_size: 80_000,
        oracle: Oracle::Int(merge_sort_sum),
    },
    Algorithm {
        module: "BinaryTrees",
        entry: "run",
        jit_size: 17,
        aot_size: 21,
        oracle: Oracle::Int(tree_count),
    },
    Algorithm {
        module: "Pi",
        entry: "runF",
        jit_size: 45_000,
        aot_size: 800_000,
        oracle: Oracle::Float(pi),
    },
];

/// Looks up an algorithm by its sample module name.
#[must_use]
pub fn by_module(module: &str) -> Option<&'static Algorithm> {
    ALGORITHMS.iter().find(|a| a.module == module)
}

/// The Fai source text of a sample module (embedded at build time).
#[must_use]
pub fn source(module: &str) -> &'static str {
    match module {
        "Fib" => include_str!("../../../samples/algorithms/Fib.fai"),
        "Collatz" => include_str!("../../../samples/algorithms/Collatz.fai"),
        "MapSum" => include_str!("../../../samples/algorithms/MapSum.fai"),
        "MergeSort" => include_str!("../../../samples/algorithms/MergeSort.fai"),
        "BinaryTrees" => include_str!("../../../samples/algorithms/BinaryTrees.fai"),
        "Pi" => include_str!("../../../samples/algorithms/Pi.fai"),
        other => panic!("unknown algorithm module: {other}"),
    }
}
