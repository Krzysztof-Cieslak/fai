//! The type representation.
//!
//! A reified [`Ty`] is an immutable, structural, span-free value (so it is a
//! sound salsa value: `Eq`/`Hash` by structure, no `'db` lifetime). Inference
//! solves over mutable type *variables* in a separate context (see `infer`);
//! the principal type it finds is reified back into this representation for
//! caching and export. A [`Scheme`] adds the quantified variables of a
//! generalized binding.

use std::fmt::{self, Write as _};
use std::sync::Arc;

/// A type-variable identifier (a slot in the solver's union-find).
///
/// In a *reified* type, a `TyVarId` only appears inside a [`Scheme`]'s body,
/// where it ranges over the scheme's quantified variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVarId(pub u32);

/// A type.
///
/// `Arc` gives cheap sharing without a global arena; structural `Eq`/`Hash` make
/// it a sound salsa value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    /// A (quantified, in a scheme) type variable.
    Var(TyVarId),
    /// A nullary type constructor: `Int`, `Float`, `Bool`, `String`, `Char`.
    Con(Con),
    /// Type application, e.g. `List a` is `App(List, a)`.
    App(Arc<Ty>, Arc<Ty>),
    /// A function type `from -> to`.
    Arrow(Arc<Ty>, Arc<Ty>),
    /// A tuple type (two or more elements).
    Tuple(Vec<Ty>),
    /// The unit type `()`.
    Unit,
    /// The error type: unifies with anything, suppresses cascades.
    Error,
}

/// A type constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Con {
    /// `Int`.
    Int,
    /// `Float`.
    Float,
    /// `Bool`.
    Bool,
    /// `String`.
    String,
    /// `Char`.
    Char,
    /// `List` (a unary constructor, applied via [`Ty::App`]).
    List,
    /// `Runtime`: the opaque built-in capability bundle passed to `main`. A
    /// placeholder until records/interfaces give it real structure; for now it is
    /// a nullary constructor threaded through `main`.
    Runtime,
}

impl Con {
    /// The constructor's display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Con::Int => "Int",
            Con::Float => "Float",
            Con::Bool => "Bool",
            Con::String => "String",
            Con::Char => "Char",
            Con::List => "List",
            Con::Runtime => "Runtime",
        }
    }

    /// Parses a constructor name, if known.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Int" => Con::Int,
            "Float" => Con::Float,
            "Bool" => Con::Bool,
            "String" => Con::String,
            "Char" => Con::Char,
            "List" => Con::List,
            "Runtime" => Con::Runtime,
            _ => return None,
        })
    }
}

/// Lowers a type-constructor name to a [`Ty`], handling `Unit` (which has its own
/// [`Ty::Unit`] form rather than a [`Con`]). Returns `None` for unknown names.
#[must_use]
pub fn con_or_unit(name: &str) -> Option<Ty> {
    if name == "Unit" { Some(Ty::Unit) } else { Con::from_name(name).map(Ty::Con) }
}

impl Ty {
    /// The `Bool` type.
    #[must_use]
    pub fn bool() -> Ty {
        Ty::Con(Con::Bool)
    }

    /// The `Int` type.
    #[must_use]
    pub fn int() -> Ty {
        Ty::Con(Con::Int)
    }

    /// A `List t` type.
    #[must_use]
    pub fn list(elem: Ty) -> Ty {
        Ty::App(Arc::new(Ty::Con(Con::List)), Arc::new(elem))
    }

    /// A function type `from -> to`.
    #[must_use]
    pub fn arrow(from: Ty, to: Ty) -> Ty {
        Ty::Arrow(Arc::new(from), Arc::new(to))
    }

    /// Builds a curried arrow `a -> b -> ... -> result`.
    #[must_use]
    pub fn arrows(params: impl IntoIterator<Item = Ty>, result: Ty) -> Ty {
        let params: Vec<Ty> = params.into_iter().collect();
        let mut ty = result;
        for p in params.into_iter().rev() {
            ty = Ty::arrow(p, ty);
        }
        ty
    }
}

/// A (possibly polymorphic) type scheme: `forall vars. ty`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scheme {
    /// The quantified variables (empty for a monomorphic type).
    pub vars: Vec<TyVarId>,
    /// The body type.
    pub ty: Ty,
    /// Preferred display spelling for each variable, parallel to `vars` (e.g. the
    /// letters written in a signature). Empty when the scheme is inferred, in
    /// which case rendering falls back to canonical names.
    pub names: Vec<String>,
}

impl Scheme {
    /// A monomorphic scheme over `ty`.
    #[must_use]
    pub fn mono(ty: Ty) -> Self {
        Self { vars: Vec::new(), ty, names: Vec::new() }
    }

    /// A scheme with explicit quantified variables (canonical naming).
    #[must_use]
    pub fn new(vars: Vec<TyVarId>, ty: Ty) -> Self {
        Self { vars, ty, names: Vec::new() }
    }

    /// Attaches preferred variable spellings (parallel to `vars`).
    #[must_use]
    pub fn with_names(mut self, names: Vec<String>) -> Self {
        self.names = names;
        self
    }
}

/// Renders a type to its canonical display string (e.g.
/// `('a -> 'b) -> List 'a -> List 'b`).
///
/// `names` supplies preferred variable spellings (e.g. from a written
/// signature); any variable without an entry is named canonically (`'a`, `'b`,
/// … in first-appearance order).
#[must_use]
pub fn render(ty: &Ty, names: &VarNames) -> String {
    let mut out = String::new();
    write_ty(&mut out, ty, names, Prec::Top);
    out
}

/// Renders a scheme (the body only — `forall` is implicit in Fai surface types).
#[must_use]
pub fn render_scheme(scheme: &Scheme) -> String {
    render(&scheme.ty, &VarNames::canonical(scheme))
}

/// Renders a standalone type with canonical variable names (`'a`, `'b`, … in
/// first-appearance order), independent of any other type's variable numbering.
#[must_use]
pub fn render_canonical(ty: &Ty) -> String {
    let mut names = VarNames::new();
    let mut order: Vec<TyVarId> = Vec::new();
    collect_vars(ty, &mut order);
    for (i, v) in order.into_iter().enumerate() {
        names.set(v, canonical_name(i));
    }
    render(ty, &names)
}

/// Collects a type's variables in first-appearance order (no duplicates).
fn collect_vars(ty: &Ty, out: &mut Vec<TyVarId>) {
    match ty {
        Ty::Var(v) => {
            if !out.contains(v) {
                out.push(*v);
            }
        }
        Ty::App(f, a) | Ty::Arrow(f, a) => {
            collect_vars(f, out);
            collect_vars(a, out);
        }
        Ty::Tuple(elems) => {
            for e in elems {
                collect_vars(e, out);
            }
        }
        Ty::Con(_) | Ty::Unit | Ty::Error => {}
    }
}

/// A mapping from type variables to their display spellings.
#[derive(Debug, Default, Clone)]
pub struct VarNames {
    names: rustc_hash::FxHashMap<TyVarId, String>,
}

impl VarNames {
    /// Empty: every variable gets a canonical name on demand.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Assigns names to a scheme's quantified variables.
    ///
    /// If the scheme carries preferred spellings (e.g. from a written signature),
    /// those are used; otherwise variables are named canonically (`'a`, `'b`, …)
    /// in ascending id order.
    #[must_use]
    pub fn canonical(scheme: &Scheme) -> Self {
        let mut names = rustc_hash::FxHashMap::default();
        if scheme.names.len() == scheme.vars.len() && !scheme.names.is_empty() {
            for (v, name) in scheme.vars.iter().zip(&scheme.names) {
                names.insert(*v, name.clone());
            }
            return Self { names };
        }
        let mut vars = scheme.vars.clone();
        vars.sort();
        for (i, v) in vars.into_iter().enumerate() {
            names.insert(v, canonical_name(i));
        }
        Self { names }
    }

    /// Records a preferred spelling for a variable.
    pub fn set(&mut self, var: TyVarId, name: String) {
        self.names.insert(var, name);
    }

    fn get(&self, var: TyVarId) -> String {
        self.names.get(&var).cloned().unwrap_or_else(|| {
            // Stable fallback for unnamed vars: derive from the id.
            canonical_name(var.0 as usize)
        })
    }
}

/// The canonical name for the `n`th variable: `'a`, `'b`, …, `'z`, `'a1`, …
fn canonical_name(n: usize) -> String {
    let letter = (b'a' + (n % 26) as u8) as char;
    let suffix = n / 26;
    if suffix == 0 { format!("'{letter}") } else { format!("'{letter}{suffix}") }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prec {
    /// Top level (no surrounding context).
    Top,
    /// Right of an arrow, or an arrow's parameter (loosest that still allows a
    /// bare tuple `a * b` without parentheses).
    Arrow,
    /// Tuple-element context: an arrow here must parenthesize, a tuple need not.
    Product,
    /// Application argument: tuples and arrows must parenthesize.
    App,
    /// Atomic position.
    Atom,
}

fn write_ty(out: &mut String, ty: &Ty, names: &VarNames, prec: Prec) {
    match ty {
        Ty::Var(v) => {
            let _ = out.write_str(&names.get(*v));
        }
        Ty::Con(c) => {
            let _ = out.write_str(c.name());
        }
        Ty::Unit => {
            let _ = out.write_str("()");
        }
        Ty::Error => {
            let _ = out.write_str("{error}");
        }
        Ty::App(func, arg) => {
            let parenthesize = prec > Prec::App;
            if parenthesize {
                let _ = out.write_char('(');
            }
            write_ty(out, func, names, Prec::App);
            let _ = out.write_char(' ');
            write_ty(out, arg, names, Prec::Atom);
            if parenthesize {
                let _ = out.write_char(')');
            }
        }
        Ty::Arrow(from, to) => {
            // An arrow must be parenthesized when it appears inside a tuple
            // element or an application argument, but not at the top level or to
            // the right of another arrow (arrows are right-associative).
            let parenthesize = prec >= Prec::Product;
            if parenthesize {
                let _ = out.write_char('(');
            }
            write_ty(out, from, names, Prec::Product);
            let _ = out.write_str(" -> ");
            write_ty(out, to, names, Prec::Arrow);
            if parenthesize {
                let _ = out.write_char(')');
            }
        }
        Ty::Tuple(elems) => {
            // A tuple binds looser than application; in an argument position (App
            // or Atom) — including nested directly inside another tuple — it must
            // be parenthesized, but as an arrow parameter (`a * b -> c`) it need
            // not be.
            let parenthesize = prec >= Prec::App;
            if parenthesize {
                let _ = out.write_char('(');
            }
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    let _ = out.write_str(" * ");
                }
                write_ty(out, e, names, Prec::App);
            }
            if parenthesize {
                let _ = out.write_char(')');
            }
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render(self, &VarNames::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(n: u32) -> Ty {
        Ty::Var(TyVarId(n))
    }

    #[test]
    fn renders_arrow_and_app() {
        // ('a -> 'b) -> List 'a -> List 'b
        let scheme = Scheme::new(
            vec![TyVarId(0), TyVarId(1)],
            Ty::arrows([Ty::arrow(v(0), v(1)), Ty::list(v(0))], Ty::list(v(1))),
        );
        assert_eq!(render_scheme(&scheme), "('a -> 'b) -> List 'a -> List 'b");
    }

    #[test]
    fn renders_tuple() {
        let scheme = Scheme::mono(Ty::Tuple(vec![Ty::int(), Ty::bool()]));
        assert_eq!(render_scheme(&scheme), "Int * Bool");
    }

    #[test]
    fn arrow_is_right_associative() {
        let scheme = Scheme::mono(Ty::arrows([Ty::int(), Ty::int()], Ty::int()));
        assert_eq!(render_scheme(&scheme), "Int -> Int -> Int");
    }

    #[test]
    fn con_round_trips() {
        for c in [Con::Int, Con::Float, Con::Bool, Con::String, Con::Char, Con::List, Con::Runtime]
        {
            assert_eq!(Con::from_name(c.name()), Some(c));
        }
        assert_eq!(Con::from_name("Widget"), None);
    }
}
