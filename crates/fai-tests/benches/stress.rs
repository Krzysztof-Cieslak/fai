//! Stress / pathological-scenario benchmarks for type inference.
//!
//! These target the hard cases that make HM inference expensive: exponential
//! type growth, very wide/deep structures, instantiation-heavy code,
//! constraint-heavy bodies, wide modules, deep dependency chains, and
//! error-laden files. Run with `cargo bench -p fai-tests --bench stress`.
//!
//! Like the other benches these are for local profiling, not a CI gate; the
//! deterministic guards in `tests/perf_guards.rs` gate incrementality.

use std::fmt::Write as _;

use divan::Bencher;
use divan::counter::ItemsCount;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_syntax::Symbol;
use fai_types::{check_file, def_type};

fn main() {
    divan::main();
}

/// Builds a database with the prelude and a single module `M` from `source`.
fn db_with(source: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let id = db.add_source("M.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

// ── exponential type growth ──────────────────────────────────────────────────
// The classic HM blow-up: each step pairs the previous value with itself, so the
// inferred type doubles in size at every level (2^depth leaves). Stresses
// unification, reification, and generalization on a huge monomorphic type.

#[divan::bench(args = [6, 9, 12])]
fn exponential_tuple_growth(bencher: Bencher, depth: usize) {
    let mut src = String::from("module M\n\nlet grow x =\n");
    src.push_str("  let p0 = (x, x)\n");
    for d in 1..depth {
        let _ = writeln!(src, "  let p{d} = (p{}, p{})", d - 1, d - 1);
    }
    let _ = writeln!(src, "  p{}", depth - 1);
    let entry = Symbol::intern("grow");

    bencher.counter(ItemsCount::new(1usize << depth)).with_inputs(|| db_with(&src)).bench_values(
        |(db, file)| {
            divan::black_box(def_type(&db, file, entry));
            db
        },
    );
}

// ── wide tuples ──────────────────────────────────────────────────────────────
// Construct then destructure a wide tuple; stresses tuple unification width.

#[divan::bench(args = [20, 60, 120])]
fn wide_tuple_roundtrip(bencher: Bencher, width: usize) {
    let mut src = String::from("module M\n\nlet f x =\n");
    let elems = (0..width).map(|_| "x").collect::<Vec<_>>().join(", ");
    let _ = writeln!(src, "  let t = ({elems})");
    let pat = (0..width).map(|i| format!("a{i}")).collect::<Vec<_>>().join(", ");
    let _ = writeln!(src, "  let ({pat}) = t");
    let _ = writeln!(src, "  a0");
    let entry = Symbol::intern("f");

    bencher.counter(ItemsCount::new(width)).with_inputs(|| db_with(&src)).bench_values(
        |(db, file)| {
            divan::black_box(def_type(&db, file, entry));
            db
        },
    );
}

// ── long curried application ─────────────────────────────────────────────────
// A function of N params fully applied to N arguments; stresses arrow unify and
// the curried-application walk. (Currently super-linear — the occurs check
// re-walks the growing result type at each binding; see PLAN.md M9.)

#[divan::bench(args = [20, 60, 120])]
fn long_application_chain(bencher: Bencher, n: usize) {
    let params = (0..n).map(|i| format!("a{i}")).collect::<Vec<_>>().join(" ");
    let sum = (0..n).map(|i| format!("a{i}")).collect::<Vec<_>>().join(" + ");
    let args = (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(" ");
    let src = format!("module M\n\nlet f {params} = {sum}\n\nlet r = f {args}\n");
    let entry = Symbol::intern("r");

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        divan::black_box(def_type(&db, file, entry));
        db
    });
}

// ── deep if/else decision tree ───────────────────────────────────────────────
// A long `if … then … else if …` chain; every branch unifies against the result.

#[divan::bench(args = [20, 100, 400])]
fn deep_if_else_chain(bencher: Bencher, n: usize) {
    let mut body = String::new();
    for i in 0..n {
        let _ = write!(body, "if x = {i} then {i} else ");
    }
    body.push('x');
    let src = format!("module M\n\nlet f x = {body}\n");
    let entry = Symbol::intern("f");

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        divan::black_box(def_type(&db, file, entry));
        db
    });
}

// ── instantiation-heavy: many uses of a polymorphic function ─────────────────
// A polymorphic `id` instantiated at many call sites; stresses scheme
// instantiation throughput.

#[divan::bench(args = [50, 200, 800])]
fn many_polymorphic_instantiations(bencher: Bencher, n: usize) {
    let mut src = String::from("module M\n\nlet id x = x\n\nlet f =\n");
    for i in 0..n {
        // Alternate the instantiation type to exercise fresh-var allocation.
        let arg = if i % 2 == 0 { "0" } else { "true" };
        let _ = writeln!(src, "  let r{i} = id {arg}");
    }
    let _ = writeln!(src, "  r0");
    let entry = Symbol::intern("f");

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        divan::black_box(def_type(&db, file, entry));
        db
    });
}

// ── constraint-heavy arithmetic ──────────────────────────────────────────────
// A long chain of arithmetic over one variable; stresses Numeric-constraint
// propagation and defaulting.

#[divan::bench(args = [50, 200, 800])]
fn long_arithmetic_chain(bencher: Bencher, n: usize) {
    let terms = (0..n).map(|i| format!("x + {i}")).collect::<Vec<_>>().join(" + ");
    let src = format!("module M\n\nlet f x = {terms}\n");
    let entry = Symbol::intern("f");

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        divan::black_box(def_type(&db, file, entry));
        db
    });
}

// ── wide module: many independent definitions in one file ────────────────────
// Complements the corpus benches (which spread defs across modules): one huge
// module, cold-checked.

#[divan::bench(args = [100, 400, 1000])]
fn wide_module_cold_check(bencher: Bencher, n: usize) {
    let mut src = String::from("module M\n\n");
    for i in 0..n {
        let _ = writeln!(src, "let f{i} x = x + {i}\n");
    }

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

// ── deep signatured dependency chain ─────────────────────────────────────────
// f0 → f1 → … → fN, all signatured (so each edge is cut and they are N singleton
// SCCs). Stresses cross-definition signature lookup at depth.

#[divan::bench(args = [50, 200, 600])]
fn deep_signatured_chain(bencher: Bencher, n: usize) {
    let mut src = String::from("module M\n\n");
    for i in 0..n {
        let _ = writeln!(src, "public f{i} : Int -> Int");
        if i + 1 < n {
            let _ = writeln!(src, "let f{i} x = f{} x + 1\n", i + 1);
        } else {
            let _ = writeln!(src, "let f{i} x = x\n");
        }
    }
    let entry = Symbol::intern("f0");

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        divan::black_box(def_type(&db, file, entry));
        db
    });
}

// ── contracts-heavy module ───────────────────────────────────────────────────
// Many `forall`/`example` contracts referencing the same function; stresses the
// per-file contract checking pass.

#[divan::bench(args = [50, 200, 600])]
fn many_contracts(bencher: Bencher, n: usize) {
    let mut src = String::from("module M\n\npublic f : Int -> Int\nlet f x = x + 1\n\n");
    for i in 0..n {
        if i % 2 == 0 {
            let _ = writeln!(src, "example: f {i} = f {i}");
        } else {
            let _ = writeln!(src, "forall n: f n = f n");
        }
    }

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}

// ── error-heavy file: cascade-suppression cost ───────────────────────────────
// Many independent type errors; stresses diagnostic emission and the error-type
// sentinel path.

#[divan::bench(args = [50, 200, 600])]
fn many_type_errors(bencher: Bencher, n: usize) {
    let mut src = String::from("module M\n\n");
    for i in 0..n {
        // `Int + Bool` — an independent type error per binding.
        let _ = writeln!(src, "let e{i} = {i} + true\n");
    }

    bencher.counter(ItemsCount::new(n)).with_inputs(|| db_with(&src)).bench_values(|(db, file)| {
        check_file(&db, file);
        db
    });
}
