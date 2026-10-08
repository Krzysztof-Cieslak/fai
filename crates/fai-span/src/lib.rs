//! Source identifiers, byte spans, source maps, and line/column mapping.
//!
//! This is the lowest crate in the Fai compiler stack: it has no dependency on
//! the query engine (`fai-db`) or serialization. It provides the position
//! vocabulary every later phase shares:
//!
//! * [`SourceId`] / [`ByteOffset`] / [`TextRange`] / [`Span`] — the span types.
//! * [`LineIndex`] / [`LineCol`] — line/column mapping (1-based, char columns).
//! * [`SourceMap`] — a database-agnostic source registry (tests / one-shot).
//! * [`SpanResolver`] / [`ResolvedSpan`] — the seam renderers use to resolve a
//!   span to a path + line/column without depending on the query engine.

mod line;
mod resolver;
mod source_map;
mod span;

pub use line::{LineCol, LineIndex};
pub use resolver::{ResolvedSpan, SpanResolver};
pub use source_map::SourceMap;
pub use span::{ByteOffset, SourceId, Span, TextRange};

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: SourceId, start: u32, end: u32) -> Span {
        Span::new(id, TextRange::new(ByteOffset::new(start), ByteOffset::new(end)))
    }

    #[test]
    fn line_index_ascii() {
        let text = "abc\ndef\nghi";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 3);
        // Start of file.
        assert_eq!(idx.line_col(text, ByteOffset::new(0)), LineCol { line: 1, column: 1 });
        // 'c' on line 1.
        assert_eq!(idx.line_col(text, ByteOffset::new(2)), LineCol { line: 1, column: 3 });
        // Start of line 2 ('d').
        assert_eq!(idx.line_col(text, ByteOffset::new(4)), LineCol { line: 2, column: 1 });
        // 'g' on line 3.
        assert_eq!(idx.line_col(text, ByteOffset::new(8)), LineCol { line: 3, column: 1 });
    }

    #[test]
    fn line_index_line_number() {
        let text = "abc\ndef\nghi";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line(ByteOffset::new(0)), 1);
        assert_eq!(idx.line(ByteOffset::new(2)), 1);
        assert_eq!(idx.line(ByteOffset::new(3)), 1); // the newline ends line 1
        assert_eq!(idx.line(ByteOffset::new(4)), 2); // start of line 2
        assert_eq!(idx.line(ByteOffset::new(8)), 3);
        assert_eq!(idx.line(ByteOffset::new(99)), 3); // clamped past the end
    }

    #[test]
    fn line_index_newline_position() {
        // The '\n' byte itself reports as the end of its line.
        let text = "ab\ncd";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_col(text, ByteOffset::new(2)), LineCol { line: 1, column: 3 });
    }

    #[test]
    fn line_index_multibyte_columns_count_chars() {
        // "héllo": 'h'=1 byte, 'é'=2 bytes (0xC3 0xA9), then "llo".
        let text = "héllo";
        let idx = LineIndex::new(text);
        // Byte offset 3 is just after 'é' (bytes 1..3), i.e. before the first 'l'.
        // That is the 3rd character → column 3.
        assert_eq!(idx.line_col(text, ByteOffset::new(3)), LineCol { line: 1, column: 3 });
    }

    #[test]
    fn line_index_offset_inside_multibyte_char() {
        // Offset 2 falls inside 'é' (bytes 1..3). Per the documented rule we count
        // every char that *starts* before the offset ('h' at 0 and 'é' at 1), so a
        // mid-char offset maps to the same column as the next boundary (offset 3).
        let text = "héllo";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_col(text, ByteOffset::new(2)), LineCol { line: 1, column: 3 });
        assert_eq!(idx.line_col(text, ByteOffset::new(3)), LineCol { line: 1, column: 3 });
    }

    #[test]
    fn line_index_offset_past_end_is_clamped() {
        let text = "ab";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_col(text, ByteOffset::new(99)), LineCol { line: 1, column: 3 });
    }

    #[test]
    fn empty_text_has_one_line() {
        let text = "";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 1);
        assert_eq!(idx.line_col(text, ByteOffset::new(0)), LineCol { line: 1, column: 1 });
    }

    fn offset(text: &str, line: u32, column: u32) -> Option<u32> {
        LineIndex::new(text).offset(text, LineCol { line, column }).map(ByteOffset::raw)
    }

    #[test]
    fn inverse_columns_count_bmp_scalars() {
        assert_eq!(offset("éa", 1, 2), Some(2));
    }

    #[test]
    fn inverse_columns_count_non_bmp_scalars() {
        assert_eq!(offset("é😀a", 1, 3), Some(6));
    }

    #[test]
    fn inverse_empty_file_starts_at_zero() {
        assert_eq!(offset("", 1, 1), Some(0));
    }

    #[test]
    fn inverse_zero_line_is_invalid() {
        assert_eq!(offset("a", 0, 1), None);
    }

    #[test]
    fn inverse_zero_column_is_invalid() {
        assert_eq!(offset("a", 1, 0), None);
    }

    #[test]
    fn inverse_line_past_eof_is_invalid() {
        assert_eq!(offset("a", 2, 1), None);
    }

    #[test]
    fn inverse_column_past_eol_is_invalid() {
        assert_eq!(offset("a\nb", 1, 3), None);
    }

    #[test]
    fn inverse_end_of_line_addresses_the_newline() {
        assert_eq!(offset("é\nb", 1, 2), Some(2));
    }

    #[test]
    fn inverse_eof_is_a_valid_position() {
        assert_eq!(offset("é😀", 1, 3), Some(6));
    }

    #[test]
    fn inverse_trailing_newline_has_an_empty_final_line() {
        assert_eq!(offset("é\n", 2, 1), Some(3));
    }

    #[test]
    fn inverse_crlf_matches_the_forward_mapping() {
        assert_eq!(offset("é\r\n😀", 1, 3), Some(3));
        assert_eq!(offset("é\r\n😀", 2, 1), Some(4));
    }

    #[test]
    fn source_map_add_and_lookup() {
        let mut map = SourceMap::new();
        let id = map.add("src/Main.fai".into(), "module Main\n".to_owned());
        assert_eq!(map.source_id("src/Main.fai".into()), Some(id));
        assert_eq!(map.path(id).unwrap(), "src/Main.fai");
        assert_eq!(map.text(id), Some("module Main\n"));
    }

    #[test]
    fn source_map_re_add_reuses_id() {
        let mut map = SourceMap::new();
        let id1 = map.add("a.fai".into(), "old".to_owned());
        let id2 = map.add("a.fai".into(), "new".to_owned());
        assert_eq!(id1, id2);
        assert_eq!(map.text(id1), Some("new"));
    }

    #[test]
    fn source_map_resolves_span() {
        let mut map = SourceMap::new();
        let id = map.add("src/M.fai".into(), "let x =\n  1\n".to_owned());
        // "1" is at byte offset 10 (after "let x =\n  ").
        let resolved = map.resolve(span(id, 10, 11)).unwrap();
        assert_eq!(resolved.path, "src/M.fai");
        assert_eq!(resolved.start, LineCol { line: 2, column: 3 });
        assert_eq!(resolved.end, LineCol { line: 2, column: 4 });
        assert_eq!(resolved.byte_start, 10);
        assert_eq!(resolved.byte_end, 11);
    }

    #[test]
    fn source_map_resolve_unknown_source_is_none() {
        let map = SourceMap::new();
        assert!(map.resolve(span(SourceId::new(7), 0, 1)).is_none());
    }

    #[test]
    fn text_range_basics() {
        let r = TextRange::new(ByteOffset::new(3), ByteOffset::new(7));
        assert_eq!(r.len(), 4);
        assert!(!r.is_empty());
        assert!(r.contains(ByteOffset::new(3)));
        assert!(r.contains(ByteOffset::new(6)));
        assert!(!r.contains(ByteOffset::new(7)));
        assert!(TextRange::empty(ByteOffset::new(5)).is_empty());
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn line_and_column_round_trip_every_scalar_boundary(text in any::<String>()) {
            let index = LineIndex::new(&text);
            for offset in text.char_indices().map(|(offset, _)| offset).chain(std::iter::once(text.len())) {
                let offset = ByteOffset::from_usize(offset);
                prop_assert_eq!(index.offset(&text, index.line_col(&text, offset)), Some(offset));
            }
        }
    }
}
