//! Reuse, in-place-update, and borrowing benchmarks.
//!
//! Local profiling only — performance is *gated* in CI by the deterministic
//! guards in `tests/perf_guards.rs`, not by these. Run with
//! `cargo bench -p fai-tests --bench reuse`.
//!
//! Each benchmark JIT-compiles and runs a small program through the public
//! driver pipeline. The interesting comparisons are between paired benches: a
//! *unique* rebuild (cells recycled in place) versus a *shared* one (cells
//! copied), and an in-place record update versus a copying one. The allocation
//! reduction itself is asserted in `tests/reuse.rs`; here we measure its
//! wall-clock effect.

use divan::Bencher;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_driver::jit_run_program;
use fai_runtime as rt;
use indoc::formatdoc;

fn main() {
    // Discard program output produced while benchmarking.
    rt::capture_start();
    divan::main();
}

/// A fresh database holding `src` (and the prelude), returning the file.
fn fresh(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

/// A program with `build`/`sum` list helpers plus an `inc` rebuilder, whose
/// `main` prints `use (build n)`.
fn list_prog(use_body: &str, n: i32) -> String {
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

        let inc xs =
          match xs with
          | [] -> []
          | x :: rest -> (x + 1) :: inc rest

        let use xs = {use_body}

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (use (build {n})))
    "#}
}

/// A record `bumpN` chain: `k` updates of a uniquely-owned record, each
/// overwriting the field in place.
fn record_prog(k: i32) -> String {
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
}

// ── list rebuild: recycled (unique) vs copied (shared) ──────────────────────

/// A unique spine flows straight into `inc`, which recycles each cons cell.
#[divan::bench(args = [50, 200, 1000])]
fn map_unique_recycles(bencher: Bencher, n: i32) {
    let src = list_prog("sum (inc xs)", n);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

/// The spine is read again by `sum xs`, so `inc` must copy each cons cell.
#[divan::bench(args = [50, 200, 1000])]
fn map_shared_copies(bencher: Bencher, n: i32) {
    let src = list_prog("sum (inc xs) + sum xs", n);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

// ── record update: in place (unique) vs copying (shared) ────────────────────

#[divan::bench(args = [50, 200, 1000])]
fn record_update_in_place(bencher: Bencher, k: i32) {
    let src = record_prog(k);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

// ── borrowing: an inspector read twice should not duplicate the structure ────

#[divan::bench(args = [50, 200, 1000])]
fn borrow_inspector_read_twice(bencher: Bencher, n: i32) {
    let src = list_prog("len xs + len xs", n);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

// ── inspect-only primitives: a boxed operand compared/read then reused ───────
// `=` / `String.length` only read their operands, so borrowing them lets the
// reused value flow on without a per-step duplication.

/// A boxed list compared on each step and then passed to the recursive call.
fn compare_prog(k: i32, n: i32) -> String {
    formatdoc! {r#"
        module M

        let build k = if k <= 0 then [] else k :: build (k - 1)

        let cmpAll k xs =
          if k <= 0 then 0
          else (if xs = xs then 1 else 0) + cmpAll (k - 1) xs

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (cmpAll {k} (build {n})))
    "#}
}

#[divan::bench(args = [50, 200, 1000])]
fn compare_heavy(bencher: Bencher, k: i32) {
    let src = compare_prog(k, 20);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

/// A boxed string read on each step (`String.length`) and then passed along.
fn string_read_prog(k: i32) -> String {
    formatdoc! {r#"
        module M

        let lenAll k s =
          if k <= 0 then 0
          else String.length s + lenAll (k - 1) s

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (lenAll {k} "abcdefghij"))
    "#}
}

#[divan::bench(args = [50, 200, 1000])]
fn string_read_heavy(bencher: Bencher, k: i32) {
    let src = string_read_prog(k);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}
