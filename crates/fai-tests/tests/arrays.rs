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
use indoc::{formatdoc, indoc};
use proptest::prelude::*;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Compiles and JIT-runs `src`, returning `(exit_code, stdout, allocations,
/// array_copies)`. An exit code of 0 implies a leak-free run (the runtime aborts
/// otherwise). `array_copies` counts shared-array buffer duplications (the
/// uniqueness-loss copies in `set`/`push`).
fn run_counted(src: &str) -> (i32, String, i64, i64) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, file);
    let allocs = rt::allocations();
    let copies = rt::array_copies();
    let out = rt::capture_take();
    (outcome.exit_code, out, allocs, copies)
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
    let (code, out, _, _) = run_counted(&src);
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
    let (code, out, a, _) = run_counted(&src);
    assert_eq!(code, 0, "clean (leak-free) exit for `{body}`:\n{out}");
    assert_eq!(out.trim(), expect, "output for `{body}`");
    a
}

/// Runs `body` and returns its shared-array buffer-copy count (asserting a clean
/// exit and expected output first). A copy is a uniqueness-loss duplication of the
/// whole buffer in `set`/`push`; zero means every mutation was in place.
#[track_caller]
fn copies(body: &str, expect: &str) -> i64 {
    let src = formatdoc! {r#"
        module M

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString ({body}))
    "#};
    let (code, out, _, c) = run_counted(&src);
    assert_eq!(code, 0, "clean (leak-free) exit for `{body}`:\n{out}");
    assert_eq!(out.trim(), expect, "output for `{body}`");
    c
}

/// Runs a full `src` module and returns its shared-array buffer-copy count
/// (asserting a clean exit and expected output first). For multi-definition
/// programs the single-expression [`copies`] cannot express.
#[track_caller]
fn copies_full(src: &str, expect: &str) -> i64 {
    let (code, out, _, c) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{src}\n{out}");
    assert_eq!(out.trim(), expect, "output:\n{src}");
    c
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
// Inlined allocation fast path: `withCapacity` and the push-grow path pop/push a
// pooled cell and write the header inline (see the codegen IR-shape tests). These
// exercise that machinery end-to-end — the inlined pop interoperates with the
// runtime free list (a freed buffer is recycled by a later inlined construction),
// the grow path's fresh-buffer alloc + element move + old-buffer free are
// balanced, and everything stays leak-free with the counters consistent.
// ===========================================================================

#[test]
fn growing_a_unique_array_from_empty_does_no_uniqueness_copies() {
    // Repeatedly pushing onto a uniquely-owned array grows it by doubling through
    // the inlined grow path (fresh pooled buffer + element move + old-buffer free),
    // never a uniqueness-loss copy. The result is correct and the run leak-free.
    let src = indoc! {r#"
        module M

        build : Int -> Array Int -> Array Int
        let build i acc = if i >= 100 then acc else build (i + 1) (Array.push i acc)

        public main : Runtime -> Unit / { Console }
        let main rt = rt.console.writeLine (Int.toString (Array.sum (build 0 Array.empty)))
    "#};
    // sum 0..99 = 4950.
    let c = copies_full(src, "4950");
    assert_eq!(c, 0, "growing a unique array by doubling is amortized growth, not a copy");
}

#[test]
fn concat_grows_without_uniqueness_copies() {
    // `Array.concat` accumulates into a buffer that starts empty and grows by
    // doubling (the inlined grow path); the accumulator stays unique, so no
    // uniqueness-loss copy occurs.
    let c = copies(
        "Array.sum (Array.concat (Array.fromList [Array.range 0 30, Array.range 30 60]))",
        "1770",
    );
    assert_eq!(c, 0, "a unique concat accumulator grows in place, never copies");
}

#[test]
fn repeated_construction_is_leak_free_and_pool_recycles() {
    // Many short-lived arrays are built and dropped in a loop: each construction's
    // inlined pool pop recycles the previous iteration's freed buffer, and each
    // drop returns it. A wrong slot offset or unbalanced counter would crash, leak
    // (a non-zero exit), or corrupt a recycled cell; a clean exit with the correct
    // sum proves the inlined pop/push interoperate with the runtime free list.
    let src = indoc! {r#"
        module M

        run : Int -> Int -> Int
        let run i acc =
          if i >= 500 then acc else run (i + 1) (acc + Array.sum (Array.range 0 8))

        public main : Runtime -> Unit / { Console }
        let main rt = rt.console.writeLine (Int.toString (run 0 0))
    "#};
    // Each `Array.range 0 8` sums to 28; 500 of them = 14000.
    let (code, out, _, _) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{out}");
    assert_eq!(out.trim(), "14000");
}

#[test]
fn with_capacity_construction_is_allocation_light() {
    // A presized builder (`withCapacity n` then in-place pushes) allocates a small
    // constant number of buffers independent of length, now that the construction
    // allocation is inlined (the pooled pop, with the runtime fallback). Mirrors
    // `builder_is_allocation_light` for the explicit `withCapacity` entry point.
    let small = allocs("Array.length (Array.init 10 (fun i -> i))", "10");
    let big = allocs("Array.length (Array.init 1000 (fun i -> i))", "1000");
    assert_eq!(small, big, "presized construction allocates independently of length");
}

// ===========================================================================
// Inlined element access: get/length/set/push compile to inline loads/stores
// (see the codegen IR-shape tests). These exercise the result representations
// end-to-end — a raw `Int`, an unboxed `f64`, and a boxed element duplicated
// inline — and the generic (type-variable element) path, asserting correctness
// and a leak-free exit.
// ===========================================================================

#[test]
fn float_array_unsafe_get_reads_inline_and_is_leak_free() {
    // A hand-written read loop over a concrete `Array Float`: each `unsafeGet`
    // inlines to a slot load unboxed to an `f64` (no per-element call or box). The
    // sum is correct and the run is leak-free (a clean exit).
    let src = indoc! {r#"
        module M

        dot : Int -> Float -> Array Float -> Int -> Float
        let dot i acc xs n =
          if i >= n then acc else dot (i + 1) (acc + Array.unsafeGet i xs) xs n

        public main : Runtime -> Unit / { Console }
        let main rt =
          let xs = Array.map Int.toFloat (Array.range 0 5)
          rt.console.writeLine (Float.toString (dot 0 0.0 xs (Array.length xs)))
    "#};
    let (code, out, _, _) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{out}");
    assert_eq!(out.trim(), "10.0", "0.0+1.0+2.0+3.0+4.0");
}

#[test]
fn boxed_element_array_unsafe_get_dups_inline_and_is_leak_free() {
    // A read loop over a concrete `Array String`: each `unsafeGet` inlines to a slot
    // load plus an inline tag-checked dup of the boxed element, so the returned
    // reference outlives the borrowed array's drop. Concatenation is correct and
    // leak-free (a mismatched dup/drop would leak or double-free).
    let src = indoc! {r#"
        module M

        cat : Int -> String -> Array String -> Int -> String
        let cat i acc xs n =
          if i >= n then acc else cat (i + 1) (acc ++ Array.unsafeGet i xs) xs n

        public main : Runtime -> Unit / { Console }
        let main rt =
          let xs = Array.fromList ["a", "b", "c"]
          rt.console.writeLine (cat 0 "" xs (Array.length xs))
    "#};
    let (code, out, _, _) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{out}");
    assert_eq!(out.trim(), "abc");
}

#[test]
fn generic_sort_over_boxed_elements_is_correct_and_leak_free() {
    // `Array.sort` is compiled once at a type variable, so its `unsafeGet`/
    // `unsafeSet` inline via the uniform path — a tag-checked dup on read and a
    // tag-checked release of the overwritten element. Sorting boxed `String`
    // elements exercises that path: the order is correct and the run is leak-free.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit / { Console }
        let main rt =
          let xs = Array.sort (Array.fromList ["banana", "apple", "cherry"])
          rt.console.writeLine (Option.withDefault "?" (Array.get 0 xs))
    "#};
    let (code, out, _, _) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{out}");
    assert_eq!(out.trim(), "apple", "the lexicographically least string sorts first");
}

// ===========================================================================
// In-place sorts: a sort over a uniquely-owned array mutates the buffer in place,
// so it duplicates the buffer a *constant* (zero) number of times regardless of
// length. A `set`/`swap` driven through recursive, tuple-threaded code used to
// lose uniqueness and copy the whole buffer once per recursion frame (O(n) copies
// of an O(n) buffer = O(n^2) work); these guard that it does not. The counter
// measures buffer *copies*, not allocations: each copy is a single allocation but
// O(length) work, so a per-element regression is invisible to the allocation count
// yet shows here as copies that scale with the length.
// ===========================================================================

#[test]
fn array_sort_does_constant_buffer_copies() {
    // `Array.sort` is an in-place median-of-three quicksort. Over a freshly built
    // (unique) input every swap updates the buffer in place, so the buffer-copy
    // count is zero and independent of the length — a uniqueness loss in the
    // recursive partition would copy once per frame, scaling with N.
    let small = copies("Array.sum (Array.sort (Array.reverse (Array.range 0 50)))", "1225");
    let big = copies("Array.sum (Array.sort (Array.reverse (Array.range 0 500)))", "124750");
    assert_eq!(small, 0, "a unique Array.sort copies no buffer (small)");
    assert_eq!(big, 0, "a unique Array.sort copies no buffer (big)");
    assert_eq!(small, big, "Array.sort buffer copies do not scale with length");
}

/// A hand-written in-place quicksort over an `Array Int` (the
/// `samples/algorithms/QuickSort.fai` shape): a tuple-returning `partition`, a
/// doubly-recursive `qsort` that destructures that tuple, and `unsafeGet`/
/// `unsafeSet` swaps — the exact recursive, tuple-threaded mutation that lost
/// uniqueness. `run n` returns the position-weighted checksum of the sorted input.
fn quicksort_prog(n: i64) -> String {
    formatdoc! {r#"
        module M

        swap : Int -> Int -> Array Int -> Array Int
        let swap i j a =
          let vi = Array.unsafeGet i a
          let vj = Array.unsafeGet j a
          Array.unsafeSet j vi (Array.unsafeSet i vj a)

        partition : Int -> Int -> Int -> Array Int -> (Int * Array Int)
        let partition hi j store a =
          if j >= hi - 1 then
            (store, swap store (hi - 1) a)
          else if Array.unsafeGet j a < Array.unsafeGet (hi - 1) a then
            partition hi (j + 1) (store + 1) (swap store j a)
          else
            partition hi (j + 1) store a

        qsort : Int -> Int -> Array Int -> Array Int
        let qsort lo hi a =
          if hi - lo <= 1 then
            a
          else
            match partition hi lo lo a with
            | (p, a2) ->
              if p - lo < hi - p - 1 then
                qsort (p + 1) hi (qsort lo p a2)
              else
                qsort lo p (qsort (p + 1) hi a2)

        gen : Int -> Array Int
        let gen n = Array.init n (fun k -> (k * 2654435761 + 12345) % n)

        checksum : Int -> Int -> Int -> Array Int -> Int
        let checksum i acc n a =
          if i >= n then acc else checksum (i + 1) (acc + i * Array.unsafeGet i a) n a

        run : Int -> Int
        let run n =
          let sorted = qsort 0 n (gen n)
          checksum 0 0 n sorted

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (run {n}))
    "#}
}

/// The oracle for [`quicksort_prog`]: sort `gen n` and take the position-weighted
/// checksum, computed independently in Rust.
fn quicksort_checksum(n: i64) -> i64 {
    let mut v: Vec<i64> = (0..n).map(|k| (k * 2654435761 + 12345) % n).collect();
    v.sort_unstable();
    v.iter().enumerate().map(|(i, &x)| i as i64 * x).sum()
}

#[test]
fn hand_written_quicksort_is_in_place() {
    // The hand-written quicksort threads the array through a tuple-returning
    // partition and a non-tail double recursion — exactly the shape that lost
    // uniqueness. Over a uniquely-owned array it does zero buffer copies,
    // independent of N; the checksum vs the Rust oracle confirms it really sorts.
    let small = copies_full(&quicksort_prog(50), &quicksort_checksum(50).to_string());
    let big = copies_full(&quicksort_prog(500), &quicksort_checksum(500).to_string());
    assert_eq!(small, 0, "a unique quicksort copies no buffer (small)");
    assert_eq!(big, 0, "a unique quicksort copies no buffer (big)");
    assert_eq!(small, big, "quicksort buffer copies do not scale with length");
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
        let (code, out, _, _) = run_counted(&src);
        prop_assert_eq!(code, 0, "leak-free exit for:\n{}\n{}", render_arr(&e), out);
        prop_assert_eq!(out.trim(), expected.to_string(), "oracle mismatch for {}", render_arr(&e));
    }
}

// ===========================================================================
// Property: over random integer arrays, an in-place sort (the standard
// `Array.sort` and a hand-written quicksort) produces the exactly-sorted sequence,
// runs leak-free, and copies the buffer zero times — the in-place guarantee that
// the recursive, tuple-threaded mutation must preserve, over arbitrary inputs
// (varied length and contents, with duplicates exercising equal-key partitions).
// ===========================================================================

/// Renders a `Vec<i64>` as a Fai `Array Int` literal (`[||]` when empty — its
/// element type is fixed to `Int` by the `Int.toString` use downstream).
fn render_int_array(xs: &[i64]) -> String {
    if xs.is_empty() {
        return "[||]".to_owned();
    }
    let elems: Vec<String> =
        xs.iter().map(|x| if *x < 0 { format!("(0 - {})", -x) } else { x.to_string() }).collect();
    format!("[| {} |]", elems.join(", "))
}

/// The oracle: `xs` sorted ascending and rendered as the comma-terminated string
/// the Fai folds below produce (`"a,b,c,"`; `""` for the empty array).
fn sorted_csv(xs: &[i64]) -> String {
    let mut v = xs.to_vec();
    v.sort_unstable();
    let mut s = String::new();
    for x in &v {
        s.push_str(&x.to_string());
        s.push(',');
    }
    s
}

/// A program that sorts the literal `lit` with the hand-written in-place quicksort
/// (tuple-returning partition + doubly-recursive qsort, the #115 shape) and prints
/// the sorted elements as a comma-terminated string.
fn quicksort_sort_prog(lit: &str) -> String {
    formatdoc! {r#"
        module M

        swap : Int -> Int -> Array Int -> Array Int
        let swap i j a =
          let vi = Array.unsafeGet i a
          let vj = Array.unsafeGet j a
          Array.unsafeSet j vi (Array.unsafeSet i vj a)

        partition : Int -> Int -> Int -> Array Int -> (Int * Array Int)
        let partition hi j store a =
          if j >= hi - 1 then
            (store, swap store (hi - 1) a)
          else if Array.unsafeGet j a < Array.unsafeGet (hi - 1) a then
            partition hi (j + 1) (store + 1) (swap store j a)
          else
            partition hi (j + 1) store a

        qsort : Int -> Int -> Array Int -> Array Int
        let qsort lo hi a =
          if hi - lo <= 1 then
            a
          else
            match partition hi lo lo a with
            | (p, a2) ->
              if p - lo < hi - p - 1 then
                qsort (p + 1) hi (qsort lo p a2)
              else
                qsort lo p (qsort (p + 1) hi a2)

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let arr = {lit}
          let sorted = qsort 0 (Array.length arr) arr
          rt.console.writeLine (Array.foldl (fun acc x -> acc ++ Int.toString x ++ ",") "" sorted)
    "#}
}

proptest! {
    // Each case is a full JIT compile + run under the global counter lock, so keep
    // the case count modest.
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// `Array.sort` over a freshly built (unique) random array sorts correctly and
    /// in place: the result matches the Rust oracle exactly, the run is leak-free,
    /// and the buffer is copied zero times.
    #[test]
    fn random_array_sort_is_correct_and_in_place(xs in prop::collection::vec(-20i64..20, 0..50)) {
        let lit = render_int_array(&xs);
        let expected = sorted_csv(&xs);
        let src = formatdoc! {r#"
            module M

            public main : Runtime -> Unit / {{ Console }}
            let main rt =
              rt.console.writeLine (Array.foldl (fun acc x -> acc ++ Int.toString x ++ ",") "" (Array.sort {lit}))
        "#};
        let (code, out, _, copies) = run_counted(&src);
        prop_assert_eq!(code, 0, "leak-free exit for {:?}:\n{}", xs, out);
        prop_assert_eq!(out.trim(), expected.as_str(), "sort oracle mismatch for {:?}", xs);
        prop_assert_eq!(copies, 0, "Array.sort must be in place (zero buffer copies) for {:?}", xs);
    }

    /// The hand-written quicksort over a random array sorts correctly and in place
    /// — the exact recursive, tuple-threaded mutation #115 was about — over
    /// arbitrary inputs, with zero buffer copies.
    #[test]
    fn random_quicksort_is_correct_and_in_place(xs in prop::collection::vec(-20i64..20, 0..50)) {
        let expected = sorted_csv(&xs);
        let (code, out, _, copies) = run_counted(&quicksort_sort_prog(&render_int_array(&xs)));
        prop_assert_eq!(code, 0, "leak-free exit for {:?}:\n{}", xs, out);
        prop_assert_eq!(out.trim(), expected.as_str(), "quicksort oracle mismatch for {:?}", xs);
        prop_assert_eq!(copies, 0, "quicksort must be in place (zero buffer copies) for {:?}", xs);
    }
}

/// Runs `src` with the bounds-check-elimination **shadow check** enabled: an
/// elided check is retained but routed to the distinct unsound-elision abort, so
/// an over-elision aborts the run (and, in-process, the test) loudly rather than
/// silently reading out of bounds. Returns `(exit_code, stdout)`. A `Guard` resets
/// the toggle even on panic.
fn run_shadow(src: &str) -> (i32, String) {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            fai_driver::set_bce_shadow(false);
        }
    }
    let _g = lock();
    fai_driver::set_bce_shadow(true);
    let _reset = Guard;
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    let outcome = jit_run_program(&db, file);
    let out = rt::capture_take();
    (outcome.exit_code, out)
}

// ===========================================================================
// Soundness: with the shadow check on, every elided bounds check is re-verified
// at run time. A correct program (in-bounds by construction) must still exit
// cleanly — an over-elision would re-fail the retained check and abort. This is
// the generative soundness net for the elimination over the same random array
// pipelines, the standard sort, and the hand-written quicksort.
// ===========================================================================
proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn shadow_check_passes_on_random_array_pipelines(e in arr_expr()) {
        let expected: i64 = eval_arr(&e).into_iter().sum();
        let src = formatdoc! {r#"
            module M

            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (Array.sum {body}))
        "#, body = render_arr(&e)};
        let (code, out) = run_shadow(&src);
        prop_assert_eq!(code, 0, "shadow-check soundness for:\n{}\n{}", render_arr(&e), out);
        prop_assert_eq!(out.trim(), expected.to_string(), "oracle mismatch for {}", render_arr(&e));
    }

    #[test]
    fn shadow_check_passes_on_random_quicksort(xs in prop::collection::vec(-20i64..20, 0..50)) {
        let expected = sorted_csv(&xs);
        let (code, out) = run_shadow(&quicksort_sort_prog(&render_int_array(&xs)));
        prop_assert_eq!(code, 0, "shadow-check soundness for quicksort {:?}:\n{}", xs, out);
        prop_assert_eq!(out.trim(), expected.as_str(), "quicksort oracle mismatch for {:?}", xs);
    }
}
