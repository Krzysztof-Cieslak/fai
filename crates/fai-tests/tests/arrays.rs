//! End-to-end `Array` tests through the public JIT pipeline
//! ([`fai_driver::jit_run_program`]): the standard-library `Array` module
//! typechecks and runs, the structural ops behave, and the Perceus in-place /
//! borrow optimizations fire (asserted via the cumulative heap-allocation count)
//! while every program stays leak-free (a clean exit code).
//!
//! The runtime's allocation and live-object counters are process-global and
//! compiled in only under `debug_assertions`, so every case serializes on
//! [`LOCK`] and the allocation-delta assertions are meaningful in a debug build
//! (the default for `cargo test`).

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::jit_run_program;
use fai_runtime as rt;
use indoc::formatdoc;
use proptest::prelude::*;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Compiles and JIT-runs `src`, returning `(exit_code, stdout, allocations)`. An
/// exit code of 0 implies a leak-free run (the runtime aborts otherwise).
fn run_counted(src: &str) -> (i32, String, i64) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, file);
    let allocs = rt::allocations();
    let out = rt::capture_take();
    (outcome.exit_code, out, allocs)
}

/// Wraps `body` (an `Int` expression) in a `main` that prints it, runs it, and
/// asserts a clean (leak-free) exit and the expected output.
#[track_caller]
fn outputs(body: &str, expect: &str) {
    let src = formatdoc! {r#"
        module M

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString ({body}))
    "#};
    let (code, out, _) = run_counted(&src);
    assert_eq!(code, 0, "clean (leak-free) exit for `{body}`:\n{out}");
    assert_eq!(out.trim(), expect, "output for `{body}`");
}

/// Runs `body` and returns its cumulative allocation count (asserting a clean
/// exit and expected output first).
#[track_caller]
fn allocs(body: &str, expect: &str) -> i64 {
    let src = formatdoc! {r#"
        module M

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString ({body}))
    "#};
    let (code, out, a) = run_counted(&src);
    assert_eq!(code, 0, "clean (leak-free) exit for `{body}`:\n{out}");
    assert_eq!(out.trim(), expect, "output for `{body}`");
    a
}

// ===========================================================================
// Build, map, fold (the headline list-rep workload, now contiguous).
// ===========================================================================

#[test]
fn map_sum_over_a_range() {
    outputs("Array.sum (Array.map (fun x -> x * 2) (Array.range 0 5))", "20");
}

#[test]
fn fold_over_an_array() {
    outputs("Array.foldl (fun acc x -> acc + x) 0 (Array.range 1 5)", "10");
}

#[test]
fn length_and_get() {
    outputs("Array.length (Array.range 0 7)", "7");
    outputs("Option.withDefault 0 (Array.get 3 (Array.range 0 7))", "3");
    outputs("Option.withDefault 99 (Array.get 100 (Array.range 0 7))", "99");
}

#[test]
fn filter_keeps_matching() {
    outputs("Array.sum (Array.filter (fun x -> x > 2) (Array.range 0 6))", "12");
}

#[test]
fn round_trips_through_list() {
    outputs("List.sum (Array.toList (Array.fromList [1, 2, 3, 4]))", "10");
}

#[test]
fn reverse_then_sum_first() {
    outputs("Option.withDefault 0 (Array.head (Array.reverse (Array.range 0 5)))", "4");
}

// ===========================================================================
// Array-literal syntax `[| … |]`.
// ===========================================================================

#[test]
fn array_literal_sums() {
    outputs("Array.sum [| 1, 2, 3, 4 |]", "10");
}

#[test]
fn empty_array_literal_has_length_zero() {
    // `sum` fixes the element type to `Int`, so the empty literal needs no
    // annotation.
    outputs("Array.sum [||]", "0");
    outputs("Array.length (Array.append [||] [| 7 |])", "1");
}

#[test]
fn array_literal_elements_evaluate_in_order() {
    outputs("Array.sum (Array.map (fun x -> x * x) [| 1, 2, 3 |])", "14");
}

#[test]
fn nested_array_literal() {
    outputs("Array.sum (Array.map Array.sum [| [| 1, 2 |], [| 3, 4 |] |])", "10");
}

// ===========================================================================
// Sort (in-place unstable quicksort), including the reverse-sorted worst case.
// ===========================================================================

#[test]
fn sorts_a_shuffled_array() {
    outputs("Array.sum (Array.sort (Array.fromList [3, 1, 4, 1, 5, 9, 2, 6]))", "31");
}

#[test]
fn sorts_a_reverse_sorted_array() {
    // The median-of-three pivot keeps this (quicksort's classic worst case) fast
    // and, more importantly here, correct.
    outputs(
        "Option.withDefault 0 (Array.head (Array.sort (Array.reverse (Array.range 0 50))))",
        "0",
    );
}

#[test]
fn sort_is_idempotent_on_a_range() {
    outputs("Array.sum (Array.sort (Array.range 0 100))", "4950");
}

// ===========================================================================
// Perceus: in-place set/push when unique, builder allocates once.
// ===========================================================================

#[test]
fn builder_is_allocation_light() {
    // `range` is one withCapacity + in-place pushes; `map` builds one more buffer;
    // `sum` folds with no allocation. The whole pipeline is a small constant
    // number of allocations, independent of the element count (no per-element
    // cons cell as the linked List would do).
    let small = allocs("Array.sum (Array.map (fun x -> x + 1) (Array.range 0 10))", "55");
    let big = allocs("Array.sum (Array.map (fun x -> x + 1) (Array.range 0 1000))", "500500");
    assert_eq!(small, big, "allocation count is independent of length (contiguous, in-place)");
}

#[test]
fn unique_set_is_in_place() {
    // Setting an element of a freshly built (unique) array overwrites in place, so
    // the allocation count is a small constant independent of the array size — a
    // buffer copy would scale with the length.
    let small = allocs(
        "Array.sum (Option.withDefault Array.empty (Array.set 0 100 (Array.range 0 10)))",
        "145",
    );
    let big = allocs(
        "Array.sum (Option.withDefault Array.empty (Array.set 0 100 (Array.range 0 1000)))",
        "499600",
    );
    assert_eq!(small, big, "a unique set is in place (no buffer copy that scales with length)");
}

// ===========================================================================
// Generate-and-run: random array pipelines agree with a Rust `Vec` oracle and
// run leak-free. This validates the hand-written `unsafe` array intrinsics and
// their codegen against an independent implementation over arbitrary programs.
// ===========================================================================

/// A structural array-pipeline expression, rendered two ways: to Fai source over
/// the public `Array` API, and to a Rust `Vec<i64>` interpreter (the oracle).
#[derive(Debug, Clone)]
enum ArrExpr {
    Range(i64, i64),
    Lit(Vec<i64>),
    Map(i64, Box<ArrExpr>),
    Filter(i64, Box<ArrExpr>),
    Reverse(Box<ArrExpr>),
    Take(i64, Box<ArrExpr>),
    Sort(Box<ArrExpr>),
    Append(Box<ArrExpr>, Box<ArrExpr>),
}

/// Renders the pipeline to a Fai expression over the public `Array` API.
fn render_arr(e: &ArrExpr) -> String {
    match e {
        ArrExpr::Range(lo, hi) => format!("(Array.range {lo} {hi})"),
        ArrExpr::Lit(xs) => {
            if xs.is_empty() {
                // `sum` fixes the element type, so a bare `[||]` is well typed here.
                "[||]".to_owned()
            } else {
                let elems: Vec<String> = xs.iter().map(i64::to_string).collect();
                format!("[| {} |]", elems.join(", "))
            }
        }
        ArrExpr::Map(k, e) => format!("(Array.map (fun x -> x + {k}) {})", render_arr(e)),
        ArrExpr::Filter(j, e) => format!("(Array.filter (fun x -> x > {j}) {})", render_arr(e)),
        ArrExpr::Reverse(e) => format!("(Array.reverse {})", render_arr(e)),
        ArrExpr::Take(n, e) => format!("(Array.take {n} {})", render_arr(e)),
        ArrExpr::Sort(e) => format!("(Array.sort {})", render_arr(e)),
        ArrExpr::Append(a, b) => format!("(Array.append {} {})", render_arr(a), render_arr(b)),
    }
}

/// Evaluates the pipeline over `Vec<i64>` — the independent oracle.
fn eval_arr(e: &ArrExpr) -> Vec<i64> {
    match e {
        ArrExpr::Range(lo, hi) => (*lo..*hi).collect(),
        ArrExpr::Lit(xs) => xs.clone(),
        ArrExpr::Map(k, e) => eval_arr(e).into_iter().map(|x| x + k).collect(),
        ArrExpr::Filter(j, e) => eval_arr(e).into_iter().filter(|&x| x > *j).collect(),
        ArrExpr::Reverse(e) => {
            let mut v = eval_arr(e);
            v.reverse();
            v
        }
        ArrExpr::Take(n, e) => {
            let take = (*n).max(0) as usize;
            eval_arr(e).into_iter().take(take).collect()
        }
        ArrExpr::Sort(e) => {
            let mut v = eval_arr(e);
            v.sort_unstable();
            v
        }
        ArrExpr::Append(a, b) => {
            let mut v = eval_arr(a);
            v.extend(eval_arr(b));
            v
        }
    }
}

/// A bounded generator: small ranges/literals and shallow nesting, so each
/// generated program JIT-compiles and runs quickly and the integer sum stays well
/// within the immediate range.
fn arr_expr() -> impl Strategy<Value = ArrExpr> {
    let leaf = prop_oneof![
        (0i64..20, 0i64..20).prop_map(|(lo, len)| ArrExpr::Range(lo, lo + len)),
        prop::collection::vec(-5i64..5, 0..4).prop_map(ArrExpr::Lit),
    ];
    leaf.prop_recursive(3, 16, 2, |inner| {
        prop_oneof![
            (-5i64..5, inner.clone()).prop_map(|(k, e)| ArrExpr::Map(k, Box::new(e))),
            (-5i64..5, inner.clone()).prop_map(|(j, e)| ArrExpr::Filter(j, Box::new(e))),
            inner.clone().prop_map(|e| ArrExpr::Reverse(Box::new(e))),
            (0i64..25, inner.clone()).prop_map(|(n, e)| ArrExpr::Take(n, Box::new(e))),
            inner.clone().prop_map(|e| ArrExpr::Sort(Box::new(e))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| ArrExpr::Append(Box::new(a), Box::new(b))),
        ]
    })
}

proptest! {
    // Each case is a full JIT compile + run under the global counter lock, so keep
    // the case count modest.
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn random_array_pipeline_matches_vec_oracle(e in arr_expr()) {
        let expected: i64 = eval_arr(&e).into_iter().sum();
        let src = formatdoc! {r#"
            module M

            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (Array.sum {body}))
        "#, body = render_arr(&e)};
        let (code, out, _) = run_counted(&src);
        prop_assert_eq!(code, 0, "leak-free exit for:\n{}\n{}", render_arr(&e), out);
        prop_assert_eq!(out.trim(), expected.to_string(), "oracle mismatch for {}", render_arr(&e));
    }
}
