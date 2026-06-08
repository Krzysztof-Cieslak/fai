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

    /// Replaces an open document's text (full-sync change notification).
    fn did_change(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }

    /// Closes an open document.
    fn did_close(&self, uri: &str) {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
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

    /// The current diagnostics published for `uri`.
    ///
    /// Diagnostics are asynchronous push notifications, so this synchronizes with
    /// a barrier request: the server processes messages strictly in order, so its
    /// response cannot arrive until every notification sent before it has been
    /// handled and its diagnostics published. We drain up to that response and
    /// return the most recent publish for `uri` (later publishes supersede
    /// earlier ones), which is robust against timing and notification coalescing.
    fn diagnostics(&mut self, uri: &str) -> Vec<Value> {
        let id: RequestId = self.next_id.into();
        self.next_id += 1;
        // An unrecognized request still gets a (null) reply, which serves as the
        // ordering barrier.
        self.client
            .sender
            .send(Message::Request(Request::new(id.clone(), "fai/sync".to_owned(), Value::Null)))
            .unwrap();
        let mut latest: Option<Vec<Value>> = None;
        loop {
            match self.recv() {
                Message::Response(r) if r.id == id => return latest.unwrap_or_default(),
                Message::Notification(n)
                    if n.method == "textDocument/publishDiagnostics" && n.params["uri"] == *uri =>
                {
                    latest = Some(n.params["diagnostics"].as_array().cloned().unwrap_or_default());
                }
                _ => {}
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
    let mut harness = Harness::start("clean", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let diagnostics = harness.diagnostics(&uri);
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
    let mut harness = Harness::start("errors", &[("Main.fai", bad)]);
    let uri = harness.did_open("Main.fai", bad);
    let diagnostics = harness.diagnostics(&uri);
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

// --- navigation & structure --------------------------------------------------

#[test]
fn document_symbol_lists_top_level_bindings() {
    let mut harness = Harness::start("docsym", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let result =
        harness.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": uri } }));
    let symbols = result.as_array().expect("an array of document symbols");
    // Sorted by name: `inc` then `two`.
    let names: Vec<&str> = symbols.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["inc", "two"], "{symbols:?}");
    // `inc` is a function (LSP kind 12), `two` a value (kind 13).
    assert_eq!(symbols[0]["kind"], 12, "{symbols:?}");
    assert_eq!(symbols[1]["kind"], 13, "{symbols:?}");
    assert_eq!(symbols[0]["detail"], "Int -> Int");
    harness.shutdown();
}

#[test]
fn workspace_symbol_finds_by_query() {
    let mut harness = Harness::start("wssym", &[("Main.fai", MAIN)]);
    let _ = harness.did_open("Main.fai", MAIN);
    let result = harness.request("workspace/symbol", json!({ "query": "inc" }));
    let symbols = result.as_array().expect("an array of symbols");
    assert_eq!(symbols.len(), 1, "only `inc` matches: {symbols:?}");
    assert_eq!(symbols[0]["name"], "inc");
    assert!(symbols[0]["location"]["uri"].as_str().unwrap().ends_with("Main.fai"), "{symbols:?}");
    assert_eq!(symbols[0]["containerName"], "Main");
    harness.shutdown();
}

#[test]
fn references_list_uses_and_declaration() {
    let mut harness = Harness::start("refs", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    // Invoke on the use `inc` in `let two = inc 1` (line 6, col 10).
    let with_decl = harness.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 6, "character": 10 },
            "context": { "includeDeclaration": true }
        }),
    );
    let locs = with_decl.as_array().expect("locations");
    assert_eq!(locs.len(), 2, "the declaration plus one use: {locs:?}");
    let lines: Vec<i64> =
        locs.iter().map(|l| l["range"]["start"]["line"].as_i64().unwrap()).collect();
    assert!(
        lines.contains(&3) && lines.contains(&6),
        "declaration on line 3, use on line 6: {lines:?}"
    );
    harness.shutdown();
}

#[test]
fn references_span_multiple_modules() {
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n\nlet four = A.inc (A.inc two)\n";
    let mut harness = Harness::start("refs-multi", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let _ = harness.did_open("B.fai", b);
    // Invoke on the `inc` binding's name in A (line 3, col 4).
    let result = harness.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri_a },
            "position": { "line": 3, "character": 4 },
            "context": { "includeDeclaration": true }
        }),
    );
    let locs = result.as_array().expect("locations");
    // One declaration in A, three uses in B.
    assert_eq!(locs.len(), 4, "{locs:?}");
    let in_b = locs.iter().filter(|l| l["uri"].as_str().unwrap().ends_with("B.fai")).count();
    assert_eq!(in_b, 3, "{locs:?}");
    harness.shutdown();
}

// --- dirty (unsaved) buffers -------------------------------------------------
//
// These exercise the case the earlier tests do not: the open buffer differs from
// the file on disk. The server overlays the buffer into the warm database, so
// analysis must track the unsaved edits, and a close must hand ownership back to
// the filesystem.

#[test]
fn did_change_analyzes_unsaved_edits_not_disk() {
    // Disk stays clean for the whole test; only the buffer changes. A reported
    // error can therefore only come from the overlaid (unsaved) text.
    let mut harness = Harness::start("dirty-change", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    assert!(harness.diagnostics(&uri).is_empty(), "clean buffer, no diagnostics");

    // Edit the buffer to a type error (the signature no longer matches the body).
    let broken = "module Main\n\npublic inc : Int -> Bool\nlet inc x = x + 1\n\npublic two : Int\nlet two = inc 1\n";
    harness.did_change(&uri, broken);
    let diagnostics = harness.diagnostics(&uri);
    assert!(!diagnostics.is_empty(), "the unsaved edit is type-checked: {diagnostics:?}");
    assert!(
        diagnostics.iter().all(|d| d["code"].as_str().unwrap().starts_with("FAI")),
        "{diagnostics:?}"
    );

    // Edit back to a clean buffer: the diagnostics clear again.
    harness.did_change(&uri, MAIN);
    assert!(harness.diagnostics(&uri).is_empty(), "fixing the buffer clears diagnostics");
    harness.shutdown();
}

#[test]
fn hover_and_definition_track_the_unsaved_buffer() {
    // On disk, `Main` has only `two`; the buffer adds `inc` and a reference to it.
    let disk = indoc! {r#"
        module Main

        public two : Int
        let two = 0
    "#};
    let mut harness = Harness::start("dirty-hover", &[("Main.fai", disk)]);
    let uri = harness.did_open("Main.fai", disk);
    let buffer = MAIN; // adds `inc` (line 3) and `let two = inc 1` (line 6)
    harness.did_change(&uri, buffer);

    // `inc` and line 6 exist only in the buffer; answers here prove the overlay
    // (the offset would be out of range against the 4-line disk file).
    let position =
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } });
    let hover = harness.request("textDocument/hover", position.clone());
    assert!(
        hover["contents"]["value"].as_str().unwrap().contains("inc : Int -> Int"),
        "hover: {hover:?}"
    );
    let definition = harness.request("textDocument/definition", position);
    let locations = definition.as_array().expect("locations");
    assert_eq!(locations[0]["range"]["start"]["line"], 3, "jumps to the buffer-only binding");
    harness.shutdown();
}

#[test]
fn did_close_reverts_the_overlay_to_disk() {
    // `A` defines `n : Int`; `B` uses it as an `Int` (valid against disk).
    let disk_a = "module A\n\npublic n : Int\nlet n = 0\n";
    let disk_b = "module B\n\npublic m : Int\nlet m = A.n + 1\n";
    let mut harness = Harness::start("dirty-close", &[("A.fai", disk_a), ("B.fai", disk_b)]);

    // Open `A` with an unsaved edit that retypes `n` to `Bool` (valid in `A`),
    // which breaks `B`'s `A.n + 1`.
    let dirty_a = "module A\n\npublic n : Bool\nlet n = true\n";
    let uri_a = harness.did_open("A.fai", dirty_a);
    let uri_b = harness.did_open("B.fai", disk_b);
    assert_eq!(harness.diagnostics(&uri_b).len(), 1, "B sees A's unsaved edit across modules");

    // Close `A` without saving: the overlay must revert to the on-disk `n : Int`.
    harness.did_close(&uri_a);
    // Re-check `B` (an identical-content change re-runs analysis): now clean.
    harness.did_change(&uri_b, disk_b);
    assert!(
        harness.diagnostics(&uri_b).is_empty(),
        "closing A restored its on-disk type, so B is valid again"
    );
    harness.shutdown();
}

#[test]
fn formatting_uses_the_unsaved_buffer() {
    // On disk the file is already canonical and has no `inc`.
    let disk = indoc! {r#"
        module Main

        public two : Int
        let two = 0
    "#};
    let mut harness = Harness::start("dirty-fmt", &[("Main.fai", disk)]);
    let uri = harness.did_open("Main.fai", disk);
    // The unsaved buffer adds a (badly spaced) `inc` that is not on disk.
    let messy =
        "module Main\n\npublic two : Int\nlet two=0\n\npublic inc : Int -> Int\nlet inc x=x+1\n";
    harness.did_change(&uri, messy);
    let result = harness.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let new_text = result.as_array().expect("edits")[0]["newText"].as_str().unwrap();
    // The buffer-only `inc` is formatted, proving the unsaved text was used.
    assert!(new_text.contains("let inc x = x + 1"), "{new_text:?}");
    assert!(new_text.contains("let two = 0") && !new_text.contains("two=0"), "{new_text:?}");
    harness.shutdown();
}
