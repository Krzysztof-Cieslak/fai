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

use fai_resolve::{AdtRef, InterfaceRef};
use fai_syntax::Symbol;

/// A type-variable identifier (a slot in the solver's union-find).
///
/// In a *reified* type, a `TyVarId` only appears inside a [`Scheme`]'s body,
/// where it ranges over the scheme's quantified variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TyVarId(pub u32);

/// A row-variable identifier (a slot in the solver's parallel row union-find).
///
/// Like [`TyVarId`], in a *reified* type it appears only inside a [`Scheme`],
/// where it ranges over the scheme's quantified row variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowVarId(pub u32);

/// An effect-row-variable identifier (a slot in the solver's parallel *effect*-row
/// union-find, distinct from the record-row one in [`RowVarId`]).
///
/// Like [`TyVarId`], in a *reified* type it appears only inside a [`Scheme`],
/// where it ranges over the scheme's quantified effect-row variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EffRowVarId(pub u32);

/// A reified record row: its fields (sorted by label text) and a tail.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordRow {
    /// The present fields, sorted by label text (canonical for layout/cache).
    pub fields: Vec<(Symbol, Ty)>,
    /// The row's tail.
    pub tail: RowEnd,
}

/// The tail of a reified record row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowEnd {
    /// Exactly the listed fields.
    Closed,
    /// The listed fields plus an open (quantified) tail.
    Open(RowVarId),
}

/// The tail of a reified effect row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffEnd {
    /// Exactly the listed effect atoms (a closed effect).
    Closed,
    /// The listed atoms plus an open (quantified) effect tail.
    Open(EffRowVarId),
}

/// A reified effect row: the host-capability atoms a function uses (the interface
/// references, sorted by qualified name for a canonical form) plus a tail.
///
/// An empty, closed effect row is the *pure* effect — the default on every arrow
/// — and renders as nothing (a bare `a -> b`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectRow {
    /// The effect atoms, sorted by interface qualified name (canonical).
    pub labels: Vec<InterfaceRef>,
    /// The row's tail.
    pub tail: EffEnd,
}

impl EffectRow {
    /// The pure (empty, closed) effect row — the default carried by every arrow
    /// until effect inference fills it in.
    #[must_use]
    pub fn pure() -> Self {
        Self { labels: Vec::new(), tail: EffEnd::Closed }
    }

    /// Whether this is the pure effect (no atoms, closed) — i.e. renders as bare.
    #[must_use]
    pub fn is_pure(&self) -> bool {
        self.labels.is_empty() && self.tail == EffEnd::Closed
    }
}

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
    /// A user-declared nominal type constructor (a `type` union), applied via
    /// [`Ty::App`]: `Option a` is `App(Adt(Option), a)`.
    Adt(AdtRef),
    /// A nominal interface type (its values are dictionaries), applied via
    /// [`Ty::App`] for any type parameters.
    Interface(InterfaceRef),
    /// An effect row used as an *interface argument* — the argument supplied for
    /// an interface's effect parameter (`Logger { Console }`). Appears only as a
    /// child of an interface [`Ty::App`]; erased after the front end.
    EffectArg(EffectRow),
    /// Type application, e.g. `List a` is `App(List, a)`.
    App(Arc<Ty>, Arc<Ty>),
    /// A function type `from -> to / effect`. The effect row records the host
    /// capabilities applying the function uses (empty = pure, rendered bare).
    Arrow(Arc<Ty>, Arc<Ty>, EffectRow),
    /// A tuple type (two or more elements).
    Tuple(Vec<Ty>),
    /// A structural record type with a row tail.
    Record(RecordRow),
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
    /// `Bytes` — an immutable contiguous binary byte buffer (distinct from the
    /// UTF-8 `String`; its elements are bytes read/written as `Int` 0–255).
    Bytes,
    /// `Char`.
    Char,
    /// `List` (a unary constructor, applied via [`Ty::App`]).
    List,
    /// `Array` — a contiguous, growable sequence (a unary constructor, applied
    /// via [`Ty::App`]).
    Array,
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
            Con::Bytes => "Bytes",
            Con::Char => "Char",
            Con::List => "List",
            Con::Array => "Array",
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
            "Bytes" => Con::Bytes,
            "Char" => Con::Char,
            "List" => Con::List,
            "Array" => Con::Array,
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

    /// An `Array t` type.
    #[must_use]
    pub fn array(elem: Ty) -> Ty {
        Ty::App(Arc::new(Ty::Con(Con::Array)), Arc::new(elem))
    }

    /// A pure function type `from -> to` (empty effect).
    #[must_use]
    pub fn arrow(from: Ty, to: Ty) -> Ty {
        Ty::Arrow(Arc::new(from), Arc::new(to), EffectRow::pure())
    }

    /// A function type `from -> to / effect`.
    #[must_use]
    pub fn arrow_eff(from: Ty, to: Ty, effect: EffectRow) -> Ty {
        Ty::Arrow(Arc::new(from), Arc::new(to), effect)
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
    /// The quantified row variables (for row-polymorphic records).
    pub row_vars: Vec<RowVarId>,
    /// Preferred spelling for each row variable, parallel to `row_vars` (`_` for
    /// an anonymous open tail, `'r` for a named one).
    pub row_names: Vec<String>,
    /// The quantified effect-row variables (for effect-polymorphic functions).
    pub effect_vars: Vec<EffRowVarId>,
    /// Preferred spelling for each effect variable, parallel to `effect_vars`.
    pub effect_names: Vec<String>,
}

impl Scheme {
    /// A monomorphic scheme over `ty`.
    #[must_use]
    pub fn mono(ty: Ty) -> Self {
        Self {
            vars: Vec::new(),
            ty,
            names: Vec::new(),
            row_vars: Vec::new(),
            row_names: Vec::new(),
            effect_vars: Vec::new(),
            effect_names: Vec::new(),
        }
    }

    /// A scheme with explicit quantified variables (canonical naming).
    #[must_use]
    pub fn new(vars: Vec<TyVarId>, ty: Ty) -> Self {
        Self {
            vars,
            ty,
            names: Vec::new(),
            row_vars: Vec::new(),
            row_names: Vec::new(),
            effect_vars: Vec::new(),
            effect_names: Vec::new(),
        }
    }

    /// Attaches preferred variable spellings (parallel to `vars`).
    #[must_use]
    pub fn with_names(mut self, names: Vec<String>) -> Self {
        self.names = names;
        self
    }

    /// Attaches quantified row variables with their spellings.
    #[must_use]
    pub fn with_rows(mut self, row_vars: Vec<RowVarId>, row_names: Vec<String>) -> Self {
        self.row_vars = row_vars;
        self.row_names = row_names;
        self
    }

    /// Attaches quantified effect-row variables with their spellings.
    #[must_use]
    pub fn with_effects(
        mut self,
        effect_vars: Vec<EffRowVarId>,
        effect_names: Vec<String>,
    ) -> Self {
        self.effect_vars = effect_vars;
        self.effect_names = effect_names;
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

/// Renders an effect row standalone as `{ Console, FileSystem | 'e }` (or `{}`
/// for the pure effect), for diagnostics. Atoms are shown in their stored order.
#[must_use]
pub fn render_effect(eff: &EffectRow) -> String {
    if eff.labels.is_empty() && eff.tail == EffEnd::Closed {
        return "{}".to_owned();
    }
    let mut out = String::from("{");
    for (i, atom) in eff.labels.iter().enumerate() {
        out.push_str(if i == 0 { " " } else { ", " });
        out.push_str(atom.name.as_str());
    }
    if let EffEnd::Open(_) = eff.tail {
        out.push_str(" | _");
    }
    out.push_str(" }");
    out
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
    let mut eff_order: Vec<EffRowVarId> = Vec::new();
    collect_eff_vars(ty, &mut eff_order);
    for (i, v) in eff_order.into_iter().enumerate() {
        names.set_eff(v, eff_canonical_name(i));
    }
    render(ty, &names)
}

/// Collects a type's effect-row variables in first-appearance order (no dups).
fn collect_eff_vars(ty: &Ty, out: &mut Vec<EffRowVarId>) {
    match ty {
        Ty::Arrow(f, a, eff) => {
            collect_eff_vars(f, out);
            collect_eff_vars(a, out);
            if let EffEnd::Open(v) = eff.tail
                && !out.contains(&v)
            {
                out.push(v);
            }
        }
        Ty::App(f, a) => {
            collect_eff_vars(f, out);
            collect_eff_vars(a, out);
        }
        Ty::Tuple(elems) => {
            for e in elems {
                collect_eff_vars(e, out);
            }
        }
        Ty::Record(row) => {
            for (_, t) in &row.fields {
                collect_eff_vars(t, out);
            }
        }
        // An effect argument's own tail variable is an effect-row variable.
        Ty::EffectArg(eff) => {
            if let EffEnd::Open(v) = eff.tail
                && !out.contains(&v)
            {
                out.push(v);
            }
        }
        Ty::Var(_) | Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => {}
    }
}

/// Collects a type's variables in first-appearance order (no duplicates).
fn collect_vars(ty: &Ty, out: &mut Vec<TyVarId>) {
    match ty {
        Ty::Var(v) => {
            if !out.contains(v) {
                out.push(*v);
            }
        }
        Ty::App(f, a) => {
            collect_vars(f, out);
            collect_vars(a, out);
        }
        Ty::Arrow(f, a, _) => {
            collect_vars(f, out);
            collect_vars(a, out);
        }
        Ty::Tuple(elems) => {
            for e in elems {
                collect_vars(e, out);
            }
        }
        Ty::Record(row) => {
            for (_, t) in &row.fields {
                collect_vars(t, out);
            }
        }
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::EffectArg(_) | Ty::Unit | Ty::Error => {}
    }
}

/// A mapping from type and row variables to their display spellings.
#[derive(Debug, Default, Clone)]
pub struct VarNames {
    names: rustc_hash::FxHashMap<TyVarId, String>,
    row_names: rustc_hash::FxHashMap<RowVarId, String>,
    eff_names: rustc_hash::FxHashMap<EffRowVarId, String>,
}

impl VarNames {
    /// Empty: every variable gets a canonical name on demand.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Assigns names to a scheme's quantified type and row variables.
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
        } else {
            let mut vars = scheme.vars.clone();
            vars.sort();
            for (i, v) in vars.into_iter().enumerate() {
                names.insert(v, canonical_name(i));
            }
        }
        let mut row_names = rustc_hash::FxHashMap::default();
        if scheme.row_names.len() == scheme.row_vars.len() {
            for (v, name) in scheme.row_vars.iter().zip(&scheme.row_names) {
                row_names.insert(*v, name.clone());
            }
        }
        let mut eff_names = rustc_hash::FxHashMap::default();
        if scheme.effect_names.len() == scheme.effect_vars.len() {
            for (v, name) in scheme.effect_vars.iter().zip(&scheme.effect_names) {
                eff_names.insert(*v, name.clone());
            }
        }
        Self { names, row_names, eff_names }
    }

    /// Records a preferred spelling for a variable.
    pub fn set(&mut self, var: TyVarId, name: String) {
        self.names.insert(var, name);
    }

    /// Records a preferred spelling for a row variable.
    pub fn set_row(&mut self, var: RowVarId, name: String) {
        self.row_names.insert(var, name);
    }

    /// Records a preferred spelling for an effect-row variable.
    pub fn set_eff(&mut self, var: EffRowVarId, name: String) {
        self.eff_names.insert(var, name);
    }

    fn get(&self, var: TyVarId) -> String {
        self.names.get(&var).cloned().unwrap_or_else(|| {
            // Stable fallback for unnamed vars: derive from the id.
            canonical_name(var.0 as usize)
        })
    }

    /// The spelling of a row variable's open tail (`_` when anonymous).
    fn get_row(&self, var: RowVarId) -> String {
        self.row_names.get(&var).cloned().unwrap_or_else(|| "_".to_owned())
    }

    /// The spelling of an effect-row variable's open tail (`_` when anonymous).
    fn get_eff(&self, var: EffRowVarId) -> String {
        self.eff_names.get(&var).cloned().unwrap_or_else(|| "_".to_owned())
    }
}

/// The canonical name for the `n`th variable: `'a`, `'b`, …, `'z`, `'a1`, …
fn canonical_name(n: usize) -> String {
    let letter = (b'a' + (n % 26) as u8) as char;
    let suffix = n / 26;
    if suffix == 0 { format!("'{letter}") } else { format!("'{letter}{suffix}") }
}

/// The canonical name for the `n`th *effect* variable: `'e`, `'f`, … (offset from
/// `'e`, the conventional effect-variable letter, to read distinctly from the
/// type variables `'a`, `'b`, … that usually accompany them).
#[must_use]
pub fn eff_canonical_name(n: usize) -> String {
    let letter = (b'e' + (n % 22) as u8) as char; // 'e'..='z'
    let suffix = n / 22;
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
        Ty::Adt(adt) => {
            let _ = out.write_str(adt.name.as_str());
        }
        Ty::Interface(i) => {
            let _ = out.write_str(i.name.as_str());
        }
        // An effect supplied as an interface argument (`Logger { Console }`).
        // Self-delimiting (`{ … }`) or a bare variable, so it never parenthesizes.
        Ty::EffectArg(effect) => write_effect_arg(out, effect, names),
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
        Ty::Arrow(from, to, effect) => {
            // An arrow must be parenthesized when it appears inside a tuple
            // element or an application argument, but not at the top level or to
            // the right of another arrow (arrows are right-associative). An effect
            // annotation binds the nearest arrow, so it sits inside the parens.
            let parenthesize = prec >= Prec::Product;
            if parenthesize {
                let _ = out.write_char('(');
            }
            write_ty(out, from, names, Prec::Product);
            let _ = out.write_str(" -> ");
            write_ty(out, to, names, Prec::Arrow);
            write_effect(out, effect, names);
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
        Ty::Record(row) => {
            // Records are self-delimiting (`{ … }`), so they never parenthesize.
            let _ = out.write_char('{');
            for (i, (label, t)) in row.fields.iter().enumerate() {
                let _ = out.write_str(if i == 0 { " " } else { ", " });
                let _ = out.write_str(label.as_str());
                let _ = out.write_str(" : ");
                write_ty(out, t, names, Prec::Top);
            }
            match row.tail {
                RowEnd::Closed => {}
                RowEnd::Open(rv) => {
                    let _ = write!(out, " | {}", names.get_row(rv));
                }
            }
            let _ = out.write_str(" }");
        }
    }
}

/// Renders an arrow's effect annotation. The pure effect (empty, closed) renders
/// as nothing (a bare arrow); otherwise ` / { Atom, … | tail }`, atoms by their
/// (already-canonicalized) qualified names and a lone open tail as `/ 'e`.
fn write_effect(out: &mut String, effect: &EffectRow, names: &VarNames) {
    if effect.is_pure() {
        return;
    }
    // A lone open tail (no atoms) is sugar: `/ 'e` rather than `/ { | 'e }`.
    if effect.labels.is_empty()
        && let EffEnd::Open(ev) = effect.tail
    {
        let _ = write!(out, " / {}", names.get_eff(ev));
        return;
    }
    let _ = out.write_str(" / {");
    for (i, atom) in effect.labels.iter().enumerate() {
        let _ = out.write_str(if i == 0 { " " } else { ", " });
        let _ = out.write_str(atom.name.as_str());
    }
    if let EffEnd::Open(ev) = effect.tail {
        let _ = write!(out, " | {}", names.get_eff(ev));
    }
    let _ = out.write_str(" }");
}

/// Renders an effect row in *interface-argument* position (`Logger { Console }`):
/// a lone open tail as the bare variable `'e`, the pure row as `{}`, otherwise
/// `{ Atom, … | tail }`. Unlike [`write_effect`] there is no leading ` / ` and
/// the pure row is explicit (it is a written argument, not an arrow's effect).
fn write_effect_arg(out: &mut String, effect: &EffectRow, names: &VarNames) {
    if effect.labels.is_empty() {
        match effect.tail {
            EffEnd::Open(ev) => {
                let _ = out.write_str(&names.get_eff(ev));
            }
            EffEnd::Closed => {
                let _ = out.write_str("{}");
            }
        }
        return;
    }
    let _ = out.write_char('{');
    for (i, atom) in effect.labels.iter().enumerate() {
        let _ = out.write_str(if i == 0 { " " } else { ", " });
        let _ = out.write_str(atom.name.as_str());
    }
    if let EffEnd::Open(ev) = effect.tail {
        let _ = write!(out, " | {}", names.get_eff(ev));
    }
    let _ = out.write_str(" }");
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
        for c in [
            Con::Int,
            Con::Float,
            Con::Bool,
            Con::String,
            Con::Bytes,
            Con::Char,
            Con::List,
            Con::Array,
        ] {
            assert_eq!(Con::from_name(c.name()), Some(c));
        }
        assert_eq!(Con::from_name("Widget"), None);
    }

    fn iface(name: &str) -> fai_resolve::InterfaceRef {
        fai_resolve::InterfaceRef::new(fai_span::SourceId::new(0), fai_syntax::Symbol::intern(name))
    }

    #[test]
    fn pure_arrow_renders_bare() {
        // The empty (pure) effect is the default and renders as a bare arrow.
        let scheme = Scheme::mono(Ty::arrow(Ty::int(), Ty::int()));
        assert_eq!(render_scheme(&scheme), "Int -> Int");
    }

    #[test]
    fn closed_effect_renders_atoms() {
        let eff = EffectRow { labels: vec![iface("Console")], tail: EffEnd::Closed };
        let scheme = Scheme::mono(Ty::arrow_eff(Ty::Con(Con::String), Ty::Unit, eff));
        assert_eq!(render_scheme(&scheme), "String -> () / { Console }");
    }

    #[test]
    fn multi_atom_effect_renders_comma_separated() {
        let eff =
            EffectRow { labels: vec![iface("Console"), iface("FileSystem")], tail: EffEnd::Closed };
        let scheme = Scheme::mono(Ty::arrow_eff(Ty::Unit, Ty::Unit, eff));
        assert_eq!(render_scheme(&scheme), "() -> () / { Console, FileSystem }");
    }

    #[test]
    fn lone_open_effect_tail_renders_as_var_sugar() {
        // `/ 'e` is sugar for `/ { | 'e }` (no atoms, just a tail).
        let eff = EffectRow { labels: vec![], tail: EffEnd::Open(EffRowVarId(0)) };
        let scheme = Scheme::mono(Ty::arrow_eff(v(0), v(1), eff))
            .with_effects(vec![EffRowVarId(0)], vec!["'e".to_owned()]);
        assert_eq!(render_scheme(&scheme), "'a -> 'b / 'e");
    }

    #[test]
    fn open_effect_with_atoms_renders_tail() {
        let eff = EffectRow { labels: vec![iface("Console")], tail: EffEnd::Open(EffRowVarId(0)) };
        let scheme = Scheme::mono(Ty::arrow_eff(Ty::Unit, Ty::Unit, eff))
            .with_effects(vec![EffRowVarId(0)], vec!["'e".to_owned()]);
        assert_eq!(render_scheme(&scheme), "() -> () / { Console | 'e }");
    }

    #[test]
    fn effect_binds_innermost_arrow_in_curried_type() {
        // `/ e` attaches to the last (saturating) arrow; the outer arrow is pure.
        let eff = EffectRow { labels: vec![iface("Console")], tail: EffEnd::Closed };
        let scheme = Scheme::mono(Ty::arrow(Ty::int(), Ty::arrow_eff(Ty::int(), Ty::Unit, eff)));
        assert_eq!(render_scheme(&scheme), "Int -> Int -> () / { Console }");
    }
}
