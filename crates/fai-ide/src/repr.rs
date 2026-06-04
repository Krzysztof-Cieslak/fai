//! The JSON shapes returned by `fai query` (see `docs/CLI.md` §4).
//!
//! These are the stable, versioned wire types. Spans are resolved late from a
//! [`SpanResolver`] so semantic values stay free of byte offsets.

use fai_span::{Span, SpanResolver};
use serde::Serialize;

/// The query output schema version (kept in step with the diagnostics schema).
pub const SCHEMA_VERSION: u32 = fai_diagnostics::SCHEMA_VERSION;

/// A 1-based source position.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Position {
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
}

/// A source range with byte offsets (CLI.md `Span`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SpanJson {
    /// File path.
    pub file: String,
    /// Start position.
    pub start: Position,
    /// End position.
    pub end: Position,
    /// Start byte offset.
    #[serde(rename = "byteStart")]
    pub byte_start: u32,
    /// End byte offset.
    #[serde(rename = "byteEnd")]
    pub byte_end: u32,
}

impl SpanJson {
    /// Resolves a [`Span`] into wire form, or `None` if its source is unknown.
    #[must_use]
    pub fn resolve(span: Span, resolver: &dyn SpanResolver) -> Option<SpanJson> {
        let r = resolver.resolve(span)?;
        Some(SpanJson {
            file: r.path.to_string(),
            start: Position { line: r.start.line, column: r.start.column },
            end: Position { line: r.end.line, column: r.end.column },
            byte_start: r.byte_start,
            byte_end: r.byte_end,
        })
    }
}

/// A span plus an optional one-line preview (CLI.md `Location`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Location {
    /// The location's span.
    pub span: SpanJson,
    /// A one-line preview of the source, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// The kind of a symbol (CLI.md `SymbolRef.kind`).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    /// A function or value binding.
    Function,
    /// A value (non-function) binding.
    Value,
    /// A module.
    Module,
}

/// Visibility of a symbol.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Exported.
    Public,
    /// Module-private.
    Private,
}

/// A named, addressable definition (CLI.md `SymbolRef`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SymbolRef {
    /// Dotted path, e.g. `Collections.map`.
    pub path: String,
    /// The binding's name.
    pub name: String,
    /// The symbol's kind.
    pub kind: SymbolKind,
    /// The owning module name.
    pub module: String,
    /// Visibility.
    pub visibility: Visibility,
    /// The written/inferred signature, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The definition's span.
    pub span: SpanJson,
}

/// A rendered type (CLI.md `TypeRepr`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TypeRepr {
    /// The display form, e.g. `('a -> 'b) -> List 'a -> List 'b`.
    pub display: String,
}

/// Human prose attached to a symbol (CLI.md `Doc`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Doc {
    /// Markdown text.
    pub markdown: String,
}

/// A checked fact attached to a symbol (CLI.md `Contract`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Contract {
    /// `"example"` or `"forall"`.
    pub kind: String,
    /// Universally-quantified binders (`[]` for `example`).
    pub binders: Vec<String>,
    /// The contract's source text.
    pub source: String,
    /// The contract's span.
    pub span: SpanJson,
}
