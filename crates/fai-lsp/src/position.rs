//! Position conversion between Fai's byte offsets and LSP's
//! `(0-based line, 0-based UTF-16 code unit)` positions.
//!
//! Fai spans are byte offsets into UTF-8 source; LSP positions are a 0-based line
//! plus a 0-based UTF-16 character offset within that line (the default LSP
//! `position_encoding`). This bridge holds a document's line-start byte offsets so
//! both directions are exact, including non-BMP characters (which are one UTF-16
//! unit per surrogate).

use lsp_types::Position;

/// A document's line-start byte offsets, for converting between byte offsets and
/// LSP positions.
pub struct LineMap<'a> {
    text: &'a str,
    /// Byte offset of the start of each line (line 0 starts at 0).
    starts: Vec<usize>,
}

impl<'a> LineMap<'a> {
    /// Builds the line map for `text`.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { text, starts }
    }

    /// The byte offset of the start of `line` (clamped to the document length).
    #[must_use]
    pub fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.text.len())
    }

    /// Converts a byte offset into an LSP position (0-based line, 0-based UTF-16).
    #[must_use]
    pub fn position(&self, byte: usize) -> Position {
        let byte = byte.min(self.text.len());
        // The line is the last line start that is `<= byte`.
        let line = match self.starts.binary_search(&byte) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        let line_start = self.starts[line];
        let character = self.text[line_start..byte].encode_utf16().count();
        Position { line: line as u32, character: character as u32 }
    }

    /// Converts an LSP position into a byte offset into the document.
    #[must_use]
    pub fn offset(&self, position: Position) -> usize {
        let line = position.line as usize;
        let line_start = self.line_start(line);
        let line_end = self.starts.get(line + 1).copied().unwrap_or(self.text.len());
        let line_text = &self.text[line_start..line_end];
        let mut units = 0u32;
        let mut byte = line_start;
        for ch in line_text.chars() {
            // Stop at the requested column, and never count the line terminator
            // (an out-of-range column clamps to the line's content end, not past
            // the newline into the next line).
            if units >= position.character || ch == '\n' || ch == '\r' {
                break;
            }
            units += ch.len_utf16() as u32;
            byte += ch.len_utf8();
        }
        byte
    }

    /// The position at the very end of the document (for a whole-document edit).
    #[must_use]
    pub fn end(&self) -> Position {
        self.position(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_ascii() {
        let text = "module M\n\nlet x = 1\n";
        let map = LineMap::new(text);
        // `x` is on line 2 (0-based), char 4.
        let byte = text.find('x').unwrap();
        let pos = map.position(byte);
        assert_eq!(pos, Position { line: 2, character: 4 });
        assert_eq!(map.offset(pos), byte);
    }

    #[test]
    fn counts_utf16_units_for_non_bmp() {
        // A non-BMP scalar (😀) is two UTF-16 units but one char.
        let text = "let s = \"😀x\"";
        let map = LineMap::new(text);
        let x = text.find('x').unwrap();
        let pos = map.position(x);
        // 8 chars before the emoji (`let s = "`), then the emoji is 2 UTF-16 units.
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 9 + 2);
        assert_eq!(map.offset(pos), x);
    }

    #[test]
    fn clamps_out_of_range() {
        let text = "ab\ncd";
        let map = LineMap::new(text);
        // A character past the line end clamps to the line end.
        let past = Position { line: 0, character: 99 };
        assert_eq!(map.offset(past), 2);
        // A byte past the document clamps to the final position.
        assert_eq!(map.position(999), map.end());
    }
}
