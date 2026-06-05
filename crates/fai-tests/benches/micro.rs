//! Microbenchmarks for the inference primitives most likely to dominate cost:
//! unification on large types, inference of deep/wide expressions, and large
//! strongly-connected components (the R16 risk: big mutually-recursive groups).
//!
//! Run with `cargo bench -p fai-tests --bench micro`.

use divan::Bencher;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_types::{Con, InferCtx, SolveTy, Ty, TyVarId, check_file, def_type};

fn main() {
    divan::main();
}

// ── unification ──────────────────────────────────────────────────────────────

/// Builds a deeply nested arrow type `a0 -> a1 -> ... -> Int` of the given depth
/// as a solver type, with fresh variables already allocated in `cx`.
fn nested_arrow(cx: &mut InferCtx, depth: usize) -> SolveTy {
    let mut ty = SolveTy::int();
    for _ in 0..depth {
        let v = cx.fresh();
        ty = SolveTy::arrow(v, ty);
    }
    ty
}

/// Unifying two structurally-identical large types (fresh variables on each
/// side) — the unification + occurs-check hot path.
#[divan::bench(args = [16, 64, 256])]
fn unify_nested_arrows(bencher: Bencher, depth: usize) {
    bencher
        .with_inputs(|| {
            let mut cx = InferCtx::new();
            let a = nested_arrow(&mut cx, depth);
            let b = nested_arrow(&mut cx, depth);
            (cx, a, b)
        })
        .bench_values(|(mut cx, a, b)| {
            divan::black_box(cx.unify(&a, &b));
            cx
        });
}

/// Unifying a fresh variable against a large concrete type (occurs check walks
/// the whole structure).
#[divan::bench(args = [16, 64, 256])]
fn unify_var_with_large_type(bencher: Bencher, depth: usize) {
    bencher
        .with_inputs(|| {
            let mut cx = InferCtx::new();
            let big = nested_arrow(&mut cx, depth);
            let v = cx.fresh();
            (cx, v, big)
        })
        .bench_values(|(mut cx, v, big)| {
            divan::black_box(cx.unify(&v, &big));
            cx
        });
}

// ── instantiate / generalize ─────────────────────────────────────────────────

/// A polymorphic scheme `'a0 -> 'a1 -> ... -> 'a0` with `n` quantified vars.
fn big_scheme(n: usize) -> fai_types::Scheme {
    let mut ty = Ty::Var(TyVarId(0));
    for i in (0..n).rev() {
        ty = Ty::arrow(Ty::Var(TyVarId(i as u32)), ty);
    }
    fai_types::Scheme::new((0..n as u32).map(TyVarId).collect(), ty)
}

/// Instantiating a large polymorphic scheme with fresh variables.
#[divan::bench(args = [8, 32, 128])]
fn instantiate_scheme(bencher: Bencher, vars: usize) {
    let scheme = big_scheme(vars);
    bencher.with_inputs(InferCtx::new).bench_values(|mut cx| {
        divan::black_box(cx.instantiate(&scheme));
        cx
    });
}

// ── rendering ────────────────────────────────────────────────────────────────

/// Rendering a moderately large type to its display string.
#[divan::bench(args = [16, 64, 256])]
fn render_type(bencher: Bencher, depth: usize) {
    let mut ty = Ty::Con(Con::Int);
    for i in 0..depth {
        ty = Ty::arrow(Ty::Var(TyVarId(i as u32)), ty);
    }
    let scheme = fai_types::Scheme::new((0..depth as u32).map(TyVarId).collect(), ty);
    bencher.bench(|| divan::black_box(fai_types::render_scheme(&scheme)));
}

// ── end-to-end inference of single large definitions ─────────────────────────

fn db_with(source: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let id = db.add_source("M.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

/// Inference of one definition with a long chain of `let`s (deep body).
#[divan::bench(args = [10, 50, 200])]
fn infer_deep_let_chain(bencher: Bencher, depth: usize) {
    let mut body = String::from("module M\n\nlet f x =\n");
    let mut prev = String::from("x");
    for d in 0..depth {
        body.push_str(&format!("  let t{d} = {prev} + {d}\n"));
        prev = format!("t{d}");
    }
    body.push_str(&format!("  {prev}\n"));

    bencher.with_inputs(|| db_with(&body)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

/// Inference over a large strongly-connected component: `n` mutually-recursive,
/// signature-less functions (exercises SCC formation + co-inference, risk R16).
#[divan::bench(args = [4, 16, 48])]
fn infer_large_scc(bencher: Bencher, n: usize) {
    let mut src = String::from("module M\n\n");
    for i in 0..n {
        let next = (i + 1) % n;
        // Each function calls the next, forming one big cycle; signature-less so
        // they all land in a single SCC.
        src.push_str(&format!("let f{i} x = if x = 0 then 0 else f{next} (x - 1)\n\n"));
    }
    let first = fai_syntax::Symbol::intern("f0");

    bencher.with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        // Drive inference of the whole SCC via one member.
        divan::black_box(def_type(&db, file, first));
        db
    });
}
