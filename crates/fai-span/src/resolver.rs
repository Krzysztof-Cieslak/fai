//! Resolving a [`Span`] to human/machine coordinates.
//!
//! Renderers (in `fai-diagnostics`) need each [`Span`] turned into a file path
//! plus line/column and byte offsets, but the authoritative source text lives in
//! the salsa database — which `fai-diagnostics` must not depend on. The
//! [`SpanResolver`] trait is the seam: implementors include [`SourceMap`] (for
//! tests / one-shot runs) and a salsa-backed resolver in `fai-db`. Renderers
//! take `&dyn SpanResolver` and stay engine-agnostic.

use camino::Utf8PathBuf;

use crate::line::LineCol;
use crate::span::Span;

/// A [`Span`] resolved to file path, line/column, and byte offsets.
///
/// The `path` is reported as the resolver knows it; relativizing to the
/// workspace root happens at the render boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpan {
    /// Path of the file the span belongs to.
    pub path: Utf8PathBuf,
    /// 1-based start position.
    pub start: LineCol,
    /// 1-based end position.
    pub end: LineCol,
    /// Start byte offset (authoritative machine coordinate).
    pub byte_start: u32,
    /// End byte offset.
    pub byte_end: u32,
}

/// Resolves [`Span`]s into [`ResolvedSpan`]s.
///
/// Returns `None` when the span's source is unknown to the resolver, so callers
/// (renderers) can degrade gracefully rather than panic.
pub trait SpanResolver {
    /// Resolves `span` to a [`ResolvedSpan`], or `None` if its source is unknown.
    fn resolve(&self, span: Span) -> Option<ResolvedSpan>;
}
