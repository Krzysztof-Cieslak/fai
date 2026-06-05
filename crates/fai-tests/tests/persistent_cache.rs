//! The on-disk object cache: a second cold build reuses the first build's
//! objects instead of regenerating them.
//!
//! Each build runs against a *fresh* database (simulating a cold process), so the
//! in-memory salsa cache cannot carry results between them — only the persistent
//! disk cache can. We point the cache at a temp directory via the process-global
//! override (no env mutation, which is `unsafe` under edition 2024) and read the
//! hit/miss tallies. This file is its own test binary, so the override and the
//! global tallies are isolated from other tests.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use fai_driver::{Session, build_native, cache_stats, reset_stats, set_cache_dir};

const SRC: &str = "module Main\n\nlet double x = x + x\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (double 21))\n";

fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-pcache-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Builds `Main.fai` against a fresh database and runs the produced binary,
/// returning its stdout.
fn build_and_run(root: &Utf8Path, out: &Utf8Path) -> String {
    let session = Session::open(root.to_owned()).expect("open session");
    let files = session.select_files(Some(Utf8Path::new("Main.fai")));
    let entry = *files.first().expect("entry file");
    let outcome = build_native(session.db(), entry, out);
    assert!(outcome.ok, "build failed: {:?}", outcome.diagnostics);
    let run = Command::new(out.as_std_path()).output().expect("run binary");
    assert_eq!(run.status.code(), Some(0), "binary should exit cleanly");
    String::from_utf8_lossy(&run.stdout).into_owned()
}

#[test]
fn second_cold_build_reuses_cached_objects() {
    let workspace = Utf8PathBuf::from_path_buf(unique_dir("ws")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("Main.fai"), SRC).unwrap();

    let cache = unique_dir("cache");
    set_cache_dir(Some(cache.clone()));

    // First cold build: every object is generated and written (disk misses).
    reset_stats();
    let out1 = workspace.join("prog1");
    assert_eq!(build_and_run(&workspace, &out1), "42\n");
    let (_, misses1) = cache_stats();
    assert!(misses1 > 0, "first build should populate the cache (misses), got {misses1}");

    // The cache directory now holds at least one object file.
    let object_count = walk_objects(&cache);
    assert!(object_count > 0, "expected cached .o files under {cache:?}");

    // Second cold build (fresh database): objects come from disk (hits, no
    // regeneration), and the program still runs correctly.
    reset_stats();
    let out2 = workspace.join("prog2");
    assert_eq!(build_and_run(&workspace, &out2), "42\n");
    let (hits2, misses2) = cache_stats();
    assert!(hits2 > 0, "second build should hit the disk cache, got {hits2} hits");
    assert_eq!(misses2, 0, "second build should regenerate nothing, got {misses2} misses");

    set_cache_dir(None);
}

/// Counts `.o` files under `dir` (recursively).
fn walk_objects(dir: &std::path::Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += walk_objects(&path);
        } else if path.extension().is_some_and(|e| e == "o") {
            count += 1;
        }
    }
    count
}
