//! Line/column mapping.
//!
//! [`LineIndex`] precomputes the byte offset of every line start so a
//! [`ByteOffset`] can be mapped to a [`LineCol`]. Columns count **Unicode
//! scalar values** (chars), 1-based, matching rustc's human-facing columns.
//! Byte offsets remain the authoritative machine coordinate;
//! line/column is the display coordinate.

use std::fmt;

use crate::span::ByteOffset;

/// A 1-based line and column position.
///
/// `column` counts Unicode scalar values (chars) from the start of the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineCol {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number, counted in Unicode scalar values.
    pub column: u32,
}

impl fmt::Display for LineCol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A precomputed table of line-start byte offsets for one source file.
///
/// Build it once per file text with [`LineIndex::new`]; convert offsets with
/// [`LineIndex::line_col`]. The same `text` used to build the index must be
/// passed to `line_col` (the index stores only line starts, not the text, to
/// avoid duplicating the salsa-owned source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Byte offset of the start of each line. Always begins with `0`.
    line_starts: Vec<u32>,
    /// Total length of the text in bytes.
    len: u32,
}

impl LineIndex {
    /// Builds a line index for `text`.
    #[must_use]
    pub fn new(text: &str) -> Self {
        debug_assert!(
            text.len() <= u32::MAX as usize,
            "source files larger than 4 GiB are not supported"
        );
        let mut line_starts = Vec::with_capacity(text.len() / 32 + 1);
        line_starts.push(0u32);
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(ByteOffset::from_usize(i + 1).raw());
            }
        }
        Self { line_starts, len: ByteOffset::from_usize(text.len()).raw() }
    }

    /// Rebuilds a line index from previously computed line starts.
    ///
    /// Lets the query engine memoize the line-start table (a plain `Vec<u32>`)
    /// and hand it back here without re-scanning the text. `len_bytes` is the
    /// byte length of the text the starts were computed from.
    #[must_use]
    pub fn from_line_starts(line_starts: Vec<u32>, len_bytes: u32) -> Self {
        debug_assert!(line_starts.first() == Some(&0), "line starts must begin with 0");
        Self { line_starts, len: len_bytes }
    }

    /// The byte offsets of each line start (always begins with `0`).
    #[must_use]
    pub fn line_starts(&self) -> &[u32] {
        &self.line_starts
    }

    /// The number of lines in the file (always at least 1).
    #[must_use]
    pub fn line_count(&self) -> u32 {
        ByteOffset::from_usize(self.line_starts.len()).raw()
    }

    /// Maps a byte offset to its 1-based line number (no text needed; columns
    /// require [`line_col`](LineIndex::line_col)).
    ///
    /// An `offset` past the end of the text is clamped to the last line.
    #[must_use]
    pub fn line(&self, offset: ByteOffset) -> u32 {
        let offset = offset.raw().min(self.len);
        let line = match self.line_starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(insertion) => insertion - 1,
        };
        ByteOffset::from_usize(line).raw() + 1
    }

    /// Maps a byte offset to a 1-based [`LineCol`].
    ///
    /// `text` must be the same text used to build this index. An `offset` past
    /// the end of the text is clamped to the end; an `offset` that does not fall
    /// on a `char` boundary counts every char that starts before it.
    #[must_use]
    pub fn line_col(&self, text: &str, offset: ByteOffset) -> LineCol {
        let offset = offset.raw().min(self.len);
        // Largest line index whose start offset is <= `offset`.
        let line = match self.line_starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(insertion) => insertion - 1,
        };
        let line_start = self.line_starts[line] as usize;
        let target = offset as usize;
        // Count Unicode scalar values from the line start up to `offset`.
        let mut column = 0u32;
        for (byte_in_slice, _) in text[line_start..].char_indices() {
            if line_start + byte_in_slice >= target {
                break;
            }
            column += 1;
        }
        LineCol { line: ByteOffset::from_usize(line).raw() + 1, column: column + 1 }
    }
}
