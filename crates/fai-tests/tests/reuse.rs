//! End-to-end reuse, in-place update, and borrowing tests through the *public*
//! JIT pipeline ([`fai_driver::jit_run_program`]).
//!
//! These complement the backend's in-crate reuse tests by exercising the whole
//! driver path — reachability, precompile diagnostics, reference counting,
//! Cranelift codegen, and the runtime — and comparing the cumulative heap
//! allocation count between a *unique* and a *shared* version of the same
//! computation. Rebuilding a unique structure recycles its cells in place (no
//! fresh allocations); a shared one must copy. A clean exit code (0) also means
//! the runtime's end-of-run leak check passed, so each program is leak-free.
//!
//! The runtime's allocation counter, console sink, and live-object counter are
//! process-global, so every case serializes on [`LOCK`]. The allocation and
//! live-object counters are compiled in only under `debug_assertions`, so the
//! allocation-delta assertions and the leak-free exit code are meaningful only in
//! a debug build (the default for `cargo test`).

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

/// Compiles and JIT-runs `src` through the driver, returning
/// `(exit_code, output, allocations)`. An exit code of 0 implies a leak-free run
/// (the runtime aborts with 70 if any object is still live at exit).
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

/// A program with `build`/`sum`/`len` list helpers plus the injected `defs`,
/// whose `main` prints `Int.toString (use (build n))`.
fn prog(defs: &str, use_body: &str, n: i32) -> String {
    formatdoc! {r#"
        module M

        let build k = if k <= 0 then [] else k :: build (k - 1)

        let sum xs =
          match xs with
          | [] -> 0
          | x :: rest -> x + sum rest

        let len xs =
          match xs with
          | [] -> 0
          | _ :: rest -> 1 + len rest

        {defs}

        let use xs = {use_body}

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (use (build {n})))
    "#}
}

/// Runs `src`, asserting a clean (leak-free) exit and `expect` output; returns
/// the cumulative allocation count.
#[track_caller]
fn allocs(src: &str, expect: &str) -> i64 {
    let (code, out, a) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{src}");
    assert_eq!(out.trim(), expect, "output:\n{src}");
    a
}

/// Runs `src`, asserting a clean (leak-free) exit and `expect` output.
#[track_caller]
fn outputs(src: &str, expect: &str) {
    let (code, out, _) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{src}");
    assert_eq!(out.trim(), expect, "output:\n{src}");
}

const INC: &str =
    "let inc xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> (x + 1) :: inc rest";
const DBL: &str =
    "let dbl xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> (x * 2) :: dbl rest";
const KEEP: &str = "let keep xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> if x > 0 then x :: keep rest else keep rest";
const REV: &str = "let rev acc xs =\n  match xs with\n  | [] -> acc\n  | x :: rest -> rev (x :: acc) rest\n\nlet reverse xs = rev [] xs";
const STUTTER: &str =
    "let stutter xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> x :: x :: stutter rest";

// ===========================================================================
// Differential reuse: a unique spine is recycled, a shared one copied (+50).
// ===========================================================================

#[test]
fn reuse_inc_unique_vs_shared() {
    let u = allocs(&prog(INC, "sum (inc xs)", 50), "1325");
    let s = allocs(&prog(INC, "sum (inc xs) + sum xs", 50), "2600");
    assert_eq!(s - u, 50, "unique inc recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_double_unique_vs_shared() {
    let u = allocs(&prog(DBL, "sum (dbl xs)", 50), "2550");
    let s = allocs(&prog(DBL, "sum (dbl xs) + sum xs", 50), "3825");
    assert_eq!(s - u, 50, "unique dbl recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_filter_keep_all_unique_vs_shared() {
    let u = allocs(&prog(KEEP, "sum (keep xs)", 50), "1275");
    let s = allocs(&prog(KEEP, "sum (keep xs) + sum xs", 50), "2550");
    assert_eq!(s - u, 50, "unique filter recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_reverse_unique_vs_shared() {
    let u = allocs(&prog(REV, "sum (reverse xs)", 50), "1275");
    let s = allocs(&prog(REV, "sum (reverse xs) + sum xs", 50), "2550");
    assert_eq!(s - u, 50, "unique reverse recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_std_map_unique_vs_shared() {
    let u = allocs(&prog("", "sum (List.map (fun x -> x + 1) xs)", 50), "1325");
    let s = allocs(&prog("", "sum (List.map (fun x -> x + 1) xs) + sum xs", 50), "2600");
    assert_eq!(s - u, 50, "List.map recycles a unique spine (u={u}, s={s})");
}

/// A rebuild whose cons goes through a user-written smart constructor `cons`. The
/// helper inliner folds `cons` back into the caller, so the matched cell is still
/// recycled in place — a unique spine reuses (zero fresh), a shared one copies
/// (+50), exactly as the hand-written `(x + 1) :: mapInc rest` would.
const CONS_MAP: &str = "let cons h t = h :: t\n\nlet mapInc xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> cons (x + 1) (mapInc rest)";

#[test]
fn reuse_through_a_user_smart_constructor() {
    let u = allocs(&prog(CONS_MAP, "sum (mapInc xs)", 50), "1325");
    let s = allocs(&prog(CONS_MAP, "sum (mapInc xs) + sum xs", 50), "2600");
    assert_eq!(
        s - u,
        50,
        "a unique rebuild through a smart constructor recycles 50 cells (u={u}, s={s})"
    );
}

#[test]
fn reuse_nested_stutter_unique_vs_shared() {
    // A two-deep rebuild (`x :: x :: stutter rest`) produces two cons cells per
    // element. Over a unique list each iteration recycles the matched cell into one
    // of them and allocates the other fresh; a shared list cannot recycle and
    // allocates both. So the gap is one cell per element — the recycled spine.
    let u = allocs(&prog(STUTTER, "sum (stutter xs)", 50), "2550");
    let s = allocs(&prog(STUTTER, "sum (stutter xs) + sum xs", 50), "3825");
    assert_eq!(s - u, 50, "unique nested rebuild recycles 50 cons cells (u={u}, s={s})");
}

/// A program over a list of records whose `main` prints `Int.toString {use_body}`
/// after binding `xs = build 50` (a descending list of `{ n = k, tag = 0 }`, all
/// with `n > 0`). `keepPos` is the **row-polymorphic** modulo-cons filter under
/// test (`r :: keepPos rest`, testing the `n` field through offset evidence); with
/// every element kept it rebuilds the same list, so it flattens to a spine-building
/// loop that recycles a unique input's cons cells in place. `sumR` (monomorphic,
/// over the concrete record type) reads the field back and re-uses `xs` in the
/// shared variant; keeping the re-use monomorphic means both variants build the
/// same single partial-application closure for the one `keepPos` call, so the
/// allocation gap is exactly the recycled-vs-copied spine.
fn rowpoly_prog(use_body: &str) -> String {
    formatdoc! {r#"
        module M

        type R = {{ n : Int, tag : Int }}

        build : Int -> List R
        let build k = if k <= 0 then [] else {{ n = k, tag = 0 }} :: build (k - 1)

        keepPos : List ({{ n : Int | 'r }}) -> List ({{ n : Int | 'r }})
        let keepPos rs =
          match rs with
          | [] -> []
          | r :: rest -> if r.n > 0 then r :: keepPos rest else keepPos rest

        sumR : List R -> Int
        let sumR rs =
          match rs with
          | [] -> 0
          | r :: rest -> r.n + sumR rest

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let xs = build 50
          rt.console.writeLine (Int.toString ({use_body}))
    "#}
}

#[test]
fn reuse_rowpoly_filter_unique_vs_shared() {
    // A row-polymorphic modulo-cons rebuild recycles a unique cons spine in place
    // (zero fresh cells) and copies a shared one (+50) — the same differential as
    // the monomorphic rebuilders, confirming an evidence-carrying loop still reuses.
    let u = allocs(&rowpoly_prog("sumR (keepPos xs)"), "1275");
    let s = allocs(&rowpoly_prog("sumR (keepPos xs) + sumR xs"), "2550");
    assert_eq!(s - u, 50, "unique row-poly rebuild recycles 50 cons cells (u={u}, s={s})");
}

/// The recycled-vs-copied gap is exactly one cons cell per element, so it grows
/// linearly with the list length.
#[track_caller]
fn reuse_gap_equals_length(n: i32) {
    let n64 = i64::from(n);
    // unique output = sum (inc xs) = n(n+3)/2; shared output also adds sum xs.
    let u = allocs(&prog(INC, "sum (inc xs)", n), &(n64 * (n64 + 3) / 2).to_string());
    let s = allocs(&prog(INC, "sum (inc xs) + sum xs", n), &(n64 * (n64 + 2)).to_string());
    assert_eq!(s - u, n64, "n={n}: shared copies n cons cells (u={u}, s={s})");
}

#[test]
fn reuse_scales_n10() {
    reuse_gap_equals_length(10);
}

#[test]
fn reuse_scales_n100() {
    reuse_gap_equals_length(100);
}

#[test]
fn reuse_scales_n250() {
    reuse_gap_equals_length(250);
}

// ===========================================================================
// Record update: a unique record is overwritten in place; a shared one copied.
// ===========================================================================

#[test]
fn record_update_in_place_is_constant() {
    let prog = |k: i32| {
        formatdoc! {r#"
            module M

            type R = {{ a : Int, n : Int }}

            bumpN : Int -> R -> R
            let bumpN k rec =
              if k <= 0 then rec else bumpN (k - 1) {{ rec with n = rec.n + 1 }}

            getN : R -> Int
            let getN rec = rec.n

            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (getN (bumpN {k} {{ a = 0, n = 0 }})))
        "#}
    };
    let a = allocs(&prog(50), "50");
    let b = allocs(&prog(100), "100");
    assert_eq!(a, b, "in-place update is independent of the update count (a={a}, b={b})");
}

#[test]
fn record_update_shared_copies_once() {
    let prog = |body: &str| {
        formatdoc! {r#"
            module M

            type R = {{ a : Int, n : Int }}

            bump : R -> R
            let bump rec = {{ rec with n = rec.n + 1 }}

            getN : R -> Int
            let getN rec = rec.n

            use : R -> Int
            let use rec = {body}

            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (use {{ a = 0, n = 10 }}))
        "#}
    };
    let u = allocs(&prog("getN (bump rec)"), "11");
    let s = allocs(&prog("getN (bump rec) + getN rec"), "21");
    assert_eq!(s - u, 1, "the shared record update copies once (u={u}, s={s})");
}

// ===========================================================================
// Correctness through the public pipeline (output + leak-free exit).
// ===========================================================================

#[test]
fn correct_inc() {
    outputs(&prog(INC, "sum (inc xs)", 10), "65");
}

#[test]
fn correct_double() {
    outputs(&prog(DBL, "sum (dbl xs)", 10), "110");
}

#[test]
fn correct_reverse() {
    outputs(&prog(REV, "sum (reverse xs)", 10), "55");
}

#[test]
fn correct_borrowed_inspector_read_twice() {
    outputs(&prog("let count xs = len xs + len xs", "count xs", 50), "100");
}

#[test]
fn correct_borrow_alongside_rebuild() {
    outputs(&prog(INC, "sum (inc xs) + len xs", 50), "1375");
}

#[test]
fn correct_tree_rebuild() {
    let src = formatdoc! {r#"
        module M

        type Tree = | Leaf Int | Node Tree Tree

        let incT t =
          match t with
          | Leaf n -> Leaf (n + 1)
          | Node l r -> Node (incT l) (incT r)

        let sumT t =
          match t with
          | Leaf n -> n
          | Node l r -> sumT l + sumT r

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let t = Node (Node (Leaf 1) (Leaf 2)) (Leaf 3)
          rt.console.writeLine (Int.toString (sumT (incT t)))
    "#};
    outputs(&src, "9");
}

#[test]
fn correct_record_update() {
    let src = formatdoc! {r#"
        module M

        type P = {{ x : Int, y : Int }}

        let shift p = {{ p with x = p.x + 1 }}

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let p = shift {{ x = 41, y = 0 }}
          rt.console.writeLine (Int.toString p.x)
    "#};
    outputs(&src, "42");
}

#[test]
fn correct_row_polymorphic_update() {
    let src = formatdoc! {r#"
        module M

        bump : {{ n : Int | 'r }} -> {{ n : Int | 'r }}
        let bump rec = {{ rec with n = rec.n + 1 }}

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let r = bump {{ n = 41, tag = 1 }}
          rt.console.writeLine (Int.toString r.n)
    "#};
    outputs(&src, "42");
}

#[test]
fn correct_nested_rebuild_chain() {
    outputs(&prog(&format!("{INC}\n\n{DBL}"), "sum (dbl (inc xs))", 10), "130");
}

#[test]
fn correct_reorder_pure_call() {
    // `bump` has a non-last recursive field whose later field calls the pure,
    // non-recursive `twice`, which the reorder-safety analysis now admits, so it
    // flattens to a loop. Built and consumed over a deep snoc-list, it runs
    // leak-free with the expected output (2 * (1 + ... + 200) = 40200).
    let src = formatdoc! {r#"
        module M

        type Snoc = | Empty | Snoc Snoc Int

        let twice x = x + x

        build : Int -> Snoc
        let build n = if n <= 0 then Empty else Snoc (build (n - 1)) n

        bump : Snoc -> Snoc
        let bump xs =
          match xs with
          | Empty -> Empty
          | Snoc rest x -> Snoc (bump rest) (twice x)

        sumS : Int -> Snoc -> Int
        let sumS acc xs =
          match xs with
          | Empty -> acc
          | Snoc rest x -> sumS (acc + x) rest

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          rt.console.writeLine (Int.toString (sumS 0 (bump (build 200))))
    "#};
    outputs(&src, "40200");
}

// ===========================================================================
// Mutual recursion: a plain-tail-recursive group flattens to a combined loop, so
// it runs in constant stack over a deep input and stays correct.
// ===========================================================================

#[test]
fn correct_mutual_even_odd() {
    let src = formatdoc! {r#"
        module M

        isEven : Int -> Bool
        let isEven n = if n <= 0 then true else isOdd (n - 1)

        isOdd : Int -> Bool
        let isOdd n = if n <= 0 then false else isEven (n - 1)

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (if isEven 10 then "even" else "odd")
    "#};
    outputs(&src, "even");
}

#[test]
fn mutual_even_odd_runs_in_constant_stack() {
    // 200000 mutual bounces: a flattened loop runs fine; ordinary mutual recursion
    // would overflow the stack (a non-zero exit).
    let src = formatdoc! {r#"
        module M

        isEven : Int -> Bool
        let isEven n = if n <= 0 then true else isOdd (n - 1)

        isOdd : Int -> Bool
        let isOdd n = if n <= 0 then false else isEven (n - 1)

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (if isEven 200000 then "even" else "odd")
    "#};
    outputs(&src, "even");
}

#[test]
fn correct_mutual_three_cycle() {
    // A three-function cycle (mod-3 classifier) flattens too.
    let src = formatdoc! {r#"
        module M

        modA : Int -> Int
        let modA n = if n <= 0 then 0 else modB (n - 1)

        modB : Int -> Int
        let modB n = if n <= 0 then 1 else modC (n - 1)

        modC : Int -> Int
        let modC n = if n <= 0 then 2 else modA (n - 1)

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (modA 100000))
    "#};
    // modA computes n mod 3, and 100000 mod 3 = 1.
    outputs(&src, "1");
}

// ===========================================================================
// Weight-balanced Dict/Set reuse: a uniquely-owned tree is rewritten in place,
// a shared one is copied. `Dict.map` preserves the tree shape, so a unique map
// recycles every node (zero fresh) while a shared map copies all `n`.
// ===========================================================================

/// A program building an `n`-entry `Dict Int Int` (unique accumulator) and
/// running `use_body` on it; prints `Int.toString (use d)`.
fn dict_prog(use_body: &str, n: i32) -> String {
    formatdoc! {r#"
        module M

        fillD : Int -> Dict Int Int -> Dict Int Int
        let fillD k d = if k <= 0 then d else fillD (k - 1) (Dict.insert k k d)

        let use d = {use_body}

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (use (fillD {n} Dict.empty)))
    "#}
}

#[test]
fn dict_map_recycles_a_unique_tree() {
    // `Dict.map` preserves the tree shape and embeds its recursion in the
    // constructor, so the reuse pass resets each matched node before recursing: a
    // uniquely-owned map recycles all `n` cells (zero fresh) while a shared map
    // copies them. The gap is exactly the `n` recycled nodes — the guard that
    // weight-balanced nodes are reused in place.
    let u = allocs(&dict_prog("Dict.size (Dict.map (fun k v -> v + 1) d)", 50), "50");
    let s =
        allocs(&dict_prog("Dict.size (Dict.map (fun k v -> v + 1) d) + Dict.size d", 50), "100");
    assert_eq!(s - u, 50, "a unique Dict.map must recycle all 50 nodes (shared copies them)");
}

/// A program building an `n`-element `Set Int` (unique accumulator) and printing
/// its size.
fn set_prog(use_body: &str, n: i32) -> String {
    formatdoc! {r#"
        module M

        fillS : Int -> Set Int -> Set Int
        let fillS k s = if k <= 0 then s else fillS (k - 1) (Set.insert k s)

        let use s = {use_body}

        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (use (fillS {n} Set.empty)))
    "#}
}

/// The build cost (cumulative allocations net of the fixed program overhead) of
/// constructing the `n`-entry collection `prog(n)` prints, whose output is `n`.
#[track_caller]
fn build_cost(prog: impl Fn(i32) -> String, n: i32) -> i64 {
    let base = allocs(&prog(0), "0");
    let at_n = allocs(&prog(n), &n.to_string());
    at_n - base
}

/// A uniquely-owned `insert` build resets each matched node *before* the recursive
/// call, so the search path is rebuilt in place: per-element allocation is flat and
/// the whole build is O(n). Were the reset left after the recursion (the child
/// shared, so the recursion path-copies), the build would be O(n log n) — its
/// per-element cost rising with the tree depth. The guard: doubling `n` at most
/// doubles the build (ratio ≤ 2.1), which the O(n log n) build (ratio ≈ 2.25 at
/// these sizes) fails. Allocation counts are deterministic, so this is not flaky.
#[track_caller]
fn build_is_linear(prog: impl Fn(i32) -> String, label: &str) {
    let n = 512;
    let c1 = build_cost(&prog, n);
    let c2 = build_cost(&prog, 2 * n);
    assert!(
        c2 * 10 <= c1 * 21,
        "{label} build is not O(n): cost({n})={c1}, cost({})={c2} (ratio {:.3}); \
         a path-copying O(n log n) build would be ~2.25x",
        2 * n,
        c2 as f64 / c1 as f64,
    );
}

#[test]
fn dict_build_allocates_linearly() {
    build_is_linear(|n| dict_prog("Dict.size d", n), "Dict.insert");
}

#[test]
fn set_build_allocates_linearly() {
    build_is_linear(|n| set_prog("Set.size s", n), "Set.insert");
}

#[test]
fn unique_dict_insert_build_is_correct_and_leak_free() {
    // The in-place insert path is exercised on a uniquely-owned accumulator built
    // by `fillD`, then observed through `toList`: a clean exit confirms leak-free
    // reference counting (every reset cell is reused or freed exactly once), and
    // the output confirms the rebuilt tree holds the right ordered entries.
    let src = formatdoc! {r#"
        module M

        fillD : Int -> Dict Int Int -> Dict Int Int
        let fillD k d = if k <= 0 then d else fillD (k - 1) (Dict.insert k (k * 10) d)

        sumKeys : List (Int * Int) -> Int
        let sumKeys pairs =
          match pairs with
          | [] -> 0
          | (k, v) :: rest -> k + v + sumKeys rest

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let d = fillD 100 Dict.empty
          rt.console.writeLine (Int.toString (sumKeys (Dict.toList d)))
    "#};
    // keys 1..100 sum to 5050; values are key*10, so total = 5050 + 50500 = 55550.
    outputs(&src, "55550");
}

// ===========================================================================
// Scalar float fields: a record of `Float`s allocates only its cell — the
// `Float` fields are raw unboxed slots, not separate boxed cells. A
// structurally identical record of immediate `Int`s is the baseline; the two
// allocate the same number of objects (before scalarization the float version
// allocated two extra boxes per record).
// ===========================================================================

#[test]
fn float_record_fields_allocate_no_boxes() {
    // Build n two-field records in a list and sum the first field. The float and
    // int programs share their list/record structure and final `Int.toString`,
    // so any allocation difference is exactly the float fields' boxes.
    // Both fields are used (so the record type is monomorphic `Float`, not
    // generalized over an unused field) — exactly the scalarized layout under test.
    let floats = formatdoc! {r#"
        module M
        let mk k = {{ x = Int.toFloat k, y = Int.toFloat k }}
        let build k = if k <= 0 then [] else mk k :: build (k - 1)
        let total xs =
          match xs with
          | [] -> 0.0
          | v :: rest ->
            let {{ x, y }} = v
            x + y + total rest
        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (Float.toInt (total (build 10))))
    "#};
    let ints = formatdoc! {r#"
        module M
        let mk k = {{ x = k, y = k }}
        let build k = if k <= 0 then [] else mk k :: build (k - 1)
        let total xs =
          match xs with
          | [] -> 0
          | v :: rest ->
            let {{ x, y }} = v
            x + y + total rest
        public main : Runtime -> Unit / {{ Console }}
        let main rt = rt.console.writeLine (Int.toString (total (build 10)))
    "#};
    let af = allocs(&floats, "110");
    let ai = allocs(&ints, "110");
    assert_eq!(af, ai, "float-field records allocate no field boxes (float={af}, int={ai})");
}

// ===========================================================================
// String building: `++` onto a uniquely-owned accumulator appends in place
// (amortized O(total length)), so a unique builder forks zero times and allocates
// only its O(log n) doublings — the structural acceptance signal for the in-place
// append. `string_copies()` counts uniqueness-loss forks (a shared accumulator
// copied); it stays zero while the accumulator is unique.
// ===========================================================================

/// Compiles and JIT-runs `src`, returning `(exit_code, output, string_copies)` —
/// the number of uniqueness-loss string forks the run performed.
fn run_string_copies(src: &str) -> (i32, String, i64) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, file);
    let copies = rt::string_copies();
    let out = rt::capture_take();
    (outcome.exit_code, out, copies)
}

/// A program that builds a string by appending the literal `"ab"` onto a fresh
/// `Int.toString 0` accumulator `n` times, printing the result's length.
fn string_build_prog(n: i32) -> String {
    formatdoc! {r#"
        module M

        let build n acc = if n <= 0 then acc else build (n - 1) (acc ++ "ab")

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          rt.console.writeLine (Int.toString (String.length (build {n} (Int.toString 0))))
    "#}
}

#[test]
fn unique_string_builder_never_forks() {
    // A recursive append onto a uniquely-owned accumulator extends it in place; the
    // build never loses uniqueness, so it forks (copies) zero times.
    let (code, out, copies) = run_string_copies(&string_build_prog(1000));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "2001", "\"0\" plus 1000 copies of \"ab\" is 2001 bytes");
    assert_eq!(copies, 0, "a unique builder appends in place, never forking");
}

#[test]
fn unique_string_builder_allocates_sublinearly() {
    // The in-place builder allocates only its O(log n) capacity doublings (the
    // literal `"ab"` is an immortal static, allocating nothing per step), so
    // doubling n adds only a small constant number of allocations. A per-step-copy
    // regression (the O(n²) build this replaces) would make the build's allocation
    // count scale with n, so the delta would scale with n too.
    let cost = |n: i32| -> i64 { allocs(&string_build_prog(n), &(1 + 2 * n).to_string()) };
    let c = cost(2000);
    let c2 = cost(4000);
    assert!(
        c2 - c <= 4,
        "doubling n adds about one grow, not ~n copies (cost(2000)={c}, cost(4000)={c2})"
    );
}

#[test]
fn foldl_concat_builder_never_forks() {
    // `List.foldl (++) "" parts` passes `(++)` first-class (through its wrapper
    // closure, not the intrinsic-inlined form). The accumulator stays uniquely
    // owned across iterations — the empty seed returns the first (fresh) element,
    // and every later append extends that unique buffer in place — so the build
    // never forks.
    let src = formatdoc! {r#"
        module M

        let parts k = if k <= 0 then [] else Int.toString k :: parts (k - 1)

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          rt.console.writeLine (Int.toString (String.length (List.foldl (++) "" (parts 500))))
    "#};
    let (code, out, copies) = run_string_copies(&src);
    assert_eq!(code, 0, "clean (leak-free) exit");
    // The digit lengths of 1..=500 sum to 9*1 + 90*2 + 401*3 = 1392.
    assert_eq!(out.trim(), "1392");
    assert_eq!(copies, 0, "a first-class foldl (++) build keeps the accumulator unique");
}

#[test]
fn concat_chain_reassociation_preserves_effect_order() {
    // A `++` chain of effectful operands evaluates them left to right; left-
    // reassociation must keep that order. `logged` prints its argument and returns
    // it, so the operands' prints appear in source order before the joined result.
    let src = formatdoc! {r#"
        module M

        logged : Runtime -> String -> String / {{ Console }}
        let logged rt s =
          let u = rt.console.writeLine s
          s

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let combined = logged rt "a" ++ logged rt "b" ++ logged rt "c"
          rt.console.writeLine combined
    "#};
    let (code, out, _) = run_counted(&src);
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "a\nb\nc\nabc", "operands evaluate left to right, then the join");
}

// ===========================================================================
// Borrowing slice views: a large substring/take/drop shares the base buffer (a
// view), a small one is copied. `string_views()` counts the views; a viewed
// result still exits leak-free (the shared base is released when the views die).
// ===========================================================================

/// Compiles and JIT-runs `src`, returning `(exit_code, output, string_views)`.
fn run_string_views(src: &str) -> (i32, String, i64) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, file);
    let views = rt::string_views();
    let out = rt::capture_take();
    (outcome.exit_code, out, views)
}

/// A program that builds an `n`-character base and prints the length of a slice of
/// it produced by `slice_expr` (in terms of `base`).
fn slice_prog(slice_expr: &str, n: i32) -> String {
    formatdoc! {r#"
        module M

        let mk k acc = if k <= 0 then acc else mk (k - 1) (acc ++ "a")

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let base = mk {n} ""
          rt.console.writeLine (Int.toString (String.length ({slice_expr})))
    "#}
}

#[test]
fn large_take_is_a_view_and_leak_free() {
    let (code, out, views) = run_string_views(&slice_prog("String.take 150 base", 200));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "150");
    assert_eq!(views, 1, "a large prefix is a borrowing view");
}

#[test]
fn small_take_is_a_copy() {
    let (code, out, views) = run_string_views(&slice_prog("String.take 8 base", 200));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "8");
    assert_eq!(views, 0, "a small prefix is copied, not viewed");
}

#[test]
fn drop_keeps_large_suffix_as_a_view() {
    let (code, out, views) = run_string_views(&slice_prog("String.drop 10 base", 200));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "190");
    assert_eq!(views, 1);
}

#[test]
fn a_slice_view_is_a_string_like_any_other() {
    // A view feeds back into the string API (equality and re-slicing) and the run
    // exits leak-free: the borrowing representation is transparent to user code.
    let src = formatdoc! {r#"
        module M

        let mk k acc = if k <= 0 then acc else mk (k - 1) (acc ++ "x")

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let base = mk 100 ""
          let view = String.drop 10 base
          let again = String.take 20 view
          let u = rt.console.writeLine (if view = String.drop 10 base then "eq" else "ne")
          rt.console.writeLine (Int.toString (String.length again))
    "#};
    let (code, out, views) = run_string_views(&src);
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "eq\n20");
    assert!(views >= 1, "the drop produced a view");
}

#[test]
fn split_into_few_large_pieces_views_them() {
    // Split a base into two large halves; both pieces are views sharing the base.
    let src = formatdoc! {r#"
        module M

        let mk k acc = if k <= 0 then acc else mk (k - 1) (acc ++ "a")

        public main : Runtime -> Unit / {{ Console }}
        let main rt =
          let half = mk 80 ""
          let text = half ++ "|" ++ half
          let parts = String.split "|" text
          rt.console.writeLine (Int.toString (List.sum (List.map String.length parts)))
    "#};
    let (code, out, views) = run_string_views(&src);
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), "160", "two 80-character pieces");
    assert_eq!(views, 2, "both large split pieces are views");
}

// ===========================================================================
// Property: destructure-and-recurse over user ADTs stays correct and leak-free.
//
// A recursive function that matches a value, projects a field, and recurses while
// the matched cell dies is reference-counted by emitting the cell's drop before
// the continuation. Generated binary trees exercise both shapes over random
// structure: a tree->Int *fold* (no reconstruction — the matched node is dropped,
// the path the array sort hit) and a tree->tree *map* (reconstruction — the node
// is reset and recycled). The tree's Node/Leaf cells are boxed children, so the
// leak-free exit (code 0) confirms every cell is released exactly once on both
// paths; the result is checked against an independent Rust oracle.
// ===========================================================================

/// A random binary tree with `Int` leaves.
#[derive(Debug, Clone)]
enum Tree {
    Leaf(i64),
    Node(Box<Tree>, Box<Tree>),
}

fn tree_strategy() -> impl Strategy<Value = Tree> {
    let leaf = (-20i64..20).prop_map(Tree::Leaf);
    leaf.prop_recursive(5, 24, 2, |inner| {
        (inner.clone(), inner).prop_map(|(l, r)| Tree::Node(Box::new(l), Box::new(r)))
    })
}

/// Renders an `Int` literal, parenthesizing negatives so `Leaf (0 - 5)` parses as
/// the constructor applied to a negative rather than a subtraction.
fn render_int_lit(n: i64) -> String {
    if n < 0 { format!("(0 - {})", -n) } else { n.to_string() }
}

/// Renders a tree as the Fai expression building it (`Node`/`Leaf` constructors).
fn render_tree(t: &Tree) -> String {
    match t {
        Tree::Leaf(n) => format!("(Leaf {})", render_int_lit(*n)),
        Tree::Node(l, r) => format!("(Node {} {})", render_tree(l), render_tree(r)),
    }
}

/// The fold oracle: `Leaf n -> n + k`; `Node l r -> (fold l) op (fold r)`, wrapping
/// (Fai `Int` is a wrapping `i64`).
fn fold_oracle(t: &Tree, op: char, k: i64) -> i64 {
    match t {
        Tree::Leaf(n) => n.wrapping_add(k),
        Tree::Node(l, r) => {
            let (a, b) = (fold_oracle(l, op, k), fold_oracle(r, op, k));
            match op {
                '+' => a.wrapping_add(b),
                '-' => a.wrapping_sub(b),
                _ => a.wrapping_mul(b),
            }
        }
    }
}

/// The map-then-sum oracle: map `Leaf n -> Leaf (n + k)`, then sum the leaves
/// (wrapping).
fn map_sum_oracle(t: &Tree, k: i64) -> i64 {
    match t {
        Tree::Leaf(n) => n.wrapping_add(k),
        Tree::Node(l, r) => map_sum_oracle(l, k).wrapping_add(map_sum_oracle(r, k)),
    }
}

proptest! {
    // Each case is a full JIT compile + run under the global counter lock, so keep
    // the case count modest.
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// A generated tree->Int fold (the matched node is dropped, not rebuilt) over a
    /// random tree matches the oracle and exits leak-free — the node drop emitted
    /// before the recursive continuation must release every cell exactly once.
    #[test]
    fn random_tree_fold_is_correct_and_leak_free(
        t in tree_strategy(),
        op in prop_oneof![Just('+'), Just('-'), Just('*')],
        k in 0i64..10,
    ) {
        let expected = fold_oracle(&t, op, k);
        let src = formatdoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            fold : Tree -> Int
            let fold t =
              match t with
              | Leaf n -> n + {k}
              | Node l r -> fold l {op} fold r

            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (fold {tree}))
        "#, tree = render_tree(&t)};
        let (code, out, _) = run_counted(&src);
        prop_assert_eq!(code, 0, "leak-free exit for {:?}:\n{}", t, out);
        prop_assert_eq!(out.trim(), expected.to_string(), "fold oracle mismatch for {:?}", t);
    }

    /// A generated tree->tree map (each matched node is reset and recycled) followed
    /// by a sum fold over a random tree matches the oracle and exits leak-free.
    #[test]
    fn random_tree_map_then_fold_is_correct_and_leak_free(
        t in tree_strategy(),
        k in 0i64..10,
    ) {
        let expected = map_sum_oracle(&t, k);
        let src = formatdoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            mapTree : Tree -> Tree
            let mapTree t =
              match t with
              | Leaf n -> Leaf (n + {k})
              | Node l r -> Node (mapTree l) (mapTree r)

            sumTree : Tree -> Int
            let sumTree t =
              match t with
              | Leaf n -> n
              | Node l r -> sumTree l + sumTree r

            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (sumTree (mapTree {tree})))
        "#, tree = render_tree(&t)};
        let (code, out, _) = run_counted(&src);
        prop_assert_eq!(code, 0, "leak-free exit for {:?}:\n{}", t, out);
        prop_assert_eq!(out.trim(), expected.to_string(), "map/fold oracle mismatch for {:?}", t);
    }
}

/// A rotation-heavy unique build (descending inserts force frequent rebalancing)
/// allocates **one cell per entry** — every rebuilt search path, *including the
/// `balance` rotations*, is recycled in place. Without inter-procedural reuse-token
/// passing the rotation branch frees the matched node and `balance` allocates
/// fresh, so the build costs ~3 cells per entry; the guard at ≤ 1.5 fails that.
#[track_caller]
fn build_recycles_balance_paths(prog: impl Fn(i32) -> String, label: &str) {
    let n = 512;
    let cost = build_cost(&prog, n);
    assert!(
        cost <= i64::from(n) * 3 / 2,
        "{label}: a unique rebalancing build should recycle the balance paths in place \
         (cost({n})={cost}, per-entry {:.3}); without forwarding it is ~3x",
        cost as f64 / f64::from(n),
    );
}

#[test]
fn dict_insert_recycles_balance_rotations() {
    build_recycles_balance_paths(|n| dict_prog("Dict.size d", n), "Dict.insert");
}

#[test]
fn set_insert_recycles_balance_rotations() {
    build_recycles_balance_paths(|n| set_prog("Set.size s", n), "Set.insert");
}

#[test]
fn unique_remove_recycles_vs_shared_copies() {
    // Removing a key from a unique tree rebuilds (and rebalances) its path in place;
    // a shared tree must copy it. The positive differential confirms the rebalanced
    // path — reached through the `balance` call — is recycled, and both exit
    // leak-free with the right size.
    let prog = |body: &str| {
        formatdoc! {r#"
            module M
            fillD : Int -> Dict Int Int -> Dict Int Int
            let fillD k d = if k <= 0 then d else fillD (k - 1) (Dict.insert k k d)
            let use d = {body}
            public main : Runtime -> Unit / {{ Console }}
            let main rt = rt.console.writeLine (Int.toString (use (fillD 64 Dict.empty)))
        "#}
    };
    let u = allocs(&prog("Dict.size (Dict.remove 32 d)"), "63");
    let s = allocs(&prog("Dict.size (Dict.remove 32 d) + Dict.size d"), "127");
    assert!(
        s > u,
        "a unique remove recycles its rebalanced path, a shared one copies (u={u}, s={s})"
    );
}
