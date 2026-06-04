//! The in-memory diagnostic model.
//!
//! These types hold [`fai_span::Span`]s (byte offsets); resolution to
//! file/line/column happens only at render time via a
//! [`SpanResolver`](fai_span::SpanResolver). The serializable wire form lives in
//! [`crate::wire`].

use fai_span::Span;

use crate::code::DiagnosticCode;
use crate::severity::Severity;

/// A secondary labelled span attached to a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// The span this label points at.
    pub span: Span,
    /// The label text.
    pub message: String,
}

impl Label {
    /// Creates a label at `span` with `message`.
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self { span, message: message.into() }
    }
}

/// A machine-applicable fix: replace the text at `span` with `replacement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The span to replace.
    pub span: Span,
    /// The replacement text.
    pub replacement: String,
}

impl Suggestion {
    /// Creates a suggestion replacing `span` with `replacement`.
    pub fn new(span: Span, replacement: impl Into<String>) -> Self {
        Self { span, replacement: replacement.into() }
    }
}

/// A single diagnostic: a coded, located message with optional labels, help,
/// and machine-applicable suggestions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Stable diagnostic code.
    pub code: DiagnosticCode,
    /// Severity.
    pub severity: Severity,
    /// The primary human-readable message.
    pub message: String,
    /// The primary span the diagnostic points at.
    pub primary: Span,
    /// Secondary labelled spans.
    pub secondary: Vec<Label>,
    /// Optional help text.
    pub help: Option<String>,
    /// Machine-applicable suggested edits.
    pub suggestions: Vec<Suggestion>,
}

impl Diagnostic {
    /// Creates a diagnostic with the given severity.
    pub fn new(
        code: DiagnosticCode,
        severity: Severity,
        message: impl Into<String>,
        primary: Span,
    ) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            primary,
            secondary: Vec::new(),
            help: None,
            suggestions: Vec::new(),
        }
    }

    /// Creates an error-severity diagnostic.
    pub fn error(code: DiagnosticCode, message: impl Into<String>, primary: Span) -> Self {
        Self::new(code, Severity::Error, message, primary)
    }

    /// Creates a warning-severity diagnostic.
    pub fn warning(code: DiagnosticCode, message: impl Into<String>, primary: Span) -> Self {
        Self::new(code, Severity::Warning, message, primary)
    }

    /// Adds help text (builder style).
    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Adds a secondary label (builder style).
    #[must_use]
    pub fn with_label(mut self, label: Label) -> Self {
        self.secondary.push(label);
        self
    }

    /// Adds a suggestion (builder style).
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestions.push(suggestion);
        self
    }
}
