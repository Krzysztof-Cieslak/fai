//! Diagnostic severity levels.

use serde::Serialize;

/// How serious a diagnostic is.
///
/// Serializes to the lowercase strings `"error"`, `"warning"`, `"info"` for the
/// JSON wire schema (`docs/CLI.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A hard error; compilation/operation does not succeed.
    Error,
    /// A warning; the operation can still succeed.
    Warning,
    /// Informational note.
    Info,
}

impl Severity {
    /// The lowercase string form used in human and JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    /// Returns `true` if this severity denotes an error.
    #[must_use]
    pub const fn is_error(self) -> bool {
        matches!(self, Severity::Error)
    }
}
