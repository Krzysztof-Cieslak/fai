//! End-to-end tests for scalar replacement of fixed-shape float aggregates
//! (SROA + multi-value returns): a non-escaping `(Float, Float)` tuple or
//! all-`Float` record is held in registers and returned multi-value, allocating
//! no heap cell, while an escaping aggregate falls back to the boxed cell. Each
//! program is JIT-run through the whole driver; a clean (0) exit also means the
//! runtime's end-of-run leak check passed.
//!
//! The allocation tests compare a fixed-shape-float-aggregate program against a
//! structurally identical *scalar* baseline (the same `toString`/runtime work):
//! equal cumulative allocation counts prove the aggregates added no heap cell.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::jit_run_program;
use fai_runtime as rt;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// Runs `src`, asserting a clean (leak-free) exit and `expect` output; returns the
/// cumulative allocation count.
#[track_caller]
fn allocs(src: &str, expect: &str) -> i64 {
    let (code, out, a) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{src}");
    assert_eq!(out.trim(), expect, "output:\n{src}");
    a
}

/// Asserts the float-aggregate program and its scalar baseline produce `expect`
/// and allocate the same number of cells (so the aggregate is allocation-free).
#[track_caller]
fn same_allocs(ffa: &str, scalar: &str, expect: &str) {
    let a = allocs(ffa, expect);
    let b = allocs(scalar, expect);
    assert_eq!(a, b, "aggregate allocates no extra cell (ffa={a}, scalar baseline={b})");
}

const VEC2: &str = "module M\n\
    public type Vec2 = { x : Float, y : Float }\n\
    public add2 : Vec2 -> Vec2 -> Vec2\n\
    let add2 a b = { x = a.x + b.x, y = a.y + b.y }\n\
    public scale2 : Float -> Vec2 -> Vec2\n\
    let scale2 k v = { x = v.x * k, y = v.y * k }\n\
    public dot2 : Vec2 -> Vec2 -> Float\n\
    let dot2 a b = a.x * b.x + a.y * b.y\n";

/// A `Vec2` threaded through smart constructors and projected, never stored: it
/// runs correctly and allocates no more than the equivalent scalar arithmetic.
#[test]
fn vec2_pipeline_is_allocation_free() {
    let ffa = format!(
        "{VEC2}\
        public main : Runtime -> Unit / {{ Console }}\n\
        let main rt =\n  \
          let a = {{ x = 1.0, y = 2.0 }}\n  \
          let b = {{ x = 3.0, y = 4.0 }}\n  \
          let c = add2 a (scale2 2.0 b)\n  \
          rt.console.writeLine (Float.toString (dot2 c c))\n"
    );
    // c = (1+6, 2+8) = (7, 10); dot2 c c = 49 + 100 = 149.
    let scalar = "module M\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt =\n  \
          let cx = 1.0 + 2.0 * 3.0\n  \
          let cy = 2.0 + 2.0 * 4.0\n  \
          rt.console.writeLine (Float.toString (cx * cx + cy * cy))\n";
    same_allocs(&ffa, scalar, "149.0");
}

/// A `(Float, Float)`-returning helper consumed component-wise allocates no more
/// than the equivalent scalar computation.
#[test]
fn float_pair_returning_helper_is_allocation_free() {
    let ffa = "module M\n\
        public mk : Int -> (Float * Float)\n\
        let mk k = (Int.toFloat k, Int.toFloat k + 1.0)\n\
        public total : Int -> Float\n\
        let total k =\n  \
          if k <= 0 then 0.0\n  \
          else\n    \
            let (a, b) = mk k\n    \
            a + b + total (k - 1)\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt = rt.console.writeLine (Int.toString (Float.toInt (total 10)))\n";
    // sum over k=1..10 of (k + (k+1)) = sum(2k+1) = 110 + 10 = 120.
    let scalar = "module M\n\
        public total : Int -> Float\n\
        let total k = if k <= 0 then 0.0 else (Int.toFloat k) + (Int.toFloat k + 1.0) + total (k - 1)\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt = rt.console.writeLine (Int.toString (Float.toInt (total 10)))\n";
    same_allocs(ffa, scalar, "120");
}

/// A `Mat2` (four-`Float` record) matrix-vector product allocates no more than the
/// equivalent scalar arithmetic.
#[test]
fn mat2_apply_is_allocation_free() {
    let ffa = "module M\n\
        public type Mat2 = { a : Float, b : Float, c : Float, d : Float }\n\
        public type Vec2 = { x : Float, y : Float }\n\
        public apply : Mat2 -> Vec2 -> Vec2\n\
        let apply m v = { x = m.a * v.x + m.b * v.y, y = m.c * v.x + m.d * v.y }\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt =\n  \
          let m = { a = 1.0, b = 2.0, c = 3.0, d = 4.0 }\n  \
          let v = { x = 5.0, y = 6.0 }\n  \
          let r = apply m v\n  \
          rt.console.writeLine (Int.toString (Float.toInt (r.x + r.y)))\n";
    // r = (1*5+2*6, 3*5+4*6) = (17, 39); 17 + 39 = 56.
    let scalar = "module M\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt =\n  \
          let rx = 1.0 * 5.0 + 2.0 * 6.0\n  \
          let ry = 3.0 * 5.0 + 4.0 * 6.0\n  \
          rt.console.writeLine (Int.toString (Float.toInt (rx + ry)))\n";
    same_allocs(ffa, scalar, "56");
}

/// A `Vec2` stored in a list escapes, so it is boxed (the in-cell `f64`-slot
/// representation): the program is still correct and leak-free.
#[test]
fn escaping_aggregate_is_boxed_and_correct() {
    let src = "module M\n\
        public type Vec2 = { x : Float, y : Float }\n\
        public build : Int -> List Vec2\n\
        let build k = if k <= 0 then [] else { x = Int.toFloat k, y = Int.toFloat k } :: build (k - 1)\n\
        public total : List Vec2 -> Float\n\
        let total xs =\n  \
          match xs with\n  \
          | [] -> 0.0\n  \
          | v :: rest -> v.x + v.y + total rest\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt = rt.console.writeLine (Int.toString (Float.toInt (total (build 10))))\n";
    // sum k=1..10 of (k + k) = 2 * 55 = 110.
    allocs(src, "110");
}

/// A spread-returning closure passed first-class (to `List.map`) goes through the
/// owned wrapper, which explodes the boxed argument and reassembles the spread
/// result: correct and leak-free.
#[test]
fn first_class_spread_closure_is_correct() {
    let src = "module M\n\
        public type Vec2 = { x : Float, y : Float }\n\
        public scale2 : Float -> Vec2 -> Vec2\n\
        let scale2 k v = { x = v.x * k, y = v.y * k }\n\
        public sumx : List Vec2 -> Float\n\
        let sumx vs =\n  \
          match vs with\n  \
          | [] -> 0.0\n  \
          | v :: rest -> v.x + sumx rest\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt =\n  \
          let pts = [{ x = 1.0, y = 2.0 }, { x = 3.0, y = 4.0 }]\n  \
          let scaled = List.map (scale2 2.0) pts\n  \
          rt.console.writeLine (Int.toString (Float.toInt (sumx scaled)))\n";
    // scaled = (2,4),(6,8); sumx = 2 + 6 = 8.
    allocs(src, "8");
}

/// A scalar-returning function whose only parameter is a float aggregate is
/// **borrow-inferred** (the parameter is merely projected), yet the spread
/// boundary consumes its cell: passed first-class (to `List.map`), the owned
/// wrapper explodes and drops the boxed argument exactly once. Forcing such a
/// spread parameter owned (rather than lent) is what keeps the caller from also
/// dropping the cell — a double-free regression guard.
#[test]
fn first_class_aggregate_param_scalar_result_is_correct() {
    let src = "module M\n\
        public type Vec2 = { x : Float, y : Float }\n\
        public length : Vec2 -> Float\n\
        let length v = v.x + v.y\n\
        public sumF : List Float -> Float\n\
        let sumF xs =\n  \
          match xs with\n  \
          | [] -> 0.0\n  \
          | x :: rest -> x + sumF rest\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt =\n  \
          let pts = [{ x = 1.0, y = 2.0 }, { x = 3.0, y = 4.0 }]\n  \
          let ls = List.map length pts\n  \
          rt.console.writeLine (Int.toString (Float.toInt (sumF ls)))\n";
    // length = x + y → [3, 7]; sumF = 3 + 7 = 10.
    allocs(src, "10");
}

/// `example`/`forall` contracts over an aggregate-consuming function run through
/// the isolated contract worker (a separate wire/synthesis path from `fai run`):
/// the spread argument is built component-wise and never materialized, and the
/// run is leak-free. A regression guard for the contract path.
#[test]
fn contracts_over_aggregate_function_pass() {
    use fai_driver::{TestConfig, test};
    let src = "module M\n\
        public type Vec2 = { x : Float, y : Float }\n\
        public length : Vec2 -> Float\n\
        let length v = Float.sqrt (v.x * v.x + v.y * v.y)\n\
        example: length { x = 3.0, y = 4.0 } >= 0.0\n\
        forall v: length v >= 0.0\n";
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    let o = test(&db, &[file], None, TestConfig::default());
    assert!(
        o.ok,
        "contracts pass: diags={:?}",
        o.diagnostics.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(o.passed, o.total, "all contracts ran and passed");
    assert_eq!(o.leaked, 0, "contract run is leak-free");
}
