//! The `edit → test` loop: `fai test` (collect contracts → synthesize harness →
//! reference-count → JIT-compile → run) over the synthetic corpus, in-process
//! (`fai_driver::test`, the same path `tests/contracts.rs` exercises).
//!
//! This is the wall-clock analogue of `inference.rs`'s edit→diagnostic loop for
//! the contract pipeline. The front end is incremental (a warm edit re-lowers
//! only the edited module), but synthesis + JIT + run are per-invocation, so the
//! warm benches measure the realistic cost of re-running a module's tests after a
//! change. The supervised, subprocess path a user actually drives is benchmarked
//! end-to-end in `fai-cli`'s `test_loop` bench. Local profiling only (not a CI
//! gate). Run with `cargo bench -p fai-tests --bench contracts`.

use divan::Bencher;
use fai_corpus::{self as corpus, CorpusSpec};
use fai_db::{FaiDatabase, SourceFile};
use fai_driver::{TestConfig, test};
use fai_types::check_file;

fn main() {
    divan::main();
}

/// Workspace sizes (leaf modules) the warm benches sweep.
const SIZES: &[usize] = &[10, 50, 200];
/// Smaller sizes for the cold baseline (each sample rebuilds + recompiles every
/// module's contracts from scratch).
const COLD_SIZES: &[usize] = &[10, 50];

/// Fewer trials than the `fai test` default (100): the benches measure the
/// incremental compile + dispatch loop, not random-generation throughput.
fn bench_config() -> TestConfig {
    TestConfig { trials: 16, ..TestConfig::default() }
}

/// Type-checks every file so the front end is warm before an edit is timed.
fn warm(db: &FaiDatabase, files: &[SourceFile]) {
    for &file in files {
        check_file(db, file);
    }
}

/// The leaf module the warm benches edit (one in the middle of the corpus).
fn target_name(modules: usize) -> String {
    format!("M{}.fai", modules / 2)
}

fn target_file(db: &FaiDatabase, files: &[SourceFile], modules: usize) -> SourceFile {
    let name = target_name(modules);
    files.iter().copied().find(|f| f.path(db) == name.as_str()).expect("target module")
}

/// Cold: a fresh database, every module's contracts collected, synthesized,
/// compiled, and run from scratch.
#[divan::bench(args = COLD_SIZES)]
fn cold_test(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules_and_contracts(modules);
    bencher.with_inputs(|| corpus::build_db(&spec)).bench_values(|(db, files)| {
        divan::black_box(test(&db, &files, None, bench_config()));
        db
    });
}

/// Warm, focused: edit one module's public body on a warmed database, then
/// re-run only *that module's* contracts — the "I changed this code, re-test it"
/// loop. ~Flat across workspace size (the cross-module firewall).
#[divan::bench(args = SIZES)]
fn warm_edit_test_one_module(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules_and_contracts(modules);
    let target = target_name(modules);
    bencher
        .with_inputs(|| {
            let (db, files) = corpus::build_db(&spec);
            warm(&db, &files);
            let file = target_file(&db, &files, modules);
            let edited = corpus::edit_public_body(&spec, modules / 2, 1);
            (db, file, edited)
        })
        .bench_values(|(mut db, file, edited)| {
            db.add_source(target.clone().into(), edited);
            divan::black_box(test(&db, &[file], None, bench_config()));
            db
        });
}

/// Warm, whole-workspace: the same edit, but re-run *every* module's contracts —
/// the "re-test everything after one edit" loop, which grows with workspace size
/// (the whole suite is JIT-compiled and run each time).
#[divan::bench(args = SIZES)]
fn warm_edit_test_all(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules_and_contracts(modules);
    let target = target_name(modules);
    bencher
        .with_inputs(|| {
            let (db, files) = corpus::build_db(&spec);
            warm(&db, &files);
            let edited = corpus::edit_public_body(&spec, modules / 2, 1);
            (db, files, edited)
        })
        .bench_values(|(mut db, files, edited)| {
            db.add_source(target.clone().into(), edited);
            divan::black_box(test(&db, &files, None, bench_config()));
            db
        });
}
