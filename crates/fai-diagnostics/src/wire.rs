//! The stable JSON wire schema for diagnostics (`docs/CLI.md` §4).
//!
//! These serializable types are a versioned public API (see
//! [`crate::SCHEMA_VERSION`]). They are produced from the in-memory
//! [`Diagnostic`] model by resolving spans through a
//! [`SpanResolver`](fai_span::SpanResolver), so positions carry both
//! line/column and byte offsets. Paths are reported exactly as the resolver
//! yields them (relativization to the workspace root is the resolver's job).
//! `serde_json` itself is invoked by `fai-cli`; this crate only
//! defines the `Serialize` shapes.

use fai_span::{Span, SpanResolver};
use serde::Serialize;

use crate::diagnostic::{Diagnostic, Label, Suggestion};
use crate::order::sort_order;
use crate::severity::Severity;

/// A 1-based source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Position {
    /// 1-based line.
    pub line: u32,
    /// 1-based column (Unicode scalar values).
    pub column: u32,
}

/// A resolved source span with both line/column and byte offsets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanWire {
    /// Workspace-relative file path.
    pub file: String,
    /// Start position.
    pub start: Position,
    /// End position.
    pub end: Position,
    /// Start byte offset.
    pub byte_start: u32,
    /// End byte offset.
    pub byte_end: u32,
}

/// A secondary labelled span in the wire schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LabelWire {
    /// The labelled span.
    pub span: SpanWire,
    /// The label text.
    pub label: String,
}

/// A machine-applicable suggestion in the wire schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestionWire {
    /// The span to replace.
    pub span: SpanWire,
    /// The replacement text.
    pub replacement: String,
}

/// A diagnostic in the wire schema (`docs/CLI.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnosticWire {
    /// Stable `FAInnnn` code.
    pub code: String,
    /// Severity.
    pub severity: Severity,
    /// Human-readable message.
    pub message: String,
    /// Primary span.
    pub primary: SpanWire,
    /// Secondary labelled spans.
    pub secondary: Vec<LabelWire>,
    /// Optional help text (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Machine-applicable suggestions.
    pub suggestions: Vec<SuggestionWire>,
}

/// Converts a single span to its wire form, degrading gracefully if the span's
/// source is unknown to the resolver.
fn span_to_wire(span: Span, resolver: &dyn SpanResolver) -> SpanWire {
    match resolver.resolve(span) {
        Some(r) => SpanWire {
            file: r.path.into_string(),
            start: Position { line: r.start.line, column: r.start.column },
            end: Position { line: r.end.line, column: r.end.column },
            byte_start: r.byte_start,
            byte_end: r.byte_end,
        },
        None => SpanWire {
            file: String::from("<unknown>"),
            start: Position { line: 0, column: 0 },
            end: Position { line: 0, column: 0 },
            byte_start: span.start().raw(),
            byte_end: span.end().raw(),
        },
    }
}

fn label_to_wire(label: &Label, resolver: &dyn SpanResolver) -> LabelWire {
    LabelWire { span: span_to_wire(label.span, resolver), label: label.message.clone() }
}

fn suggestion_to_wire(s: &Suggestion, resolver: &dyn SpanResolver) -> SuggestionWire {
    SuggestionWire { span: span_to_wire(s.span, resolver), replacement: s.replacement.clone() }
}

/// Converts one diagnostic to its wire form.
#[must_use]
pub fn diagnostic_to_wire(diag: &Diagnostic, resolver: &dyn SpanResolver) -> DiagnosticWire {
    DiagnosticWire {
        code: diag.code.as_str().to_owned(),
        severity: diag.severity,
        message: diag.message.clone(),
        primary: span_to_wire(diag.primary, resolver),
        secondary: diag.secondary.iter().map(|l| label_to_wire(l, resolver)).collect(),
        help: diag.help.clone(),
        suggestions: diag.suggestions.iter().map(|s| suggestion_to_wire(s, resolver)).collect(),
    }
}

/// Converts a batch of diagnostics to wire form, sorted deterministically by
/// `(file, byte_start, code)`.
#[must_use]
pub fn to_wire(diags: &[Diagnostic], resolver: &dyn SpanResolver) -> Vec<DiagnosticWire> {
    sort_order(diags, resolver)
        .into_iter()
        .map(|i| diagnostic_to_wire(&diags[i], resolver))
        .collect()
}
