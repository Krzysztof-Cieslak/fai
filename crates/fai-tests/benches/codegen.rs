// This benchmark calls the runtime's closure/apply primitives, which are FFI
// `unsafe`; that is the only hand-written unsafe here.
#![allow(unsafe_code)]

//! Backend benchmarks: the lower → reference-count → Cranelift → JIT pipeline,
//! plus a few runtime primitives. Local profiling only (not a CI gate).
//!
//! Run with `cargo bench -p fai-tests --bench codegen`.

use divan::Bencher;
use fai_codegen::object_for_def;
use fai_core::core;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_driver::{jit_run_program, object_code};
use fai_rc::rc;
use fai_runtime as rt;
use fai_syntax::Symbol;

fn main() {
    // Discard program output produced while benchmarking `jit_run`.
    rt::capture_start();
    divan::main();
}

/// A small program (a single arithmetic `main`).
const SMALL: &str = "module M\n\npublic main : Runtime -> Unit\nlet main r = r.console.writeLine (Int.toString (1 + 2 * 3))\n";

/// A medium program: a helper chain plus higher-order use.
const MEDIUM: &str = "module M\n\nlet inc x = x + 1\n\nlet double x = x + x\n\nlet apply f x = f x\n\nlet step x = double (inc x)\n\npublic main : Runtime -> Unit\nlet main r = r.console.writeLine (Int.toString (apply step (step 10)))\n";

/// Builds a fresh database holding `src` (and the prelude), returning the file.
fn fresh(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

// ── the compile pipeline (fresh database each iteration) ─────────────────────

#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn lower(bencher: Bencher, program: (&str, &str)) {
    bencher
        .with_inputs(|| fresh(program.1))
        .bench_values(|(db, file)| divan::black_box(core(&db, file, Symbol::intern("main"))));
}

#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn reference_count(bencher: Bencher, program: (&str, &str)) {
    bencher
        .with_inputs(|| fresh(program.1))
        .bench_values(|(db, file)| divan::black_box(rc(&db, file, Symbol::intern("main"))));
}

#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn aot_object(bencher: Bencher, program: (&str, &str)) {
    bencher.with_inputs(|| fresh(program.1)).bench_values(|(db, file)| {
        divan::black_box(object_code(&db, file, Symbol::intern("main")))
    });
}

#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn jit_compile_and_run(bencher: Bencher, program: (&str, &str)) {
    bencher
        .with_inputs(|| fresh(program.1))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

/// A program of `n` independent functions, all summed by `main`, so the closure
/// reachable from `main` has ~`n` definitions to code-generate.
fn many_defs(n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("module M\n\n");
    for i in 0..n {
        let _ = writeln!(s, "let f{i} x = x + {i} - {i} * 2\n");
    }
    let calls = (0..n).map(|i| format!("f{i} {i}")).collect::<Vec<_>>().join(" + ");
    let _ = write!(
        s,
        "public main : Runtime -> Unit\nlet main r = r.console.writeLine (Int.toString ({calls}))\n"
    );
    s
}

/// Code generation across the definitions reachable from `main`, in parallel
/// (each `object_code` is an independent query on a per-worker database handle —
/// the shape `build_native` uses). Run with `RAYON_NUM_THREADS=1` vs unset to
/// compare serial and parallel. Uses a fresh database so the in-memory cache is
/// cold and real code generation runs; `object_code` is called directly, so the
/// on-disk cache is not involved.
#[divan::bench(args = [50, 200])]
fn aot_codegen_reachable(bencher: Bencher, n: usize) {
    use fai_db::Db;
    use fai_driver::{object_code, reachable_defs};
    use rayon::prelude::*;

    let src = many_defs(n);
    bencher
        .counter(divan::counter::ItemsCount::new(n))
        .with_inputs(|| {
            let (db, file) = fresh(&src);
            let reachable = reachable_defs(&db, file);
            (db, reachable)
        })
        .bench_values(|(db, reachable)| {
            let objs: Vec<_> = reachable
                .par_iter()
                .map_with(db.clone_box(), |dbh, def| {
                    let db: &dyn Db = &**dbh;
                    db.source_file(def.file).map(|f| object_code(db, f, def.name))
                })
                .collect();
            divan::black_box(objs)
        });
}

/// JIT-compiling and running the closure reachable from `main` over a many-def
/// program. The lower/reference-count gather and the per-function Cranelift
/// code generation run in parallel (the serial JIT link/finalize and the tiny
/// run remain). Run with `RAYON_NUM_THREADS=1` vs unset to compare.
#[divan::bench(args = [50, 200])]
fn jit_compile_reachable(bencher: Bencher, n: usize) {
    let src = many_defs(n);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

/// Object codegen straight from a pre-lowered definition (no salsa, no front
/// end) — the pure Cranelift cost.
#[divan::bench]
fn object_for_def_only(bencher: Bencher) {
    let (db, file) = fresh(MEDIUM);
    let lowered = rc(&db, file, Symbol::intern("main"));
    let namer = |_: fai_resolve::DefId| "fai_M_main".to_owned();
    let arity = |_: fai_resolve::DefId| 1usize;
    let abi = |_: fai_resolve::DefId| fai_core::ir::FnAbi::default();
    bencher.bench(|| divan::black_box(object_for_def(&lowered, &namer, &arity, &abi)));
}

// ── runtime primitives ──────────────────────────────────────────────────────

#[divan::bench]
fn prim_int_add_immediate(bencher: Bencher) {
    bencher.bench(|| {
        let r = rt::fai_int_add(rt::fai_box_int(2), rt::fai_box_int(3));
        divan::black_box(r)
    });
}

#[divan::bench]
fn prim_int_add_boxed(bencher: Bencher) {
    bencher.bench(|| {
        let r = rt::fai_int_add(rt::fai_box_int(1 << 62), rt::fai_box_int(1 << 62));
        rt::fai_drop(divan::black_box(r));
    });
}

#[divan::bench]
fn prim_string_concat(bencher: Bencher) {
    bencher.bench(|| {
        let a = rt::fai_int_to_string(rt::fai_box_int(12_345));
        let b = rt::fai_int_to_string(rt::fai_box_int(67_890));
        rt::fai_drop(divan::black_box(rt::fai_string_concat(a, b)));
    });
}

unsafe extern "C" fn add2(_env: *const i64, args: *const i64) -> i64 {
    // SAFETY: two owned arguments consumed by `fai_int_add`.
    unsafe { rt::fai_int_add(*args, *args.add(1)) }
}

#[divan::bench]
fn prim_make_closure_and_apply(bencher: Bencher) {
    bencher.bench(|| {
        // SAFETY: a fresh arity-2 closure applied to two owned arguments.
        let r = unsafe {
            let closure = rt::fai_make_closure(add2 as *const u8, 2, 0, std::ptr::null());
            let args = [rt::fai_box_int(2), rt::fai_box_int(3)];
            rt::fai_apply_n(closure, 2, args.as_ptr())
        };
        divan::black_box(r)
    });
}

#[divan::bench]
fn prim_dup_drop_boxed(bencher: Bencher) {
    let value = rt::fai_box_int(1 << 62);
    bencher.bench(|| {
        rt::fai_dup(value);
        rt::fai_drop(value);
    });
    rt::fai_drop(value);
}
