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
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use camino::Utf8PathBuf;
use divan::Bencher;
use fai_db::{DbSpanResolver, FaiDatabase, SourceFile};
use fai_tests::corpus::{self, CorpusSpec};
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
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
            "fai-lsp-bench-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (path, source) in corpus::generate(spec) {
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
