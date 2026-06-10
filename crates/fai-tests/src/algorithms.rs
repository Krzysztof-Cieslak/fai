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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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

/// The count-weighted sum of a histogram of `n` keys (`i % 256`) tallied into an
/// ordered map: an order-independent reduction that exercises map build/query and
/// the structural comparison hot path. The Fai version builds a `Dict`.
#[must_use]
pub fn dict_histogram(n: i64) -> i64 {
    let buckets = 256;
    let mut counts: BTreeMap<i64, i64> = BTreeMap::new();
    for i in 0..n {
        *counts.entry(i % buckets).or_insert(0) += 1;
    }
    counts.iter().map(|(key, count)| key * count).sum()
}

/// The total length of the words in the string `"0 1 2 … n-1"` after joining the
/// numbers with spaces and splitting them back apart: a heap-`String` workload.
#[must_use]
pub fn word_count(n: i64) -> i64 {
    let words: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    let text = words.join(" ");
    text.split(' ').map(|w| w.chars().count() as i64).sum()
}

/// The sum of doubling `[0, n)` plus the sum of `[0, n)` — the shared-list twin of
/// [`map_sum`], which equals `3 * sum [0, n)`. `black_box` per element defeats
/// LLVM's scalar-evolution collapse of this arithmetic series to a closed form, so
/// the benchmark times a real loop rather than a constant (as [`map_sum`] does).
#[must_use]
pub fn map_sum_shared(n: i64) -> i64 {
    let mut acc: i64 = 0;
    for x in 0..n {
        acc = acc.wrapping_add(std::hint::black_box(x.wrapping_mul(2)));
        acc = acc.wrapping_add(std::hint::black_box(x));
    }
    acc
}

/// The sum of the distinct values among `[0, n)` reduced modulo a bucket count,
/// collected into an ordered set. The Fai version builds a `Set`.
#[must_use]
pub fn set_dedup(n: i64) -> i64 {
    let buckets = 1000;
    let distinct: BTreeSet<i64> = (0..n).map(|i| i % buckets).collect();
    distinct.iter().sum()
}

/// The sum over `[0, n)` of `((x + 1) * 2) + 3`, the composed/partially-applied
/// pipeline the Fai version folds with closures. `black_box` per element defeats
/// LLVM's scalar-evolution collapse of this arithmetic series to a closed form, so
/// the benchmark times a real loop rather than a constant (as [`map_sum`] does).
#[must_use]
pub fn fold_pipeline(n: i64) -> i64 {
    let mut acc: i64 = 0;
    for x in 0..n {
        acc = acc.wrapping_add(std::hint::black_box(((x + 1).wrapping_mul(2)).wrapping_add(3)));
    }
    acc
}

/// The dispatched-score sum over `[0, n)`: index `i` selects one of three scoring
/// functions (`2i`, `i+1`, `-i`), mirroring the Fai interface dispatch.
#[must_use]
pub fn interface_dispatch(n: i64) -> i64 {
    (0..n)
        .map(|i| match i % 3 {
            0 => i.wrapping_mul(2),
            1 => i + 1,
            _ => -i,
        })
        .fold(0, i64::wrapping_add)
}

/// The position checksum of five particles after `n` semi-implicit Euler steps
/// under a central spring force. Floating point, compared within tolerance; the
/// summation order matches the Fai fold so rounding agrees.
#[must_use]
pub fn particles(n: i64) -> f64 {
    let dt = 0.01;
    // Each body is [x, y, vx, vy].
    let mut bodies: Vec<[f64; 4]> =
        (0..5).map(|k| [k as f64 + 1.0, k as f64 + 2.0, 0.0, 0.0]).collect();
    for _ in 0..n {
        for b in &mut bodies {
            let nvx = b[2] + dt * (0.0 - b[0]);
            let nvy = b[3] + dt * (0.0 - b[1]);
            b[0] += dt * nvx;
            b[1] += dt * nvy;
            b[2] = nvx;
            b[3] = nvy;
        }
    }
    let mut acc = 0.0;
    for b in &bodies {
        acc = acc + b[0] + b[1];
    }
    acc
}

/// The number of solutions to the `n`-queens puzzle (backtracking count).
#[must_use]
pub fn nqueens(n: i64) -> i64 {
    fn safe(c: i64, placed: &[i64]) -> bool {
        placed.iter().enumerate().all(|(i, &q)| {
            let d = i as i64 + 1;
            q != c && q - c != d && c - q != d
        })
    }
    fn solve(size: i64, row: i64, placed: &mut Vec<i64>) -> i64 {
        if row >= size {
            return 1;
        }
        let mut count = 0;
        for c in 0..size {
            if safe(c, placed) {
                placed.insert(0, c);
                count += solve(size, row + 1, placed);
                placed.remove(0);
            }
        }
        count
    }
    solve(n, 0, &mut Vec::new())
}

/// The sum of all entries of the `n`-by-`n` product `A·B`, with `A(i,j)=(i+j)%7`
/// and `B(i,j)=(i*j+1)%5`. The Fai version uses lists of lists.
#[must_use]
pub fn matrix_multiply(n: i64) -> i64 {
    let rows: Vec<Vec<i64>> = (0..n).map(|i| (0..n).map(|j| (i + j) % 7).collect()).collect();
    let cols: Vec<Vec<i64>> = (0..n).map(|j| (0..n).map(|i| (i * j + 1) % 5).collect()).collect();
    let mut acc = 0;
    for arow in &rows {
        for bcol in &cols {
            acc += arow.iter().zip(bcol).map(|(x, y)| x * y).sum::<i64>();
        }
    }
    acc
}

/// The edit distance between two length-`n` integer sequences (`i%7` and
/// `(i*3)%7`) by the two-row dynamic program the Fai version mirrors.
#[must_use]
pub fn levenshtein(n: i64) -> i64 {
    let a: Vec<i64> = (0..n).map(|i| i % 7).collect();
    let b: Vec<i64> = (0..n).map(|i| i * 3 % 7).collect();
    let mut row: Vec<i64> = (0..=b.len() as i64).collect();
    for (i, &ai) in a.iter().enumerate() {
        let mut diag = row[0];
        row[0] = i as i64 + 1;
        for (j, &bj) in b.iter().enumerate() {
            let cost = i64::from(ai != bj);
            let up = row[j + 1];
            row[j + 1] = (row[j] + 1).min(up + 1).min(diag + cost);
            diag = up;
        }
    }
    row[b.len()]
}

/// The live-cell count after `n` Conway generations from the R-pentomino seed.
/// Deterministic, so the BST-ordered Fai version and this hashed reference agree.
#[must_use]
pub fn game_of_life(n: i64) -> i64 {
    const OFFSETS: [(i64, i64); 8] =
        [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)];
    let mut live: HashSet<(i64, i64)> =
        [(1, 0), (2, 0), (0, 1), (1, 1), (1, 2)].into_iter().collect();
    for _ in 0..n {
        let mut counts: HashMap<(i64, i64), i64> = HashMap::new();
        for &(x, y) in &live {
            for (dx, dy) in OFFSETS {
                *counts.entry((x + dx, y + dy)).or_insert(0) += 1;
            }
        }
        live = counts
            .into_iter()
            .filter(|&(cell, c)| c == 3 || (c == 2 && live.contains(&cell)))
            .map(|(cell, _)| cell)
            .collect();
    }
    live.len() as i64
}

/// The spectral-norm estimate of the Hilbert-like matrix after ten power-iteration
/// rounds. Floating point within tolerance; all sums fold left to match the Fai
/// version's accumulation order.
#[must_use]
pub fn spectral_norm(n: i64) -> f64 {
    let n = n as usize;
    let eval_a = |i: usize, j: usize| {
        let s = (i + j) as f64;
        1.0 / (s * (s + 1.0) / 2.0 + i as f64 + 1.0)
    };
    let mul_av = |u: &[f64]| -> Vec<f64> {
        (0..n).map(|i| (0..n).fold(0.0, |acc, j| acc + eval_a(i, j) * u[j])).collect()
    };
    let mul_atv = |u: &[f64]| -> Vec<f64> {
        (0..n).map(|i| (0..n).fold(0.0, |acc, j| acc + eval_a(j, i) * u[j])).collect()
    };
    let mul_at_av = |u: &[f64]| mul_atv(&mul_av(u));
    let mut u = vec![1.0; n];
    let mut v = u.clone();
    for _ in 0..10 {
        v = mul_at_av(&u);
        u = mul_at_av(&v);
    }
    let dot = |xs: &[f64], ys: &[f64]| xs.iter().zip(ys).fold(0.0, |acc, (x, y)| acc + x * y);
    (dot(&u, &v) / dot(&v, &v)).sqrt()
}

/// The clamped-magnitude sum `Σ min(|z|², 4)` over an `n`-by-`n` Mandelbrot grid
/// after a fixed iteration count. A continuous (rounding-robust) float result.
#[must_use]
pub fn mandelbrot(n: i64) -> f64 {
    let max_iter = 50;
    let coord = |lo: f64, span: f64, p: i64| lo + span * p as f64 / n as f64;
    let escape = |cx: f64, cy: f64| {
        let (mut zx, mut zy) = (0.0, 0.0);
        for _ in 0..max_iter {
            let nzx = zx * zx - zy * zy + cx;
            let nzy = 2.0 * zx * zy + cy;
            zx = nzx;
            zy = nzy;
        }
        let m = zx * zx + zy * zy;
        if m < 4.0 { m } else { 4.0 }
    };
    let mut acc = 0.0;
    for j in 0..n {
        let cy = coord(-1.25, 2.5, j);
        for i in 0..n {
            acc += escape(coord(-2.0, 2.5, i), cy);
        }
    }
    acc
}

/// The Ackermann function at `m = 3`: `ack(3, n) = 2^(n+3) - 3`.
#[must_use]
pub fn ackermann(n: i64) -> i64 {
    fn ack(m: i64, n: i64) -> i64 {
        if m == 0 {
            n + 1
        } else if n == 0 {
            ack(m - 1, 1)
        } else {
            ack(m - 1, ack(m, n - 1))
        }
    }
    ack(3, n)
}

/// The xor of `n` successive xorshift64 states (seeded constant). Drives the same
/// bitwise operations as the Fai `Int` intrinsics, on the matching `u64` bit
/// patterns.
#[must_use]
pub fn prng_xorshift(n: i64) -> i64 {
    let mut state: u64 = 88_172_645_463_325_252;
    let mut acc: u64 = 0;
    for _ in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        acc ^= state;
    }
    acc as i64
}

/// The value of a generated `n`-number arithmetic expression, evaluated with `*`
/// binding tighter than `+`/`-` (the precedence the Fai parser implements).
#[must_use]
pub fn expr_eval(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let num = |i: i64| i % 9 + 1;
    // Pass 1: fold the multiplicative operator (`i % 3 == 1`) into the current term.
    let mut terms = vec![num(0)];
    let mut add_ops = Vec::new();
    for i in 0..(n - 1) {
        let rhs = num(i + 1);
        if i % 3 == 1 {
            let last = terms.last_mut().expect("a term is always present");
            *last = last.wrapping_mul(rhs);
        } else {
            add_ops.push(i % 3);
            terms.push(rhs);
        }
    }
    // Pass 2: fold the additive operators left to right.
    let mut acc = terms[0];
    for (k, &op) in add_ops.iter().enumerate() {
        acc = if op == 0 { acc.wrapping_add(terms[k + 1]) } else { acc.wrapping_sub(terms[k + 1]) };
    }
    acc
}

/// The number of nodes reachable from `0` in the deterministic `n`-node graph
/// whose node `i` points to `(i+1)%n`, `(2i+1)%n`, and `(3i+2)%n`.
#[must_use]
pub fn graph_bfs(n: i64) -> i64 {
    let neighbors = |i: i64| [(i + 1) % n, (2 * i + 1) % n, (3 * i + 2) % n];
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(0);
    let mut frontier = vec![0i64];
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for node in frontier {
            for nb in neighbors(node) {
                if visited.insert(nb) {
                    next.push(nb);
                }
            }
        }
        frontier = next;
    }
    visited.len() as i64
}

/// The number of ways to make amount `n` from the coins `[1,2,5,10,25,50]`, modulo
/// a large prime (a dynamic program over a sub-amount table).
#[must_use]
pub fn coin_change(n: i64) -> i64 {
    const MODULUS: i64 = 1_000_000_007;
    let coins = [1i64, 2, 5, 10, 25, 50];
    let amount = n as usize;
    let mut ways = vec![0i64; amount + 1];
    ways[0] = 1;
    for &coin in &coins {
        let c = coin as usize;
        for a in c..=amount {
            ways[a] = (ways[a] + ways[a - c]) % MODULUS;
        }
    }
    ways[amount]
}

/// The `n`th Fibonacci number with two's-complement wrapping (the memoized Fai
/// version caches the same wrapping sums).
#[must_use]
pub fn fib_memo(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    let (mut a, mut b) = (0i64, 1i64);
    for _ in 2..=n {
        let c = a.wrapping_add(b);
        a = b;
        b = c;
    }
    b
}

/// The position-weighted checksum `Σ i * x[i]` of a scrambled `n`-element input
/// after sorting it ascending (so the value detects any ordering error).
#[must_use]
pub fn quicksort_sum(n: i64) -> i64 {
    let mut v: Vec<i64> =
        (0..n).map(|k| (k.wrapping_mul(2_654_435_761).wrapping_add(12345)) % n).collect();
    v.sort_unstable();
    v.iter().enumerate().fold(0i64, |acc, (i, &x)| acc.wrapping_add(i as i64 * x))
}

/// The number of primes below `n` (Sieve of Eratosthenes).
#[must_use]
pub fn sieve(n: i64) -> i64 {
    if n < 2 {
        return 0;
    }
    let n = n as usize;
    let mut composite = vec![false; n];
    let mut p = 2;
    while p * p < n {
        if !composite[p] {
            let mut m = p * p;
            while m < n {
                composite[m] = true;
                m += p;
            }
        }
        p += 1;
    }
    composite[2..].iter().filter(|&&c| !c).count() as i64
}

/// The position checksum of five gravitating bodies after `n` all-pairs steps.
/// Floating point within tolerance; the force accumulation and final sum fold in
/// the same order as the Fai version.
#[must_use]
pub fn nbody(n: i64) -> f64 {
    let dt = 0.01;
    // Each body is [x, y, z, vx, vy, vz, mass].
    let mut bodies: Vec<[f64; 7]> = (0..5)
        .map(|i| {
            [i as f64, (i * i % 7) as f64, (i % 3) as f64, 0.0, 0.0, 0.0, 1.0 + (i % 5) as f64]
        })
        .collect();
    for _ in 0..n {
        let snapshot = bodies.clone();
        for (me_idx, b) in bodies.iter_mut().enumerate() {
            let me = snapshot[me_idx];
            let (mut ax, mut ay, mut az) = (0.0, 0.0, 0.0);
            for (o_idx, o) in snapshot.iter().enumerate() {
                if o_idx == me_idx {
                    continue;
                }
                let dx = o[0] - me[0];
                let dy = o[1] - me[1];
                let dz = o[2] - me[2];
                let d2 = dx * dx + dy * dy + dz * dz;
                let f = o[6] / (d2 * d2.sqrt());
                ax += f * dx;
                ay += f * dy;
                az += f * dz;
            }
            b[3] = me[3] + dt * ax;
            b[4] = me[4] + dt * ay;
            b[5] = me[5] + dt * az;
        }
        for b in &mut bodies {
            b[0] += dt * b[3];
            b[1] += dt * b[4];
            b[2] += dt * b[5];
        }
    }
    let mut acc = 0.0;
    for b in &bodies {
        acc = acc + b[0] + b[1] + b[2];
    }
    acc
}

/// The maximum pancake-flip count over every permutation of `[1, n]`
/// (fannkuch-redux, the max-flips figure).
#[must_use]
pub fn fannkuch(n: i64) -> i64 {
    fn flips(mut p: Vec<i64>) -> i64 {
        let mut count = 0;
        while p[0] > 1 {
            let k = p[0] as usize;
            p[..k].reverse();
            count += 1;
        }
        count
    }
    fn perms(xs: &[i64]) -> Vec<Vec<i64>> {
        if xs.is_empty() {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for (i, &x) in xs.iter().enumerate() {
            let mut rest = xs.to_vec();
            rest.remove(i);
            for mut p in perms(&rest) {
                let mut whole = vec![x];
                whole.append(&mut p);
                out.push(whole);
            }
        }
        out
    }
    let base: Vec<i64> = (1..=n).collect();
    perms(&base).into_iter().map(flips).max().unwrap_or(0)
}

/// The number of connected components among `n` nodes after linking each `i` (not
/// a multiple of 5) to `i/2` via union-find.
#[must_use]
pub fn union_find(n: i64) -> i64 {
    fn find(parent: &HashMap<i64, i64>, mut x: i64) -> i64 {
        while let Some(&p) = parent.get(&x) {
            if p == x {
                break;
            }
            x = p;
        }
        x
    }
    let mut parent: HashMap<i64, i64> = HashMap::new();
    for i in 1..n {
        if i % 5 != 0 {
            let ra = find(&parent, i);
            let rb = find(&parent, i / 2);
            if ra != rb {
                parent.insert(ra, rb);
            }
        }
    }
    let roots: HashSet<i64> = (0..n).map(|i| find(&parent, i)).collect();
    roots.len() as i64
}

/// The length of the serialized balanced JSON tree of `n` nodes the Fai version
/// builds and prints.
#[must_use]
pub fn json_serialize(n: i64) -> i64 {
    enum Json {
        Null,
        Bool(bool),
        Int(i64),
        Arr(Vec<Json>),
        Obj(Vec<(String, Json)>),
    }
    fn leaf(seed: i64) -> Json {
        match seed % 3 {
            0 => Json::Null,
            1 => Json::Bool(true),
            _ => Json::Int(seed),
        }
    }
    fn build(seed: i64, size: i64) -> Json {
        if size <= 1 {
            return leaf(seed);
        }
        let half = size / 2;
        let (l, r) = (build(seed + 1, half), build(seed + 2, size - half - 1));
        if seed % 2 == 0 {
            Json::Arr(vec![l, r])
        } else {
            Json::Obj(vec![("a".to_owned(), l), ("b".to_owned(), r)])
        }
    }
    fn ser(j: &Json) -> String {
        match j {
            Json::Null => "null".to_owned(),
            Json::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Json::Int(k) => k.to_string(),
            Json::Arr(items) => {
                format!("[{}]", items.iter().map(ser).collect::<Vec<_>>().join(","))
            }
            Json::Obj(fields) => format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(k, v)| format!("\"{k}\":{}", ser(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
    ser(&build(0, n)).chars().count() as i64
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
    Algorithm {
        module: "DictHistogram",
        entry: "run",
        jit_size: 2_000,
        aot_size: 30_000,
        oracle: Oracle::Int(dict_histogram),
    },
    Algorithm {
        module: "WordCount",
        entry: "run",
        jit_size: 5_000,
        aot_size: 80_000,
        oracle: Oracle::Int(word_count),
    },
    Algorithm {
        module: "MapSumShared",
        entry: "run",
        jit_size: 100_000,
        aot_size: 1_500_000,
        oracle: Oracle::Int(map_sum_shared),
    },
    Algorithm {
        module: "SetDedup",
        entry: "run",
        jit_size: 2000,
        aot_size: 12000,
        oracle: Oracle::Int(set_dedup),
    },
    Algorithm {
        module: "FoldPipeline",
        entry: "run",
        jit_size: 50_000,
        aot_size: 800_000,
        oracle: Oracle::Int(fold_pipeline),
    },
    Algorithm {
        module: "InterfaceDispatch",
        entry: "run",
        jit_size: 20_000,
        aot_size: 400_000,
        oracle: Oracle::Int(interface_dispatch),
    },
    Algorithm {
        module: "Particles",
        entry: "runF",
        jit_size: 5_000,
        aot_size: 100_000,
        oracle: Oracle::Float(particles),
    },
    Algorithm {
        module: "NQueens",
        entry: "run",
        jit_size: 8,
        aot_size: 11,
        oracle: Oracle::Int(nqueens),
    },
    Algorithm {
        module: "MatrixMultiply",
        entry: "run",
        jit_size: 30,
        aot_size: 90,
        oracle: Oracle::Int(matrix_multiply),
    },
    Algorithm {
        module: "Levenshtein",
        entry: "run",
        jit_size: 40,
        aot_size: 150,
        oracle: Oracle::Int(levenshtein),
    },
    Algorithm {
        module: "GameOfLife",
        entry: "run",
        jit_size: 30,
        aot_size: 80,
        oracle: Oracle::Int(game_of_life),
    },
    Algorithm {
        module: "SpectralNorm",
        entry: "runF",
        jit_size: 100,
        aot_size: 1_000,
        oracle: Oracle::Float(spectral_norm),
    },
    Algorithm {
        module: "Mandelbrot",
        entry: "runF",
        jit_size: 60,
        aot_size: 400,
        oracle: Oracle::Float(mandelbrot),
    },
    Algorithm {
        module: "Ackermann",
        entry: "run",
        jit_size: 6,
        aot_size: 9,
        oracle: Oracle::Int(ackermann),
    },
    Algorithm {
        module: "PrngXorshift",
        entry: "run",
        jit_size: 50_000,
        aot_size: 1_000_000,
        oracle: Oracle::Int(prng_xorshift),
    },
    Algorithm {
        module: "ExprEval",
        entry: "run",
        jit_size: 300,
        aot_size: 4_000,
        oracle: Oracle::Int(expr_eval),
    },
    Algorithm {
        module: "GraphBFS",
        entry: "run",
        jit_size: 1000,
        aot_size: 3000,
        oracle: Oracle::Int(graph_bfs),
    },
    Algorithm {
        module: "CoinChange",
        entry: "run",
        jit_size: 500,
        aot_size: 1500,
        oracle: Oracle::Int(coin_change),
    },
    Algorithm {
        module: "FibMemo",
        entry: "run",
        jit_size: 500,
        aot_size: 4_000,
        oracle: Oracle::Int(fib_memo),
    },
    Algorithm {
        module: "QuickSort",
        entry: "run",
        jit_size: 2_000,
        aot_size: 20_000,
        oracle: Oracle::Int(quicksort_sum),
    },
    Algorithm {
        module: "Sieve",
        entry: "run",
        jit_size: 1500,
        aot_size: 4000,
        oracle: Oracle::Int(sieve),
    },
    Algorithm {
        module: "NBody",
        entry: "runF",
        jit_size: 1_000,
        aot_size: 100_000,
        oracle: Oracle::Float(nbody),
    },
    Algorithm {
        module: "Fannkuch",
        entry: "run",
        jit_size: 7,
        aot_size: 8,
        oracle: Oracle::Int(fannkuch),
    },
    Algorithm {
        module: "UnionFind",
        entry: "run",
        jit_size: 1000,
        aot_size: 2000,
        oracle: Oracle::Int(union_find),
    },
    Algorithm {
        module: "JsonSerialize",
        entry: "run",
        jit_size: 2_000,
        aot_size: 30_000,
        oracle: Oracle::Int(json_serialize),
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
        "DictHistogram" => include_str!("../../../samples/algorithms/DictHistogram.fai"),
        "WordCount" => include_str!("../../../samples/algorithms/WordCount.fai"),
        "MapSumShared" => include_str!("../../../samples/algorithms/MapSumShared.fai"),
        "SetDedup" => include_str!("../../../samples/algorithms/SetDedup.fai"),
        "FoldPipeline" => include_str!("../../../samples/algorithms/FoldPipeline.fai"),
        "InterfaceDispatch" => include_str!("../../../samples/algorithms/InterfaceDispatch.fai"),
        "Particles" => include_str!("../../../samples/algorithms/Particles.fai"),
        "NQueens" => include_str!("../../../samples/algorithms/NQueens.fai"),
        "MatrixMultiply" => include_str!("../../../samples/algorithms/MatrixMultiply.fai"),
        "Levenshtein" => include_str!("../../../samples/algorithms/Levenshtein.fai"),
        "GameOfLife" => include_str!("../../../samples/algorithms/GameOfLife.fai"),
        "SpectralNorm" => include_str!("../../../samples/algorithms/SpectralNorm.fai"),
        "Mandelbrot" => include_str!("../../../samples/algorithms/Mandelbrot.fai"),
        "Ackermann" => include_str!("../../../samples/algorithms/Ackermann.fai"),
        "PrngXorshift" => include_str!("../../../samples/algorithms/PrngXorshift.fai"),
        "ExprEval" => include_str!("../../../samples/algorithms/ExprEval.fai"),
        "GraphBFS" => include_str!("../../../samples/algorithms/GraphBFS.fai"),
        "CoinChange" => include_str!("../../../samples/algorithms/CoinChange.fai"),
        "FibMemo" => include_str!("../../../samples/algorithms/FibMemo.fai"),
        "QuickSort" => include_str!("../../../samples/algorithms/QuickSort.fai"),
        "Sieve" => include_str!("../../../samples/algorithms/Sieve.fai"),
        "NBody" => include_str!("../../../samples/algorithms/NBody.fai"),
        "Fannkuch" => include_str!("../../../samples/algorithms/Fannkuch.fai"),
        "UnionFind" => include_str!("../../../samples/algorithms/UnionFind.fai"),
        "JsonSerialize" => include_str!("../../../samples/algorithms/JsonSerialize.fai"),
        other => panic!("unknown algorithm module: {other}"),
    }
}
