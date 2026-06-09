//! Shared test harness for the language-server end-to-end tests.
//!
//! Each test stands up a real workspace on disk, runs [`fai_lsp::serve`] on a
//! background thread against one half of an in-memory [`Connection`], and drives
//! the other half as a client: it performs the initialize handshake, then opens
//! documents and issues requests, asserting on the JSON the server returns.
//!
//! Not every scenario file uses every helper, so dead-code warnings are allowed.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use camino::Utf8PathBuf;
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
pub struct Harness {
    client: Connection,
    server: Option<JoinHandle<()>>,
    workspace: PathBuf,
    next_id: i32,
}

impl Harness {
    /// Starts a server over a workspace containing `files` (`(name, contents)`),
    /// and completes the initialize handshake.
    pub fn start(tag: &str, files: &[(&str, &str)]) -> Self {
        Self::start_with_caps(tag, files, json!({})).0
    }

    /// Convenience: start over a single `Main.fai` with `text`, already opened,
    /// returning the harness and the document URI.
    pub fn open_main(tag: &str, text: &str) -> (Self, String) {
        let harness = Self::start(tag, &[("Main.fai", text)]);
        let uri = harness.did_open("Main.fai", text);
        (harness, uri)
    }

    /// Like [`Self::start`], but sends `capabilities` in the initialize request
    /// and returns the server's `InitializeResult` (for capability assertions).
    pub fn start_with_caps(
        tag: &str,
        files: &[(&str, &str)],
        capabilities: Value,
    ) -> (Self, Value) {
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
                json!({ "capabilities": capabilities }),
            )))
            .unwrap();
        let harness = Self { client, server: Some(server), workspace, next_id: 2 };
        let init = harness.await_response(&1.into());
        harness
            .client
            .sender
            .send(Message::Notification(Notification::new("initialized".to_owned(), json!({}))))
            .unwrap();
        (harness, init)
    }

    /// The `file://` URI (as a string) for a workspace file.
    pub fn uri(&self, name: &str) -> String {
        Url::from_file_path(self.workspace.join(name)).unwrap().to_string()
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.client
            .sender
            .send(Message::Notification(Notification::new(method.to_owned(), params)))
            .unwrap();
    }

    /// Sends a request and returns the (deserialized) result value.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id: RequestId = self.next_id.into();
        self.next_id += 1;
        self.client
            .sender
            .send(Message::Request(Request::new(id.clone(), method.to_owned(), params)))
            .unwrap();
        self.await_response(&id)
    }

    /// A position-keyed request (`{ textDocument, position }`) — hover, definition,
    /// completion, signature help, prepareRename, …
    pub fn at(&mut self, method: &str, uri: &str, position: Value) -> Value {
        self.request(method, json!({ "textDocument": { "uri": uri }, "position": position }))
    }

    pub fn hover(&mut self, uri: &str, position: Value) -> Value {
        self.at("textDocument/hover", uri, position)
    }

    /// The hover markdown at `position`, if any.
    pub fn hover_text(&mut self, uri: &str, position: Value) -> Option<String> {
        self.hover(uri, position)["contents"]["value"].as_str().map(str::to_owned)
    }

    pub fn definition(&mut self, uri: &str, position: Value) -> Value {
        self.at("textDocument/definition", uri, position)
    }

    pub fn completion(&mut self, uri: &str, position: Value) -> Value {
        self.at("textDocument/completion", uri, position)
    }

    /// Resolves a completion item (the verbatim item the server returned),
    /// filling in lazily-computed detail such as documentation.
    pub fn resolve_completion(&mut self, item: Value) -> Value {
        self.request("completionItem/resolve", item)
    }

    pub fn signature_help(&mut self, uri: &str, position: Value) -> Value {
        self.at("textDocument/signatureHelp", uri, position)
    }

    pub fn prepare_rename(&mut self, uri: &str, position: Value) -> Value {
        self.at("textDocument/prepareRename", uri, position)
    }

    pub fn references(&mut self, uri: &str, position: Value, include_declaration: bool) -> Value {
        self.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": uri },
                "position": position,
                "context": { "includeDeclaration": include_declaration }
            }),
        )
    }

    pub fn rename(&mut self, uri: &str, position: Value, new_name: &str) -> Value {
        self.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": position,
                "newName": new_name
            }),
        )
    }

    pub fn code_actions(&mut self, uri: &str, range: Value) -> Value {
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": range,
                "context": { "diagnostics": [] }
            }),
        )
    }

    pub fn document_symbols(&mut self, uri: &str) -> Value {
        self.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": uri } }))
    }

    pub fn workspace_symbols(&mut self, query: &str) -> Value {
        self.request("workspace/symbol", json!({ "query": query }))
    }

    pub fn inlay_hints(&mut self, uri: &str, range: Value) -> Value {
        self.request(
            "textDocument/inlayHint",
            json!({ "textDocument": { "uri": uri }, "range": range }),
        )
    }

    pub fn semantic_tokens(&mut self, uri: &str) -> Value {
        self.request("textDocument/semanticTokens/full", json!({ "textDocument": { "uri": uri } }))
    }

    pub fn formatting(&mut self, uri: &str) -> Value {
        self.request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        )
    }

    pub fn range_formatting(&mut self, uri: &str, range: Value) -> Value {
        self.request(
            "textDocument/rangeFormatting",
            json!({
                "textDocument": { "uri": uri },
                "range": range,
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        )
    }

    /// Opens a document, returning its URI.
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

    /// Replaces an open document's text (full-sync change notification).
    pub fn did_change(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }

    /// Sends an incremental change replacing `range` with `text`.
    pub fn did_change_range(&self, uri: &str, range: Value, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ { "range": range, "text": text } ]
            }),
        );
    }

    /// Notifies that a document was saved (no included text).
    pub fn did_save(&self, uri: &str) {
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": uri } }));
    }

    /// Closes an open document.
    pub fn did_close(&self, uri: &str) {
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
    pub fn diagnostics(&mut self, uri: &str) -> Vec<Value> {
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

    /// Whether `uri` currently has any error/warning diagnostic.
    pub fn has_diagnostics(&mut self, uri: &str) -> bool {
        !self.diagnostics(uri).is_empty()
    }

    /// The diagnostic codes currently published for `uri`.
    pub fn diagnostic_codes(&mut self, uri: &str) -> Vec<String> {
        self.diagnostics(uri).iter().filter_map(|d| d["code"].as_str().map(str::to_owned)).collect()
    }

    fn recv(&self) -> Message {
        self.client
            .receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("the server should respond before the timeout")
    }

    /// Cleanly shuts the server down and joins its thread.
    pub fn shutdown(mut self) {
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

// --- position helpers --------------------------------------------------------

/// The LSP position (`{ line, character }`) at the start of `needle` in `text`.
pub fn position_of(text: &str, needle: &str) -> Value {
    byte_to_position(text, find(text, needle))
}

/// The LSP position just after `needle` in `text`.
pub fn position_after(text: &str, needle: &str) -> Value {
    byte_to_position(text, find(text, needle) + needle.len())
}

/// The position at the start of `needle`, with the column measured in UTF-8
/// bytes (for a client that negotiated the UTF-8 position encoding).
pub fn position_of_utf8(text: &str, needle: &str) -> Value {
    let byte = find(text, needle);
    let (mut line, mut line_start) = (0u32, 0usize);
    for (i, ch) in text.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    json!({ "line": line, "character": (byte - line_start) as u32 })
}

/// The LSP position `offset` bytes into `needle` in `text`.
pub fn position_within(text: &str, needle: &str, offset: usize) -> Value {
    byte_to_position(text, find(text, needle) + offset)
}

/// An explicit `{ line, character }` position.
pub fn pos(line: u32, character: u32) -> Value {
    json!({ "line": line, "character": character })
}

/// An LSP range from `start` to `end` (both `{ line, character }`).
pub fn range(start: Value, end: Value) -> Value {
    json!({ "start": start, "end": end })
}

/// A range spanning the whole document (line 0 to a line past the end).
pub fn whole_document() -> Value {
    json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 100_000, "character": 0 } })
}

fn find(text: &str, needle: &str) -> usize {
    text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found in source"))
}

/// Converts a byte offset to an LSP `{ line, character }` (UTF-16 columns).
fn byte_to_position(text: &str, byte: usize) -> Value {
    let (mut line, mut col) = (0u32, 0u32);
    for (i, ch) in text.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
    }
    json!({ "line": line, "character": col })
}

// --- edit helpers ------------------------------------------------------------

/// Applies LSP `TextEdit`s (JSON) to ASCII `text`, returning the new text — what
/// an editor does after the server returns a code action / rename / formatting
/// edit. Edits are applied right-to-left so earlier offsets stay valid.
pub fn apply_text_edits(text: &str, edits: &[Value]) -> String {
    let byte_of = |line: u64, character: u64| -> usize {
        let mut idx = 0usize;
        for (i, l) in text.split_inclusive('\n').enumerate() {
            if i as u64 == line {
                let content = l.strip_suffix('\n').unwrap_or(l);
                return idx + (character as usize).min(content.len());
            }
            idx += l.len();
        }
        text.len()
    };
    let mut spans: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|e| {
            let (s, en) = (&e["range"]["start"], &e["range"]["end"]);
            (
                byte_of(s["line"].as_u64().unwrap(), s["character"].as_u64().unwrap()),
                byte_of(en["line"].as_u64().unwrap(), en["character"].as_u64().unwrap()),
                e["newText"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    spans.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = text.to_owned();
    for (start, end, new_text) in spans {
        out.replace_range(start..end, &new_text);
    }
    out
}

/// The edits a `WorkspaceEdit.changes` map holds for the file ending in `suffix`.
pub fn changes_for(workspace_edit: &Value, suffix: &str) -> Vec<Value> {
    workspace_edit["changes"]
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| k.ends_with(suffix)))
        .and_then(|(_, v)| v.as_array().cloned())
        .unwrap_or_default()
}

/// The titles of a `textDocument/codeAction` response.
pub fn action_titles(actions: &Value) -> Vec<String> {
    actions
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["title"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The completion labels of a `textDocument/completion` response (array form).
pub fn completion_labels(result: &Value) -> Vec<String> {
    result
        .as_array()
        .map(|a| a.iter().filter_map(|i| i["label"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}
