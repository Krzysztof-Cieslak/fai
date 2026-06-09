//! End-to-end language-server test over *real* stdio.
//!
//! Spawns the `fai` binary in `lsp` mode and drives it through
//! `Content-Length`-framed JSON-RPC on its actual stdin/stdout — exercising
//! `fai_lsp::run_stdio` / `lsp_server::Connection::stdio`, the transport the
//! in-memory `serve` tests cannot reach — over an actual `samples/` project.

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use lsp_server::{Message, Notification, Request, RequestId};
use lsp_types::Url;
use serde_json::{Value, json};

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn unique_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fai-lsp-stdio-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The 0-based `(line, character)` of `needle` in `text` (ASCII samples).
fn position(text: &str, needle: &str) -> (u32, u32) {
    let byte = text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found"));
    let line = text[..byte].matches('\n').count() as u32;
    let line_start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    (line, (byte - line_start) as u32)
}

/// A `fai lsp` subprocess plus framed access to its stdio.
struct Lsp {
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    workspace: PathBuf,
    next_id: i32,
}

impl Lsp {
    /// Starts `fai lsp -C <workspace>` and completes the initialize handshake.
    fn start(workspace: PathBuf) -> Self {
        Self::start_with_init(workspace, json!({ "capabilities": {} }))
    }

    /// Like [`start`](Self::start), but with caller-supplied `initialize` params
    /// (e.g. to set `initializationOptions.examples`).
    fn start_with_init(workspace: PathBuf, init_params: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fai"))
            .arg("lsp")
            .arg("-C")
            .arg(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // the server logs to stderr; keep it off the test output
            .spawn()
            .expect("spawn `fai lsp`");
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut lsp =
            Self { child: Some(child), stdin: Some(stdin), stdout, workspace, next_id: 2 };

        lsp.send(Message::Request(Request::new(1.into(), "initialize".to_owned(), init_params)));
        lsp.read_response(&1.into());
        lsp.notify("initialized", json!({}));
        lsp
    }

    fn send(&mut self, message: Message) {
        let writer = self.stdin.as_mut().expect("stdin open");
        message.write(writer).unwrap();
        writer.flush().unwrap();
    }

    fn read(&mut self) -> Message {
        Message::read(&mut self.stdout).unwrap().expect("the server closed the stream")
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(Message::Notification(Notification::new(method.to_owned(), params)));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id: RequestId = self.next_id.into();
        self.next_id += 1;
        self.send(Message::Request(Request::new(id.clone(), method.to_owned(), params)));
        self.read_response(&id)
    }

    fn read_response(&mut self, id: &RequestId) -> Value {
        loop {
            if let Message::Response(r) = self.read()
                && &r.id == id
            {
                assert!(r.error.is_none(), "server error: {:?}", r.error);
                return r.result.unwrap_or(Value::Null);
            }
        }
    }

    fn uri(&self, name: &str) -> String {
        Url::from_file_path(self.workspace.join(name)).unwrap().to_string()
    }

    fn did_open(&mut self, name: &str, text: &str) -> String {
        let uri = self.uri(name);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": { "uri": uri, "languageId": "fai", "version": 1, "text": text }
            }),
        );
        uri
    }

    fn did_change(&mut self, name: &str, version: i32, text: &str) -> String {
        let uri = self.uri(name);
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        );
        uri
    }

    fn did_save(&mut self, name: &str, text: &str) -> String {
        let uri = self.uri(name);
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri }, "text": text }),
        );
        uri
    }

    fn await_diagnostics(&mut self, uri: &str) -> Vec<Value> {
        loop {
            if let Message::Notification(n) = self.read()
                && n.method == "textDocument/publishDiagnostics"
                && n.params["uri"] == *uri
            {
                return n.params["diagnostics"].as_array().cloned().unwrap_or_default();
            }
        }
    }

    fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        // Close stdin so the server's stdio reader hits EOF and the process exits.
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let status = child.wait().unwrap();
            assert!(status.success(), "`fai lsp` should exit 0, got {status}");
        }
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

#[test]
fn lsp_serves_a_sample_project_over_stdio() {
    let workspace = unique_workspace();
    // A real, self-contained sample (uses locals and `Float.sqrt` from std).
    let src = std::fs::read_to_string(samples_dir().join("Locals.fai")).unwrap();
    std::fs::write(workspace.join("Locals.fai"), &src).unwrap();

    let mut lsp = Lsp::start(workspace);
    let uri = lsp.did_open("Locals.fai", &src);

    // Diagnostics: the clean sample reports none, end to end over real stdio.
    let diagnostics = lsp.await_diagnostics(&uri);
    assert!(diagnostics.is_empty(), "a clean sample has no diagnostics: {diagnostics:?}");

    // Hover over the local `a2` in `Float.sqrt (a2 + b2)`.
    let (line, character) = position(&src, "a2 + b2");
    let at = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
    let hover = lsp.request("textDocument/hover", at.clone());
    let value = hover["contents"]["value"].as_str().unwrap_or("");
    assert!(value.contains("Float"), "hover should report the local's type: {hover:?}");

    // Go-to-definition jumps to the `let a2` binding (an earlier line, same file).
    let definition = lsp.request("textDocument/definition", at);
    let locations = definition.as_array().expect("an array of locations");
    assert!(locations[0]["uri"].as_str().unwrap().ends_with("Locals.fai"), "{definition:?}");
    let (binding_line, _) = position(&src, "a2 = a * a");
    assert_eq!(locations[0]["range"]["start"]["line"], binding_line, "{definition:?}");

    lsp.shutdown();
}

/// The diagnostic codes in a `publishDiagnostics` payload.
fn codes(diags: &[Value]) -> Vec<String> {
    diags.iter().filter_map(|d| d["code"].as_str().map(str::to_owned)).collect()
}

#[test]
fn example_failure_appears_on_save_and_clears_on_edit() {
    let workspace = unique_workspace();
    let src = "module Bad\nexample: 1 = 2\n";
    std::fs::write(workspace.join("Bad.fai"), src).unwrap();
    let mut lsp = Lsp::start(workspace);

    // Opening type-checks the file but does not evaluate its examples.
    let uri = lsp.did_open("Bad.fai", src);
    assert!(codes(&lsp.await_diagnostics(&uri)).is_empty(), "no example eval on open");

    // Saving evaluates the closed example (in the isolated worker) and reports
    // the failure as FAI6001 — without a separate `fai test`.
    lsp.did_save("Bad.fai", src);
    let on_save = codes(&lsp.await_diagnostics(&uri));
    assert!(on_save.contains(&"FAI6001".to_owned()), "expected FAI6001 on save: {on_save:?}");

    // Editing the file clears the cached failure (it is recomputed on next save),
    // so it does not linger on stale text.
    lsp.did_change("Bad.fai", 2, "module Bad\nexample: 1 = 2\n// edited\n");
    let after_edit = codes(&lsp.await_diagnostics(&uri));
    assert!(after_edit.is_empty(), "an edit clears the example diagnostic: {after_edit:?}");

    lsp.shutdown();
}

#[test]
fn examples_disabled_by_initialization_option() {
    let workspace = unique_workspace();
    let src = "module Bad\nexample: 1 = 2\n";
    std::fs::write(workspace.join("Bad.fai"), src).unwrap();
    let mut lsp = Lsp::start_with_init(
        workspace,
        json!({ "capabilities": {}, "initializationOptions": { "examples": false } }),
    );

    let uri = lsp.did_open("Bad.fai", src);
    assert!(codes(&lsp.await_diagnostics(&uri)).is_empty());

    // With the option off, saving does not evaluate examples either.
    lsp.did_save("Bad.fai", src);
    let on_save = codes(&lsp.await_diagnostics(&uri));
    assert!(on_save.is_empty(), "examples disabled: no FAI6001 on save: {on_save:?}");

    lsp.shutdown();
}
