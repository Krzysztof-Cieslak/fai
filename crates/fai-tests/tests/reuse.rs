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
//! process-global, so every case serializes on [`LOCK`].

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
