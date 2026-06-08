//! A Language Server for Fai, speaking standard LSP (JSON-RPC over stdio).
//!
//! Editors use this; agents use `fai query` instead. It reuses the warm query
//! database via [`fai_driver::Session`] and the [`fai_ide`] engine, so hover,
//! go-to-definition, and formatting share the same answers as `fai query`, and
//! diagnostics share `fai check`'s. Open buffers are overlaid into the database as
//! in-memory edits, so analysis tracks unsaved changes.
//!
//! The protocol surface is intentionally small (LSP v1): `textDocument` sync,
//! `publishDiagnostics`, `hover`, `definition`, and `formatting`.

use std::collections::HashMap;
use std::error::Error;

use camino::Utf8PathBuf;
use fai_db::SourceFile;
use fai_driver::{DirtyFile, Session, check, fmt};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    Location as LspLocation, MarkupContent, MarkupKind, NumberOrString, OneOf, Position,
    PublishDiagnosticsParams, Range, ReferenceParams, ServerCapabilities, SymbolInformation,
    SymbolKind as LspSymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
    WorkspaceSymbolParams,
};

mod position;
use position::LineMap;

/// Runs the language server over real stdio until the client shuts it down.
/// Returns the process exit code.
#[must_use]
pub fn run_stdio(root: Utf8PathBuf) -> i32 {
    let (connection, io_threads) = Connection::stdio();
    let result = serve(&connection, root);
    drop(connection);
    let _ = io_threads.join();
    match result {
        Ok(()) => 0,
        Err(err) => {
            tracing::error!("fai lsp: {err}");
            1
        }
    }
}

/// Serves LSP over `connection` until shutdown. Exposed (taking a [`Connection`])
/// so tests can drive an in-memory client/server pair.
///
/// # Errors
/// Returns an error if the initialize handshake or workspace setup fails.
pub fn serve(connection: &Connection, root: Utf8PathBuf) -> Result<(), Box<dyn Error>> {
    let capabilities = serde_json::to_value(server_capabilities())?;
    let _init = connection.initialize(capabilities)?;
    let mut server = Server::new(root)?;
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    break;
                }
                server.on_request(connection, req);
            }
            Message::Notification(note) => server.on_notification(connection, note),
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// The capabilities Fai's language server advertises.
fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}

/// The server's mutable state: the warm session plus the open buffers.
struct Server {
    session: Session,
    root: Utf8PathBuf,
    /// Open documents' current text, by URI.
    open: HashMap<Url, String>,
}

impl Server {
    fn new(root: Utf8PathBuf) -> Result<Self, Box<dyn Error>> {
        let root = root.canonicalize_utf8().unwrap_or(root);
        let session = Session::open(root.clone())?;
        Ok(Self { session, root, open: HashMap::new() })
    }

    // --- notifications ----------------------------------------------------

    fn on_notification(&mut self, conn: &Connection, note: Notification) {
        match note.method.as_str() {
            "textDocument/didOpen" => {
                if let Ok(p) = note.extract::<DidOpenTextDocumentParams>("textDocument/didOpen") {
                    self.open.insert(p.text_document.uri.clone(), p.text_document.text);
                    self.refresh(conn, &p.text_document.uri);
                }
            }
            "textDocument/didChange" => {
                if let Ok(mut p) =
                    note.extract::<DidChangeTextDocumentParams>("textDocument/didChange")
                {
                    // Full sync: the last change carries the whole document.
                    if let Some(change) = p.content_changes.pop() {
                        self.open.insert(p.text_document.uri.clone(), change.text);
                    }
                    self.refresh(conn, &p.text_document.uri);
                }
            }
            "textDocument/didClose" => {
                if let Ok(p) = note.extract::<DidCloseTextDocumentParams>("textDocument/didClose") {
                    let uri = &p.text_document.uri;
                    self.open.remove(uri);
                    // On close the buffer is no longer authoritative — ownership
                    // returns to the filesystem — so drop the in-memory overlay and
                    // restore the database to the on-disk content. Otherwise a file
                    // closed without saving would leave its unsaved edits in the
                    // warm session for any module that references it.
                    self.revert_to_disk(uri);
                    // Clear the closed file's diagnostics.
                    self.publish(conn, uri, vec![]);
                }
            }
            _ => {}
        }
    }

    // --- requests ---------------------------------------------------------

    fn on_request(&mut self, conn: &Connection, req: Request) {
        match req.method.as_str() {
            "textDocument/hover" => {
                if let Ok((id, params)) = req.extract::<HoverParams>("textDocument/hover") {
                    respond(conn, id, &self.hover(&params));
                }
            }
            "textDocument/definition" => {
                if let Ok((id, params)) =
                    req.extract::<GotoDefinitionParams>("textDocument/definition")
                {
                    respond(conn, id, &self.definition(&params));
                }
            }
            "textDocument/formatting" => {
                if let Ok((id, params)) =
                    req.extract::<DocumentFormattingParams>("textDocument/formatting")
                {
                    respond(conn, id, &self.formatting(&params));
                }
            }
            "textDocument/documentSymbol" => {
                if let Ok((id, params)) =
                    req.extract::<DocumentSymbolParams>("textDocument/documentSymbol")
                {
                    respond(conn, id, &self.document_symbols(&params));
                }
            }
            "workspace/symbol" => {
                if let Ok((id, params)) = req.extract::<WorkspaceSymbolParams>("workspace/symbol") {
                    respond(conn, id, &self.workspace_symbols(&params));
                }
            }
            "textDocument/references" => {
                if let Ok((id, params)) = req.extract::<ReferenceParams>("textDocument/references")
                {
                    respond(conn, id, &self.references(&params));
                }
            }
            // An unsupported request still needs a reply so the client is not
            // left waiting; a null result is the conventional "no answer".
            _ => respond(conn, req.id, &serde_json::Value::Null),
        }
    }

    fn hover(&self, params: &HoverParams) -> Option<Hover> {
        let pos = &params.text_document_position_params;
        let (file, offset) = self.locate(&pos.text_document.uri, pos.position)?;
        let result = fai_ide::hover_at(self.session.db(), file, offset, &self.session.resolver());
        let ty = result.ty?;
        // Label the type with the referenced name when there is one.
        let value = match &result.name {
            Some(name) => format!("```fai\n{name} : {}\n```", ty.display),
            None => format!("```fai\n{}\n```", ty.display),
        };
        let range = result.span.and_then(|span| self.span_range(&pos.text_document.uri, &span));
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
            range,
        })
    }

    fn definition(&self, params: &GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let pos = &params.text_document_position_params;
        let (file, offset) = self.locate(&pos.text_document.uri, pos.position)?;
        let result =
            fai_ide::definition_at(self.session.db(), file, offset, &self.session.resolver());
        let locations: Vec<LspLocation> =
            result.definitions.iter().filter_map(|d| self.to_lsp_location(&d.span)).collect();
        (!locations.is_empty()).then_some(GotoDefinitionResponse::Array(locations))
    }

    fn formatting(&self, params: &DocumentFormattingParams) -> Option<Vec<TextEdit>> {
        let uri = &params.text_document.uri;
        let rel = self.relative(uri)?;
        let text = self.open.get(uri)?;
        let files = self.session.select_files(Some(&rel));
        let file = files.first()?;
        // The open buffer is already overlaid into the database, so the warm
        // formatter sees the unsaved text.
        let result = fmt(self.session.db(), &[*file]);
        let formatted = result.files.first()?.formatted.clone();
        let lines = LineMap::new(text);
        let range = Range { start: Position { line: 0, character: 0 }, end: lines.end() };
        Some(vec![TextEdit { range, new_text: formatted }])
    }

    fn document_symbols(&self, params: &DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
        let uri = &params.text_document.uri;
        let rel = self.relative(uri)?;
        let file = *self.session.select_files(Some(&rel)).first()?;
        let result = fai_ide::document_symbols(self.session.db(), file, &self.session.resolver());
        let symbols: Vec<DocumentSymbol> =
            result.outline.iter().filter_map(|n| self.to_document_symbol(n)).collect();
        Some(DocumentSymbolResponse::Nested(symbols))
    }

    fn workspace_symbols(&self, params: &WorkspaceSymbolParams) -> Option<Vec<SymbolInformation>> {
        let files = self.session.user_files();
        let result = fai_ide::workspace_symbols(
            self.session.db(),
            &files,
            &params.query,
            &self.session.resolver(),
            fai_ide::ListOpts::default(),
        );
        Some(result.symbols.iter().filter_map(|s| self.to_symbol_information(s)).collect())
    }

    fn references(&self, params: &ReferenceParams) -> Option<Vec<LspLocation>> {
        let pos = &params.text_document_position;
        let (file, offset) = self.locate(&pos.text_document.uri, pos.position)?;
        let locations = fai_ide::references_at(
            self.session.db(),
            &self.session.user_files(),
            file,
            offset,
            &self.session.resolver(),
            params.context.include_declaration,
        );
        Some(locations.iter().filter_map(|l| self.to_lsp_location(&l.span)).collect())
    }

    // --- helpers ----------------------------------------------------------

    /// Converts an IDE outline node into an LSP [`DocumentSymbol`] (recursively
    /// nesting children under nested modules).
    #[allow(deprecated)] // the `deprecated` field is required by the struct literal.
    fn to_document_symbol(&self, node: &fai_ide::OutlineNode) -> Option<DocumentSymbol> {
        let range = self.range_in_file(&node.symbol.span)?;
        let children: Vec<DocumentSymbol> =
            node.children.iter().filter_map(|c| self.to_document_symbol(c)).collect();
        Some(DocumentSymbol {
            name: node.symbol.name.clone(),
            detail: node.symbol.signature.clone(),
            kind: lsp_symbol_kind(node.symbol.kind),
            tags: None,
            deprecated: None,
            range,
            // No separate name span is tracked, so the selection range is the
            // whole declaration; it is trivially contained by `range`.
            selection_range: range,
            children: (!children.is_empty()).then_some(children),
        })
    }

    /// Converts an IDE symbol reference into an LSP [`SymbolInformation`].
    #[allow(deprecated)] // the `deprecated` field is required by the struct literal.
    fn to_symbol_information(
        &self,
        symbol: &fai_ide::repr::SymbolRef,
    ) -> Option<SymbolInformation> {
        Some(SymbolInformation {
            name: symbol.name.clone(),
            kind: lsp_symbol_kind(symbol.kind),
            tags: None,
            deprecated: None,
            location: self.to_lsp_location(&symbol.span)?,
            container_name: Some(symbol.module.clone()),
        })
    }

    /// Converts an IDE span into a range, reading the span's own file (the open
    /// buffer when present, else the database copy).
    fn range_in_file(&self, span: &fai_ide::repr::SpanJson) -> Option<Range> {
        let text = self.file_text(&span.file)?;
        let lines = LineMap::new(&text);
        Some(Range {
            start: lines.position(span.byte_start as usize),
            end: lines.position(span.byte_end as usize),
        })
    }

    /// Overlays a document's current text into the session and republishes its
    /// diagnostics.
    fn refresh(&mut self, conn: &Connection, uri: &Url) {
        let Some(rel) = self.relative(uri) else { return };
        let Some(text) = self.open.get(uri).cloned() else { return };
        let dirty = [DirtyFile { path: rel.to_string(), hash: None, content: Some(text.clone()) }];
        if self.session.apply_dirty(&dirty).is_err() {
            return;
        }
        let files = self.session.select_files(Some(&rel));
        let diagnostics = match files.first() {
            Some(&file) => {
                let result = check(self.session.db(), &[file]);
                let lines = LineMap::new(&text);
                result.diagnostics.iter().map(|d| to_lsp_diagnostic(d, &lines)).collect()
            }
            None => vec![],
        };
        self.publish(conn, uri, diagnostics);
    }

    /// Restores a closed document's database entry to the on-disk file, dropping
    /// any unsaved overlay. A document with no disk copy (an unsaved, untitled
    /// buffer) has nothing to revert to and is left as-is.
    fn revert_to_disk(&mut self, uri: &Url) {
        let Some(rel) = self.relative(uri) else { return };
        if !self.root.join(&rel).exists() {
            return;
        }
        // A content-less dirty entry re-reads the file from disk.
        let dirty = [DirtyFile { path: rel.to_string(), hash: None, content: None }];
        let _ = self.session.apply_dirty(&dirty);
    }

    fn publish(&self, conn: &Connection, uri: &Url, diagnostics: Vec<LspDiagnostic>) {
        let params = PublishDiagnosticsParams { uri: uri.clone(), diagnostics, version: None };
        let note = Notification::new("textDocument/publishDiagnostics".to_owned(), params);
        let _ = conn.sender.send(Message::Notification(note));
    }

    /// The workspace-relative path for a document URI (the form the database and
    /// the IDE engine key on).
    fn relative(&self, uri: &Url) -> Option<Utf8PathBuf> {
        let path = uri.to_file_path().ok()?;
        let path = Utf8PathBuf::from_path_buf(path).ok()?;
        let path = path.canonicalize_utf8().unwrap_or(path);
        Some(path.strip_prefix(&self.root).unwrap_or(&path).to_owned())
    }

    /// The document URI for a workspace-relative path.
    fn uri_for(&self, rel: &str) -> Option<Url> {
        Url::from_file_path(self.root.join(rel).as_std_path()).ok()
    }

    /// The text of `rel` — the open buffer if any, else the database's copy.
    fn file_text(&self, rel: &str) -> Option<String> {
        if let Some(uri) = self.uri_for(rel)
            && let Some(text) = self.open.get(&uri)
        {
            return Some(text.clone());
        }
        let db = self.session.db();
        let file = db.all_source_files().into_iter().find(|f| f.path(db).as_str() == rel)?;
        Some(file.text(db).clone())
    }

    /// Resolves an LSP position in a document to the database file and the byte
    /// offset the IDE engine's position queries accept.
    fn locate(&self, uri: &Url, position: Position) -> Option<(SourceFile, u32)> {
        let rel = self.relative(uri)?;
        let text = self.open.get(uri)?;
        let file = *self.session.select_files(Some(&rel)).first()?;
        let offset = LineMap::new(text).offset(position) as u32;
        Some((file, offset))
    }

    /// Converts an IDE span (byte offsets) into a range within an open document.
    fn span_range(&self, uri: &Url, span: &fai_ide::repr::SpanJson) -> Option<Range> {
        let text = self.open.get(uri)?;
        let lines = LineMap::new(text);
        Some(Range {
            start: lines.position(span.byte_start as usize),
            end: lines.position(span.byte_end as usize),
        })
    }

    /// Converts an IDE span (workspace-relative path + byte offsets) to an LSP
    /// location.
    fn to_lsp_location(&self, span: &fai_ide::repr::SpanJson) -> Option<LspLocation> {
        let uri = self.uri_for(&span.file)?;
        let text = self.file_text(&span.file)?;
        let lines = LineMap::new(&text);
        let range = Range {
            start: lines.position(span.byte_start as usize),
            end: lines.position(span.byte_end as usize),
        };
        Some(LspLocation { uri, range })
    }
}

/// Sends a successful response for `id`.
fn respond<T: serde::Serialize>(conn: &Connection, id: RequestId, result: &T) {
    let value = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
    let _ = conn.sender.send(Message::Response(Response { id, result: Some(value), error: None }));
}

/// Maps a Fai diagnostic to its LSP form, using `lines` (the file's text) to
/// turn byte offsets into ranges.
fn to_lsp_diagnostic(d: &fai_diagnostics::Diagnostic, lines: &LineMap) -> LspDiagnostic {
    let range = Range {
        start: lines.position(d.primary.start().raw() as usize),
        end: lines.position(d.primary.end().raw() as usize),
    };
    let message = match &d.help {
        Some(help) => format!("{}\n{help}", d.message),
        None => d.message.clone(),
    };
    LspDiagnostic {
        range,
        severity: Some(severity(d.severity)),
        code: Some(NumberOrString::String(d.code.as_str().to_owned())),
        source: Some("fai".to_owned()),
        message,
        ..LspDiagnostic::default()
    }
}

fn severity(severity: fai_diagnostics::Severity) -> DiagnosticSeverity {
    match severity {
        fai_diagnostics::Severity::Error => DiagnosticSeverity::ERROR,
        fai_diagnostics::Severity::Warning => DiagnosticSeverity::WARNING,
        fai_diagnostics::Severity::Info => DiagnosticSeverity::INFORMATION,
    }
}

/// Maps an IDE symbol kind to its LSP counterpart.
fn lsp_symbol_kind(kind: fai_ide::repr::SymbolKind) -> LspSymbolKind {
    match kind {
        fai_ide::repr::SymbolKind::Function => LspSymbolKind::FUNCTION,
        fai_ide::repr::SymbolKind::Value => LspSymbolKind::VARIABLE,
        fai_ide::repr::SymbolKind::Module => LspSymbolKind::MODULE,
    }
}
