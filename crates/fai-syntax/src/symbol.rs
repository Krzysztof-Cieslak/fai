//! Interned identifiers.
//!
//! Identifiers, labels, and other repeated strings intern to [`Symbol`], a
//! `Copy` handle compared by id. Interning is backed by a process-global
//! [`lasso::ThreadedRodeo`], so a `Symbol` carries no lifetime and equality is
//! stable within a process — which keeps the span-free item tree a plain `Eq`
//! value, so early cutoff stays sound. The global interner never shrinks; for the
//! long-lived daemon its storage can later move behind this same API.

use std::fmt;
use std::sync::LazyLock;

use lasso::{Spur, ThreadedRodeo};

/// The process-global string interner.
static INTERNER: LazyLock<ThreadedRodeo> = LazyLock::new(ThreadedRodeo::new);

/// An interned string, compared by id.
///
/// Construct with [`Symbol::intern`] and read back with [`Symbol::as_str`].
/// `Symbol` deliberately does **not** implement `Ord`: ids reflect interning
/// order, not lexicographic order, so any *observable* ordering must sort by
/// [`Symbol::as_str`] to stay deterministic.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(Spur);

impl Symbol {
    /// Interns `text`, returning a stable handle. Equal strings yield equal
    /// symbols within a process.
    #[must_use]
    pub fn intern(text: &str) -> Self {
        Self(INTERNER.get_or_intern(text))
    }

    /// Returns the interned string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        INTERNER.resolve(&self.0)
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({:?})", self.as_str())
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Symbol;

    #[test]
    fn equal_strings_intern_equal() {
        let a = Symbol::intern("alpha");
        let b = Symbol::intern("alpha");
        let c = Symbol::intern("beta");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn round_trips_to_str() {
        let s = Symbol::intern("getX");
        assert_eq!(s.as_str(), "getX");
        assert_eq!(s.to_string(), "getX");
    }

    #[test]
    fn debug_shows_text() {
        assert_eq!(format!("{:?}", Symbol::intern("x")), "Symbol(\"x\")");
    }
}
