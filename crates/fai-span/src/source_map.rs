//! A lightweight, database-agnostic source registry.
//!
//! [`SourceMap`] owns file paths, text, and line indices keyed by [`SourceId`].
//! In the incremental daemon the authoritative text lives in the salsa
//! `SourceFile` input; `SourceMap` is the standalone store used by
//! tests and any one-shot path, and serves as a ready-made [`SpanResolver`].

use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};

use crate::line::{LineCol, LineIndex};
use crate::resolver::{ResolvedSpan, SpanResolver};
use crate::span::{ByteOffset, SourceId, Span};

/// One registered source file: its path, text, and precomputed line index.
#[derive(Debug)]
struct SourceFile {
    path: Utf8PathBuf,
    text: String,
    line_index: LineIndex,
}

/// A registry mapping [`SourceId`]s to file paths and text.
#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
    by_path: HashMap<Utf8PathBuf, SourceId>,
}

impl SourceMap {
    /// Creates an empty source map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a file, returning its [`SourceId`].
    ///
    /// If `path` was already registered, its entry is replaced and the existing
    /// id is reused (so spans referring to it stay valid).
    pub fn add(&mut self, path: Utf8PathBuf, text: String) -> SourceId {
        let line_index = LineIndex::new(&text);
        if let Some(&id) = self.by_path.get(&path) {
            self.files[id.index()] = SourceFile { path, text, line_index };
            return id;
        }
        let id = SourceId::new(ByteOffset::from_usize(self.files.len()).raw());
        self.by_path.insert(path.clone(), id);
        self.files.push(SourceFile { path, text, line_index });
        id
    }

    /// Looks up the [`SourceId`] previously assigned to `path`.
    #[must_use]
    pub fn source_id(&self, path: &Utf8Path) -> Option<SourceId> {
        self.by_path.get(path).copied()
    }

    /// Returns the path of `id`, if registered.
    #[must_use]
    pub fn path(&self, id: SourceId) -> Option<&Utf8Path> {
        self.files.get(id.index()).map(|f| f.path.as_path())
    }

    /// Returns the text of `id`, if registered.
    #[must_use]
    pub fn text(&self, id: SourceId) -> Option<&str> {
        self.files.get(id.index()).map(|f| f.text.as_str())
    }

    /// Returns the line index of `id`, if registered.
    #[must_use]
    pub fn line_index(&self, id: SourceId) -> Option<&LineIndex> {
        self.files.get(id.index()).map(|f| &f.line_index)
    }

    /// Maps a byte offset within `id` to a [`LineCol`].
    #[must_use]
    pub fn line_col(&self, id: SourceId, offset: ByteOffset) -> Option<LineCol> {
        let file = self.files.get(id.index())?;
        Some(file.line_index.line_col(&file.text, offset))
    }
}

impl SpanResolver for SourceMap {
    fn resolve(&self, span: Span) -> Option<ResolvedSpan> {
        let file = self.files.get(span.source().index())?;
        let start = file.line_index.line_col(&file.text, span.start());
        let end = file.line_index.line_col(&file.text, span.end());
        Some(ResolvedSpan {
            path: file.path.clone(),
            start,
            end,
            byte_start: span.start().raw(),
            byte_end: span.end().raw(),
        })
    }
}
