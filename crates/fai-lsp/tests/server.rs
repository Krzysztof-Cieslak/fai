//! End-to-end tests for the language server over an in-memory connection.
//!
//! Each test stands up a real workspace on disk, runs [`fai_lsp::serve`] on a
//! background thread against one half of an in-memory [`Connection`], and drives
//! the other half as a client: it performs the initialize handshake, then opens
//! documents and issues requests, asserting on the JSON the server returns.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use camino::Utf8PathBuf;
use indoc::indoc;
use lsp_server::{Connection, Message, Notification, Request, RequestId};
use lsp_types::Url;
use serde_json::{Value, json};

/// A unique temporary workspace directory (created on disk).
fn unique_workspace(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fai-lsp-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A running server plus the client end of its connection.
struct Harness {
    client: Connection,
    server: Option<JoinHandle<()>>,
    workspace: PathBuf,
    next_id: i32,
}

impl Harness {
    /// Starts a server over a workspace containing `files` (`(name, contents)`),
    /// and completes the initialize handshake.
    fn start(tag: &str, files: &[(&str, &str)]) -> Self {
        let workspace = unique_workspace(tag);
        for (name, contents) in files {
            std::fs::write(workspace.join(name), contents).unwrap();
        }
        let root = Utf8PathBuf::from_path_buf(workspace.clone()).unwrap();
        let (server_conn, client) = Connection::memory();
        let server = std::thread::spawn(move || {
            let _ = fai_lsp::serve(&server_conn, root);
        });

        // Client side of the initialize handshake: request, await result,
        // confirm with the `initialized` notification.
        client
            .sender
            .send(Message::Request(Request::new(
                1.into(),
                "initialize".to_owned(),
                json!({ "capabilities": {} }),
            )))
            .unwrap();
        let harness = Self { client, server: Some(server), workspace, next_id: 2 };
        let _ = harness.await_response(&1.into());
        harness
            .client
            .sender
            .send(Message::Notification(Notification::new("initialized".to_owned(), json!({}))))
            .unwrap();
        harness
    }

    /// The `file://` URI (as a string) for a workspace file.
    fn uri(&self, name: &str) -> String {
        Url::from_file_path(self.workspace.join(name)).unwrap().to_string()
    }

    fn notify(&self, method: &str, params: Value) {
        self.client
            .sender
            .send(Message::Notification(Notification::new(method.to_owned(), params)))
            .unwrap();
    }

    /// Sends a request and returns the (deserialized) result value.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id: RequestId = self.next_id.into();
        self.next_id += 1;
        self.client
            .sender
            .send(Message::Request(Request::new(id.clone(), method.to_owned(), params)))
            .unwrap();
        self.await_response(&id)
    }

    /// Opens a document, returning its URI.
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

    /// Waits for the response to `id`, skipping notifications (e.g. diagnostics).
    fn await_response(&self, id: &RequestId) -> Value {
        loop {
            match self.recv() {
                Message::Response(r) if &r.id == id => {
                    assert!(r.error.is_none(), "server error: {:?}", r.error);
                    return r.result.unwrap_or(Value::Null);
                }
                Message::Response(other) => panic!("unexpected response: {other:?}"),
                Message::Request(req) => panic!("unexpected server request: {req:?}"),
                Message::Notification(_) => {}
            }
        }
    }

    /// Waits for the next `publishDiagnostics` for `uri`, returning the diagnostics.
    fn await_diagnostics(&self, uri: &str) -> Vec<Value> {
        loop {
            if let Message::Notification(n) = self.recv()
                && n.method == "textDocument/publishDiagnostics"
                && n.params["uri"] == *uri
            {
                return n.params["diagnostics"].as_array().cloned().unwrap_or_default();
            }
        }
    }

    fn recv(&self) -> Message {
        self.client
            .receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("the server should respond before the timeout")
    }

    /// Cleanly shuts the server down and joins its thread.
    fn shutdown(mut self) {
        let _ = self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

const MAIN: &str = indoc! {r#"
    module Main

    public inc : Int -> Int
    let inc x = x + 1

    public two : Int
    let two = inc 1
"#};

#[test]
fn publishes_empty_diagnostics_for_a_clean_file() {
    let harness = Harness::start("clean", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let diagnostics = harness.await_diagnostics(&uri);
    assert!(diagnostics.is_empty(), "a well-typed file has no diagnostics: {diagnostics:?}");
    harness.shutdown();
}

#[test]
fn publishes_type_errors_as_diagnostics() {
    let bad = indoc! {r#"
        module Main

        public bad : Int -> Bool
        let bad x = x + 1
    "#};
    let harness = Harness::start("errors", &[("Main.fai", bad)]);
    let uri = harness.did_open("Main.fai", bad);
    let diagnostics = harness.await_diagnostics(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let d = &diagnostics[0];
    assert!(d["code"].as_str().unwrap().starts_with("FAI"), "{d:?}");
    assert_eq!(d["severity"], 1, "a type error is an LSP error (severity 1)");
    assert_eq!(d["source"], "fai");
    // The diagnostic points inside the file (line 3, the body `x + 1`).
    assert_eq!(d["range"]["start"]["line"], 3);
    harness.shutdown();
}

#[test]
fn hover_reports_the_referenced_type() {
    let mut harness = Harness::start("hover", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    // Hover over `inc` in `let two = inc 1` (line 6, column 10).
    let result = harness.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } }),
    );
    let value = result["contents"]["value"].as_str().unwrap();
    assert!(value.contains("inc : Int -> Int"), "hover text: {value:?}");
    // The hover range underlines the reference `inc`.
    assert_eq!(result["range"]["start"], json!({ "line": 6, "character": 10 }));
    assert_eq!(result["range"]["end"], json!({ "line": 6, "character": 13 }));
    harness.shutdown();
}

#[test]
fn definition_jumps_to_the_binding() {
    let mut harness = Harness::start("def", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let result = harness.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } }),
    );
    let locations = result.as_array().expect("an array of locations");
    assert_eq!(locations.len(), 1, "{locations:?}");
    // It jumps to the `inc` binding (line 3).
    assert!(locations[0]["uri"].as_str().unwrap().ends_with("Main.fai"));
    assert_eq!(locations[0]["range"]["start"]["line"], 3);
    harness.shutdown();
}

#[test]
fn formatting_returns_canonical_text() {
    let messy = "module Main\n\npublic two : Int\nlet two=inc 1\n\npublic inc : Int -> Int\nlet inc x=x+1\n";
    let mut harness = Harness::start("fmt", &[("Main.fai", messy)]);
    let uri = harness.did_open("Main.fai", messy);
    let result = harness.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let edits = result.as_array().expect("an array of edits");
    let new_text = edits[0]["newText"].as_str().unwrap();
    // The canonical form spaces the operators and the `=`.
    assert!(new_text.contains("let two = inc 1"), "{new_text:?}");
    assert!(new_text.contains("let inc x = x + 1"), "{new_text:?}");
    assert!(!new_text.contains("=inc") && !new_text.contains("x=x"), "{new_text:?}");
    harness.shutdown();
}
