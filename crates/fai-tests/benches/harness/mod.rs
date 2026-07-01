//! Shared scaffolding for the language-server benchmarks (`lsp` and
//! `lsp_scenarios`): the corpus warmers, the probe-position helpers, and a thin
//! client that drives the real `fai lsp` server over an in-memory connection.
//!
//! It is `#[path]`-included by both bench binaries (via `mod harness;`), so an
//! item unused by one of them is expected — hence the module-wide `dead_code`
//! allowance.
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use camino::Utf8PathBuf;
use fai_corpus::realworld::{self, Probe};
use fai_corpus::{self as corpus, CorpusSpec};
use fai_db::{FaiDatabase, SourceFile};
use fai_types::check_file;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::Url;
use serde_json::{Value, json};

/// Workspace sizes (leaf modules) for the warm-analysis benches.
pub const SIZES: &[usize] = &[10, 50, 200];
/// Smaller set for the end-to-end benches (each stands up a real server over a
/// workspace written to disk, so setup is heavier).
pub const RT_SIZES: &[usize] = &[10, 50];

// ── synthetic corpus ─────────────────────────────────────────────────────────

/// The leaf module the benches probe (one in the middle of the corpus).
pub fn target_name(modules: usize) -> String {
    format!("M{}.fai", modules / 2)
}

/// The byte offset of a cross-module call (`Core.fN`) in `text` — a reference
/// whose hover has a type and whose definition lives in another module. The
/// corpus is ASCII, so the byte offset doubles as the column.
pub fn reference_byte(text: &str) -> usize {
    text.find("f0 x").expect("a leaf body calls Core.f0")
}

/// A warmed in-memory corpus: every file inferred, ready to answer queries.
pub fn warm_corpus(modules: usize) -> (FaiDatabase, Vec<SourceFile>) {
    let (db, files) = corpus::build_db(&CorpusSpec::with_modules(modules));
    for &file in &files {
        check_file(&db, file);
    }
    (db, files)
}

pub fn target_file(db: &FaiDatabase, files: &[SourceFile], modules: usize) -> SourceFile {
    let name = target_name(modules);
    files.iter().copied().find(|f| f.path(db) == name.as_str()).expect("target module")
}

/// The source the corpus generates for `name`.
pub fn corpus_source(spec: &CorpusSpec, name: &str) -> String {
    corpus::generate(spec)
        .into_iter()
        .find(|(path, _)| path == name)
        .map(|(_, source)| source)
        .expect("generated file")
}

// ── real-world store application ─────────────────────────────────────────────

/// A warmed database holding the real-world app, plus its files by path.
pub fn warm_app() -> (FaiDatabase, BTreeMap<&'static str, SourceFile>) {
    let (db, files) = realworld::load_app();
    for &file in files.values() {
        check_file(&db, file);
    }
    (db, files)
}

pub fn app_file(files: &BTreeMap<&'static str, SourceFile>, probe: &Probe) -> SourceFile {
    *files.get(probe.path).unwrap_or_else(|| panic!("real-world fixture {} loaded", probe.path))
}

/// The app's `(basename, source)` files for a temp workspace (the server keys
/// modules by their `module` declaration, so the workspace path is the basename).
pub fn app_workspace() -> Vec<(String, String)> {
    realworld::app_sources()
        .into_iter()
        .map(|(path, source)| {
            (path.strip_prefix("samples/store/").unwrap_or(path).to_owned(), source)
        })
        .collect()
}

// ── position helpers ─────────────────────────────────────────────────────────

/// The 0-based `(line, character)` of `needle` in `text` (ASCII corpus).
pub fn position(text: &str, needle: &str) -> (u32, u32) {
    let byte = text.find(needle).expect("needle present");
    let line = text[..byte].matches('\n').count() as u32;
    let line_start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    (line, (byte - line_start) as u32)
}

/// The 0-based `(line, character)` of the end of `text` (for whole-document
/// ranges and end-of-file edits).
pub fn end_position(text: &str) -> (u32, u32) {
    let line = text.matches('\n').count() as u32;
    let column = text.rsplit('\n').next().unwrap_or("").chars().count() as u32;
    (line, column)
}

/// Advances a `(line, character)` cursor past `chunk` (used to track the caret
/// while replaying keystroke inserts).
pub fn advance(cursor: (u32, u32), chunk: &str) -> (u32, u32) {
    let newlines = chunk.matches('\n').count() as u32;
    if newlines == 0 {
        (cursor.0, cursor.1 + chunk.chars().count() as u32)
    } else {
        let last = chunk.rsplit('\n').next().unwrap_or("");
        (cursor.0 + newlines, last.chars().count() as u32)
    }
}

/// LSP formatting options (2-space indent, matching `fai fmt`).
pub fn fmt_options() -> Value {
    json!({ "tabSize": 2, "insertSpaces": true })
}

// ── the client harness ───────────────────────────────────────────────────────

/// A running server over a corpus on disk, plus the client end of its connection.
pub struct Server {
    client: Connection,
    handle: Option<JoinHandle<()>>,
    dir: Utf8PathBuf,
    next_id: Cell<i32>,
}

impl Server {
    /// Writes the corpus to a fresh temp directory, starts the server, and
    /// completes the initialize handshake.
    pub fn start(spec: &CorpusSpec) -> Self {
        Self::start_files(corpus::generate(spec))
    }

    /// Writes `(name, source)` files to a fresh temp directory (the workspace
    /// root), starts the server, and completes the initialize handshake.
    pub fn start_files(files: Vec<(String, String)>) -> Self {
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

    pub fn uri(&self, name: &str) -> String {
        Url::from_file_path(self.dir.join(name).as_std_path()).unwrap().to_string()
    }

    fn send(&self, message: Message) {
        self.client.sender.send(message).unwrap();
    }

    fn recv(&self) -> Message {
        self.client.receiver.recv_timeout(Duration::from_secs(120)).expect("server response")
    }

    /// Consumes any buffered server messages, returning once the stream has been
    /// quiet for a short window (used to swallow the flurry of on-open
    /// diagnostics before a timed loop begins).
    pub fn drain(&self) {
        while self.client.receiver.recv_timeout(Duration::from_millis(50)).is_ok() {}
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(method.to_owned(), params)));
    }

    /// Sends a request and returns its result, skipping any notifications.
    pub fn request(&self, method: &str, params: Value) -> Value {
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

    pub fn did_open(&self, name: &str, text: &str) -> String {
        let uri = self.uri(name);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": "fai", "version": 1, "text": text }
            }),
        );
        uri
    }

    /// A whole-document `didChange` (the full-text sync path).
    pub fn did_change(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }

    /// An incremental `didChange` replacing `[start, end)` with `text` — the
    /// keystroke-level range-edit sync path.
    pub fn did_change_range(&self, uri: &str, start: (u32, u32), end: (u32, u32), text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ {
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 }
                    },
                    "text": text
                } ]
            }),
        );
    }

    /// Waits for the next `publishDiagnostics` for `uri`.
    pub fn await_diagnostics(&self, uri: &str) {
        loop {
            if let Message::Notification(n) = self.recv()
                && n.method == "textDocument/publishDiagnostics"
                && n.params["uri"] == *uri
            {
                return;
            }
        }
    }

    /// Waits until a `publishDiagnostics` has arrived for every URI in `uris`
    /// (any order; non-matching notifications are discarded). Used to time a
    /// cross-module edit's fan-out across all open dependents.
    pub fn await_diagnostics_for_all(&self, uris: &HashSet<String>) {
        let mut remaining = uris.clone();
        while !remaining.is_empty() {
            if let Message::Notification(n) = self.recv()
                && n.method == "textDocument/publishDiagnostics"
                && let Some(uri) = n.params["uri"].as_str()
            {
                remaining.remove(uri);
            }
        }
    }

    pub fn shutdown(mut self) {
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

// ── standing a server over a probed workspace ────────────────────────────────

/// An open target document with a hover/definition position, on a warm server.
pub fn open_target(spec: &CorpusSpec, modules: usize) -> (Server, String, Value) {
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

/// Stands a server over the app and opens the probe's file (analyzed on open).
pub fn open_app_target(probe: &Probe) -> (Server, String) {
    let files = app_workspace();
    let basename = probe.path.strip_prefix("samples/store/").unwrap_or(probe.path).to_owned();
    let text = files.iter().find(|(p, _)| *p == basename).map(|(_, s)| s.clone()).unwrap();
    let server = Server::start_files(files);
    let uri = server.did_open(&basename, &text);
    server.await_diagnostics(&uri);
    (server, uri)
}

/// A position request payload at the probe.
pub fn app_params(probe: &Probe, uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": probe.line, "character": probe.character }
    })
}
