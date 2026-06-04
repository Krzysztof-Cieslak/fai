//! Stable, position-independent identities for resolution.
//!
//! Every identity here is **span-free** so it can travel inside salsa-cached
//! values without breaking early cutoff: editing a body or reformatting a file
//! never changes a [`DefId`]. Module identity is the file's [`SourceId`] (its
//! path), per the M2 decision that a module *is* its file; the header name is a
//! separate, validated-unique display/addressing label (see `module` queries).

use fai_span::SourceId;
use fai_syntax::Symbol;

/// A top-level value binding, keyed by its file and name.
///
/// The file (a [`SourceId`]) is the module's stable identity; the name is the
/// binding's. Two bindings with the same name in one file are a duplicate-
/// definition error, so this pair is unique for well-formed input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefId {
    /// The defining file (the module's identity).
    pub file: SourceId,
    /// The binding's name.
    pub name: Symbol,
}

impl DefId {
    /// Builds a `DefId` for `name` defined in `file`.
    #[must_use]
    pub fn new(file: SourceId, name: Symbol) -> Self {
        Self { file, name }
    }
}

/// A local binding slot (a `let`/lambda/parameter variable) within one body.
///
/// Allocated densely per body during resolution; meaningful only relative to the
/// body it was produced for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId(u32);

impl LocalId {
    /// Builds a local id from a raw slot index.
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        Self(u32::try_from(index).expect("local slot overflow"))
    }

    /// The backing slot index.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a reference resolved to.
///
/// Produced for each referencing expression; the [`Res::Error`] case is the
/// resolution sentinel that suppresses downstream cascades (a name that could
/// not be resolved still yields a well-formed result so inference can proceed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Res {
    /// A local variable (parameter, lambda binder, or local `let`).
    Local(LocalId),
    /// A top-level definition (this module's, or another module's public one).
    Def(DefId),
    /// A built-in prelude name (primitive or `.fai`-prelude export).
    Builtin(Symbol),
    /// Resolution failed; bound to the error sentinel.
    Error,
}

/// Returns whether `name` is in the upper-case (constructor/module) namespace.
///
/// Mirrors the lexer's rule (`fai-syntax` lexer): an identifier is `UpperIdent`
/// iff its first byte is ASCII-uppercase. Resolution re-derives this from the
/// interned text to classify a `Field` base as a qualified module reference
/// (`Foo.bar`) versus record field access.
#[must_use]
pub fn is_upper(name: Symbol) -> bool {
    name.as_str().as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn def_id_is_copy_and_eq() {
        let f = SourceId::new(0);
        let a = DefId::new(f, Symbol::intern("map"));
        let b = DefId::new(f, Symbol::intern("map"));
        let c = DefId::new(SourceId::new(1), Symbol::intern("map"));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn local_id_round_trips() {
        assert_eq!(LocalId::from_index(7).index(), 7);
    }

    #[test]
    fn casing_matches_lexer_rule() {
        assert!(is_upper(Symbol::intern("Foo")));
        assert!(!is_upper(Symbol::intern("foo")));
        assert!(!is_upper(Symbol::intern("_x")));
    }
}
