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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use fai_driver::{Session, build_native, cache_stats, reset_stats, set_cache_dir};
use indoc::indoc;

/// The cache directory override and hit/miss tallies are process-global, so the
/// tests in this binary must not run their cache sections concurrently.
static CACHE_TEST_LOCK: Mutex<()> = Mutex::new(());

const SRC: &str = indoc! {r#"
    module Main

    let double x = x + x

    public main : Runtime -> Unit
    let main runtime = runtime.console.writeLine (Int.toString (double 21))
"#};

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
    // Run the artifact `build_native` actually produced (Windows adds `.exe`).
    let artifact = outcome.artifact.expect("artifact path");
    let run = Command::new(artifact.as_std_path()).output().expect("run binary");
    assert_eq!(run.status.code(), Some(0), "binary should exit cleanly");
    String::from_utf8_lossy(&run.stdout).into_owned()
}

#[test]
fn second_cold_build_reuses_cached_objects() {
    let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// A different program (→ 99) whose `main` lives in the same module/symbol as
/// `SRC` (→ 42): a symbol-keyed cache would wrongly reuse the 42 object.
const SRC_99: &str = indoc! {r#"
    module Main

    public main : Runtime -> Unit
    let main runtime = runtime.console.writeLine (Int.toString (33 + 66))
"#};

#[test]
fn changed_source_is_not_served_a_stale_object() {
    let _guard = CACHE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let workspace = Utf8PathBuf::from_path_buf(unique_dir("stale-ws")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let cache = unique_dir("stale-cache");
    set_cache_dir(Some(cache));

    // Populate the cache with the first program (same module + `main` symbol).
    std::fs::write(workspace.join("Main.fai"), SRC).unwrap();
    assert_eq!(build_and_run(&workspace, &workspace.join("p1")), "42\n");

    // Replace it with a different body and build cold again: the content-addressed
    // key must miss the stale object and produce the new result.
    std::fs::write(workspace.join("Main.fai"), SRC_99).unwrap();
    assert_eq!(
        build_and_run(&workspace, &workspace.join("p2")),
        "99\n",
        "a changed program must not be served the previous cached object"
    );

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
