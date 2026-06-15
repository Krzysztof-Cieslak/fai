//! End-to-end tests for combinator-pipeline deforestation (`fai_core::fuse`).
//!
//! A recognized chain of directly-nested standard combinators fuses to a single
//! synthesized loop that materializes no intermediate sequence. These tests run
//! the fused program through the public JIT pipeline and assert both the result
//! (equal to the unfused oracle) and the cumulative heap-allocation count.
//!
//! Two allocation signals prove deforestation:
//!
//! * A **`List`** producer unfused builds one cons cell per element (`O(n)`
//!   allocations), so a fused chain's allocation count being **independent of `n`**
//!   proves the spine is gone.
//! * An **`Array`** producer unfused builds a constant number of buffers
//!   (independent of `n` already), so we instead compare against a pure-arithmetic
//!   baseline computing the same value: equal allocation counts prove the chain
//!   built **zero** buffers beyond the fixed program overhead.
//!
//! The runtime's allocation counter is process-global and compiled in only under
//! `debug_assertions`, so every case serializes on [`LOCK`].

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::jit_run_program;
use fai_runtime as rt;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Compiles and JIT-runs a module whose `run : Int -> Int` body is `body`,
/// printing `run n`. `body` is placed two-space-indented under `let run n =`, so a
/// multi-line body's continuation lines must already be two-space indented.
/// Returns `(exit_code, stdout, allocations)`.
fn run_int(body: &str, n: i64) -> (i32, String, i64) {
    let _g = lock();
    let src = format!(
        "module M\n\n\
         public run : Int -> Int\n\
         let run n =\n  {body}\n\n\
         public main : Runtime -> Unit / {{ Console }}\n\
         let main rt = rt.console.writeLine (Int.toString (run {n}))\n",
    );
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src);
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, file);
    let allocs = rt::allocations();
    let out = rt::capture_take();
    (outcome.exit_code, out, allocs)
}

/// Asserts `run n = expect` (a clean, leak-free exit) for the pipeline `body`.
#[track_caller]
fn outputs(body: &str, n: i64, expect: &str) {
    let (code, out, _) = run_int(body, n);
    assert_eq!(code, 0, "clean exit for `{body}` (n={n}):\n{out}");
    assert_eq!(out.trim(), expect, "output for `{body}` (n={n})");
}

/// Returns the allocation count of `run n` (asserting a clean exit + output).
#[track_caller]
fn allocs(body: &str, n: i64, expect: &str) -> i64 {
    let (code, out, a) = run_int(body, n);
    assert_eq!(code, 0, "clean exit for `{body}` (n={n}):\n{out}");
    assert_eq!(out.trim(), expect, "output for `{body}` (n={n})");
    a
}

// ===========================================================================
// Behavior: a fused pipeline computes the same result as the unfused oracle.
// ===========================================================================

#[test]
fn array_map_sum_over_range() {
    // The `map_sum` row: sum of doubling [0, n).
    outputs("Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))", 5, "20");
    outputs("Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))", 100, "9900");
}

#[test]
fn array_map_sum_over_value() {
    // The `map_sum_shared` shape: the source is a let-bound, shared value, so it
    // stays materialized while the adjacent map->sum fuses.
    outputs(
        "let xs = Array.range 0 n\n  Array.sum (Array.map (fun x -> x * 2) xs) + Array.sum xs",
        4,
        "18",
    );
}

#[test]
fn list_foldl_over_range_with_composed_step() {
    // foldl with a composed step over a List range: acc + (x + 1) * 2.
    outputs("List.foldl (fun acc x -> acc + (x + 1) * 2) 0 (List.range 0 n)", 3, "12");
}

#[test]
fn array_filter_sum_over_range() {
    outputs("Array.sum (Array.filter (fun x -> x > 2) (Array.range 0 n))", 6, "12");
}

#[test]
fn list_map_sum_over_value() {
    outputs("List.sum (List.map (fun x -> x + 1) [1, 2, 3])", 0, "9");
}

#[test]
fn array_length_after_filter() {
    outputs("Array.length (Array.filter (fun x -> x > 2) (Array.range 0 n))", 6, "3");
}

#[test]
fn array_map_filter_sum_over_value() {
    // A multi-stage chain over a value source: map then filter then sum, one loop.
    outputs(
        "let xs = Array.range 0 n\n  Array.sum (Array.filter (fun y -> y > 4) (Array.map (fun x -> x * 2) xs))",
        6,
        "24",
    );
}

#[test]
fn array_init_source_sum() {
    // `Array.init n f` as a producer, consumed by sum.
    outputs("Array.sum (Array.map (fun x -> x + 1) (Array.init n (fun i -> i * i)))", 4, "18");
}

#[test]
fn array_repeat_source_sum() {
    outputs("Array.sum (Array.repeat n 3)", 5, "15");
}

#[test]
fn all_short_circuits() {
    outputs("if Array.all (fun x -> x < 100) (Array.range 0 n) then 1 else 0", 10, "1");
    outputs("if Array.all (fun x -> x < 5) (Array.range 0 n) then 1 else 0", 10, "0");
}

#[test]
fn any_short_circuits() {
    outputs("if Array.any (fun x -> x > 5) (Array.range 0 n) then 1 else 0", 10, "1");
    outputs("if List.any (fun x -> x > 50) (List.range 0 n) then 1 else 0", 10, "0");
}

#[test]
fn member_over_a_mapped_value() {
    outputs(
        "if Array.member 6 (Array.map (fun x -> x * 2) (Array.range 0 n)) then 1 else 0",
        5,
        "1",
    );
    outputs(
        "if Array.member 7 (Array.map (fun x -> x * 2) (Array.range 0 n)) then 1 else 0",
        5,
        "0",
    );
}

#[test]
fn find_returns_first_match() {
    outputs(
        "Option.withDefault 0 (Array.find (fun x -> x > 4) (Array.map (fun x -> x * 2) (Array.range 0 n)))",
        10,
        "6",
    );
    outputs("Option.withDefault 99 (Array.find (fun x -> x > 999) (Array.range 0 n))", 10, "99");
}

#[test]
fn array_foldr_over_range() {
    // foldr drives the loop downward; cons-building the range reversed checks order.
    outputs("List.length (Array.foldr (fun x acc -> x :: acc) [] (Array.range 0 n))", 5, "5");
    outputs("Array.foldr (fun x acc -> x - acc) 0 (Array.range 0 n)", 4, "-2");
}

#[test]
fn array_terminal_map_builder() {
    // A terminal map builds one array (the result); the range is fused away.
    outputs("Array.sum (Array.map (fun x -> x + 1) (Array.range 0 n))", 4, "10");
    // Build then independently sum: the builder produces a real array.
    outputs("let ys = Array.map (fun x -> x * 10) (Array.range 0 n)\n  Array.sum ys", 3, "30");
}

#[test]
fn list_terminal_map_builder_preserves_order() {
    outputs("List.sum (List.map (fun x -> x * x) (List.range 1 (n + 1)))", 3, "14");
    outputs(
        "let ys = List.map (fun x -> x + 1) (List.range 0 n)\n  Option.withDefault 0 (List.head ys)",
        5,
        "1",
    );
}

#[test]
fn list_literal_unrolls() {
    outputs("List.sum (List.map (fun x -> x + 1) [10, 20, 30])", 0, "63");
    outputs("if List.any (fun x -> x > 25) [10, 20, 30] then 1 else 0", 0, "1");
}

#[test]
fn array_literal_unrolls() {
    outputs("Array.sum (Array.map (fun x -> x * 2) [| 1, 2, 3 |])", 0, "12");
}

#[test]
fn literal_map_sum_eliminates_the_literal() {
    // Unrolled to straight-line arithmetic: no list cells, no map buffer — the
    // allocation count equals a pure-arithmetic baseline computing the same value.
    let fused = allocs("List.sum (List.map (fun x -> x + 1) [10, 20, 30])", 0, "63");
    let baseline = allocs("11 + 21 + 31", 0, "63");
    assert_eq!(
        fused, baseline,
        "a small literal pipeline unrolls to no allocations (fused={fused}, baseline={baseline})"
    );
}

#[test]
fn list_filter_builder_preserves_order() {
    outputs(
        "let ys = List.filter (fun x -> x % 2 = 0) (List.range 0 n)\n  Option.withDefault 0 (List.head ys)",
        6,
        "0",
    );
    outputs("List.sum (List.filter (fun x -> x > 2) (List.range 0 n))", 6, "12");
}

// ===========================================================================
// Allocation: deforestation removes the intermediate sequences.
// ===========================================================================

#[test]
fn map_sum_over_range_allocates_zero_buffers() {
    // Fully fused (range + literal map + sum) is a raw accumulator loop building
    // nothing — so its allocation count equals a pure-arithmetic baseline that
    // computes the same value (`n * (n - 1)` = sum of 2*[0,n)). Any residual buffer
    // would show as extra allocations over the baseline.
    let n = 50;
    let value = n * (n - 1);
    let fused =
        allocs("Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))", n, &value.to_string());
    let baseline = allocs("n * (n - 1)", n, &value.to_string());
    assert_eq!(
        fused, baseline,
        "fused map_sum must allocate no buffers (fused={fused}, baseline={baseline})"
    );
}

#[test]
fn map_sum_over_range_is_independent_of_n() {
    let small = allocs("Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))", 10, "90");
    let big = allocs("Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))", 1000, "999000");
    assert_eq!(small, big, "fused map_sum allocations must not scale with n");
}

#[test]
fn fold_pipeline_builds_no_list_spine() {
    // Unfused, `List.range 0 n` builds n cons cells (O(n) allocations); fused, the
    // count is independent of n.
    let small = allocs("List.foldl (fun acc x -> acc + x) 0 (List.range 0 n)", 10, "45");
    let big = allocs("List.foldl (fun acc x -> acc + x) 0 (List.range 0 n)", 1000, "499500");
    assert_eq!(small, big, "fused fold_pipeline allocations must not scale with n (no list spine)");
}

/// Compiles and JIT-runs a full module `src`, returning `(exit_code, stdout,
/// allocations)`.
fn run_full(src: &str) -> (i32, String, i64) {
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

#[test]
fn composed_caf_fold_allocations_are_independent_of_n() {
    // The real `FoldPipeline` shape: a CAF composed from `>>` and a partial
    // application (`shift 3`), folded over a range. Confining the composition to
    // arithmetic (then deforesting the fold) leaves nothing per element to allocate,
    // so the total is independent of `n`. Without confinement the CAF is rebuilt
    // per element — two `>>` closures and a `shift` partial application each — so the
    // count would grow with `n`. run(n) = sum over [0, n) of (2x + 5) = n^2 + 4n.
    let module = |n: i64| {
        format!(
            "module M\n\n\
             let shift k x = x + k\n\n\
             let transform = (fun x -> x + 1) >> (fun x -> x * 2) >> shift 3\n\n\
             public run : Int -> Int\n\
             let run n = List.foldl (fun acc x -> acc + transform x) 0 (List.range 0 n)\n\n\
             public main : Runtime -> Unit / {{ Console }}\n\
             let main rt = rt.console.writeLine (Int.toString (run {n}))\n",
        )
    };
    let (code_s, out_s, small) = run_full(&module(10));
    let (code_b, out_b, big) = run_full(&module(1000));
    assert_eq!((code_s, out_s.trim()), (0, "140"), "clean exit, correct sum (n=10)");
    assert_eq!((code_b, out_b.trim()), (0, "1004000"), "clean exit, correct sum (n=1000)");
    assert_eq!(small, big, "composed-CAF fold allocations must not scale with n");
}

#[test]
fn list_map_sum_builds_no_intermediate_spine() {
    let small = allocs("List.sum (List.map (fun x -> x + 1) (List.range 0 n))", 10, "55");
    let big = allocs("List.sum (List.map (fun x -> x + 1) (List.range 0 n))", 1000, "500500");
    assert_eq!(small, big, "fused list map->sum allocations must not scale with n");
}
