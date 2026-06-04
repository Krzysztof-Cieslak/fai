//! Source identifiers and byte-offset spans.
//!
//! Spans are split into two types, mirroring the "spans in a side-table" design
//! (`Agents.md` §9):
//!
//! * [`TextRange`] is **file-relative** — a `[start, end)` byte range with no
//!   notion of which file it belongs to. AST/IR side-tables store these, since
//!   the owning file is constant per tree.
//! * [`Span`] is **file-qualified** — a [`SourceId`] plus a [`TextRange`]. This
//!   is what diagnostics and the public API carry, so a single span is
//!   self-describing (including for cross-file secondary labels).
//!
//! Byte offsets are `u32` ([`ByteOffset`]): source files are limited to 4 GiB,
//! which keeps positions compact (rust-analyzer makes the same trade-off).

/// Identifies a single source file within a [`SourceMap`](crate::SourceMap) or
/// the query database.
///
/// This is an opaque, position-independent handle: it never encodes a path or
/// byte offset, so it stays stable across edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(u32);

impl SourceId {
    /// Creates a `SourceId` from a raw index.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw index backing this id.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns the index as a `usize`, for indexing into a registry.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A byte offset into a source file.
///
/// Offsets are `u32`; source files larger than 4 GiB are not supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteOffset(u32);

impl ByteOffset {
    /// The zero offset (start of file).
    pub const ZERO: Self = Self(0);

    /// Creates a `ByteOffset` from a raw byte position.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw byte position.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns the offset as a `usize`, for slicing into text.
    #[must_use]
    pub const fn to_usize(self) -> usize {
        self.0 as usize
    }

    /// Creates a `ByteOffset` from a `usize` byte position.
    ///
    /// # Panics
    ///
    /// Panics if `value` exceeds [`u32::MAX`] — i.e. the source file is larger
    /// than 4 GiB, which is an unsupported compiler invariant rather than a
    /// user-program error.
    #[must_use]
    pub fn from_usize(value: usize) -> Self {
        Self(u32::try_from(value).expect("source files larger than 4 GiB are not supported"))
    }
}

impl From<u32> for ByteOffset {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

/// A file-relative half-open byte range `[start, end)`.
///
/// Invariant: `start <= end`. The default is the empty range at offset 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TextRange {
    start: ByteOffset,
    end: ByteOffset,
}

impl TextRange {
    /// Creates a range from `start` to `end`.
    ///
    /// # Panics
    ///
    /// Panics if `start > end` (a compiler invariant).
    #[must_use]
    pub fn new(start: ByteOffset, end: ByteOffset) -> Self {
        assert!(start.raw() <= end.raw(), "TextRange start must be <= end");
        Self { start, end }
    }

    /// Creates an empty range at `offset`.
    #[must_use]
    pub fn empty(offset: ByteOffset) -> Self {
        Self { start: offset, end: offset }
    }

    /// The start offset.
    #[must_use]
    pub const fn start(self) -> ByteOffset {
        self.start
    }

    /// The end offset.
    #[must_use]
    pub const fn end(self) -> ByteOffset {
        self.end
    }

    /// The length of the range in bytes.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.raw() - self.start.raw()
    }

    /// Returns `true` if the range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.raw() == self.end.raw()
    }

    /// Returns `true` if `offset` lies within `[start, end)`.
    #[must_use]
    pub fn contains(self, offset: ByteOffset) -> bool {
        self.start.raw() <= offset.raw() && offset.raw() < self.end.raw()
    }
}

/// A file-qualified source span: a [`SourceId`] plus a [`TextRange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    source: SourceId,
    range: TextRange,
}

impl Span {
    /// Creates a span in `source` covering `range`.
    #[must_use]
    pub const fn new(source: SourceId, range: TextRange) -> Self {
        Self { source, range }
    }

    /// The file this span belongs to.
    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }

    /// The file-relative range.
    #[must_use]
    pub const fn range(self) -> TextRange {
        self.range
    }

    /// The start offset.
    #[must_use]
    pub const fn start(self) -> ByteOffset {
        self.range.start()
    }

    /// The end offset.
    #[must_use]
    pub const fn end(self) -> ByteOffset {
        self.range.end()
    }
}
