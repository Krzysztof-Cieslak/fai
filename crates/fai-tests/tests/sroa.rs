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

/// Like [`same_allocs`], but the zero-extra-allocation guarantee holds only when a
/// result aggregate of `ret_fields` components fits the target's return-register
/// budget. A multi-value return must fit entirely in registers (arguments spill
/// to the stack, but returns cannot), so on a target whose budget is narrower
/// than the result the aggregate is returned **boxed** — there only correctness
/// and leak-freedom are asserted, not the allocation count.
#[track_caller]
fn same_allocs_when_register_returned(ffa: &str, scalar: &str, expect: &str, ret_fields: usize) {
    if fai_core::ir::max_spread_return() >= ret_fields {
        same_allocs(ffa, scalar, expect);
    } else {
        allocs(ffa, expect);
        allocs(scalar, expect);
    }
}

const WIDE: &str = r#"module Wide
public type V2 = { a : Float, b : Float }
public echo2 : V2 -> V2
let echo2 v = v
public type V3 = { x : Float, y : Float, z : Float }
public shift3 : V3 -> V3
let shift3 v = { x = v.x + 1.0, y = v.y + 2.0, z = v.z + 3.0 }
public echo3 : V3 -> V3
let echo3 v = v
public sum3 : V3 -> Float
let sum3 v = v.x + v.y + v.z
public create3 : Int -> V3
let create3 n = if n > 0 then create3 (n - 1) else { x = 1.0, y = 2.0, z = 3.0 }
public type V8 = { a : Float, b : Float, c : Float, d : Float, e : Float, f : Float, g : Float, h : Float }
public add8 : V8 -> V8 -> V8
let add8 x y = { a = x.a + y.a, b = x.b + y.b, c = x.c + y.c, d = x.d + y.d, e = x.e + y.e, f = x.f + y.f, g = x.g + y.g, h = x.h + y.h }
public sum8 : V8 -> Float
let sum8 x = x.a + x.b + x.c + x.d + x.e + x.f + x.g + x.h
"#;

#[track_caller]
fn module_allocs(library: &str, main: &str, expected: &str) -> i64 {
    let _guard = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Wide.fai".into(), library.into());
    let id = db.add_source("M.fai".into(), main.into());
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, db.source_file(id).unwrap());
    let allocations = rt::allocations();
    assert_eq!(outcome.exit_code, 0, "{:?}", outcome.diagnostics);
    assert_eq!(rt::capture_take().trim(), expected);
    allocations
}

#[test]
fn three_float_results_cross_a_module_without_a_cell() {
    let main = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Float.toInt (Wide.sum3 (Wide.shift3 { x = 1.0, y = 2.0, z = 3.0 }))))\n";
    let scalar = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Float.toInt (2.0 + 4.0 + 6.0)))\n";
    let wide = module_allocs(WIDE, main, "12");
    let baseline = module_allocs(WIDE, scalar, "12");
    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        assert_eq!(wide, baseline, "a direct three-component result needs no heap cell");
    }
}

#[test]
fn eight_float_results_support_spilled_arguments() {
    let main = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let a = { a = 1.0, b = 2.0, c = 3.0, d = 4.0, e = 5.0, f = 6.0, g = 7.0, h = 8.0 }\n  let b = Wide.add8 a a\n  let c = Wide.add8 b a\n  r.console.writeLine (Int.toString (Float.toInt (Wide.sum8 c)))\n";
    let scalar = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Float.toInt 108.0))\n";
    let wide = module_allocs(WIDE, main, "108");
    let baseline = module_allocs(WIDE, scalar, "108");
    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        assert_eq!(wide, baseline, "wide results remain in registers across repeated calls");
    }
}

#[test]
fn wide_return_first_class_wrapper_keeps_the_uniform_boundary() {
    let main = "module M\nlet apply n f x = if n = 0 then f x else apply (n - 1) f x\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let v = apply 1 Wide.shift3 { x = 1.0, y = 2.0, z = 3.0 }\n  r.console.writeLine (Int.toString (Float.toInt (Wide.sum3 v)))\n";
    module_allocs(WIDE, main, "12");
}

#[test]
fn scalar_only_factory_keeps_its_previous_return_budget() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Wide.fai".into(), WIDE.into());
    let file = db.source_file(id).unwrap();
    let name = fai_syntax::Symbol::intern("create3");
    if fai_core::ir::max_spread_return() < 3 {
        assert!(fai_core::abi::abi(&db, file, name).spread_return().is_none());
        assert!(fai_core::pretty_def(&fai_rc::rc(&db, file, name)).contains("(join "));
    }
}

#[test]
fn wide_results_preserve_signed_zero_and_nan_payloads() {
    let main = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let v = Wide.echo3 { x = -0.0, y = Float.fromBits 0x7ff8000000001234, z = Float.fromBits 0xfff8000000005678 }\n  let ok = Float.toBits v.x = 0x8000000000000000 && Float.toBits v.y = 0x7ff8000000001234 && Float.toBits v.z = 0xfff8000000005678\n  r.console.writeLine (if ok then \"yes\" else \"no\")\n";
    module_allocs(WIDE, main, "yes");
}

#[test]
fn conditional_values_rewrite_two_component_projections() {
    let main = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let v = Wide.echo2 { a = 1.0, b = 2.0 }\n  let ok = v.a = 1.0 && v.b = 2.0\n  r.console.writeLine (if ok then \"yes\" else \"no\")\n";
    module_allocs(WIDE, main, "yes");
}

#[test]
fn opaque_wide_return_uses_the_physical_layout_without_exposing_it() {
    let library = "module Wide\npublic opaque type V = { x : Float, y : Float, z : Float }\npublic make : Float -> Float -> Float -> V\nlet make x y z = { x = x, y = y, z = z }\npublic shift : V -> V\nlet shift v = { x = v.x + 1.0, y = v.y + 2.0, z = v.z + 3.0 }\npublic sum : V -> Float\nlet sum v = v.x + v.y + v.z\n";
    let main = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Float.toInt (Wide.sum (Wide.shift (Wide.make 1.0 2.0 3.0)))))\n";
    let baseline = "module M\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Float.toInt (Wide.sum (Wide.make 2.0 4.0 6.0))))\n";
    let wide = module_allocs(library, main, "12");
    let scalar = module_allocs(library, baseline, "12");
    if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        assert_eq!(wide, scalar, "the abstract shift result adds no box");
    }
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
    same_allocs_when_register_returned(&ffa, scalar, "149.0", 2);
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
    same_allocs_when_register_returned(ffa, scalar, "120", 2);
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
    same_allocs_when_register_returned(ffa, scalar, "56", 2);
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
