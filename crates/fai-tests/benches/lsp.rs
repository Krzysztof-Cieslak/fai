//! Per-request latency benchmarks for the editor-facing language-server
//! features: diagnostics (the edit→error loop), hover, go-to-definition,
//! completion (and its lazy `completionItem/resolve`), signature help,
//! find-references, rename (and prepare-rename), document & workspace symbols,
//! semantic tokens, inlay hints, formatting (whole-document, range, and
//! on-type), and code actions.
//!
//! Two levels are measured, because both are interesting:
//!
//! * **`analysis_*`** — the warm analysis the server runs to answer a request,
//!   called directly (`fai check`'s per-file diagnostics, the `fai-ide` query,
//!   or `fai-driver`'s formatter). This is the dominant, low-noise cost and
//!   shows how it scales with workspace size.
//! * **`roundtrip_*`** — the same feature end to end through the real `fai lsp`
//!   server over an in-memory connection: decode → overlay the unsaved buffer →
//!   compute → encode → reply. This is the client-perceived latency and includes
//!   the JSON-RPC transport and cross-thread hop on top of the analysis.
//!
//! Each feature is exercised over both the synthetic corpus (parameterized by
//! workspace size) and the hand-written multi-module store application
//! (`fai-corpus::realworld`, whose `Probe` arguments link each report row to the
//! exact source line it measured). Multi-step *scenarios* (an editing session,
//! keystroke-incremental edits, cross-module propagation, a rename refactor)
//! live in the companion `lsp_scenarios` bench.
//!
//! Local profiling only (not a CI gate; CI just compiles it).
//! Run with `cargo bench -p fai-tests --bench lsp`.

use std::cell::Cell;

use divan::Bencher;
use fai_corpus::realworld::{self, Op, Probe};
use fai_corpus::{self as corpus, CorpusSpec};
use fai_db::{DbSpanResolver, SourceFile};
use fai_types::check_file;
use serde_json::{Value, json};

// The corpus warmers, probe-position helpers, and the in-memory `fai lsp` client
// are shared with the `lsp_scenarios` bench.
mod harness;
use harness::*;

fn main() {
    divan::main();
}

// ── warm analysis over the synthetic corpus ──────────────────────────────────

/// Hover: the type at a reference on a warmed database (`fai-ide`'s `hover_at`,
/// the call behind `textDocument/hover`).
#[divan::bench(args = SIZES)]
fn analysis_hover(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::hover_at(&db, file, offset, &resolver));
}

/// Go-to-definition: resolve a reference to its definition site (`fai-ide`'s
/// `definition_at`, the call behind `textDocument/definition`).
#[divan::bench(args = SIZES)]
fn analysis_definition(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::definition_at(&db, file, offset, &resolver));
}

/// Diagnostics: edit one file's body on a warm database, then recompute *that
/// file's* diagnostics — the edit→error loop the server runs on `didChange`
/// (`fai check` over the edited file; the cross-module firewall keeps it flat).
#[divan::bench(args = SIZES)]
fn analysis_diagnostics(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let target = target_name(modules);
    bencher
        .with_inputs(|| {
            let (db, files) = warm_corpus(modules);
            let file = files.iter().copied().find(|f| f.path(&db) == target.as_str()).unwrap();
            let edited = corpus::edit_private_body(&spec, modules / 2, 1);
            (db, file, edited)
        })
        .bench_values(|(mut db, file, edited)| {
            db.add_source(target.clone().into(), edited);
            divan::black_box(fai_driver::check(&db, &[file]));
            db
        });
}

/// Completion: candidates at a reference position (`fai-ide`'s `completions_at`,
/// behind `textDocument/completion`).
#[divan::bench(args = SIZES)]
fn analysis_completion(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    bencher.bench_local(|| fai_ide::completions_at(&db, file, offset));
}

/// Signature help: the active call's signature at a position inside an
/// application (`fai-ide`'s `signature_help_at`, behind `textDocument/signatureHelp`).
#[divan::bench(args = SIZES)]
fn analysis_signature_help(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    bencher.bench_local(|| fai_ide::signature_help_at(&db, file, offset));
}

/// Find-references: every use of `Core.f0` across the workspace (`fai-ide`'s
/// `references_at`, behind `textDocument/references`). The reverse lookup scans
/// every module, so this grows with workspace size.
#[divan::bench(args = SIZES)]
fn analysis_references(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::references_at(&db, &files, file, offset, &resolver, true));
}

/// Rename: the workspace edit renaming `Core.f0` (`fai-ide`'s `rename_at`, behind
/// `textDocument/rename`) — the same cross-module reverse lookup as references.
#[divan::bench(args = SIZES)]
fn analysis_rename(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::rename_at(&db, &files, file, offset, "renamed", &resolver));
}

/// Document symbols: the outline of one module (`fai-ide`'s `document_symbols`,
/// behind `textDocument/documentSymbol`).
#[divan::bench(args = SIZES)]
fn analysis_document_symbols(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::document_symbols(&db, file, &resolver));
}

// ── warm analysis over the real-world store application ───────────────────────
//
// The same `fai-ide` queries, but probing the hand-written multi-module app
// (`fai-corpus::realworld`) instead of the synthetic corpus. Each bench is
// parameterized by a `Probe` whose `Display` is `"<path>#L<line>"`, so the
// rendered report links every row to the exact source line it measured.

#[divan::bench(args = realworld::probes(Op::Hover))]
fn analysis_hover_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::hover_at(&db, file, probe.offset, &resolver));
}

#[divan::bench(args = realworld::probes(Op::Definition))]
fn analysis_definition_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::definition_at(&db, file, probe.offset, &resolver));
}

#[divan::bench(args = realworld::probes(Op::Completion))]
fn analysis_completion_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    bencher.bench_local(|| fai_ide::completions_at(&db, file, probe.offset));
}

#[divan::bench(args = realworld::probes(Op::SignatureHelp))]
fn analysis_signature_help_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    bencher.bench_local(|| fai_ide::signature_help_at(&db, file, probe.offset));
}

#[divan::bench(args = realworld::probes(Op::References))]
fn analysis_references_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let all: Vec<SourceFile> = files.values().copied().collect();
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::references_at(&db, &all, file, probe.offset, &resolver, true));
}

#[divan::bench(args = realworld::probes(Op::Rename))]
fn analysis_rename_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let all: Vec<SourceFile> = files.values().copied().collect();
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::rename_at(&db, &all, file, probe.offset, "renamed", &resolver));
}

#[divan::bench(args = realworld::probes(Op::DocumentSymbols))]
fn analysis_document_symbols_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::document_symbols(&db, file, &resolver));
}

/// The edit → diagnostics loop on a real app file: append a trivia edit, then
/// recompute that file's diagnostics (the cross-module firewall keeps the rest
/// cached).
#[divan::bench(args = realworld::probes(Op::Diagnostics))]
fn analysis_diagnostics_real(bencher: Bencher, probe: &Probe) {
    bencher
        .with_inputs(|| {
            let (db, files) = warm_app();
            let file = app_file(&files, probe);
            (db, file, realworld::edit(probe.path, 1))
        })
        .bench_values(|(mut db, file, edited)| {
            db.add_source(probe.path.into(), edited);
            check_file(&db, file);
            divan::black_box(fai_types::check_file::accumulated::<fai_db::Diag>(&db, file));
            db
        });
}

// ── end to end through the real `fai lsp` server (synthetic corpus) ───────────

/// Hover over a reference, full round trip through the server.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_hover(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    bencher.bench_local(|| server.request("textDocument/hover", params.clone()));
    server.shutdown();
}

/// Go-to-definition on a reference, full round trip through the server.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_definition(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    bencher.bench_local(|| server.request("textDocument/definition", params.clone()));
    server.shutdown();
}

/// The edit→diagnostics loop: a `didChange` carrying an unsaved edit, timed until
/// the server publishes the file's diagnostics.
///
/// `bench_local` keeps this single-threaded — one shared connection cannot be
/// driven from divan's parallel samplers without interleaving replies. The edits
/// are pre-generated (so only the round trip is timed) and cycled, so each
/// `didChange` differs from the last and forces a real recompute.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_diagnostics(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let server = Server::start(&spec);
    let name = target_name(modules);
    let text = corpus_source(&spec, &name);
    let uri = server.did_open(&name, &text);
    server.await_diagnostics(&uri);
    let edits: Vec<String> =
        (0..16).map(|r| corpus::edit_private_body(&spec, modules / 2, r)).collect();
    let next = Cell::new(0usize);
    bencher.bench_local(|| {
        let edited = &edits[next.get() % edits.len()];
        next.set(next.get() + 1);
        server.did_change(&uri, edited);
        server.await_diagnostics(&uri);
    });
    server.shutdown();
}

/// Completion at a reference, full round trip through the server.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_completion(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    bencher.bench_local(|| server.request("textDocument/completion", params.clone()));
    server.shutdown();
}

/// Signature help at a call, full round trip through the server.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_signature_help(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    bencher.bench_local(|| server.request("textDocument/signatureHelp", params.clone()));
    server.shutdown();
}

/// Find-references on `Core.f0`, full round trip (cross-module reverse lookup).
#[divan::bench(args = RT_SIZES)]
fn roundtrip_references(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    let mut request = params.clone();
    request["context"] = json!({ "includeDeclaration": true });
    bencher.bench_local(|| server.request("textDocument/references", request.clone()));
    server.shutdown();
}

/// Rename `Core.f0`, full round trip (the workspace edit across every dependent).
#[divan::bench(args = RT_SIZES)]
fn roundtrip_rename(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    let mut request = params.clone();
    request["newName"] = json!("renamed");
    bencher.bench_local(|| server.request("textDocument/rename", request.clone()));
    server.shutdown();
}

/// Document symbols for the target module, full round trip through the server.
#[divan::bench(args = RT_SIZES)]
fn roundtrip_document_symbols(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, uri, _params) = open_target(&spec, modules);
    let request = json!({ "textDocument": { "uri": uri } });
    bencher.bench_local(|| server.request("textDocument/documentSymbol", request.clone()));
    server.shutdown();
}

// ── end to end over the real-world sample application ─────────────────────────
//
// The same round trips, but through a real server standing over the multi-module
// app written to a temp workspace. Each bench is parameterized by a `Probe`, so
// the report links every row to the source line it measured.

#[divan::bench(args = realworld::probes(Op::Hover))]
fn roundtrip_hover_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let params = app_params(probe, &uri);
    bencher.bench_local(|| server.request("textDocument/hover", params.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::Definition))]
fn roundtrip_definition_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let params = app_params(probe, &uri);
    bencher.bench_local(|| server.request("textDocument/definition", params.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::Completion))]
fn roundtrip_completion_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let params = app_params(probe, &uri);
    bencher.bench_local(|| server.request("textDocument/completion", params.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::SignatureHelp))]
fn roundtrip_signature_help_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let params = app_params(probe, &uri);
    bencher.bench_local(|| server.request("textDocument/signatureHelp", params.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::References))]
fn roundtrip_references_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let mut request = app_params(probe, &uri);
    request["context"] = json!({ "includeDeclaration": true });
    bencher.bench_local(|| server.request("textDocument/references", request.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::Rename))]
fn roundtrip_rename_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let mut request = app_params(probe, &uri);
    request["newName"] = json!("renamed");
    bencher.bench_local(|| server.request("textDocument/rename", request.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::DocumentSymbols))]
fn roundtrip_document_symbols_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let request = json!({ "textDocument": { "uri": uri } });
    bencher.bench_local(|| server.request("textDocument/documentSymbol", request.clone()));
    server.shutdown();
}

/// The edit → `publishDiagnostics` loop over a real app file, cycling trivia
/// edits so each round trip forces a genuine recompute.
#[divan::bench(args = realworld::probes(Op::Diagnostics))]
fn roundtrip_diagnostics_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let edits: Vec<String> = (0..16).map(|r| realworld::edit(probe.path, r + 1)).collect();
    let next = Cell::new(0usize);
    bencher.bench_local(|| {
        let edited = &edits[next.get() % edits.len()];
        next.set(next.get() + 1);
        server.did_change(&uri, edited);
        server.await_diagnostics(&uri);
    });
    server.shutdown();
}

// ── additional editor features ───────────────────────────────────────────────
//
// The benches above cover the classic navigation/edit set; these measure the
// rest of the language server's surface — the whole-file passes an editor fires
// on nearly every change (semantic tokens, inlay hints), workspace-wide symbol
// search, the formatter (whole-document, range, and on-type), prepare-rename,
// lazy completion-item documentation, and code actions — each warm (the direct
// `fai-ide`/`fai-driver` call) and, where it adds signal, end to end through the
// server.

// --- semantic tokens (textDocument/semanticTokens/full) ----------------------

#[divan::bench(args = SIZES)]
fn analysis_semantic_tokens(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    bencher.bench_local(|| fai_ide::semantic_tokens(&db, file));
}

#[divan::bench(args = realworld::probes(Op::SemanticTokens))]
fn analysis_semantic_tokens_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    bencher.bench_local(|| fai_ide::semantic_tokens(&db, file));
}

#[divan::bench(args = RT_SIZES)]
fn roundtrip_semantic_tokens(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, uri, _params) = open_target(&spec, modules);
    let request = json!({ "textDocument": { "uri": uri } });
    bencher.bench_local(|| server.request("textDocument/semanticTokens/full", request.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::SemanticTokens))]
fn roundtrip_semantic_tokens_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let request = json!({ "textDocument": { "uri": uri } });
    bencher.bench_local(|| server.request("textDocument/semanticTokens/full", request.clone()));
    server.shutdown();
}

// --- inlay hints (textDocument/inlayHint) ------------------------------------

#[divan::bench(args = SIZES)]
fn analysis_inlay_hints(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let end = file.text(&db).len() as u32;
    bencher.bench_local(|| fai_ide::inlay_hints(&db, file, 0, end));
}

#[divan::bench(args = realworld::probes(Op::InlayHints))]
fn analysis_inlay_hints_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let end = file.text(&db).len() as u32;
    bencher.bench_local(|| fai_ide::inlay_hints(&db, file, 0, end));
}

#[divan::bench(args = RT_SIZES)]
fn roundtrip_inlay_hints(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let text = corpus_source(&spec, &target_name(modules));
    let (server, uri, _params) = open_target(&spec, modules);
    let (line, character) = end_position(&text);
    let request = json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": line, "character": character }
        }
    });
    bencher.bench_local(|| server.request("textDocument/inlayHint", request.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::InlayHints))]
fn roundtrip_inlay_hints_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let (line, character) = end_position(&realworld::read_file(probe.path));
    let request = json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": line, "character": character }
        }
    });
    bencher.bench_local(|| server.request("textDocument/inlayHint", request.clone()));
    server.shutdown();
}

// --- workspace symbols (workspace/symbol) ------------------------------------

/// A synthetic-corpus query matching one symbol per leaf module (`g0`), so the
/// result set — and the scan — grows with workspace size.
const WS_QUERY: &str = "g0";

#[divan::bench(args = SIZES)]
fn analysis_workspace_symbols(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| {
        fai_ide::workspace_symbols(&db, &files, WS_QUERY, &resolver, fai_ide::ListOpts::default())
    });
}

#[divan::bench]
fn analysis_workspace_symbols_real(bencher: Bencher) {
    let (db, files) = warm_app();
    let all: Vec<SourceFile> = files.values().copied().collect();
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| {
        fai_ide::workspace_symbols(
            &db,
            &all,
            realworld::WORKSPACE_QUERY,
            &resolver,
            fai_ide::ListOpts::default(),
        )
    });
}

#[divan::bench(args = RT_SIZES)]
fn roundtrip_workspace_symbols(bencher: Bencher, modules: usize) {
    let server = Server::start(&CorpusSpec::with_modules(modules));
    let request = json!({ "query": WS_QUERY });
    bencher.bench_local(|| server.request("workspace/symbol", request.clone()));
    server.shutdown();
}

#[divan::bench]
fn roundtrip_workspace_symbols_real(bencher: Bencher) {
    let server = Server::start_files(app_workspace());
    let request = json!({ "query": realworld::WORKSPACE_QUERY });
    bencher.bench_local(|| server.request("workspace/symbol", request.clone()));
    server.shutdown();
}

// --- formatting (textDocument/formatting, rangeFormatting, onTypeFormatting) --

#[divan::bench(args = SIZES)]
fn analysis_formatting(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    bencher.bench_local(|| fai_driver::fmt(&db, &[file]));
}

#[divan::bench(args = realworld::probes(Op::Formatting))]
fn analysis_formatting_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    bencher.bench_local(|| fai_driver::fmt(&db, &[file]));
}

#[divan::bench(args = RT_SIZES)]
fn roundtrip_formatting(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, uri, _params) = open_target(&spec, modules);
    let request = json!({ "textDocument": { "uri": uri }, "options": fmt_options() });
    bencher.bench_local(|| server.request("textDocument/formatting", request.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::Formatting))]
fn roundtrip_formatting_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let request = json!({ "textDocument": { "uri": uri }, "options": fmt_options() });
    bencher.bench_local(|| server.request("textDocument/formatting", request.clone()));
    server.shutdown();
}

/// "Format selection" — the whole-file formatter restricted to a range (here the
/// whole document, so it exercises the full format plus the range filter).
#[divan::bench(args = realworld::probes(Op::Formatting))]
fn roundtrip_range_formatting_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let (line, character) = end_position(&realworld::read_file(probe.path));
    let request = json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": line, "character": character }
        },
        "options": fmt_options()
    });
    bencher.bench_local(|| server.request("textDocument/rangeFormatting", request.clone()));
    server.shutdown();
}

/// On-type formatting: a newline trigger reformats the construct just completed
/// (the line above the cursor).
#[divan::bench(args = realworld::probes(Op::Formatting))]
fn roundtrip_on_type_formatting_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let request = json!({
        "textDocument": { "uri": uri },
        "position": { "line": probe.line, "character": probe.character },
        "ch": "\n",
        "options": fmt_options()
    });
    bencher.bench_local(|| server.request("textDocument/onTypeFormatting", request.clone()));
    server.shutdown();
}

// --- prepare rename (textDocument/prepareRename) -----------------------------

#[divan::bench(args = SIZES)]
fn analysis_prepare_rename(bencher: Bencher, modules: usize) {
    let (db, files) = warm_corpus(modules);
    let file = target_file(&db, &files, modules);
    let offset = reference_byte(file.text(&db)) as u32;
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::prepare_rename_at(&db, file, offset, &resolver));
}

#[divan::bench(args = realworld::probes(Op::PrepareRename))]
fn analysis_prepare_rename_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| fai_ide::prepare_rename_at(&db, file, probe.offset, &resolver));
}

#[divan::bench(args = RT_SIZES)]
fn roundtrip_prepare_rename(bencher: Bencher, modules: usize) {
    let spec = CorpusSpec::with_modules(modules);
    let (server, _uri, params) = open_target(&spec, modules);
    bencher.bench_local(|| server.request("textDocument/prepareRename", params.clone()));
    server.shutdown();
}

#[divan::bench(args = realworld::probes(Op::PrepareRename))]
fn roundtrip_prepare_rename_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let params = app_params(probe, &uri);
    bencher.bench_local(|| server.request("textDocument/prepareRename", params.clone()));
    server.shutdown();
}

// --- completion item resolve (completionItem/resolve) ------------------------

/// The first completion item in `result` that carries a `data` identity (so a
/// `completionItem/resolve` on it fetches real documentation).
fn resolvable_completion_item(result: &Value) -> Option<Value> {
    let items = match result.as_array() {
        Some(array) => array.clone(),
        None => result.get("items")?.as_array()?.clone(),
    };
    items.into_iter().find(|item| item.get("data").is_some_and(|d| !d.is_null()))
}

#[divan::bench]
fn analysis_completion_resolve_real(bencher: Bencher) {
    let (db, files) = warm_app();
    let file = *files.get(realworld::COMPLETION_RESOLVE_FILE).expect("hub file loaded");
    let file_u32 = file.source(&db).index() as u32;
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| {
        fai_ide::completion_docs(&db, file_u32, realworld::COMPLETION_RESOLVE_NAME, &resolver)
    });
}

#[divan::bench(args = realworld::probes(Op::Completion))]
fn roundtrip_completion_resolve_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let params = app_params(probe, &uri);
    let completion = server.request("textDocument/completion", params);
    let item = resolvable_completion_item(&completion).expect("a resolvable completion item");
    bencher.bench_local(|| server.request("completionItem/resolve", item.clone()));
    server.shutdown();
}

// --- code actions (textDocument/codeAction) ----------------------------------

#[divan::bench(args = realworld::probes(Op::CodeAction))]
fn analysis_code_actions_real(bencher: Bencher, probe: &Probe) {
    let (db, files) = warm_app();
    let file = app_file(&files, probe);
    let all: Vec<SourceFile> = files.values().copied().collect();
    let resolver = DbSpanResolver::new(&db);
    bencher.bench_local(|| {
        fai_ide::code_actions_at(&db, &all, file, probe.offset, probe.offset, &resolver)
    });
}

#[divan::bench(args = realworld::probes(Op::CodeAction))]
fn roundtrip_code_actions_real(bencher: Bencher, probe: &Probe) {
    let (server, uri) = open_app_target(probe);
    let request = json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": probe.line, "character": probe.character },
            "end": { "line": probe.line, "character": probe.character }
        },
        "context": { "diagnostics": [] }
    });
    bencher.bench_local(|| server.request("textDocument/codeAction", request.clone()));
    server.shutdown();
}
