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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

            public main : Runtime -> Unit
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

            public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (use (fillD {n} Dict.empty)))
    "#}
}

#[test]
fn dict_map_recycles_a_unique_tree() {
    // `Dict.map` preserves the tree shape, so each matched node is rebuilt as a
    // same-size node: a uniquely-owned map recycles all `n` cells (zero fresh),
    // while a shared map must copy them. The gap is exactly the `n` recycled
    // nodes — the guard that weight-balanced nodes are reused in place. (`Set`
    // shares the identical node/insert/balance machinery.)
    let u = allocs(&dict_prog("Dict.size (Dict.map (fun k v -> v + 1) d)", 50), "50");
    let s =
        allocs(&dict_prog("Dict.size (Dict.map (fun k v -> v + 1) d) + Dict.size d", 50), "100");
    assert_eq!(s - u, 50, "a unique Dict.map must recycle all 50 nodes (shared copies them)");
}
