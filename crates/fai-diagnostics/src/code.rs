//! Stable diagnostic codes (`FAInnnn`).
//!
//! Codes are an API: never renumbered, allocated by phase (`AGENTS.md` §10).
//! The representation is a thin `&'static str` newtype; each phase crate owns
//! its codes as a `pub const CODES: &[CodeInfo]` slice, and the
//! `fai-tests` crate aggregates them to assert format and global uniqueness.
//!
//! Phase ranges: `FAI0xxx` tooling/CLI, `FAI1xxx` lex/parse, `FAI2xxx`
//! resolve/visibility, `FAI3xxx` types/rows, `FAI4xxx` exhaustiveness/patterns,
//! `FAI5xxx` capabilities, `FAI6xxx` contracts.

use std::fmt;

use crate::severity::Severity;

/// A stable diagnostic code such as `FAI0001`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DiagnosticCode(&'static str);

impl DiagnosticCode {
    /// Creates a code from its string form (e.g. `"FAI0001"`).
    ///
    /// The format is validated by [`DiagnosticCode::has_valid_format`] and the
    /// catalog test, not here, so this stays usable in `const` contexts.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self(code)
    }

    /// The string form, e.g. `"FAI0001"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Returns `true` if the code matches the canonical shape `FAI` followed by
    /// exactly four ASCII digits.
    #[must_use]
    pub fn has_valid_format(self) -> bool {
        let bytes = self.0.as_bytes();
        bytes.len() == 7 && &bytes[0..3] == b"FAI" && bytes[3..].iter().all(u8::is_ascii_digit)
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Catalog metadata for one diagnostic code.
///
/// Phase crates expose `pub const CODES: &[CodeInfo]`; the catalog test checks
/// that every code is well-formed, unique across the workspace, and documented,
/// and renders the error-code catalog (`docs/ERROR_CODES.md`) from these entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeInfo {
    /// The stable code.
    pub code: DiagnosticCode,
    /// A short human title for the catalog.
    pub title: &'static str,
    /// The severity this code is normally emitted at.
    pub default_severity: Severity,
    /// A one-or-two-sentence explanation of what triggers the diagnostic and how
    /// to resolve it — the prose shown in the error-code catalog.
    pub explanation: &'static str,
}
