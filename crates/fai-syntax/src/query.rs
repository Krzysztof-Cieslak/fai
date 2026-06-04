//! The incremental front-end queries.
//!
//! These are the thin `salsa` wrappers over the pure lexer/layout/parser. The
//! [`parse`] query holds the full AST (it changes on every edit, so it does not
//! cut off); the span-free [`item_tree`] is the firewall: trivia and body edits
//! leave it unchanged, so its dependents (e.g. [`public_binding_count`]) are cut
//! off early. Parse diagnostics are emitted into the [`Diag`](fai_db::Diag)
//! accumulator; callers collect them at the boundary.
//!
//! Equality for the salsa return values comes from `salsa::Update`'s per-field
//! dispatch, which falls back to `PartialEq` for the AST types — so there is no
//! hand-written `unsafe`; only salsa's macro-generated `unsafe` lives here (hence
//! the scoped `allow` on this module).

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Severity;
use fai_span::SourceId;

use crate::Comment;
use crate::ast::{ItemKind, Module, Visibility};
use crate::parse_module;

/// The cached result of parsing one file: the AST plus its comment trivia.
///
/// Diagnostics are not stored here — they flow through the accumulator — but a
/// cheap [`has_errors`](ParsedModule::has_errors) flag lets callers (e.g. `fmt`)
/// skip files that did not parse cleanly without re-collecting them.
#[derive(Debug, PartialEq, Eq, salsa::Update)]
pub struct ParsedModule {
    /// The parsed module.
    pub module: Module,
    /// Comment trivia, in source order.
    pub comments: Vec<Comment>,
    /// Whether parsing produced any error-severity diagnostic.
    pub has_errors: bool,
}

/// A span-free summary of a module's items — the early-cutoff firewall.
///
/// It contains only names, kinds, visibility, and order, so editing a comment or
/// a body leaves it unchanged. Signature *types* are intentionally omitted until
/// resolution needs them.
#[derive(Debug, Clone, PartialEq, Eq, salsa::Update)]
pub struct ItemTree {
    /// The declared module name, if any.
    pub module_name: Option<crate::Symbol>,
    /// The items, in source order.
    pub items: Vec<ItemSummary>,
}

/// One item's position-independent summary in an [`ItemTree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemSummary {
    /// What the item is.
    pub kind: ItemTreeKind,
    /// The item's name (signatures and bindings only).
    pub name: Option<crate::Symbol>,
    /// The item's visibility.
    pub visibility: Visibility,
}

/// The kind of an [`ItemSummary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemTreeKind {
    /// A type signature.
    Signature,
    /// A value binding.
    Binding,
    /// An `example` contract.
    Example,
    /// A `forall` contract.
    Forall,
    /// An unparseable item.
    Error,
}

/// Builds the span-free [`ItemTree`] for a parsed module (pure).
#[must_use]
pub fn build_item_tree(module: &Module) -> ItemTree {
    let items = module
        .items
        .iter()
        .map(|item| {
            let (kind, name, visibility) = match &item.kind {
                ItemKind::Signature { visibility, name, .. } => {
                    (ItemTreeKind::Signature, Some(*name), *visibility)
                }
                ItemKind::Binding { visibility, name, .. } => {
                    (ItemTreeKind::Binding, Some(*name), *visibility)
                }
                ItemKind::Example { .. } => (ItemTreeKind::Example, None, Visibility::Private),
                ItemKind::Forall { .. } => (ItemTreeKind::Forall, None, Visibility::Private),
                ItemKind::Error => (ItemTreeKind::Error, None, Visibility::Private),
            };
            ItemSummary { kind, name, visibility }
        })
        .collect();
    ItemTree { module_name: module.name, items }
}

/// Parses `file`, emitting diagnostics into the accumulator.
#[salsa::tracked]
pub fn parse(db: &dyn Db, file: SourceFile) -> Arc<ParsedModule> {
    let source: SourceId = file.source(db);
    let parsed = parse_module(source, file.text(db).as_str());
    let has_errors = parsed.diagnostics.iter().any(|diag| diag.severity == Severity::Error);
    for diagnostic in parsed.diagnostics {
        emit(db, diagnostic);
    }
    Arc::new(ParsedModule { module: parsed.module, comments: parsed.comments, has_errors })
}

/// The span-free item tree for `file` (the early-cutoff firewall).
#[salsa::tracked]
pub fn item_tree(db: &dyn Db, file: SourceFile) -> ItemTree {
    let parsed = parse(db, file);
    build_item_tree(&parsed.module)
}

/// The number of `public` items in `file` — a small dependent of [`item_tree`]
/// used to demonstrate early cutoff. (In M1 `public` sits on signatures; binding
/// visibility is associated during resolution.)
#[salsa::tracked]
pub fn public_item_count(db: &dyn Db, file: SourceFile) -> usize {
    item_tree(db, file).items.iter().filter(|item| item.visibility == Visibility::Public).count()
}
