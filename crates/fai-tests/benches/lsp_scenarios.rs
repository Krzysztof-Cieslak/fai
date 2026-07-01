//! Multi-step, real-world *scenario* benchmarks for the `fai lsp` server.
//!
//! Where the companion `lsp` bench measures each request in isolation on an
//! already-warm server, these replay the interleaved traffic an editor actually
//! produces, so they capture costs the single-shot benches cannot:
//!
//! * **`editing_session_real`** — typing a new binding into an app file with
//!   keystroke-level *incremental* range edits, firing completion after each
//!   chunk (and signature help + hover at existing calls), then deleting it back
//!   to the baseline. Exercises the incremental-sync path and the request mix a
//!   developer generates while typing.
//! * **`keystroke_diagnostics[_real]`** — the type-a-character → `publishDiagnostics`
//!   loop driven by *range* edits (`textDocument/didChange` with a range), the
//!   realistic keystroke path the full-text diagnostics bench does not cover.
//! * **`analysis_propagation` / `roundtrip_propagation`** — a breaking change to a
//!   hub module's public signature with every dependent open, timed until all of
//!   them re-diagnose. The cross-module fan-out (contrast the firewalled
//!   private-body edit the `lsp` diagnostics benches measure, which is flat).
//! * **`refactor_rename[_real]`** — the `prepareRename` → `rename` a client
//!   performs to refactor a workspace-wide symbol.
//! * **`quickfix_real`** — a typo (an unbound name) → the quick fix that
//!   qualifies it: `didChange` → diagnostics → `codeAction`.
//!
//! Local profiling only (not a CI gate; CI just compiles it).
//! Run with `cargo bench -p fai-tests --bench lsp_scenarios`.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use divan::Bencher;
use fai_corpus::realworld::{self, Op, Probe};
use fai_corpus::{self as corpus, CorpusSpec};
use serde_json::json;

// The corpus warmers, probe-position helpers, and the in-memory `fai lsp` client
// are shared with the `lsp` bench.
mod harness;
use harness::*;

fn main() {
    divan::main();
}

// ── editing session ──────────────────────────────────────────────────────────

/// Type a new binding into an app file, keystroke by keystroke (incremental range
/// edits), firing completion after each chunk plus a signature-help and a hover
/// at existing call sites, then delete it back to the baseline. Times the whole
/// session — the sync + request traffic an editor produces while a developer
/// types — repeatably (each iteration ends where it began).
#[divan::bench(args = realworld::probes(Op::Completion))]
fn editing_session_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let text = realworld::read_file(probe.path);
    let base = end_position(&text);

    // Stable existing call sites (all in the same file as the completion probe).
    let hover = &realworld::probes(Op::Hover)[0];
    let sig = &realworld::probes(Op::SignatureHelp)[0];
    let hover_params = json!({ "textDocument": { "uri": uri }, "position": { "line": hover.line, "character": hover.character } });
    let sig_params = json!({ "textDocument": { "uri": uri }, "position": { "line": sig.line, "character": sig.character } });

    // Keystroke groups appended at end of file (a private scratch binding).
    let chunks = ["\nlet scratch =", " Catalog.", "label", " sample"];
    bencher.bench_local(|| {
        let mut cur = base;
        for chunk in chunks {
            server.did_change_range(&uri, cur, cur, chunk);
            cur = advance(cur, chunk);
            let at =
                json!({ "textDocument": { "uri": uri }, "position": { "line": cur.0, "character": cur.1 } });
            let _ = server.request("textDocument/completion", at);
        }
        let _ = server.request("textDocument/signatureHelp", sig_params.clone());
        let _ = server.request("textDocument/hover", hover_params.clone());
        // Reset: delete the inserted text so the next iteration starts fresh.
        server.did_change_range(&uri, base, cur, "");
    });
    server.shutdown();
}

// ── keystroke → diagnostics loop (incremental range sync) ────────────────────

/// The type-a-character → `publishDiagnostics` loop over the synthetic corpus,
/// driven by *range* edits (insert then delete a line at end of file), so it
/// exercises the server's incremental-sync path rather than full-text replacement.
#[divan::bench(args = RT_SIZES)]
fn keystroke_diagnostics(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let name = target_name(modules);
    let text = corpus_source(&spec, &name);
    let server = Server::start(&spec);
    let uri = server.did_open(&name, &text);
    server.await_diagnostics(&uri);
    let base = end_position(&text);
    let after = (base.0 + 1, 0);
    bencher.bench_local(|| {
        server.did_change_range(&uri, base, base, "// k\n");
        server.await_diagnostics(&uri);
        server.did_change_range(&uri, base, after, "");
        server.await_diagnostics(&uri);
    });
    server.shutdown();
}

/// The same keystroke → diagnostics loop over a real app file.
#[divan::bench(args = realworld::probes(Op::Diagnostics))]
fn keystroke_diagnostics_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let base = end_position(&realworld::read_file(probe.path));
    let after = (base.0 + 1, 0);
    bencher.bench_local(|| {
        server.did_change_range(&uri, base, base, "// k\n");
        server.await_diagnostics(&uri);
        server.did_change_range(&uri, base, after, "");
        server.await_diagnostics(&uri);
    });
    server.shutdown();
}

// ── cross-module propagation ─────────────────────────────────────────────────

/// Warm re-check fan-out: change the shared `Core` module's public signature on a
/// fully-inferred workspace, then re-check *every* file. Unlike the firewalled
/// private-body edit (flat with workspace size), this invalidates every dependent,
/// so its cost grows with the workspace — the fan-out the firewall usually avoids.
#[divan::bench(args = SIZES)]
fn analysis_propagation(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    bencher
        .with_inputs(|| {
            let (db, files) = warm_corpus(modules);
            (db, files, corpus::edit_core_signature(&spec))
        })
        .bench_values(|(mut db, files, broken)| {
            db.add_source("Core.fai".into(), broken);
            divan::black_box(fai_driver::check(&db, &files));
            db
        });
}

/// End-to-end propagation: with `Core` and every dependent leaf open, toggle
/// `Core`'s public signature and time until *all* open dependents have
/// re-published diagnostics — the client-perceived cost of a breaking change to a
/// widely-used API.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_propagation(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let generated: BTreeMap<String, String> = corpus::generate(&spec).into_iter().collect();
    let server = Server::start(&spec);

    // Open Core plus every dependent leaf module (only open files are diagnosed).
    let mut uris = HashSet::new();
    let core_uri = server.did_open("Core.fai", &generated["Core.fai"]);
    uris.insert(core_uri.clone());
    for i in 0..modules {
        let name = format!("M{i}.fai");
        uris.insert(server.did_open(&name, &generated[&name]));
    }
    server.drain(); // swallow the flurry of on-open diagnostics

    let original = generated["Core.fai"].clone();
    let broken = corpus::edit_core_signature(&spec);
    let toggle = Cell::new(false);
    bencher.bench_local(|| {
        // Alternate the signature so every edit forces a real re-propagation.
        let src = if toggle.get() { &original } else { &broken };
        toggle.set(!toggle.get());
        server.did_change(&core_uri, src);
        server.await_diagnostics_for_all(&uris);
    });
    server.shutdown();
}

// ── rename refactor ──────────────────────────────────────────────────────────

/// The `prepareRename` → `rename` a client performs to rename a workspace-wide
/// symbol (`Core.f0`, referenced by every leaf) on the synthetic corpus.
#[divan::bench(args = RT_SIZES)]
fn refactor_rename(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let name = target_name(modules);
    let text = corpus_source(&spec, &name);
    let server = Server::start(&spec);
    let uri = server.did_open(&name, &text);
    server.await_diagnostics(&uri);
    let (line, character) = position(&text, "f0 x");
    let pos = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
    bencher.bench_local(|| {
        let _ = server.request("textDocument/prepareRename", pos.clone());
        let mut rename = pos.clone();
        rename["newName"] = json!("renamed");
        let _ = server.request("textDocument/rename", rename);
    });
    server.shutdown();
}

/// The same prepare→rename refactor over the app's hot symbol (`Catalog.label`,
/// referenced across the store), so the workspace edit spans several files.
#[divan::bench(args = realworld::probes(Op::Rename))]
fn refactor_rename_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let pos = app_params(probe, &uri);
    bencher.bench_local(|| {
        let _ = server.request("textDocument/prepareRename", pos.clone());
        let mut rename = pos.clone();
        rename["newName"] = json!("renamed");
        let _ = server.request("textDocument/rename", rename);
    });
    server.shutdown();
}

// ── quick fix (typo → qualify) ───────────────────────────────────────────────

/// A typo → quick-fix cycle: type an unbound name (`label` instead of
/// `Catalog.label`), let the server recompute diagnostics, then request the
/// "qualify as `Catalog.label`" code action — and undo. Measures the produce-a-fix
/// path (the `lsp` code-action bench measures the common no-fix-available case).
#[divan::bench]
fn quickfix_real(bencher: Bencher) {
    let baseline = realworld::read_file(realworld::CODE_ACTION_FILE);
    let (unbound, line, character) = realworld::edit_unbound_name();
    let basename = realworld::CODE_ACTION_FILE
        .strip_prefix("samples/store/")
        .unwrap_or(realworld::CODE_ACTION_FILE)
        .to_owned();
    let server = Server::start_files(app_workspace());
    let uri = server.did_open(&basename, &baseline);
    server.await_diagnostics(&uri);
    let code_action = json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character + 5 }
        },
        "context": { "diagnostics": [] }
    });
    bencher.bench_local(|| {
        server.did_change(&uri, &unbound);
        server.await_diagnostics(&uri);
        let _ = server.request("textDocument/codeAction", code_action.clone());
        server.did_change(&uri, &baseline);
        server.await_diagnostics(&uri);
    });
    server.shutdown();
}
