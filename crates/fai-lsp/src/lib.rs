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
    CodeAction as LspCodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CompletionItem, CompletionItemKind,
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic as LspDiagnostic,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
    DocumentRangeFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    Documentation, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams,
    HoverProviderCapability, InlayHint as LspInlayHint, InlayHintKind, InlayHintLabel,
    InlayHintParams, Location as LspLocation, MarkupContent, MarkupKind, NumberOrString, OneOf,
    ParameterInformation, ParameterLabel, Position, PositionEncodingKind, PrepareRenameResponse,
    PublishDiagnosticsParams, Range, ReferenceParams, RenameOptions, RenameParams, SemanticToken,
    SemanticTokenType, SemanticTokens, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelp as LspSignatureHelp,
    SignatureHelpOptions, SignatureHelpParams, SignatureInformation, SymbolInformation,
    SymbolKind as LspSymbolKind, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit, WorkspaceSymbolParams,
};

mod position;
use position::{Encoding, LineMap};

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
    // Split handshake: read the client's capabilities first so the advertised
    // position encoding can match what the client supports.
    let (id, init_params) = connection.initialize_start()?;
    let encoding = negotiate_encoding(&init_params);
    let capabilities = serde_json::to_value(server_capabilities(encoding))?;
    connection.initialize_finish(id, serde_json::json!({ "capabilities": capabilities }))?;
    let mut server = Server::new(root, encoding)?;
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

/// The encoding's LSP wire kind, for the advertised `position_encoding`.
fn encoding_kind(encoding: Encoding) -> PositionEncodingKind {
    match encoding {
        Encoding::Utf8 => PositionEncodingKind::UTF8,
        Encoding::Utf16 => PositionEncodingKind::UTF16,
    }
}

/// Picks the position encoding: UTF-8 when the client lists it (Fai's native byte
/// offsets, so no re-encoding), else the LSP default of UTF-16.
fn negotiate_encoding(init_params: &serde_json::Value) -> Encoding {
    let offered = init_params
        .get("capabilities")
        .and_then(|c| c.get("general"))
        .and_then(|g| g.get("positionEncodings"))
        .and_then(serde_json::Value::as_array);
    match offered {
        Some(encodings) if encodings.iter().any(|e| e.as_str() == Some("utf-8")) => Encoding::Utf8,
        _ => Encoding::Utf16,
    }
}

/// The capabilities Fai's language server advertises (with the negotiated
/// position `encoding`).
fn server_capabilities(encoding: Encoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding_kind(encoding)),
        // Incremental sync: the client sends only the changed ranges.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        completion_provider: Some(CompletionOptions {
            // `.` triggers member completion; identifier characters trigger
            // automatically without being listed.
            trigger_characters: Some(vec![".".to_owned()]),
            // Items are returned with a kind and rendered type eagerly; the `///`
            // docs and contracts are filled in on `completionItem/resolve`.
            resolve_provider: Some(true),
            ..CompletionOptions::default()
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            // Fai calls are juxtaposition, so a space moves between arguments.
            trigger_characters: Some(vec![" ".to_owned()]),
            ..SignatureHelpOptions::default()
        }),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: fai_ide::SEMANTIC_TOKEN_TYPES
                        .iter()
                        .map(|&t| SemanticTokenType::new(t))
                        .collect(),
                    token_modifiers: vec![],
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..SemanticTokensOptions::default()
            },
        )),
        ..ServerCapabilities::default()
    }
}

/// The server's mutable state: the warm session plus the open buffers.
struct Server {
    session: Session,
    root: Utf8PathBuf,
    /// Open documents' current text, by URI.
    open: HashMap<Url, String>,
    /// The negotiated position encoding for all conversions.
    encoding: Encoding,
}

impl Server {
    fn new(root: Utf8PathBuf, encoding: Encoding) -> Result<Self, Box<dyn Error>> {
        let root = root.canonicalize_utf8().unwrap_or(root);
        let session = Session::open(root.clone())?;
        Ok(Self { session, root, open: HashMap::new(), encoding })
    }

    /// A line map over `text` under the negotiated position encoding.
    fn line_map<'t>(&self, text: &'t str) -> LineMap<'t> {
        LineMap::with_encoding(text, self.encoding)
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
                if let Ok(p) = note.extract::<DidChangeTextDocumentParams>("textDocument/didChange")
                {
                    let uri = p.text_document.uri;
                    self.apply_changes(&uri, p.content_changes);
                    self.refresh(conn, &uri);
                }
            }
            "textDocument/didSave" => {
                if let Ok(p) = note.extract::<DidSaveTextDocumentParams>("textDocument/didSave") {
                    // The buffer is already authoritative; just re-run diagnostics
                    // (also picking up the included text when the client sends it).
                    if let Some(text) = p.text {
                        self.open.insert(p.text_document.uri.clone(), text);
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
                    // Clear the closed file's diagnostics, then refresh the rest:
                    // reverting may have changed what other open files see.
                    self.publish(conn, uri, vec![]);
                    self.publish_all_open(conn);
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
            "textDocument/rangeFormatting" => {
                if let Ok((id, params)) =
                    req.extract::<DocumentRangeFormattingParams>("textDocument/rangeFormatting")
                {
                    respond(conn, id, &self.range_formatting(&params));
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
            "textDocument/prepareRename" => {
                if let Ok((id, params)) =
                    req.extract::<TextDocumentPositionParams>("textDocument/prepareRename")
                {
                    respond(conn, id, &self.prepare_rename(&params));
                }
            }
            "textDocument/rename" => {
                if let Ok((id, params)) = req.extract::<RenameParams>("textDocument/rename") {
                    respond(conn, id, &self.rename(&params));
                }
            }
            "textDocument/completion" => {
                if let Ok((id, params)) = req.extract::<CompletionParams>("textDocument/completion")
                {
                    respond(conn, id, &self.completion(&params));
                }
            }
            "completionItem/resolve" => {
                if let Ok((id, item)) = req.extract::<CompletionItem>("completionItem/resolve") {
                    respond(conn, id, &self.resolve_completion(item));
                }
            }
            "textDocument/signatureHelp" => {
                if let Ok((id, params)) =
                    req.extract::<SignatureHelpParams>("textDocument/signatureHelp")
                {
                    respond(conn, id, &self.signature_help(&params));
                }
            }
            "textDocument/codeAction" => {
                if let Ok((id, params)) = req.extract::<CodeActionParams>("textDocument/codeAction")
                {
                    respond(conn, id, &self.code_actions(&params));
                }
            }
            "textDocument/inlayHint" => {
                if let Ok((id, params)) = req.extract::<InlayHintParams>("textDocument/inlayHint") {
                    respond(conn, id, &self.inlay_hints(&params));
                }
            }
            "textDocument/semanticTokens/full" => {
                if let Ok((id, params)) =
                    req.extract::<SemanticTokensParams>("textDocument/semanticTokens/full")
                {
                    respond(conn, id, &self.semantic_tokens(&params));
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
        // The type, labelled with the referenced name when there is one, …
        let signature = match &result.name {
            Some(name) => format!("{name} : {}", ty.display),
            None => ty.display.clone(),
        };
        let mut value = format!("```fai\n{signature}\n```");
        // … then the definition's doc prose, …
        if let Some(doc) = &result.doc {
            value.push_str("\n\n");
            value.push_str(&doc.markdown);
        }
        // … then its attached contracts as a fenced block.
        if !result.contracts.is_empty() {
            value.push_str("\n\n```fai\n");
            for contract in &result.contracts {
                value.push_str(contract.source.trim_end());
                value.push('\n');
            }
            value.push_str("```");
        }
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
        let lines = self.line_map(text);
        let range = Range { start: Position { line: 0, character: 0 }, end: lines.end() };
        Some(vec![TextEdit { range, new_text: formatted }])
    }

    fn range_formatting(&self, params: &DocumentRangeFormattingParams) -> Option<Vec<TextEdit>> {
        let uri = &params.text_document.uri;
        let rel = self.relative(uri)?;
        let original = self.open.get(uri)?;
        let file = *self.session.select_files(Some(&rel)).first()?;
        let result = fmt(self.session.db(), &[file]);
        let formatted = &result.files.first()?.formatted;
        // The formatter is whole-file; restrict its edits to the lines the request
        // asked for, so a "format selection" only touches the selection.
        Some(range_formatting_edits(original, formatted, params.range))
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

    fn prepare_rename(&self, params: &TextDocumentPositionParams) -> Option<PrepareRenameResponse> {
        let (file, offset) = self.locate(&params.text_document.uri, params.position)?;
        let target =
            fai_ide::prepare_rename_at(self.session.db(), file, offset, &self.session.resolver())?;
        let range = self.range_in_file(&target.span)?;
        Some(PrepareRenameResponse::RangeWithPlaceholder { range, placeholder: target.name })
    }

    fn rename(&self, params: &RenameParams) -> Option<WorkspaceEdit> {
        let pos = &params.text_document_position;
        let (file, offset) = self.locate(&pos.text_document.uri, pos.position)?;
        let locations = fai_ide::rename_at(
            self.session.db(),
            &self.session.user_files(),
            file,
            offset,
            &params.new_name,
            &self.session.resolver(),
        )?;
        // Group the per-occurrence replacements by file into a workspace edit.
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for loc in locations {
            if let Some(uri) = self.uri_for(&loc.span.file)
                && let Some(range) = self.range_in_file(&loc.span)
            {
                changes
                    .entry(uri)
                    .or_default()
                    .push(TextEdit { range, new_text: params.new_name.clone() });
            }
        }
        Some(WorkspaceEdit { changes: Some(changes), ..WorkspaceEdit::default() })
    }

    fn completion(&self, params: &CompletionParams) -> Option<CompletionResponse> {
        let pos = &params.text_document_position;
        let (file, offset) = self.locate(&pos.text_document.uri, pos.position)?;
        let result = fai_ide::completions_at(self.session.db(), file, offset);
        let items: Vec<CompletionItem> = result
            .items
            .into_iter()
            .map(|i| CompletionItem {
                label: i.label,
                kind: Some(lsp_completion_kind(i.kind)),
                detail: i.detail,
                // The symbol identity for a later `completionItem/resolve`; absent
                // for items without an addressable definition (fields, locals).
                data: i.data.and_then(|d| serde_json::to_value(d).ok()),
                ..CompletionItem::default()
            })
            .collect();
        Some(CompletionResponse::Array(items))
    }

    /// Fills a chosen completion item's documentation lazily: the definition's
    /// `///` doc prose and attached contracts, prefixed with its type signature
    /// (mirroring hover). Items without a `data` identity are returned unchanged.
    fn resolve_completion(&self, mut item: CompletionItem) -> CompletionItem {
        let Some(raw) = item.data.clone() else { return item };
        let Ok(data) = serde_json::from_value::<fai_ide::CompletionData>(raw) else { return item };
        let resolved = fai_ide::completion_docs(
            self.session.db(),
            data.file,
            &data.name,
            &self.session.resolver(),
        );
        // The type signature, then the doc prose, then the contracts — the same
        // composition as hover, except the type comes from the item's own detail
        // (its general scheme) since completion has no use-site instantiation.
        let mut value = String::new();
        if let Some(detail) = &item.detail {
            value.push_str(&format!("```fai\n{} : {detail}\n```", item.label));
        }
        if let Some(doc) = &resolved.doc {
            if !value.is_empty() {
                value.push_str("\n\n");
            }
            value.push_str(&doc.markdown);
        }
        if !resolved.contracts.is_empty() {
            if !value.is_empty() {
                value.push_str("\n\n");
            }
            value.push_str("```fai\n");
            for contract in &resolved.contracts {
                value.push_str(contract.source.trim_end());
                value.push('\n');
            }
            value.push_str("```");
        }
        if !value.is_empty() {
            item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }));
        }
        item
    }

    fn signature_help(&self, params: &SignatureHelpParams) -> Option<LspSignatureHelp> {
        let pos = &params.text_document_position_params;
        let (file, offset) = self.locate(&pos.text_document.uri, pos.position)?;
        let help = fai_ide::signature_help_at(self.session.db(), file, offset)?;
        let parameters = help
            .parameters
            .iter()
            .map(|p| ParameterInformation {
                label: ParameterLabel::LabelOffsets([p.start, p.end]),
                documentation: None,
            })
            .collect();
        let info = SignatureInformation {
            label: help.label,
            documentation: None,
            parameters: Some(parameters),
            active_parameter: Some(help.active_parameter),
        };
        Some(LspSignatureHelp {
            signatures: vec![info],
            active_signature: Some(0),
            active_parameter: Some(help.active_parameter),
        })
    }

    fn code_actions(&self, params: &CodeActionParams) -> Option<CodeActionResponse> {
        let uri = &params.text_document.uri;
        let rel = self.relative(uri)?;
        let file = *self.session.select_files(Some(&rel)).first()?;
        let text = self.open.get(uri)?;
        let lines = self.line_map(text);
        let start = lines.offset(params.range.start) as u32;
        let end = lines.offset(params.range.end) as u32;
        let actions = fai_ide::code_actions_at(
            self.session.db(),
            &self.session.user_files(),
            file,
            start,
            end,
            &self.session.resolver(),
        );
        let response: CodeActionResponse = actions
            .into_iter()
            .map(|action| {
                // Group the action's edits into a workspace edit keyed by file URI.
                let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                for edit in action.edits {
                    if let Some(euri) = self.uri_for(&edit.span.file)
                        && let Some(range) = self.range_in_file(&edit.span)
                    {
                        changes
                            .entry(euri)
                            .or_default()
                            .push(TextEdit { range, new_text: edit.new_text });
                    }
                }
                CodeActionOrCommand::CodeAction(LspCodeAction {
                    title: action.title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..WorkspaceEdit::default()
                    }),
                    ..LspCodeAction::default()
                })
            })
            .collect();
        Some(response)
    }

    fn inlay_hints(&self, params: &InlayHintParams) -> Option<Vec<LspInlayHint>> {
        let uri = &params.text_document.uri;
        let rel = self.relative(uri)?;
        let file = *self.session.select_files(Some(&rel)).first()?;
        let text = self.open.get(uri)?;
        let lines = self.line_map(text);
        let start = lines.offset(params.range.start) as u32;
        let end = lines.offset(params.range.end) as u32;
        let hints = fai_ide::inlay_hints(self.session.db(), file, start, end);
        Some(
            hints
                .into_iter()
                .map(|h| LspInlayHint {
                    position: lines.position(h.offset as usize),
                    label: InlayHintLabel::String(h.label),
                    kind: Some(InlayHintKind::TYPE),
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(true),
                    padding_right: None,
                    data: None,
                })
                .collect(),
        )
    }

    fn semantic_tokens(&self, params: &SemanticTokensParams) -> Option<SemanticTokensResult> {
        let uri = &params.text_document.uri;
        let rel = self.relative(uri)?;
        let file = *self.session.select_files(Some(&rel)).first()?;
        let text = self.open.get(uri)?;
        let lines = self.line_map(text);
        let tokens = fai_ide::semantic_tokens(self.session.db(), file);
        let data = encode_semantic_tokens(text, &lines, &tokens);
        Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data }))
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
            // The hierarchy conveys nesting, so a nested member shows its bare
            // name (`deep`), not its module-qualified one (`Inner.deep`).
            name: node.symbol.name.rsplit('.').next().unwrap_or(&node.symbol.name).to_owned(),
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
        let lines = self.line_map(&text);
        Some(Range {
            start: lines.position(span.byte_start as usize),
            end: lines.position(span.byte_end as usize),
        })
    }

    /// Applies a document's content changes (incremental ranges, or a full-text
    /// replacement when a change carries no range) to its open buffer, in order.
    fn apply_changes(
        &mut self,
        uri: &Url,
        changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
    ) {
        let mut text = self.open.get(uri).cloned().unwrap_or_default();
        for change in changes {
            match change.range {
                Some(range) => {
                    let lines = self.line_map(&text);
                    let start = lines.offset(range.start);
                    let end = lines.offset(range.end).max(start);
                    text.replace_range(start..end, &change.text);
                }
                None => text = change.text,
            }
        }
        self.open.insert(uri.clone(), text);
    }

    /// Overlays a document's current text into the session, then republishes
    /// diagnostics for every open document — a cross-module edit can invalidate
    /// another open file, so all of them are refreshed.
    fn refresh(&mut self, conn: &Connection, uri: &Url) {
        let Some(rel) = self.relative(uri) else { return };
        let Some(text) = self.open.get(uri).cloned() else { return };
        let dirty = [DirtyFile { path: rel.to_string(), hash: None, content: Some(text) }];
        if self.session.apply_dirty(&dirty).is_err() {
            return;
        }
        self.publish_all_open(conn);
    }

    /// Recomputes and publishes diagnostics for every open document.
    fn publish_all_open(&self, conn: &Connection) {
        for (uri, text) in &self.open {
            let diagnostics = match self.relative(uri) {
                Some(rel) => match self.session.select_files(Some(&rel)).first() {
                    Some(&file) => {
                        let result = check(self.session.db(), &[file]);
                        let lines = self.line_map(text);
                        result.diagnostics.iter().map(|d| to_lsp_diagnostic(d, &lines)).collect()
                    }
                    None => vec![],
                },
                None => vec![],
            };
            self.publish(conn, uri, diagnostics);
        }
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
        let offset = self.line_map(text).offset(position) as u32;
        Some((file, offset))
    }

    /// Converts an IDE span (byte offsets) into a range within an open document.
    fn span_range(&self, uri: &Url, span: &fai_ide::repr::SpanJson) -> Option<Range> {
        let text = self.open.get(uri)?;
        let lines = self.line_map(text);
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
        let lines = self.line_map(&text);
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

/// The whole-file formatter's edits restricted to `range`'s lines: the original
/// and formatted texts are line-diffed, and each changed hunk whose original
/// lines overlap the requested span becomes a `TextEdit` replacing those whole
/// lines. So "format selection" only rewrites the selection.
fn range_formatting_edits(original: &str, formatted: &str, range: Range) -> Vec<TextEdit> {
    use similar::{DiffTag, TextDiff};

    if original == formatted {
        return Vec::new();
    }
    // A selection ending at column 0 does not include that trailing line.
    let lo = range.start.line;
    let hi = if range.end.character == 0 && range.end.line > range.start.line {
        range.end.line.saturating_sub(1)
    } else {
        range.end.line
    };
    let overlaps = |o1: u32, o2: u32| {
        if o1 == o2 { o1 >= lo && o1 <= hi + 1 } else { o1 <= hi && o2 > lo }
    };

    let diff = TextDiff::from_lines(original, formatted);
    let new_lines = diff.new_slices();
    let mut edits = Vec::new();
    for op in diff.ops() {
        let (tag, old, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let (o1, o2) = (old.start as u32, old.end as u32);
        if !overlaps(o1, o2) {
            continue;
        }
        edits.push(TextEdit {
            range: Range {
                start: Position { line: o1, character: 0 },
                end: Position { line: o2, character: 0 },
            },
            new_text: new_lines[new.start..new.end].concat(),
        });
    }
    edits
}

/// Delta-encodes engine semantic tokens into the LSP wire form (UTF-16 columns),
/// splitting any token that crosses a line so each emitted token is single-line.
fn encode_semantic_tokens(
    text: &str,
    lines: &LineMap,
    tokens: &[fai_ide::SemToken],
) -> Vec<SemanticToken> {
    let mut out = Vec::new();
    let (mut prev_line, mut prev_char) = (0u32, 0u32);
    for token in tokens {
        let token_type = token.kind.index();
        for (line, character, length) in
            line_pieces(text, lines, token.offset as usize, token.length as usize)
        {
            if length == 0 {
                continue;
            }
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 { character - prev_char } else { character };
            out.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset: 0,
            });
            (prev_line, prev_char) = (line, character);
        }
    }
    out
}

/// Splits the byte range `[offset, offset+length)` into single-line
/// `(line, utf16 start column, utf16 length)` pieces, excluding line terminators.
fn line_pieces(text: &str, lines: &LineMap, offset: usize, length: usize) -> Vec<(u32, u32, u32)> {
    let end = (offset + length).min(text.len());
    let mut pieces = Vec::new();
    let mut cur = offset;
    while cur < end {
        let start_pos = lines.position(cur);
        let next_line_start = lines.line_start(start_pos.line as usize + 1);
        let mut content_end = next_line_start.min(end);
        // Drop a trailing CR/LF so the token covers only the line's content.
        while content_end > cur && matches!(text.as_bytes()[content_end - 1], b'\n' | b'\r') {
            content_end -= 1;
        }
        let end_pos = lines.position(content_end);
        pieces.push((start_pos.line, start_pos.character, end_pos.character - start_pos.character));
        cur = next_line_start.max(cur + 1);
    }
    pieces
}

/// Maps an IDE completion kind to its LSP counterpart.
fn lsp_completion_kind(kind: fai_ide::CompletionKind) -> CompletionItemKind {
    match kind {
        fai_ide::CompletionKind::Function => CompletionItemKind::FUNCTION,
        fai_ide::CompletionKind::Value => CompletionItemKind::VARIABLE,
        fai_ide::CompletionKind::Constructor => CompletionItemKind::CONSTRUCTOR,
        fai_ide::CompletionKind::Field => CompletionItemKind::FIELD,
        fai_ide::CompletionKind::Module => CompletionItemKind::MODULE,
    }
}
