//! Niche representation of the standard-library `Option`.
//!
//! A monomorphic `Option P` whose payload `P` occupies a single runtime tag-class
//! is represented **without** a `Some` wrapper cell: `Some x` is the payload `x`
//! itself, and `None` is a sentinel drawn from the tag-class the payload leaves
//! free. Which sentinel — and so which payloads qualify — is the [`NicheKind`]:
//!
//! * [`NicheKind::A`] — the payload is *always a boxed pointer* (a `String`,
//!   tuple, non-empty record, array, function/closure, interface, or an ADT with
//!   no nullary constructor). `None` is the immediate `1`; a boxed `Some p` is
//!   distinguishable from it by the low tag bit.
//! * [`NicheKind::B`] — any other monomorphic payload (`Int`, `Bool`, `Char`,
//!   `List`, a nullary-bearing ADT, …). `None` is a single global boxed sentinel,
//!   distinct from every `Some` value (immediate or boxed).
//!
//! Excluded (kept as a standard boxed ADT): a `Float` payload (a `Some Float`
//! boxes the `f64` either way, no saving), a non-monomorphic payload (a type
//! variable), and a payload that is itself a niche `Option` (nesting — the single
//! global sentinel would alias `Some None`).
//!
//! The decision is type-directed and made once, at lowering (which has the
//! database needed to recognize the prelude `Option` and to inspect an ADT's
//! constructors). It is then carried explicitly on the IR — codegen never
//! recomputes it, since the wire form strips the ADT identity the worker would
//! need.

use fai_db::Db;
use fai_resolve::{AdtRef, prelude_source, type_decls};
use fai_span::SourceId;
use fai_types::{Con, Ty};

/// How a niche `Option`'s `None` is encoded (and thus which tag-class its payload
/// occupies). See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum NicheKind {
    /// Always-boxed payload: `None` is the immediate `1`, `Some p` is the pointer.
    A,
    /// Possibly-immediate payload: `None` is the global boxed sentinel, `Some x`
    /// is `x` in its uniform representation.
    B,
}

/// The niche scheme for `ty` if it is a niche-eligible monomorphic prelude
/// `Option`, else `None` (a standard boxed `Option`, or not an `Option` at all).
#[must_use]
pub fn niche_scheme(db: &dyn Db, ty: &Ty) -> Option<NicheKind> {
    scheme_for(db, ty, prelude_source(db))
}

/// As [`niche_scheme`], reusing an already-resolved prelude `SourceId` (avoiding a
/// repeated lookup when classifying many types).
fn scheme_for(db: &dyn Db, ty: &Ty, prelude: Option<SourceId>) -> Option<NicheKind> {
    let (adt, payload) = option_application(ty)?;
    if adt.name.as_str() != "Option" || Some(adt.file) != prelude {
        return None;
    }
    payload_scheme(db, payload, prelude)
}

/// Decomposes `App(Adt, payload)` into its head ADT and argument.
fn option_application(ty: &Ty) -> Option<(&AdtRef, &Ty)> {
    match ty {
        Ty::App(head, arg) => match head.as_ref() {
            Ty::Adt(adt) => Some((adt, arg)),
            _ => None,
        },
        _ => None,
    }
}

/// The scheme for a niche `Option` carrying `payload`, or `None` if `payload`
/// disqualifies it (a `Float`, a non-monomorphic type, or a nested niche
/// `Option`).
fn payload_scheme(db: &dyn Db, payload: &Ty, prelude: Option<SourceId>) -> Option<NicheKind> {
    match payload {
        // A `Some Float` boxes the `f64` either way (no wrapper saving), and a type
        // variable / error / effect is not a known monomorphic representation.
        Ty::Con(Con::Float) | Ty::Var(_) | Ty::Error | Ty::EffectArg(_) => None,
        _ => {
            // Nesting: a niche `Option` payload would let `Some None` alias the
            // outer `None`, so it stays standard.
            if scheme_for(db, payload, prelude).is_some() {
                return None;
            }
            if always_boxed(db, payload) { Some(NicheKind::A) } else { Some(NicheKind::B) }
        }
    }
}

/// Whether values of `payload` are *always* a boxed pointer (never an immediate
/// and never the global sentinel) — the Scheme-A condition. Excludes `Float`
/// (handled in [`payload_scheme`]); a bare `Int`/`Bool`/`Char`/`Unit`/`List`,
/// the empty record, and a nullary-bearing ADT are not always boxed (Scheme B).
fn always_boxed(db: &dyn Db, payload: &Ty) -> bool {
    match payload {
        Ty::Con(Con::String | Con::Bytes | Con::Array)
        | Ty::Tuple(_)
        | Ty::Interface(_)
        | Ty::Arrow(..) => true,
        Ty::Record(row) => !row.fields.is_empty(),
        Ty::Adt(adt) => adt_all_non_nullary(db, adt),
        Ty::App(head, _) => boxed_head(db, head),
        _ => false,
    }
}

/// The always-boxed test on a type-application head (peeling further `App`s): an
/// `Array` application is boxed; a `List` application is not (it has `[]`); an ADT
/// application is boxed iff the ADT has no nullary constructor.
fn boxed_head(db: &dyn Db, head: &Ty) -> bool {
    match head {
        Ty::Con(Con::Array) => true,
        Ty::Con(Con::List) => false,
        Ty::Adt(adt) => adt_all_non_nullary(db, adt),
        Ty::App(inner, _) => boxed_head(db, inner),
        _ => false,
    }
}

/// Whether `adt` is a discriminated union whose every constructor takes at least
/// one field (so every value is a boxed cell, never a nullary immediate). A
/// transparent alias or an unknown type is conservatively not always boxed.
fn adt_all_non_nullary(db: &dyn Db, adt: &AdtRef) -> bool {
    let Some(file) = db.source_file(adt.file) else {
        return false;
    };
    let decls = type_decls(db, file);
    let Some(info) = decls.type_named(adt.name) else {
        return false;
    };
    if info.is_alias || info.ctors.is_empty() {
        return false;
    }
    info.ctors.iter().all(|&c| decls.ctor(c).is_some_and(|ci| ci.arity >= 1))
}

#[cfg(test)]
mod tests {
    //! Eligibility of `niche_scheme`: which monomorphic `Option P` types get a
    //! wrapper-free representation, and under which scheme. One focused `#[test]`
    //! per case.

    use std::sync::Arc;

    use fai_db::{Db, FaiDatabase};
    use fai_resolve::{AdtRef, prelude_source};
    use fai_syntax::Symbol;
    use fai_types::{Con, Ty};

    use super::{NicheKind, niche_scheme};

    /// A database with the standard library loaded plus a user module declaring an
    /// always-boxed ADT (`Boxed`, all constructors non-nullary), a nullary-bearing
    /// ADT (`Mixed`), and a user-defined `Option` (to test name-vs-identity).
    fn db() -> FaiDatabase {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source(
            "M.fai".into(),
            concat!(
                "module M\n\n",
                "type Boxed =\n  | A Int\n  | B Int\n\n",
                "type Mixed =\n  | Empty\n  | Full Int\n\n",
                "type Option 'a =\n  | MyNone\n  | MySome 'a\n",
            )
            .to_owned(),
        );
        db
    }

    /// The prelude `Option` applied to `payload`.
    fn option_of(db: &FaiDatabase, payload: Ty) -> Ty {
        let sid = prelude_source(db).expect("prelude loaded");
        Ty::App(Arc::new(Ty::Adt(AdtRef::new(sid, Symbol::intern("Option")))), Arc::new(payload))
    }

    /// A user ADT named `name`, declared in the (single) user source file.
    fn user_adt(db: &FaiDatabase, name: &str) -> Ty {
        let file =
            db.all_source_files().into_iter().find(|f| f.path(db) == "M.fai").expect("user module");
        Ty::Adt(AdtRef::new(file.source(db), Symbol::intern(name)))
    }

    #[test]
    fn string_payload_is_scheme_a() {
        let db = db();
        assert_eq!(niche_scheme(&db, &option_of(&db, Ty::Con(Con::String))), Some(NicheKind::A));
    }

    #[test]
    fn tuple_payload_is_scheme_a() {
        let db = db();
        let tuple = Ty::Tuple(vec![Ty::int(), Ty::list(Ty::int())]);
        assert_eq!(niche_scheme(&db, &option_of(&db, tuple)), Some(NicheKind::A));
    }

    #[test]
    fn array_payload_is_scheme_a() {
        let db = db();
        assert_eq!(niche_scheme(&db, &option_of(&db, Ty::array(Ty::int()))), Some(NicheKind::A));
    }

    #[test]
    fn arrow_payload_is_scheme_a() {
        let db = db();
        let f = Ty::arrow(Ty::int(), Ty::int());
        assert_eq!(niche_scheme(&db, &option_of(&db, f)), Some(NicheKind::A));
    }

    #[test]
    fn all_non_nullary_adt_payload_is_scheme_a() {
        let db = db();
        assert_eq!(
            niche_scheme(&db, &option_of(&db, user_adt(&db, "Boxed"))),
            Some(NicheKind::A),
            "an ADT whose every constructor takes a field is always boxed"
        );
    }

    #[test]
    fn int_payload_is_scheme_b() {
        let db = db();
        assert_eq!(niche_scheme(&db, &option_of(&db, Ty::int())), Some(NicheKind::B));
    }

    #[test]
    fn bool_payload_is_scheme_b() {
        let db = db();
        assert_eq!(niche_scheme(&db, &option_of(&db, Ty::bool())), Some(NicheKind::B));
    }

    #[test]
    fn list_payload_is_scheme_b() {
        let db = db();
        assert_eq!(niche_scheme(&db, &option_of(&db, Ty::list(Ty::int()))), Some(NicheKind::B));
    }

    #[test]
    fn nullary_bearing_adt_payload_is_scheme_b() {
        let db = db();
        assert_eq!(
            niche_scheme(&db, &option_of(&db, user_adt(&db, "Mixed"))),
            Some(NicheKind::B),
            "an ADT with a nullary constructor straddles immediate/boxed"
        );
    }

    #[test]
    fn float_payload_is_excluded() {
        let db = db();
        assert_eq!(
            niche_scheme(&db, &option_of(&db, Ty::Con(Con::Float))),
            None,
            "a Some Float boxes the f64 either way, so no niche"
        );
    }

    #[test]
    fn type_variable_payload_is_excluded() {
        let db = db();
        let var = Ty::Var(fai_types::TyVarId(0));
        assert_eq!(niche_scheme(&db, &option_of(&db, var)), None, "not monomorphic");
    }

    #[test]
    fn nested_niche_option_payload_is_excluded() {
        let db = db();
        // `Option (Option Int)`: the inner `Option Int` is niche (Scheme B), so the
        // outer must stay standard (a single sentinel would alias `Some None`).
        let inner = option_of(&db, Ty::int());
        assert_eq!(niche_scheme(&db, &option_of(&db, inner)), None);
    }

    #[test]
    fn option_of_a_standard_option_is_scheme_b() {
        let db = db();
        // The inner `Option Float` is *not* niche (Float excluded), so it is an
        // ordinary nullary-bearing payload — the outer niches under Scheme B.
        let inner = option_of(&db, Ty::Con(Con::Float));
        assert_eq!(niche_scheme(&db, &option_of(&db, inner)), Some(NicheKind::B));
    }

    #[test]
    fn non_option_type_is_not_niche() {
        let db = db();
        assert_eq!(niche_scheme(&db, &Ty::list(Ty::int())), None);
        assert_eq!(niche_scheme(&db, &Ty::int()), None);
    }

    #[test]
    fn user_defined_option_is_not_the_prelude_option() {
        let db = db();
        // A user `type Option` shares the name but not the declaring file, so it is
        // never niched (its layout is unknown to this representation).
        let user_option = user_adt(&db, "Option");
        let applied = Ty::App(Arc::new(user_option), Arc::new(Ty::Con(Con::String)));
        assert_eq!(niche_scheme(&db, &applied), None);
    }
}
