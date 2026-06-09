//! Latency benchmarks for the editor-facing language-server features:
//! diagnostics (the edit→error loop), hover (tooltips), and go-to-definition.
//!
//! Two levels are measured, because both are interesting:
//!
//! * **`analysis_*`** — the warm analysis the server runs to answer a request,
//!   called directly (`fai check`'s per-file diagnostics, and `fai-ide`'s
//!   `hover_at`/`definition_at`). This is the dominant, low-noise cost and shows
//!   how it scales with workspace size.
//! * **`roundtrip_*`** — the same three features end to end through the real
//!   `fai lsp` server over an in-memory connection: decode → overlay the unsaved
//!   buffer → compute → encode → reply. This is the client-perceived latency and
//!   includes the JSON-RPC transport and cross-thread hop on top of the analysis.
//!
//! Local profiling only (not a CI gate; CI just compiles it).
//! Run with `cargo bench -p fai-tests --bench lsp`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use camino::Utf8PathBuf;
use divan::Bencher;
use fai_corpus::realworld::{self, Op, Probe};
use fai_corpus::{self as corpus, CorpusSpec};
use fai_db::{DbSpanResolver, FaiDatabase, SourceFile};
use fai_types::check_file;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::Url;
use serde_json::{Value, json};

fn main() {
    divan::main();
}

/// Workspace sizes (leaf modules) for the warm-analysis benches.
const SIZES: &[usize] = &[10, 50, 200];
/// Smaller set for the end-to-end benches (each stands up a real server over a
/// workspace written to disk, so setup is heavier).
const RT_SIZES: &[usize] = &[10, 50];

/// The leaf module the benches probe (one in the middle of the corpus).
fn target_name(modules: usize) -> String {
    format!("M{}.fai", modules / 2)
}

/// The byte offset of a cross-module call (`Core.fN`) in `text` — a reference
/// whose hover has a type and whose definition lives in another module. The
/// corpus is ASCII, so the byte offset doubles as the column.
fn reference_byte(text: &str) -> usize {
    text.find("f0 x").expect("a leaf body calls Core.f0")
}

// ── warm analysis (the work the server does per request) ─────────────────────

/// A warmed in-memory corpus: every file inferred, ready to answer queries.
fn warm_corpus(modules: usize) -> (FaiDatabase, Vec<SourceFile>) {
    let (db, files) = corpus::build_db(&CorpusSpec::with_modules(modules));
    for &file in &files {
        check_file(&db, file);
    }
    (db, files)
}

fn target_file(db: &FaiDatabase, files: &[SourceFile], modules: usize) -> SourceFile {
    let name = target_name(modules);
    files.iter().copied().find(|f| f.path(db) == name.as_str()).expect("target module")
}

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

// ── warm analysis over the real-world sample application ──────────────────────
//
// The same `fai-ide` queries, but probing the hand-written multi-module app
// (`fai-corpus::realworld`) instead of the synthetic corpus. Each bench is
// parameterized by a `Probe` whose `Display` is `"<path>#L<line>"`, so the
// rendered report links every row to the exact source line it measured.

/// A warmed database holding the real-world app, plus its files by path.
fn warm_app() -> (FaiDatabase, BTreeMap<&'static str, SourceFile>) {
    let (db, files) = realworld::load_app();
    for &file in files.values() {
        check_file(&db, file);
    }
    (db, files)
}

fn app_file(files: &BTreeMap<&'static str, SourceFile>, probe: &Probe) -> SourceFile {
    *files.get(probe.path).unwrap_or_else(|| panic!("real-world fixture {} loaded", probe.path))
}

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

// ── end to end through the real `fai lsp` server ─────────────────────────────

/// A running server over a corpus on disk, plus the client end of its connection.
struct Server {
    client: Connection,
    handle: Option<JoinHandle<()>>,
    dir: Utf8PathBuf,
    next_id: Cell<i32>,
}

impl Server {
    /// Writes the corpus to a fresh temp directory, starts the server, and
    /// completes the initialize handshake.
    fn start(spec: &CorpusSpec) -> Self {
        Self::start_files(corpus::generate(spec))
    }

    /// Writes `(name, source)` files to a fresh temp directory (the workspace
    /// root), starts the server, and completes the initialize handshake.
    fn start_files(files: Vec<(String, String)>) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
            "fai-lsp-bench-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (path, source) in &files {
            std::fs::write(dir.join(path), source).unwrap();
        }
        let (server_conn, client) = Connection::memory();
        let root = dir.clone();
        let handle = std::thread::spawn(move || {
            let _ = fai_lsp::serve(&server_conn, root);
        });
        let server = Self { client, handle: Some(handle), dir, next_id: Cell::new(2) };
        server.send(Message::Request(Request::new(
            1.into(),
            "initialize".to_owned(),
            json!({ "capabilities": {} }),
        )));
        server.await_response(&1.into());
        server.notify("initialized", json!({}));
        server
    }

    fn uri(&self, name: &str) -> String {
        Url::from_file_path(self.dir.join(name).as_std_path()).unwrap().to_string()
    }

    fn send(&self, message: Message) {
        self.client.sender.send(message).unwrap();
    }

    fn recv(&self) -> Message {
        self.client.receiver.recv_timeout(Duration::from_secs(120)).expect("server response")
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(method.to_owned(), params)));
    }

    /// Sends a request and returns its result, skipping any notifications.
    fn request(&self, method: &str, params: Value) -> Value {
        let id: RequestId = self.next_id.get().into();
        self.next_id.set(self.next_id.get() + 1);
        self.send(Message::Request(Request::new(id.clone(), method.to_owned(), params)));
        self.await_response(&id)
    }

    fn await_response(&self, id: &RequestId) -> Value {
        loop {
            if let Message::Response(r) = self.recv()
                && &r.id == id
            {
                return r.result.unwrap_or(Value::Null);
            }
        }
    }

    fn did_open(&self, name: &str, text: &str) -> String {
        let uri = self.uri(name);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": "fai", "version": 1, "text": text }
            }),
        );
        uri
    }

    fn did_change(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }

    /// Waits for the next `publishDiagnostics` for `uri`.
    fn await_diagnostics(&self, uri: &str) {
        loop {
            if let Message::Notification(n) = self.recv()
                && n.method == "textDocument/publishDiagnostics"
                && n.params["uri"] == *uri
            {
                return;
            }
        }
    }

    fn shutdown(mut self) {
        let _ = self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The source the corpus generates for `name`.
fn corpus_source(spec: &CorpusSpec, name: &str) -> String {
    corpus::generate(spec)
        .into_iter()
        .find(|(path, _)| path == name)
        .map(|(_, source)| source)
        .expect("generated file")
}

/// The 0-based `(line, character)` of `needle` in `text` (ASCII corpus).
fn position(text: &str, needle: &str) -> (u32, u32) {
    let byte = text.find(needle).expect("needle present");
    let line = text[..byte].matches('\n').count() as u32;
    let line_start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    (line, (byte - line_start) as u32)
}

/// An open target document with a hover/definition position, on a warm server.
fn open_target(spec: &CorpusSpec, modules: usize) -> (Server, String, Value) {
    let server = Server::start(spec);
    let name = target_name(modules);
    let text = corpus_source(spec, &name);
    let uri = server.did_open(&name, &text);
    server.await_diagnostics(&uri); // the file is analyzed on open (warm)
    let (line, character) = position(&text, "f0 x");
    let params = json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    });
    (server, uri, params)
}

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

/// The app's `(basename, source)` files for a temp workspace (the server keys
/// modules by their `module` declaration, so the workspace path is the basename).
fn app_workspace() -> Vec<(String, String)> {
    realworld::app_sources()
        .into_iter()
        .map(|(path, source)| (path.strip_prefix("samples/").unwrap_or(path).to_owned(), source))
        .collect()
}

/// Stands a server over the app and opens the probe's file (analyzed on open).
fn open_app_target(probe: &Probe) -> (Server, String) {
    let files = app_workspace();
    let basename = probe.path.strip_prefix("samples/").unwrap_or(probe.path).to_owned();
    let text = files.iter().find(|(p, _)| *p == basename).map(|(_, s)| s.clone()).unwrap();
    let server = Server::start_files(files);
    let uri = server.did_open(&basename, &text);
    server.await_diagnostics(&uri);
    (server, uri)
}

/// A position request payload at the probe.
fn app_params(probe: &Probe, uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": probe.line, "character": probe.character }
    })
}

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
