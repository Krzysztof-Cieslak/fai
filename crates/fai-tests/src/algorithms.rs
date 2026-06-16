//! Reference Rust implementations of the well-known algorithms the runtime
//! benchmarks compare against Fai, plus the registry tying each to its Fai sample
//! module and workload sizes.
//!
//! Each implementation **matches its Fai sample's data representation**, so the
//! benchmark measures the runtime/codegen gap (Fai's boxed, reference-counted
//! values vs Rust's, both via their native backends) rather than an incidental
//! data-structure difference: a workload that iterates or indexes uses a
//! contiguous `Vec` against Fai's `Array`, and one that is naturally persistent —
//! backtracking, or a cons-pattern-matched parser — uses the [`PList`] cons-list
//! (the faithful twin of Fai's linked `List`) against Fai's `List`. The code is
//! otherwise idiomatic within that representation (unboxed scalars, iterators).
//! They are shared by the in-process JIT bench (the Rust side and the correctness
//! oracle), the `algo-baseline` binary (the Rust side of the subprocess AOT
//! bench), and the sample validation tests.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// A persistent, reference-counted singly-linked list — the faithful Rust twin of
/// Fai's `List` (immutable cons cells shared through `Rc`, refcounted like Fai's,
/// so prepending is O(1) and sharing is free). The backtracking and parser oracles
/// hold their data in one of these rather than a contiguous `Vec`, matching their
/// Fai samples: prepend-and-scan stacks (`nqueens`), permutation generation and
/// reversal (`fannkuch`), and a token stream consumed by a recursive-descent
/// parser (`expr_eval`).
enum PList<T> {
    /// The empty list.
    Nil,
    /// A head element and the (shared) tail.
    Cons(T, Rc<PList<T>>),
}

/// Release a long, uniquely-owned `PList` iteratively. The compiler's default drop
/// recurses one frame per cons cell, which overflows a small stack (e.g. a Windows
/// test thread) on a list thousands long; peeling it with `try_unwrap` frees one
/// node per loop iteration instead. A still-shared tail is left to its other
/// owners (it is not this caller's to free).
fn drop_list<T>(mut link: Rc<PList<T>>) {
    while let Ok(node) = Rc::try_unwrap(link) {
        match node {
            PList::Cons(_, rest) => link = rest,
            PList::Nil => return,
        }
    }
}

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

/// Sum of the descending sequence `[n-1, …, 0]` after sorting it ascending.
/// Idiomatic Rust uses a `Vec` and `Vec::sort`; the Fai sample matches it with an
/// `Array` and the standard `Array.sort`.
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

/// The count-weighted sum of a histogram of `n` keys (`i % 256`) tallied into a
/// hash map: an order-independent reduction that exercises map build/query and the
/// hash hot path. The Fai version builds a `HashDict`.
#[must_use]
pub fn dict_histogram(n: i64) -> i64 {
    let buckets = 256;
    let mut counts: HashMap<i64, i64> = HashMap::new();
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

/// The length of the string built by appending the literal `"ab"` onto `"0"` a
/// total of `n` times — an incremental string-building workload. The Fai version
/// builds it with `++`, exercising the runtime's in-place amortized append.
#[must_use]
pub fn string_build(n: i64) -> i64 {
    let mut s = 0.to_string();
    for _ in 0..n {
        s.push_str("ab");
    }
    s.chars().count() as i64
}

/// The total length of 200 half-length substrings of a length-`n` base — a
/// slice-heavy workload. The Fai version takes borrowing slice views (no per-piece
/// copy); the Rust reference takes `&str` prefixes.
#[must_use]
pub fn string_slice(n: i64) -> i64 {
    let base: String = "a".repeat(n.max(0) as usize);
    let half = (n / 2).max(0) as usize;
    let mut acc = 0i64;
    for _ in 0..200 {
        acc += base.chars().take(half).count() as i64;
    }
    acc
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
/// collected into a hash set. The Fai version builds a `HashSet`.
#[must_use]
pub fn set_dedup(n: i64) -> i64 {
    let buckets = 1000;
    let distinct: HashSet<i64> = (0..n).map(|i| i % buckets).collect();
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

/// A register-resident vector/matrix kinematics loop (the SROA showcase): each
/// step squares a 2x2 contraction matrix, applies it to the running 2-vector, and
/// accumulates the components. Mirrors `samples/algorithms/VecMat.fai` exactly
/// (same `f64` operation order), so the fixed-shape-float-aggregate program and
/// this oracle agree bit-for-bit.
#[must_use]
pub fn vec_mat(n: i64) -> f64 {
    let (ra, rb, rc, rd) = (0.5_f64, 0.0 - 0.25, 0.25, 0.5);
    let (mut vx, mut vy, mut acc) = (1.0_f64, 1.0, 0.0);
    let mut i = 0;
    while i < n {
        // m = mulM rot rot
        let ma = ra * ra + rb * rc;
        let mb = ra * rb + rb * rd;
        let mc = rc * ra + rd * rc;
        let md = rc * rb + rd * rd;
        // w = apply m v
        let wx = ma * vx + mb * vy;
        let wy = mc * vx + md * vy;
        acc = acc + wx + wy;
        vx = wx;
        vy = wy;
        i += 1;
    }
    acc
}

/// The number of solutions to the `n`-queens puzzle (backtracking count). The
/// placement so far is a persistent cons-list of chosen columns (most recent
/// first), matching the Fai sample: `c :: placed` shares the tail in O(1), where a
/// `Vec` would copy, so backtracking is the linked structure's natural shape.
#[must_use]
pub fn nqueens(n: i64) -> i64 {
    use PList::{Cons, Nil};
    // Whether column `c` is safe against the placed queens, `d` rows away from the
    // nearest (1 for the immediately previous row).
    fn safe_from(c: i64, d: i64, placed: &PList<i64>) -> bool {
        match placed {
            Nil => true,
            Cons(q, rest) => *q != c && q - c != d && c - q != d && safe_from(c, d + 1, rest),
        }
    }
    fn solve(size: i64, row: i64, placed: &Rc<PList<i64>>) -> i64 {
        if row >= size {
            return 1;
        }
        let mut count = 0;
        for c in 0..size {
            if safe_from(c, 1, placed) {
                count += solve(size, row + 1, &Rc::new(Cons(c, Rc::clone(placed))));
            }
        }
        count
    }
    solve(n, 0, &Rc::new(Nil))
}

/// The sum of all entries of the `n`-by-`n` product `A·B`, with `A(i,j)=(i+j)%7`
/// and `B(i,j)=(i*j+1)%5`. Both sides use an array of rows (Fai `Array (Array
/// Int)` against Rust `Vec<Vec<i64>>`).
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

/// The sum of all entries of the `n`-by-`n` product `A·B` over `Float` matrices,
/// with `A(i,j)=(i+j)%7` and `B(i,j)=(i*j+1)%5` (each cast to `f64`). The float
/// twin of [`matrix_multiply`], exercising unboxed `Array Float` rows/columns;
/// folds left to match the Fai version's accumulation order. Every entry is a
/// small integer, so the `f64` sum is exact (well within 2⁵³).
#[must_use]
pub fn float_matrix_multiply(n: i64) -> f64 {
    let rows: Vec<Vec<f64>> =
        (0..n).map(|i| (0..n).map(|j| ((i + j) % 7) as f64).collect()).collect();
    let cols: Vec<Vec<f64>> =
        (0..n).map(|j| (0..n).map(|i| ((i * j + 1) % 5) as f64).collect()).collect();
    let mut acc = 0.0;
    for arow in &rows {
        let mut inner = 0.0;
        for bcol in &cols {
            inner += arow.iter().zip(bcol).fold(0.0, |d, (x, y)| d + x * y);
        }
        acc += inner;
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
/// binding tighter than `+`/`-`. Mirrors the Fai sample exactly: a token cons-list
/// is parsed by recursive descent into an `Expr` tree (threading the remaining
/// tokens through `Option`, the linked structure a parser naturally consumes) and
/// then evaluated — rather than a contiguous two-pass fold — so the comparison is
/// matched in both representation and algorithm.
#[must_use]
pub fn expr_eval(n: i64) -> i64 {
    use PList::{Cons, Nil};
    type TokList = Rc<PList<Token>>;
    type Parsed = Option<(Expr, TokList)>;

    enum Token {
        Num(i64),
        Plus,
        Minus,
        Star,
    }
    enum Expr {
        Num(i64),
        Add(Box<Expr>, Box<Expr>),
        Sub(Box<Expr>, Box<Expr>),
        Mul(Box<Expr>, Box<Expr>),
    }

    // The number at position `i` (1..9).
    fn num(i: i64) -> i64 {
        i % 9 + 1
    }
    // The operator between numbers `i` and `i+1`, cycling +, *, -.
    fn op_for(i: i64) -> Token {
        match i % 3 {
            0 => Token::Plus,
            1 => Token::Star,
            _ => Token::Minus,
        }
    }
    // `count` numbers separated by cycling operators: the list the Fai `genTokens`
    // builds, assembled back-to-front so a long stream does not recurse.
    fn gen_tokens(count: i64) -> TokList {
        let mut list = Rc::new(Nil);
        let mut i = count - 1;
        while i >= 0 {
            list = Rc::new(Cons(Token::Num(num(i)), list));
            if i > 0 {
                list = Rc::new(Cons(op_for(i - 1), list));
            }
            i -= 1;
        }
        list
    }

    // A factor is a single number.
    fn parse_factor(tokens: &TokList) -> Parsed {
        match &**tokens {
            Cons(Token::Num(k), rest) => Some((Expr::Num(*k), Rc::clone(rest))),
            _ => None,
        }
    }
    // A term is a factor followed by zero or more `* factor`. The Fai writes the
    // tail recursively; the cycling operators make the input long, so the tail is
    // a loop here (the parse result, an `Expr` tree over a token cons-list, is the
    // same).
    fn parse_term(tokens: &TokList) -> Parsed {
        let (mut left, mut rest) = parse_factor(tokens)?;
        loop {
            let more = match &*rest {
                Cons(Token::Star, more) => Rc::clone(more),
                _ => return Some((left, rest)),
            };
            let (right, rest2) = parse_factor(&more)?;
            left = Expr::Mul(Box::new(left), Box::new(right));
            rest = rest2;
        }
    }
    // An expression is a term followed by zero or more `(+|-) term`.
    fn parse_expr(tokens: &TokList) -> Parsed {
        let (mut left, mut rest) = parse_term(tokens)?;
        loop {
            let (subtract, more) = match &*rest {
                Cons(Token::Plus, more) => (false, Rc::clone(more)),
                Cons(Token::Minus, more) => (true, Rc::clone(more)),
                _ => return Some((left, rest)),
            };
            let (right, rest2) = parse_term(&more)?;
            let (l, r) = (Box::new(left), Box::new(right));
            left = if subtract { Expr::Sub(l, r) } else { Expr::Add(l, r) };
            rest = rest2;
        }
    }
    // Evaluate the tree. The left-leaning additive spine can be thousands deep, so
    // the walk uses an explicit work stack rather than native recursion — and it
    // *consumes* the tree (moving each child out of its `Box`), so nothing deeply
    // nested is left for the compiler's recursive drop to overflow on.
    fn eval(root: Expr) -> i64 {
        enum Work {
            Node(Expr),
            Apply(fn(i64, i64) -> i64),
        }
        // Queue the combine beneath both operands, so popping yields them in order.
        fn push_binary(work: &mut Vec<Work>, f: fn(i64, i64) -> i64, a: Expr, b: Expr) {
            work.push(Work::Apply(f));
            work.push(Work::Node(a));
            work.push(Work::Node(b));
        }
        let mut work = vec![Work::Node(root)];
        let mut vals: Vec<i64> = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Node(Expr::Num(k)) => vals.push(k),
                Work::Node(Expr::Add(a, b)) => push_binary(&mut work, i64::wrapping_add, *a, *b),
                Work::Node(Expr::Sub(a, b)) => push_binary(&mut work, i64::wrapping_sub, *a, *b),
                Work::Node(Expr::Mul(a, b)) => push_binary(&mut work, i64::wrapping_mul, *a, *b),
                Work::Apply(f) => {
                    let a = vals.pop().expect("left operand");
                    let b = vals.pop().expect("right operand");
                    vals.push(f(a, b));
                }
            }
        }
        vals.pop().expect("a single result")
    }

    // `eval` consumes the AST (no deep tree is left to drop); the token list is
    // freed iteratively, since it can be thousands of nodes long.
    let tokens = gen_tokens(n);
    let result = match parse_expr(&tokens) {
        Some((e, _)) => eval(e),
        None => 0,
    };
    drop_list(tokens);
    result
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

/// The `n`th Fibonacci number with two's-complement wrapping, computed top-down
/// with a `HashMap` memo threaded through the recursion -- the same algorithm as
/// the Fai sample (which threads a `HashDict`), so the benchmark compares the two
/// associative containers under recursion rather than two different algorithms.
#[must_use]
pub fn fib_memo(n: i64) -> i64 {
    fn go(k: i64, memo: &mut HashMap<i64, i64>) -> i64 {
        if k < 2 {
            return k;
        }
        if let Some(&v) = memo.get(&k) {
            return v;
        }
        let v = go(k - 1, memo).wrapping_add(go(k - 2, memo));
        memo.insert(k, v);
        v
    }
    go(n, &mut HashMap::new())
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

/// The same position-weighted checksum as [`quicksort_sum`], but standing in for
/// the Fai version that sorts a linked `List` with the standard library's stable
/// `List.sortBy` — the linked counterpart to `merge_sort`'s `Array` sort. Sorting
/// is order-total, so the checksum is independent of stability.
#[must_use]
pub fn list_sort_sum(n: i64) -> i64 {
    let mut v: Vec<i64> =
        (0..n).map(|k| (k.wrapping_mul(2_654_435_761).wrapping_add(12345)) % n).collect();
    v.sort();
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
/// (fannkuch-redux, the max-flips figure). Mirrors the Fai sample on the
/// persistent cons-list: permutations and the permutation collection are linked
/// lists, and a flip is a take/reverse/append of the leading run — so both sides
/// pay the same linked traversal cost. The size stays small because the work is
/// `n!` permutations, not because of the representation.
#[must_use]
pub fn fannkuch(n: i64) -> i64 {
    use PList::{Cons, Nil};
    type Perm = Rc<PList<i64>>;
    type Perms = Rc<PList<Perm>>;

    fn take(k: i64, xs: &Perm) -> Perm {
        match &**xs {
            Cons(h, t) if k > 0 => Rc::new(Cons(*h, take(k - 1, t))),
            _ => Rc::new(Nil),
        }
    }
    fn drop(k: i64, xs: &Perm) -> Perm {
        match &**xs {
            Cons(_, t) if k > 0 => drop(k - 1, t),
            _ => Rc::clone(xs),
        }
    }
    fn reverse(xs: &Perm) -> Perm {
        let mut acc = Rc::new(Nil);
        let mut cur = Rc::clone(xs);
        loop {
            let next = match &*cur {
                Nil => break,
                Cons(h, t) => {
                    acc = Rc::new(Cons(*h, acc));
                    Rc::clone(t)
                }
            };
            cur = next;
        }
        acc
    }
    fn append(xs: &Perm, ys: &Perm) -> Perm {
        match &**xs {
            Nil => Rc::clone(ys),
            Cons(h, t) => Rc::new(Cons(*h, append(t, ys))),
        }
    }
    // Reverse the first `k` elements: `reverse (take k xs) ++ drop k xs`.
    fn reverse_first(k: i64, xs: &Perm) -> Perm {
        append(&reverse(&take(k, xs)), &drop(k, xs))
    }
    // Flip the leading run until the head reaches 1, counting the flips.
    fn flips_from(acc: i64, perm: &Perm) -> i64 {
        match &**perm {
            Cons(first, _) if *first > 1 => flips_from(acc + 1, &reverse_first(*first, perm)),
            _ => acc,
        }
    }
    // Remove the first occurrence of `y`.
    fn remove_first(y: i64, xs: &Perm) -> Perm {
        match &**xs {
            Nil => Rc::new(Nil),
            Cons(x, rest) => {
                if *x == y {
                    Rc::clone(rest)
                } else {
                    Rc::new(Cons(*x, remove_first(y, rest)))
                }
            }
        }
    }
    // Prepend `x` to every permutation in `ps`.
    fn prepend_each(x: i64, ps: &Perms) -> Perms {
        match &**ps {
            Nil => Rc::new(Nil),
            Cons(p, rest) => Rc::new(Cons(Rc::new(Cons(x, Rc::clone(p))), prepend_each(x, rest))),
        }
    }
    fn concat(xss: &Perms, ys: &Perms) -> Perms {
        match &**xss {
            Nil => Rc::clone(ys),
            Cons(p, rest) => Rc::new(Cons(Rc::clone(p), concat(rest, ys))),
        }
    }
    // Every permutation of `xs`: for each element, prepend it to every permutation
    // of the rest, concatenating the results (the Fai `concatMap`).
    fn perms(xs: &Perm) -> Perms {
        match &**xs {
            Nil => Rc::new(Cons(Rc::new(Nil), Rc::new(Nil))),
            Cons(_, _) => {
                let mut elems = Vec::new();
                let mut cur = Rc::clone(xs);
                loop {
                    let next = match &*cur {
                        Nil => break,
                        Cons(h, t) => {
                            elems.push(*h);
                            Rc::clone(t)
                        }
                    };
                    cur = next;
                }
                let mut out = Rc::new(Nil);
                for &x in elems.iter().rev() {
                    out = concat(&prepend_each(x, &perms(&remove_first(x, xs))), &out);
                }
                out
            }
        }
    }
    fn range(lo: i64, hi: i64) -> Perm {
        let mut xs = Rc::new(Nil);
        for v in (lo..hi).rev() {
            xs = Rc::new(Cons(v, xs));
        }
        xs
    }
    // Fold the maximum flip count over the permutations (a loop, so the 40k-long
    // permutation list does not recurse).
    let mut acc = 0;
    let mut cur = perms(&range(1, n + 1));
    loop {
        let next = match &*cur {
            Nil => break,
            Cons(p, rest) => {
                acc = acc.max(flips_from(0, p));
                Rc::clone(rest)
            }
        };
        cur = next;
    }
    acc
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

/// Safe integer division: `None` on a zero divisor. The shared helper for the
/// `OptionEval` workload, whose Fai twin threads the resulting `Option Int` — a
/// monomorphic `Option` whose payload is a bare `Int`, stored without a `Some`
/// cell — through a chain of these.
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { None } else { Some(a / b) }
}

/// Evaluate `i*i`, then divide by `i%3`, the quotient by `i%4`, and the sum of
/// the two quotients by `i%5`, short-circuiting to `None` at any zero divisor.
fn eval_chain(i: i64) -> Option<i64> {
    let x = safe_div(i * i, i % 3)?;
    let y = safe_div(x, i % 4)?;
    safe_div(x + y, i % 5)
}

/// Sum the safe-evaluation results over `[0, n)`, taking the chain at `i` or, when
/// it fails, the one at `i + 1` (an `Option` fallback); a pair that both fail
/// contributes nothing. Exercises the niche `Option Int` as a function result and
/// an (owned) argument.
#[must_use]
pub fn option_eval(n: i64) -> i64 {
    let mut acc = 0i64;
    for i in 0..n {
        if let Some(v) = eval_chain(i).or(eval_chain(i + 1)) {
            acc += v;
        }
    }
    acc
}

/// Integer division with a -1 failure sentinel — the `Int`-only twin of
/// [`safe_div`] (no real non-negative quotient is -1, so the sentinel never
/// collides with a success).
fn safe_div_sentinel(a: i64, b: i64) -> i64 {
    if b == 0 { -1 } else { a / b }
}

/// The `Int`-only twin of [`eval_chain`]: -1 marks failure instead of `None`.
fn eval_chain_sentinel(i: i64) -> i64 {
    let x = safe_div_sentinel(i * i, i % 3);
    if x == -1 {
        return -1;
    }
    let y = safe_div_sentinel(x, i % 4);
    if y == -1 {
        return -1;
    }
    safe_div_sentinel(x + y, i % 5)
}

/// The `Int`-only twin of [`option_eval`]: the same safe-evaluation sum, but
/// failure is the sentinel -1 rather than the niche `Option`. With no `Option`
/// there is no niche representation to preserve, so this is the baseline an
/// `Option Int`-threading evaluator should match once its niche encoding survives
/// the loop without per-iteration allocation. Computes the identical result to
/// [`option_eval`].
#[must_use]
pub fn int_eval(n: i64) -> i64 {
    let mut acc = 0i64;
    for i in 0..n {
        let first = eval_chain_sentinel(i);
        let v = if first == -1 { eval_chain_sentinel(i + 1) } else { first };
        if v != -1 {
            acc += v;
        }
    }
    acc
}

/// Follow "next pointer" chains through a lookup table, summing visited keys. The
/// `HashMap` stands in for the Fai linear association list (keys are unique, so
/// the looked-up value is identical); a missing key is the niche `None`, ending
/// the walk, and a per-walk fuel bounds the (cyclic) table so it terminates.
#[must_use]
pub fn option_path(n: i64) -> i64 {
    let size = 100i64;
    let table: HashMap<i64, i64> = (0..size).map(|i| (i, (i * 2 + 1) % size)).collect();
    let mut total = 0i64;
    for i in 0..n {
        let mut key = i % (size * 2);
        let mut fuel = size;
        let mut acc = 0i64;
        while fuel > 0 {
            match table.get(&key) {
                None => break,
                Some(&next) => {
                    acc += key;
                    key = next;
                    fuel -= 1;
                }
            }
        }
        total += acc;
    }
    total
}

/// Binary-search-tree `find`, summing the hits. The `BTreeMap` of `key -> key*3`
/// for `key` in `[0, m)` mirrors the balanced BST the Fai version builds; queries
/// at `i % (2m)` miss half the time (the niche `None`), and `find` returns the
/// niche `Option Int` up through the recursion.
#[must_use]
pub fn option_tree_find(n: i64) -> i64 {
    let m = 1000i64;
    let tree: BTreeMap<i64, i64> = (0..m).map(|k| (k, k * 3)).collect();
    let mut total = 0i64;
    for i in 0..n {
        if let Some(&v) = tree.get(&(i % (m * 2))) {
            total += v;
        }
    }
    total
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
        jit_size: 5000,
        aot_size: 100000,
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
        jit_size: 3000,
        aot_size: 50000,
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
        module: "VecMat",
        entry: "runF",
        jit_size: 100_000,
        aot_size: 2_000_000,
        oracle: Oracle::Float(vec_mat),
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
        module: "FloatMatrixMultiply",
        entry: "run",
        jit_size: 30,
        aot_size: 90,
        oracle: Oracle::Float(float_matrix_multiply),
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
        jit_size: 50,
        aot_size: 200,
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
        jit_size: 2000,
        aot_size: 30000,
        oracle: Oracle::Int(graph_bfs),
    },
    Algorithm {
        module: "CoinChange",
        entry: "run",
        jit_size: 2000,
        aot_size: 20000,
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
        jit_size: 5000,
        aot_size: 100000,
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
        jit_size: 2000,
        aot_size: 30000,
        oracle: Oracle::Int(union_find),
    },
    Algorithm {
        module: "JsonSerialize",
        entry: "run",
        jit_size: 2_000,
        aot_size: 30_000,
        oracle: Oracle::Int(json_serialize),
    },
    Algorithm {
        module: "StringBuild",
        entry: "run",
        jit_size: 20_000,
        aot_size: 1_000_000,
        oracle: Oracle::Int(string_build),
    },
    Algorithm {
        module: "StringSlice",
        entry: "run",
        jit_size: 2_000,
        aot_size: 20_000,
        oracle: Oracle::Int(string_slice),
    },
    Algorithm {
        module: "OptionEval",
        entry: "run",
        jit_size: 5_000,
        aot_size: 100_000,
        oracle: Oracle::Int(option_eval),
    },
    Algorithm {
        module: "IntEval",
        entry: "run",
        jit_size: 5_000,
        aot_size: 100_000,
        oracle: Oracle::Int(int_eval),
    },
    Algorithm {
        module: "OptionPath",
        entry: "run",
        jit_size: 500,
        aot_size: 4_000,
        oracle: Oracle::Int(option_path),
    },
    Algorithm {
        module: "OptionTreeFind",
        entry: "run",
        jit_size: 5_000,
        aot_size: 100_000,
        oracle: Oracle::Int(option_tree_find),
    },
    Algorithm {
        module: "ListSort",
        entry: "run",
        jit_size: 2_000,
        aot_size: 20_000,
        oracle: Oracle::Int(list_sort_sum),
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
        "VecMat" => include_str!("../../../samples/algorithms/VecMat.fai"),
        "NQueens" => include_str!("../../../samples/algorithms/NQueens.fai"),
        "MatrixMultiply" => include_str!("../../../samples/algorithms/MatrixMultiply.fai"),
        "FloatMatrixMultiply" => {
            include_str!("../../../samples/algorithms/FloatMatrixMultiply.fai")
        }
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
        "StringBuild" => include_str!("../../../samples/algorithms/StringBuild.fai"),
        "StringSlice" => include_str!("../../../samples/algorithms/StringSlice.fai"),
        "OptionEval" => include_str!("../../../samples/algorithms/OptionEval.fai"),
        "IntEval" => include_str!("../../../samples/algorithms/IntEval.fai"),
        "OptionPath" => include_str!("../../../samples/algorithms/OptionPath.fai"),
        "OptionTreeFind" => include_str!("../../../samples/algorithms/OptionTreeFind.fai"),
        "ListSort" => include_str!("../../../samples/algorithms/ListSort.fai"),
        other => panic!("unknown algorithm module: {other}"),
    }
}
