//! Code intelligence for Fai: the engine behind `fai query` and the LSP.
//!
//! This crate turns the resolution and type queries into the read-only,
//! JSON-shaped answers specified in `docs/CLI.md` §8 (`symbols`, `def`, `refs`,
//! `type`, `docs`, `outline`, `api`, `dependents`, the call hierarchy, `caps`,
//! and `search`). It also answers position-based queries — [`hover_at`] and
//! [`definition_at`], keyed by a byte offset — that power the language server.
//! It produces no diagnostics of its own, so it owns no `FAInnnn` codes.

pub mod code_action;
pub mod completion;
pub mod highlight;
pub mod query;
pub mod repr;
pub mod target;

pub use code_action::{CodeAction, CodeActionEdit, code_actions_at};
pub use completion::{CompletionItem, CompletionKind, CompletionResult, completions_at};
pub use highlight::{
    InlayHint, SEMANTIC_TOKEN_TYPES, SemKind, SemToken, inlay_hints, semantic_tokens,
};
pub use query::{
    ApiResult, CallEdge, CallHierarchyResult, CapsResult, DefResult, DependentsResult, DocsResult,
    HoverResult, ListOpts, OutlineNode, OutlineResult, ParamInfo, RefsResult, RenameTarget,
    SearchHit, SearchResult, SignatureHelp, SymbolsResult, TypeResult, api, callees, callers, caps,
    def, definition_at, dependents, docs, document_symbols, hover_at, outline, prepare_rename_at,
    references_at, refs, rename_at, search, signature_help_at, symbols, type_at, workspace_symbols,
};
pub use target::{ResolvedTarget, resolve_target};
