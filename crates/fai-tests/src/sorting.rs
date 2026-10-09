//! Shared deterministic sorting distributions and order-sensitive checksums.

/// Fai peer for the distribution benchmark, also used for untimed validation.
pub const SOURCE: &str = include_str!("../../../samples/algorithms/SortPatterns.fai");

/// Stable distribution names/ids shared by all three benchmark languages.
pub const PATTERNS: &[&str] =
    &["SortAscending", "SortDescending", "SortShuffled", "SortEqual", "SortFewKeys", "SortRuns"];

/// A named distribution at a particular workload size.
#[derive(Clone, Copy)]
pub struct Case {
    /// Distribution index in `PATTERNS`.
    pub pattern: usize,
    /// Element count.
    pub n: usize,
}

impl std::fmt::Display for Case {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/n={}", PATTERNS[self.pattern], self.n)
    }
}

/// Small compute and larger delivered-binary workload sizes for every pattern.
pub const CASES: &[Case] = &[
    Case { pattern: 0, n: 6000 },
    Case { pattern: 0, n: 80000 },
    Case { pattern: 1, n: 6000 },
    Case { pattern: 1, n: 80000 },
    Case { pattern: 2, n: 6000 },
    Case { pattern: 2, n: 80000 },
    Case { pattern: 3, n: 6000 },
    Case { pattern: 3, n: 80000 },
    Case { pattern: 4, n: 6000 },
    Case { pattern: 4, n: 80000 },
    Case { pattern: 5, n: 6000 },
    Case { pattern: 5, n: 80000 },
];

/// Generates input identically to the Fai and OCaml implementations.
pub fn input(pattern: usize, n: usize) -> Vec<i64> {
    match pattern {
        0 => (0..n as i64).collect(),
        1 => (0..n as i64).rev().collect(),
        2 => {
            let mut values: Vec<_> = (0..n as i64).collect();
            let mut state = 1u64;
            for i in (1..n).rev() {
                state = (state * 1664525 + 1013904223) & 2147483647;
                values.swap(i, state as usize % (i + 1));
            }
            values
        }
        3 => vec![7; n],
        4 => (0..n).map(|i| (i % 4) as i64).collect(),
        5 => (0..n).map(|i| (n - ((i / 32 + 1) * 32).min(n) + i % 32) as i64).collect(),
        _ => panic!("unknown sorting distribution"),
    }
}

/// Position-weighted checksum, shared by validation and timed workloads.
pub fn checksum(values: &[i64]) -> i64 {
    values
        .iter()
        .enumerate()
        .fold(0i64, |sum, (i, x)| sum.wrapping_add((i as i64).wrapping_mul(*x)))
}

/// Sorts one generated input and checksums its order.
pub fn run(pattern: usize, n: usize) -> i64 {
    let mut values = input(pattern, n);
    values.sort();
    checksum(&values)
}

/// Fills the sample's entry with a particular distribution and size.
pub fn program(case: Case) -> String {
    SOURCE.replace("run 1 80000", &format!("run {} {}", case.pattern, case.n))
}
