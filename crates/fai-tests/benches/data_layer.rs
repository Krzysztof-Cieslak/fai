// The runtime-primitive benches call FFI `unsafe` constructors; that is the only
// hand-written unsafe here.
#![allow(unsafe_code)]

//! Benchmarks for the data layer: inference and exhaustiveness over record- and
//! union-heavy modules, lowering of `match` and records, and the structural
//! runtime primitives (`compare`, Float arithmetic, composite construction).
//!
//! Local profiling only — performance is *gated* in CI by the deterministic
//! guards in `tests/perf_guards.rs`, not by these. Run with
//! `cargo bench -p fai-tests --bench data_layer`.

use divan::Bencher;
use fai_core::core;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_runtime as rt;
use fai_syntax::Symbol;
use fai_types::check_file;

fn main() {
    divan::main();
}

/// A fresh database holding `src` (and the prelude), returning the file.
fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

// ── synthetic data-layer modules ─────────────────────────────────────────────

/// A record type of `fields` `Int` fields, a builder, and an accessor function
/// that sums every field — exercising row unification and constant-offset field
/// access at inference time.
fn record_module(fields: usize) -> String {
    let labels: Vec<String> = (0..fields).map(|i| format!("f{i}")).collect();
    let decl = labels.iter().map(|l| format!("{l} : Int")).collect::<Vec<_>>().join(", ");
    let lits = labels.iter().map(|l| format!("{l} = 0")).collect::<Vec<_>>().join(", ");
    let sum = labels.iter().map(|l| format!("r.{l}")).collect::<Vec<_>>().join(" + ");
    format!(
        "module M\n\ntype R = {{ {decl} }}\n\npublic mk : R\nlet mk = {{ {lits} }}\n\npublic total : R -> Int\nlet total r = {sum}\n"
    )
}

/// A union of `ctors` single-field constructors plus an exhaustive `match` over
/// them — exercising constructor resolution and exhaustiveness checking.
fn union_match_module(ctors: usize) -> String {
    let variants = (0..ctors).map(|i| format!("  | C{i} Int")).collect::<Vec<_>>().join("\n");
    let arms = (0..ctors).map(|i| format!("  | C{i} x -> x + {i}")).collect::<Vec<_>>().join("\n");
    format!(
        "module M\n\ntype T =\n{variants}\n\npublic eval : T -> Int\nlet eval t =\n  match t with\n{arms}\n"
    )
}

/// A `match` over many `Int` literal arms ending in a wildcard — the literal
/// exhaustiveness/redundancy path.
fn literal_match_module(arms: usize) -> String {
    let cases = (0..arms).map(|i| format!("  | {i} -> {i}")).collect::<Vec<_>>().join("\n");
    format!(
        "module M\n\npublic classify : Int -> Int\nlet classify n =\n  match n with\n{cases}\n  | _ -> 0 - 1\n"
    )
}

// ── inference / exhaustiveness ───────────────────────────────────────────────

#[divan::bench(args = [8, 32, 128])]
fn check_record_module(bencher: Bencher, fields: usize) {
    let src = record_module(fields);
    bencher.with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

#[divan::bench(args = [8, 32, 128])]
fn check_union_match_module(bencher: Bencher, ctors: usize) {
    let src = union_match_module(ctors);
    bencher.with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

#[divan::bench(args = [16, 64, 256])]
fn check_literal_match_exhaustiveness(bencher: Bencher, arms: usize) {
    let src = literal_match_module(arms);
    bencher.with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

// ── lowering ─────────────────────────────────────────────────────────────────

/// Lowering a `match` over many constructors to its decision tree.
#[divan::bench(args = [8, 32, 128])]
fn lower_match_decision_tree(bencher: Bencher, ctors: usize) {
    let src = union_match_module(ctors);
    bencher
        .with_inputs(|| db_with(&src))
        .bench_values(|(db, file)| divan::black_box(core(&db, file, Symbol::intern("eval"))));
}

/// Lowering a record builder + accessor (constant-offset projections).
#[divan::bench(args = [8, 32, 128])]
fn lower_record_access(bencher: Bencher, fields: usize) {
    let src = record_module(fields);
    bencher
        .with_inputs(|| db_with(&src))
        .bench_values(|(db, file)| divan::black_box(core(&db, file, Symbol::intern("total"))));
}

// ── runtime primitives ───────────────────────────────────────────────────────

/// A `Float` value from an `f64`.
fn flt(x: f64) -> rt::Value {
    rt::fai_box_float(x.to_bits() as i64)
}

/// A nested record `((1, 2), (3, 4))` as boxed data (tag 0).
fn nested_pair() -> rt::Value {
    // SAFETY: each `fai_make_data` receives the stated number of owned fields.
    unsafe {
        let inner1 = {
            let f = [rt::fai_box_int(1), rt::fai_box_int(2)];
            rt::fai_make_data(0, 2, f.as_ptr())
        };
        let inner2 = {
            let f = [rt::fai_box_int(3), rt::fai_box_int(4)];
            rt::fai_make_data(0, 2, f.as_ptr())
        };
        let f = [inner1, inner2];
        rt::fai_make_data(0, 2, f.as_ptr())
    }
}

/// Structural `compare` over a nested composite (recursive field walk). The
/// result is an immediate `Int`, so no drop is needed; both operands are
/// consumed by `fai_compare`.
#[divan::bench]
fn prim_compare_nested_data(bencher: Bencher) {
    bencher.bench(|| divan::black_box(rt::fai_compare(nested_pair(), nested_pair())));
}

/// A short Float arithmetic chain (each op boxes a fresh result).
#[divan::bench]
fn prim_float_arithmetic(bencher: Bencher) {
    bencher.bench(|| {
        let r = rt::fai_float_add(flt(1.5), rt::fai_float_mul(flt(2.0), flt(3.0)));
        rt::fai_drop(divan::black_box(r));
    });
}

/// Constructing a two-field composite and projecting a boxed field.
#[divan::bench]
fn prim_make_data_and_project(bencher: Bencher) {
    bencher.bench(|| {
        // SAFETY: a two-field data value, then a projection that consumes it.
        let field = unsafe {
            let fields = [rt::fai_box_int(1 << 62), rt::fai_box_int(2)];
            let d = rt::fai_make_data(1, 2, fields.as_ptr());
            rt::fai_data_field(d, 0)
        };
        rt::fai_drop(divan::black_box(field));
    });
}
