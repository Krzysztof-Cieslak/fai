//! Daemon-path benchmarks: the content-addressed cache key, the run-bundle
//! serialization hop, the wire framing, and the workspace file-state sync. Local
//! profiling only (not a CI gate; CI just compiles it).
//!
//! Run with `cargo bench -p fai-tests --bench daemon`.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use divan::{Bencher, black_box};
use fai_core::{fingerprint_def, from_wire};
use fai_corpus::{self as corpus, CorpusSpec};
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_driver::{Rendered, Session, WireBundle, build_run_bundle};
use fai_rc::rc;
use fai_resolve::DefId;
use fai_server::protocol::{Response, ServerMessage, read_frame, write_frame};
use fai_syntax::Symbol;

fn main() {
    divan::main();
}

/// A small program (a single arithmetic `main`).
const SMALL: &str = "module M\n\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (1 + 2 * 3))\n";

/// A medium program: a helper chain plus higher-order use.
const MEDIUM: &str = "module M\n\nlet inc x = x + 1\n\nlet double x = x + x\n\nlet apply f x = f x\n\nlet step x = double (inc x)\n\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (apply step (step 10)))\n";

/// A fresh database holding `src` (and the prelude), warmed through inference.
fn fresh(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn namer(def: DefId) -> String {
    format!("fai_M_{}", def.name)
}

// ── the content-addressed cache key ──────────────────────────────────────────

#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn fingerprint(bencher: Bencher, program: (&str, &str)) {
    let (db, file) = fresh(program.1);
    let lowered = rc(&db, file, Symbol::intern("main"));
    bencher.bench(|| {
        black_box(fingerprint_def(&lowered, &namer, &|_| 1, &|_| fai_core::ir::FnAbi::default()))
    });
}

// ── the run-bundle path (warm front end → wire → JSON → reconstruct) ──────────

/// Build the portable bundle from a warm database (front end memoized; measures
/// reachability + wire conversion).
#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn build_bundle_warm(bencher: Bencher, program: (&str, &str)) {
    let (db, file) = fresh(program.1);
    let _ = build_run_bundle(&db, file); // warm
    // `FaiDatabase` isn't `Sync`; `bench_local` keeps the run single-threaded.
    bencher.bench_local(|| black_box(build_run_bundle(&db, file)));
}

/// The serialize → deserialize → reconstruct hop the worker pays for the bundle.
#[divan::bench(args = [("small", SMALL), ("medium", MEDIUM)])]
fn bundle_json_round_trip(bencher: Bencher, program: (&str, &str)) {
    let (db, file) = fresh(program.1);
    let bundle = build_run_bundle(&db, file).bundle.expect("clean program");
    bencher.bench(|| {
        let json = serde_json::to_vec(&bundle).unwrap();
        let decoded: WireBundle = serde_json::from_slice(&json).unwrap();
        black_box(from_wire(&decoded))
    });
}

// ── the wire framing ─────────────────────────────────────────────────────────

fn sample_message() -> ServerMessage {
    ServerMessage::Result(Response::Command(Rendered {
        stdout: "ok: 0 errors\n".repeat(40),
        stderr: String::new(),
        exit: 0,
    }))
}

#[divan::bench]
fn frame_encode(bencher: Bencher) {
    let message = sample_message();
    bencher.bench(|| {
        let mut buf = Vec::new();
        write_frame(&mut buf, &message).unwrap();
        black_box(buf)
    });
}

#[divan::bench]
fn frame_decode(bencher: Bencher) {
    let mut buf = Vec::new();
    write_frame(&mut buf, &sample_message()).unwrap();
    bencher.bench(|| {
        let mut cursor = Cursor::new(&buf);
        let message: ServerMessage = read_frame(&mut cursor).unwrap();
        black_box(message)
    });
}

// ── workspace file-state sync ────────────────────────────────────────────────

/// Writes a generated corpus to a fresh temp directory and returns its path.
fn corpus_on_disk(spec: &CorpusSpec) -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
        "fai-bench-ws-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    for (path, source) in corpus::generate(spec) {
        std::fs::write(dir.join(path), source).unwrap();
    }
    dir
}

/// Cold open: scan + read + seed the database for a workspace (daemon startup).
#[divan::bench(args = [10, 50])]
fn session_open_cold(bencher: Bencher, modules: usize) {
    let dir = corpus_on_disk(&CorpusSpec::with_modules(modules));
    bencher.with_inputs(|| dir.clone()).bench_values(|dir| black_box(Session::open(dir).unwrap()));
}

/// Warm resync with no changes: the stat-gated scan the daemon runs per request.
#[divan::bench(args = [10, 50])]
fn session_resync_unchanged(bencher: Bencher, modules: usize) {
    let dir = corpus_on_disk(&CorpusSpec::with_modules(modules));
    let mut session = Session::open(dir).unwrap();
    bencher.bench_local(move || black_box(session.sync_from_disk()).unwrap());
}
