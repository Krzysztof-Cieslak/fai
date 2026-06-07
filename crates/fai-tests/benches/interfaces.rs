//! Benchmarks for interfaces, capabilities, and row-polymorphic field access
//! (offset evidence): inference, lowering, and JIT execution of programs that
//! exercise dictionary dispatch and evidence passing, plus the pure evidence
//! requirement computation.
//!
//! Local profiling only — performance is gated in CI by the deterministic guards
//! in `tests/perf_guards.rs`, not by these. Run with
//! `cargo bench -p fai-tests --bench interfaces`.

use divan::Bencher;
use fai_core::core;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_driver::jit_run_program;
use fai_runtime as rt;
use fai_syntax::Symbol;
use fai_types::{RecordRow, RowEnd, RowVarId, Scheme, Ty, check_file, evidence_requirements};

fn main() {
    // Discard program output produced while benchmarking `jit_run`.
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

// ── synthetic modules ────────────────────────────────────────────────────────

/// An interface with `methods` `Unit -> Int` methods, an instance implementing
/// them, and a `main` that dispatches one — exercising dictionary construction
/// and type-directed method access.
fn interface_module(methods: usize) -> String {
    let decls =
        (0..methods).map(|i| format!("  m{i} : Unit -> Int")).collect::<Vec<_>>().join("\n");
    let impls = (0..methods).map(|i| format!("m{i} u = {i}")).collect::<Vec<_>>().join(", ");
    format!(
        "module M\n\ninterface Big =\n{decls}\n\nlet inst = {{ Big with {impls} }}\n\n\
         public main : Runtime -> Unit\nlet main r = r.console.writeLine (Int.toString (inst.m0 ()))\n"
    )
}

/// A row-polymorphic accessor reading one field of a record with `fields`
/// fields — exercising offset-evidence elaboration and the dynamic field read.
fn row_poly_module(fields: usize) -> String {
    let lits = (0..fields).map(|i| format!("f{i} = {i}")).collect::<Vec<_>>().join(", ");
    format!(
        "module M\n\nget : {{ f0 : Int | 'r }} -> Int\nlet get rec = rec.f0\n\n\
         public main : Runtime -> Unit\n\
         let main r = r.console.writeLine (Int.toString (get {{ {lits} }}))\n"
    )
}

/// A least-authority capability record threaded through `depth` helpers before
/// it reaches the console — exercising evidence threading across call chains.
fn capability_module(depth: usize) -> String {
    let mut out = String::from("module M\n\n");
    for i in 0..depth {
        let body = if i == 0 {
            "env.console.writeLine \"deep\"".to_owned()
        } else {
            format!("helper{} env", i - 1)
        };
        out.push_str(&format!(
            "helper{i} : {{ console : Console | 'r }} -> Unit\nlet helper{i} env = {body}\n\n"
        ));
    }
    out.push_str(&format!("public main : Runtime -> Unit\nlet main r = helper{} r\n", depth - 1));
    out
}

// ── inference + lowering + JIT ────────────────────────────────────────────────

#[divan::bench(args = [4, 16, 64])]
fn check_interface_module(bencher: Bencher, methods: usize) {
    let src = interface_module(methods);
    bencher.with_inputs(|| fresh(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

#[divan::bench(args = [4, 16, 64])]
fn lower_interface_instance(bencher: Bencher, methods: usize) {
    let src = interface_module(methods);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(core(&db, file, Symbol::intern("inst"))));
}

#[divan::bench(args = [4, 16, 64])]
fn jit_interface_dispatch(bencher: Bencher, methods: usize) {
    let src = interface_module(methods);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

#[divan::bench(args = [8, 32, 128])]
fn lower_row_polymorphic_access(bencher: Bencher, fields: usize) {
    let src = row_poly_module(fields);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(core(&db, file, Symbol::intern("main"))));
}

#[divan::bench(args = [8, 32, 128])]
fn jit_row_polymorphic_access(bencher: Bencher, fields: usize) {
    let src = row_poly_module(fields);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

#[divan::bench(args = [2, 8, 32])]
fn jit_capability_threading(bencher: Bencher, depth: usize) {
    let src = capability_module(depth);
    bencher
        .with_inputs(|| fresh(&src))
        .bench_values(|(db, file)| divan::black_box(jit_run_program(&db, file).exit_code));
}

// ── the evidence requirement computation (pure) ──────────────────────────────

/// A function type whose single open record carries `fields` labels.
fn wide_open_record(fields: usize) -> Scheme {
    let mut row: Vec<(Symbol, Ty)> =
        (0..fields).map(|i| (Symbol::intern(&format!("f{i}")), Ty::int())).collect();
    row.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let record = Ty::Record(RecordRow { fields: row, tail: RowEnd::Open(RowVarId(0)) });
    Scheme::mono(Ty::arrow(record, Ty::int()))
}

#[divan::bench(args = [8, 32, 128])]
fn evidence_requirements_wide(bencher: Bencher, fields: usize) {
    let scheme = wide_open_record(fields);
    bencher.bench(|| divan::black_box(evidence_requirements(&scheme)));
}
