//! The supervised `edit → fai test` loop, end to end through the real `fai`
//! binary and its per-workspace daemon — the latency a user actually feels:
//! client → warm daemon → build test plan → spawn worker subprocess(es) → JIT →
//! run → stream results back.
//!
//! The in-process compile+run cost is benchmarked separately (and at larger
//! sizes) in `fai-tests`' `contracts` bench; this one adds the daemon round trip
//! and the worker subprocess + IPC that only the real binary exercises. Local
//! profiling only (not a CI gate). Run with `cargo bench -p fai-cli --bench
//! test_loop`.
//!
//! Not run on Windows for the same reason as the daemon tests: the spawned
//! daemon inherits the client's captured stdio and would block until its idle
//! timeout. The bench still compiles there (CI builds every target); only the
//! run is skipped — and the Benchmarks workflow runs on Linux regardless.

use std::cell::Cell;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use divan::Bencher;
use fai_corpus::{self as corpus, CorpusSpec};

fn main() {
    // The daemon would block on Windows (it inherits the client's captured
    // stdio), so the benches are not run there; on every other platform this
    // runs the full divan suite.
    #[cfg(not(windows))]
    divan::main();
}

/// Workspace sizes (leaf modules) the warm benches sweep.
const SIZES: &[usize] = &[10, 50, 200];
/// Smaller sizes for the cold baseline (each sample auto-spawns a fresh daemon
/// and checks + tests the whole workspace from scratch).
const COLD_SIZES: &[usize] = &[10, 50];

/// A short base directory for the daemon's Unix socket: macOS caps socket paths
/// at ~104 bytes and its default temp dir is long enough to overflow once the
/// per-workspace socket file is appended.
fn unique_socket_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    base.join(format!("fai-rtb-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)))
}

fn unique(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-bench-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// An isolated on-disk workspace with its own daemon endpoint and cache. The
/// daemon auto-spawns on the first command and is stopped on drop.
struct Workspace {
    dir: PathBuf,
    runtime_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Workspace {
    /// Writes the generated corpus to a fresh workspace (no daemon yet).
    fn new(spec: &CorpusSpec) -> Self {
        let dir = unique("ws");
        std::fs::create_dir_all(&dir).unwrap();
        for (path, source) in corpus::generate(spec) {
            std::fs::write(dir.join(path), source).unwrap();
        }
        Self { dir, runtime_dir: unique_socket_dir(), cache_dir: unique("cache") }
    }

    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .env("FAI_RUNTIME_DIR", &self.runtime_dir)
            .env("FAI_CACHE_DIR", &self.cache_dir)
            .env("FAI_DAEMON_IDLE_TIMEOUT", "120");
        command
    }

    /// Runs `fai <args> -C <workspace>` and returns its output.
    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).arg("-C").arg(&self.dir).output().unwrap()
    }

    /// Overwrites a module's source on disk (the edit the daemon resyncs).
    fn write(&self, name: &str, source: &str) {
        std::fs::write(self.dir.join(name), source).unwrap();
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.cache_dir);
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Starts the daemon and warms it through both `check` and `test`.
fn warmed(spec: &CorpusSpec) -> Workspace {
    let ws = Workspace::new(spec);
    let _ = ws.run(&["check"]); // auto-spawns the daemon and warms the front end
    let _ = ws.run(&["test", "--count", "16", "--seed", "0"]);
    ws
}

/// Sixteen pre-generated value-preserving edits to the middle module's public
/// body, cycled so each timed run is a genuine change the daemon must resync.
fn edits(spec: &CorpusSpec, modules: usize) -> Vec<String> {
    (0..16).map(|r| corpus::edit_public_body(spec, modules / 2, r + 1)).collect()
}

/// Cold: a fresh workspace each sample — the first `fai test` auto-spawns the
/// daemon and checks + tests everything from scratch.
#[divan::bench(args = COLD_SIZES)]
fn cold_test(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules_and_contracts(modules);
    bencher.with_inputs(|| Workspace::new(&spec)).bench_values(|ws| {
        divan::black_box(ws.run(&["test", "--count", "16", "--seed", "0"]));
        ws // dropped after timing → daemon stopped, dirs removed
    });
}

/// Warm, focused: edit the middle module's public body, then re-run only that
/// module's contracts through the warm daemon (`fai test M{n/2}.fai`).
#[divan::bench(args = SIZES)]
fn warm_edit_test_one_module(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules_and_contracts(modules);
    let ws = warmed(&spec);
    let target = format!("M{}.fai", modules / 2);
    let edits = edits(&spec, modules);
    let next = Cell::new(0usize);
    bencher.bench_local(|| {
        let edited = &edits[next.get() % edits.len()];
        next.set(next.get() + 1);
        ws.write(&target, edited);
        divan::black_box(ws.run(&["test", &target, "--count", "16", "--seed", "0"]))
    });
}

/// Warm, whole-workspace: the same edit, but re-run every module's contracts
/// (`fai test`) through the warm daemon.
#[divan::bench(args = SIZES)]
fn warm_edit_test_all(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules_and_contracts(modules);
    let ws = warmed(&spec);
    let target = format!("M{}.fai", modules / 2);
    let edits = edits(&spec, modules);
    let next = Cell::new(0usize);
    bencher.bench_local(|| {
        let edited = &edits[next.get() % edits.len()];
        next.set(next.get() + 1);
        ws.write(&target, edited);
        divan::black_box(ws.run(&["test", "--count", "16", "--seed", "0"]))
    });
}
