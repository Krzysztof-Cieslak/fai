//! End-to-end type-inference benchmarks over synthetic workspaces.
//!
//! These measure wall-clock cost for local profiling — performance is *gated* in
//! CI by the deterministic guards in `tests/perf_guards.rs`, not by these. Run
//! with `cargo bench -p fai-tests --bench inference`.
//!
//! The headline comparison: `cold_check` grows with workspace size, while the
//! `warm_*_edit` benchmarks (which re-check after one edit on a warmed database)
//! should stay roughly flat — that is the cross-module firewall paying off.

use divan::Bencher;
use divan::counter::ItemsCount;
use fai_db::{FaiDatabase, SourceFile};
use fai_tests::corpus::{self, CorpusSpec};
use fai_types::check_file;

fn main() {
    divan::main();
}

/// Workspace sizes (number of leaf modules) the scaling benches sweep.
const SIZES: &[usize] = &[10, 50, 200];

/// Type-checks every file, driving resolution + inference + contracts.
fn check_all(db: &FaiDatabase, files: &[SourceFile]) {
    for &file in files {
        check_file(db, file);
    }
}

/// Cold check: a freshly built database, every file inferred from scratch.
#[divan::bench(args = SIZES)]
fn cold_check(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    bencher
        .counter(ItemsCount::new(spec.total_defs()))
        .with_inputs(|| corpus::build_db(&spec))
        .bench_values(|(db, files)| {
            check_all(&db, &files);
            db
        });
}

/// Warm incremental: edit one module's private body on an already-warmed
/// database, then re-check. Should be roughly independent of workspace size.
#[divan::bench(args = SIZES)]
fn warm_private_body_edit(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    bencher
        .counter(ItemsCount::new(spec.total_defs()))
        .with_inputs(|| {
            let (db, files) = corpus::build_db(&spec);
            check_all(&db, &files); // warm it (untimed)
            // Pre-compute the edited source so only the edit + re-check is timed.
            let edited = corpus::edit_private_body(&spec, modules / 2, 1);
            // Alternate revision each sample is unnecessary: a fresh db per sample
            // means revision 1 is always a real change relative to the warm state.
            (db, files, edited)
        })
        .bench_values(|(mut db, files, edited)| {
            let target = format!("M{}.fai", modules / 2);
            db.add_source(target.into(), edited);
            check_all(&db, &files);
            db
        });
}

/// Warm incremental, single-file latency: edit one module's private body, then
/// recompute only *that file's* diagnostics — the LSP-style "diagnostics for the
/// file you are editing". Thanks to the firewall this is ~flat across workspace
/// sizes (the other benches re-check every file, so they include an O(n) verify
/// walk on top of the O(1) recompute).
#[divan::bench(args = SIZES)]
fn warm_edit_single_file_diagnostic(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let target = format!("M{}.fai", modules / 2);
    bencher
        .with_inputs(|| {
            let (db, files) = corpus::build_db(&spec);
            check_all(&db, &files);
            let edited = corpus::edit_private_body(&spec, modules / 2, 1);
            let file =
                files.iter().copied().find(|f| f.path(&db) == &target).expect("target module");
            (db, file, edited)
        })
        .bench_values(|(mut db, file, edited)| {
            db.add_source(target.clone().into(), edited);
            check_file(&db, file);
            divan::black_box(fai_types::check_file::accumulated::<fai_db::Diag>(&db, file));
            db
        });
}

/// Warm incremental: edit a public signature in the shared `Core` module, then
/// re-check. Re-checks the dependents (grows with workspace size, but far less
/// than a cold check).
#[divan::bench(args = SIZES)]
fn warm_public_signature_edit(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    bencher
        .counter(ItemsCount::new(spec.total_defs()))
        .with_inputs(|| {
            let (db, files) = corpus::build_db(&spec);
            check_all(&db, &files);
            let edited = corpus::edit_core_signature(&spec);
            (db, files, edited)
        })
        .bench_values(|(mut db, files, edited)| {
            db.add_source("Core.fai".into(), edited);
            check_all(&db, &files);
            db
        });
}

/// Warm incremental: a trivia (comment) edit. The cross-module firewall keeps
/// this independent of workspace size even though the edited file's own bodies
/// are re-checked.
#[divan::bench(args = SIZES)]
fn warm_comment_edit(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    bencher
        .counter(ItemsCount::new(spec.total_defs()))
        .with_inputs(|| {
            let (db, files) = corpus::build_db(&spec);
            check_all(&db, &files);
            let edited = corpus::edit_comment(&spec, modules / 2, 1);
            (db, files, edited)
        })
        .bench_values(|(mut db, files, edited)| {
            let target = format!("M{}.fai", modules / 2);
            db.add_source(target.into(), edited);
            check_all(&db, &files);
            db
        });
}
