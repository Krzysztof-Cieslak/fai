//! Position conversion between Fai's byte offsets and LSP positions.
//!
//! Fai spans are byte offsets into UTF-8 source; an LSP position is a 0-based line
//! plus a 0-based character offset within that line, measured in the negotiated
//! [`Encoding`] (UTF-16 by default, or UTF-8 when the client offers it). This
//! bridge holds a document's line-start byte offsets so both directions are exact,
//! including non-BMP characters (two UTF-16 units, four UTF-8 bytes).

use lsp_types::Position;

/// The unit an LSP position's `character` field counts, negotiated at
/// initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// `character` counts UTF-8 bytes (matches Fai's native offsets).
    Utf8,
    /// `character` counts UTF-16 code units (the LSP default).
    Utf16,
}

/// A document's line-start byte offsets, for converting between byte offsets and
/// LSP positions under a fixed [`Encoding`].
pub struct LineMap<'a> {
    text: &'a str,
    /// Byte offset of the start of each line (line 0 starts at 0).
    starts: Vec<usize>,
    encoding: Encoding,
}

impl<'a> LineMap<'a> {
    /// Builds the line map for `text` measuring positions in `encoding`.
    #[must_use]
    pub fn with_encoding(text: &'a str, encoding: Encoding) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        Self { text, starts, encoding }
    }

    /// The byte offset of the start of `line` (clamped to the document length).
    #[must_use]
    pub fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.text.len())
    }

    /// The width of `ch` in the active encoding's units.
    fn width(&self, ch: char) -> u32 {
        match self.encoding {
            Encoding::Utf8 => ch.len_utf8() as u32,
            Encoding::Utf16 => ch.len_utf16() as u32,
        }
    }

    /// Converts a byte offset into an LSP position (0-based line, 0-based column
    /// in the active encoding).
    #[must_use]
    pub fn position(&self, byte: usize) -> Position {
        let byte = byte.min(self.text.len());
        // The line is the last line start that is `<= byte`.
        let line = match self.starts.binary_search(&byte) {
            Ok(line) => line,
            Err(next) => next - 1,
        };
        let line_start = self.starts[line];
        let character = match self.encoding {
            Encoding::Utf8 => (byte - line_start) as u32,
            Encoding::Utf16 => self.text[line_start..byte].encode_utf16().count() as u32,
        };
        Position { line: line as u32, character }
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
            units += self.width(ch);
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
        let map = LineMap::with_encoding(text, Encoding::Utf16);
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
        let map = LineMap::with_encoding(text, Encoding::Utf16);
        let x = text.find('x').unwrap();
        let pos = map.position(x);
        // 8 chars before the emoji (`let s = "`), then the emoji is 2 UTF-16 units.
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 9 + 2);
        assert_eq!(map.offset(pos), x);
    }

    #[test]
    fn counts_utf8_bytes_under_utf8_encoding() {
        // Under UTF-8, `character` is a byte count: the emoji is four bytes.
        let text = "let s = \"😀x\"";
        let map = LineMap::with_encoding(text, Encoding::Utf8);
        let x = text.find('x').unwrap();
        let pos = map.position(x);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character as usize, x, "UTF-8 column is the byte offset in the line");
        assert_eq!(map.offset(pos), x);
    }

    #[test]
    fn clamps_out_of_range() {
        let text = "ab\ncd";
        let map = LineMap::with_encoding(text, Encoding::Utf16);
        // A character past the line end clamps to the line end.
        let past = Position { line: 0, character: 99 };
        assert_eq!(map.offset(past), 2);
        // A byte past the document clamps to the final position.
        assert_eq!(map.position(999), map.end());
    }
}
