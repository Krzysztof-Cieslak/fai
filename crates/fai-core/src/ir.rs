//! The Core intermediate representation: a typed, desugared tree.
//!
//! Core is produced by lowering the surface AST (see [`crate::lower`]). It is a
//! tree under a uniform **consume** convention: every operation consumes (takes
//! ownership of) its operands, so a value flows linearly and the only
//! reference-counting work is on named variables — duplicate before a non-final
//! use, drop a dead one. Those `Dup`/`Drop` nodes are absent after lowering and
//! inserted by `fai-rc`.
//!
//! Functions are lambda-lifted: a [`LoweredDef`] holds the definition's entry
//! [`CoreFn`] plus one [`CoreFn`] per nested lambda. A lifted function records
//! its captured variables, which a [`ExprKind::MakeClosure`] supplies at the
//! original lambda's position.

use fai_resolve::{DefId, LocalId};
use fai_types::{Con, RowEnd, Ty};

use crate::niche::NicheKind;

/// The unboxed-`f64` field bitmap for a sequence of **declared** field types in
/// layout order: bit `i` set when field `i`'s type is a monomorphic `Float` (not a
/// type variable instantiated to `Float`). Bounded at 64 fields — a wider cell
/// stays all-uniform. The single definition of "which fields scalarize", shared by
/// lowering (records/tuples/constructors) and the contract value generators.
#[must_use]
pub fn scalar_field_mask<'a>(field_types: impl IntoIterator<Item = &'a Ty>) -> u64 {
    let mut mask = 0u64;
    for (i, t) in field_types.into_iter().enumerate() {
        if i < 64 && matches!(t, Ty::Con(Con::Float)) {
            mask |= 1u64 << i;
        }
    }
    mask
}

/// The largest field count a fixed-shape float aggregate may have and still be
/// kept in registers / passed component-wise. A wider aggregate stays a boxed cell
/// (the spilled cost outweighs the saving past a handful of fields). This bounds
/// a spread **parameter**: arguments past the argument registers spill to the
/// stack, so a spread parameter is register-eligible up to this width on every
/// target. A spread **result** is bounded more tightly by [`max_spread_return`].
pub const FFA_MAX_FIELDS: usize = 8;

/// The largest fixed-shape-float-aggregate result returned **in registers** (a
/// Cranelift multi-result signature); a wider result is returned as the boxed
/// scalar-slot cell instead. Unlike arguments — which spill to the stack, so a
/// spread parameter is register-eligible up to [`FFA_MAX_FIELDS`] — a multi-value
/// *return* must fit entirely in the target's return registers.
///
/// This is the host target's floating-point return-register budget. The compiler
/// only ever targets the host (both the JIT and the AOT object path build for the
/// host triple), so it is a compile-time constant, and the object cache key
/// already includes the host triple. AArch64 returns up to eight `f64`s (V0–V7);
/// x86-64 System V returns two (XMM0–XMM1); the Windows x64 convention returns
/// one; any other target conservatively returns one.
#[must_use]
pub const fn max_spread_return() -> usize {
    if cfg!(target_arch = "aarch64") {
        FFA_MAX_FIELDS
    } else if cfg!(all(target_arch = "x86_64", not(target_os = "windows"))) {
        2
    } else {
        1
    }
}

/// If `ty` is a **fixed-shape float aggregate** (FFA) — a tuple whose every
/// element is concretely `Float`, or a *closed* record whose every field is
/// concretely `Float`, with `1..=FFA_MAX_FIELDS` fields — returns the component
/// count (in canonical layout order: records label-sorted, tuples positional);
/// otherwise `None`.
///
/// An FFA is held as its scalar `f64` components in registers and returned via a
/// multi-value signature rather than a heap cell (scalar replacement of
/// aggregates). The predicate is purely structural — a type variable instantiated
/// to `Float`, a mixed aggregate, a nested aggregate, an open/row-polymorphic
/// record, and an opaque type (which is a nominal `Ty::Adt` from another file) all
/// fail it, so they keep the boxed representation.
#[must_use]
pub fn ffa_arity(ty: &Ty) -> Option<usize> {
    let count = match ty {
        Ty::Tuple(elems) if elems.iter().all(|t| matches!(t, Ty::Con(Con::Float))) => elems.len(),
        Ty::Record(row)
            if row.tail == RowEnd::Closed
                && row.fields.iter().all(|(_, t)| matches!(t, Ty::Con(Con::Float))) =>
        {
            row.fields.len()
        }
        _ => return None,
    };
    (1..=FFA_MAX_FIELDS).contains(&count).then_some(count)
}

/// Identifies a [`CoreFn`] within a [`LoweredDef`] (index into its `fns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FnId(pub u32);

impl FnId {
    /// The backing index.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The native calling-convention shape of a definition's entry, derived from its
/// type signature.
///
/// Two dimensions:
///
/// * **Argument transport** ([`FnAbi::register_abi`]). A *direct-callable*
///   definition — non-row-polymorphic (no offset evidence) with at least one
///   parameter — passes its value arguments in registers, with the entry symbol
///   `fn(env, a0, …, aN) -> ret`. Every other definition (row-polymorphic, which
///   is only ever reached curried through `apply_n`, and nullary value bindings)
///   keeps the uniform spilled-array entry `fn(env, args) -> ret`.
/// * **`Float` representation** ([`Repr::ScalarFloat`] in [`FnAbi::params`]/
///   [`FnAbi::ret`]). A scalar `Float` parameter or result is unboxed: in the
///   register ABI it is an `f64` register; in the uniform ABI it is raw `f64` bits
///   in the argument slot / return word.
/// * **`Int` representation** ([`Repr::ScalarInt`]). A
///   monomorphic `Int` parameter or result is carried **untagged** (a raw `i64`,
///   not a low-bit-tagged immediate) — but only on the **register ABI**, where a
///   direct caller receives it raw and skips the tag/box round-trip. Uniform
///   (row-polymorphic / nullary) entries are reached only through `apply_n`, which
///   boxes everything, so they keep ints tagged: a tagged immediate is already a
///   valid uniform word (unlike a `Float`), so unboxing them would be a pure
///   wrapper round-trip with no direct-call beneficiary. The Cranelift parameter
///   type is `i64` either way — raw-ness is conventional, so it is part of the
///   cache key (see the fingerprint).
///
/// Direct, saturated callers marshal arguments per this shape; the first-class
/// form (`apply_n` / the static closure) always uses the uniform all-boxed array
/// ABI, bridged by a wrapper. Derived from the stable signature so it is
/// body-edit-independent.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct FnAbi {
    /// The representation of each runtime parameter (offset-evidence parameters
    /// first, then source parameters).
    pub params: Vec<Repr>,
    /// The representation of the result.
    pub ret: Repr,
    /// Whether the entry uses the register-passing calling convention
    /// (`fn(env, a0, …, aN) -> ret`, value arguments in registers) instead of the
    /// uniform spilled-array entry (`fn(env, args) -> ret`). `true` exactly for a
    /// **direct-callable** definition: non-row-polymorphic (no offset evidence)
    /// with at least one parameter. Row-polymorphic and nullary definitions are
    /// reached only through `apply_n`, so a register entry would add a wrapper hop
    /// for no benefit; they keep the uniform ABI (and the raw-bits `Float`
    /// representation).
    pub register_abi: bool,
}

/// The native representation of one entry parameter or result. Mutually exclusive
/// by construction (a slot can't be both a scalar float and a raw int).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Repr {
    /// A uniform tagged/boxed 64-bit word.
    #[default]
    Uniform,
    /// An unboxed scalar `Float`: an `f64` register on the register ABI, raw `f64`
    /// bits in the slot/return word on the uniform ABI.
    ScalarFloat,
    /// An untagged scalar `Int` (a raw `i64`). Register ABI only — a uniform entry
    /// (reached via `apply_n`) keeps ints tagged, since a tagged immediate is a
    /// valid uniform word.
    ScalarInt,
    /// A niche `Option` carried without its `Some` wrapper (one `i64` word: the
    /// payload, or the scheme's `None` sentinel). Register ABI only — a uniform
    /// entry, reached via `apply_n`, keeps the standard boxed `Option` (the
    /// wrapper converts at the bridge). See [`crate::niche`].
    Niche(NicheKind),
    /// A **fixed-shape float aggregate** (FFA) carried as its scalar `f64`
    /// components rather than a heap cell: a parameter occupies N consecutive
    /// `f64` registers, a result is returned as an N-value (multi-result)
    /// signature. The inner vector is the per-component representation (currently
    /// always [`Repr::ScalarFloat`]; nesting is future work). Register ABI only —
    /// a uniform entry (reached via `apply_n`) keeps the boxed cell, bridged by
    /// the wrapper. See [`ffa_arity`] and [`crate::sroa`].
    Spread(Vec<Repr>),
}

impl FnAbi {
    /// Derives the calling-convention shape from a definition's type `scheme` and
    /// its `source_params` count (the syntactic `let f a b = …` binders). The
    /// leading offset-evidence parameters (integers) are uniform; each source
    /// parameter is unboxed when its declared type is exactly `Float` (a raw `f64`)
    /// or exactly `Int` (a raw `i64`), and the result likewise. Untagged-`Int` is
    /// kept only for the register ABI (a uniform entry, reached only via `apply_n`,
    /// keeps ints tagged). Reading the *signature* (not the body) keeps a caller's
    /// compiled code independent of a callee's body.
    #[must_use]
    pub fn from_scheme(
        scheme: &fai_types::Scheme,
        source_params: usize,
        niche: &dyn Fn(&Ty) -> Option<NicheKind>,
    ) -> FnAbi {
        let evidence = fai_types::evidence_count(scheme);
        let mut params = vec![Repr::Uniform; evidence];
        let mut ty = &scheme.ty;
        for _ in 0..source_params {
            match ty {
                Ty::Arrow(from, to, _) => {
                    params.push(scalar_repr(from, niche));
                    ty = to;
                }
                // Fewer arrows than declared source parameters cannot happen for a
                // well-typed binding; stop defensively rather than panic.
                _ => break,
            }
        }
        // Direct-callable iff non-row-polymorphic (no evidence) with a parameter:
        // exactly the definitions a saturated call reaches as a bare `Global` head.
        let register_abi = evidence == 0 && source_params > 0;
        let mut ret = scalar_repr(ty, niche);
        // A multi-value return must fit in the target's return registers; a wider
        // fixed-shape float aggregate result is returned as a boxed cell instead
        // (a spread *parameter* is unaffected — arguments spill to the stack).
        if let Repr::Spread(c) = &ret
            && c.len() > max_spread_return()
        {
            ret = Repr::Uniform;
        }
        // Untagged ints and niche `Option`s are carried only on the register ABI; a
        // uniform entry (reached via `apply_n`) keeps a tagged immediate / a
        // standard boxed `Option` — both valid uniform words — bridged by the
        // wrapper. Floats stay unboxed on both ABIs (a float is never a uniform word).
        if !register_abi {
            for p in &mut params {
                if matches!(p, Repr::ScalarInt | Repr::Niche(_) | Repr::Spread(_)) {
                    *p = Repr::Uniform;
                }
            }
            if matches!(ret, Repr::ScalarInt | Repr::Niche(_) | Repr::Spread(_)) {
                ret = Repr::Uniform;
            }
        }
        FnAbi { params, ret, register_abi }
    }

    /// The calling-convention shape of a non-source synthetic definition (a
    /// mutual-recursion combined loop) that is **direct-called** — register
    /// transport, all-boxed (uniform `i64`) arguments and result. `arity` is its
    /// runtime parameter count.
    #[must_use]
    pub fn register_uniform(arity: usize) -> FnAbi {
        FnAbi { params: vec![Repr::Uniform; arity], ret: Repr::Uniform, register_abi: arity > 0 }
    }

    /// Whether runtime parameter `i` is passed as an unboxed `Float` (raw bits).
    #[must_use]
    pub fn float_param(&self, i: usize) -> bool {
        matches!(self.params.get(i), Some(Repr::ScalarFloat))
    }

    /// Whether runtime parameter `i` is passed as an untagged `Int` (a raw `i64`).
    #[must_use]
    pub fn int_param(&self, i: usize) -> bool {
        matches!(self.params.get(i), Some(Repr::ScalarInt))
    }

    /// Whether the result is an unboxed `Float`.
    #[must_use]
    pub fn float_return(&self) -> bool {
        self.ret == Repr::ScalarFloat
    }

    /// Whether the result is an untagged `Int` (a raw `i64`).
    #[must_use]
    pub fn int_return(&self) -> bool {
        self.ret == Repr::ScalarInt
    }

    /// The niche `Option` scheme of runtime parameter `i`, if it is one.
    #[must_use]
    pub fn niche_param(&self, i: usize) -> Option<NicheKind> {
        match self.params.get(i) {
            Some(Repr::Niche(k)) => Some(*k),
            _ => None,
        }
    }

    /// The niche `Option` scheme of the result, if it is one.
    #[must_use]
    pub fn niche_return(&self) -> Option<NicheKind> {
        match self.ret {
            Repr::Niche(k) => Some(k),
            _ => None,
        }
    }

    /// The component representations of runtime parameter `i` if it is a spread
    /// (fixed-shape float aggregate) parameter, else `None`.
    #[must_use]
    pub fn spread_param(&self, i: usize) -> Option<&[Repr]> {
        match self.params.get(i) {
            Some(Repr::Spread(c)) => Some(c),
            _ => None,
        }
    }

    /// The component representations of the result if it is a spread result, else
    /// `None`. When `Some`, the entry returns a Cranelift multi-result signature.
    #[must_use]
    pub fn spread_return(&self) -> Option<&[Repr]> {
        match &self.ret {
            Repr::Spread(c) => Some(c),
            _ => None,
        }
    }

    /// Whether any parameter is an unboxed `Float` (raw bits in its slot).
    #[must_use]
    pub fn any_float_param(&self) -> bool {
        self.params.contains(&Repr::ScalarFloat)
    }

    /// Whether the entry uses the plain uniform ABI (no unboxed `Float` or untagged
    /// `Int` parameter or result), so direct calls need no marshalling and no
    /// bridging wrapper.
    #[must_use]
    pub fn is_uniform(&self) -> bool {
        self.ret == Repr::Uniform && self.params.iter().all(|r| *r == Repr::Uniform)
    }
}

/// The native representation of a parameter/result type: an unboxed `Float`, an
/// untagged `Int`, a spread of `f64` components (a fixed-shape float aggregate), a
/// niche `Option` (per `niche`), or a uniform word.
fn scalar_repr(ty: &Ty, niche: &dyn Fn(&Ty) -> Option<NicheKind>) -> Repr {
    match ty {
        Ty::Con(Con::Float) => Repr::ScalarFloat,
        Ty::Con(Con::Int) => Repr::ScalarInt,
        _ => {
            if let Some(n) = ffa_arity(ty) {
                return Repr::Spread(vec![Repr::ScalarFloat; n]);
            }
            match niche(ty) {
                Some(k) => Repr::Niche(k),
                None => Repr::Uniform,
            }
        }
    }
}

/// A lowered top-level definition: its entry function plus lifted lambdas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoweredDef {
    /// The definition this lowering belongs to.
    pub def: DefId,
    /// The functions; `fns[0]` is the definition's entry, the rest are lifted
    /// lambdas referenced by [`FnId`].
    pub fns: Vec<CoreFn>,
    /// Per-entry-parameter borrow flags (set by reference counting): a borrowed
    /// parameter is lent by direct callers and not dropped here. Empty before
    /// reference counting (and means all-owned). Lifted lambdas are always owned.
    pub entry_borrowed: Vec<bool>,
    /// A token-taking specialized entry, when this definition can recycle reuse
    /// tokens forwarded by a caller (set by reference counting when its
    /// [`reuse_signature`](../../fai_rc/fn.reuse_signature.html) is non-empty). Its
    /// leading parameters are the reuse tokens (one per accepted size-class slot,
    /// not reference-counted), followed by the same source parameters as the entry.
    /// Code generation emits it as `{base}__reuse`; `None` when the definition
    /// accepts no tokens.
    pub reuse_entry: Option<CoreFn>,
    /// The component locals of each spread (fixed-shape float aggregate) entry
    /// parameter, set by the SROA pass (see [`crate::sroa`]). Indexed by entry
    /// parameter position: `Some([c0, …, cN])` for a [`Repr::Spread`] parameter
    /// (whose N `f64` registers code generation binds to `c0 … cN`), `None` for an
    /// ordinary parameter. Empty (all-`None`) before SROA. The aggregate's own
    /// parameter slot in [`CoreFn::params`] stays the ABI anchor and carries no
    /// runtime value; the body references the component locals instead.
    pub entry_spread_params: Vec<Option<Vec<LocalId>>>,
}

impl LoweredDef {
    /// The definition's entry function.
    #[must_use]
    pub fn entry(&self) -> &CoreFn {
        &self.fns[0]
    }

    /// Whether entry parameter `i` is borrowed (lent by direct callers).
    #[must_use]
    pub fn entry_param_borrowed(&self, i: usize) -> bool {
        self.entry_borrowed.get(i).copied().unwrap_or(false)
    }

    /// Whether the entry borrows any parameter (so it needs an owned-ABI wrapper).
    #[must_use]
    pub fn borrows_any(&self) -> bool {
        self.entry_borrowed.iter().any(|&b| b)
    }

    /// The top-level definitions referenced as values anywhere in this lowering
    /// (for reachability). Includes prelude helpers reached as `Global`, which
    /// resolution records as builtins rather than dependencies.
    #[must_use]
    pub fn referenced_globals(&self) -> Vec<DefId> {
        let mut out = Vec::new();
        for f in &self.fns {
            collect_globals(&f.body, &mut out);
        }
        out
    }
}

/// Collects `Global` references in `expr`, in first-seen order (with duplicates).
fn collect_globals(expr: &CExpr, out: &mut Vec<DefId>) {
    match &expr.kind {
        ExprKind::Global(def) => out.push(*def),
        ExprKind::Lit(_) | ExprKind::Local(_) | ExprKind::MakeClosure { .. } | ExprKind::Error => {}
        ExprKind::Prim { args, .. } => {
            for a in args {
                collect_globals(a, out);
            }
        }
        ExprKind::MakeData { args, .. } => {
            for a in args {
                collect_globals(a, out);
            }
        }
        ExprKind::DataTag { base, .. } => collect_globals(base, out),
        ExprKind::DataField { base, .. } => collect_globals(base, out),
        ExprKind::App { func, args, .. } => {
            collect_globals(func, out);
            for a in args {
                collect_globals(a, out);
            }
        }
        ExprKind::If { cond, then, els } => {
            collect_globals(cond, out);
            collect_globals(then, out);
            collect_globals(els, out);
        }
        ExprKind::Let { value, body, .. } => {
            collect_globals(value, out);
            collect_globals(body, out);
        }
        ExprKind::Spread { components } => {
            for c in components {
                collect_globals(c, out);
            }
        }
        ExprKind::LetMany { value, body, .. } => {
            collect_globals(value, out);
            collect_globals(body, out);
        }
        ExprKind::Reset { value, body, .. } => {
            collect_globals(value, out);
            collect_globals(body, out);
        }
        ExprKind::FreeReuse { body, .. } => collect_globals(body, out),
        ExprKind::Dup { body, .. } | ExprKind::Drop { body, .. } => collect_globals(body, out),
        ExprKind::Join { body, .. } => collect_globals(body, out),
        ExprKind::Recur { args } => {
            for a in args {
                collect_globals(a, out);
            }
        }
        ExprKind::HoleStart { body, .. } => collect_globals(body, out),
        ExprKind::HoleFill { cell, .. } => collect_globals(cell, out),
        ExprKind::HoleClose { base, .. } => collect_globals(base, out),
    }
}

/// One compiled function: parameters, captured environment, and a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreFn {
    /// Parameter slots, in order (owned by the body).
    pub params: Vec<LocalId>,
    /// Captured slots, in order (borrowed; supplied by `MakeClosure`). Empty for
    /// a definition's entry function.
    pub captures: Vec<LocalId>,
    /// The function body.
    pub body: CExpr,
}

/// A decoded literal value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Lit {
    /// A 64-bit integer (decoded from its lexeme).
    Int(i64),
    /// A 64-bit float, stored as its IEEE-754 bit pattern (so `Eq`/`Hash` hold).
    Float(u64),
    /// A Unicode scalar value (an immediate code point, like `Bool`/`Int`).
    Char(char),
    /// A boolean.
    Bool(bool),
    /// A string's decoded UTF-8 bytes.
    Str(Vec<u8>),
    /// The unit value.
    Unit,
}

/// A primitive operation, lowered to a runtime call. Every primitive consumes
/// its operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Prim {
    /// `+`
    IntAdd,
    /// `-`
    IntSub,
    /// `*`
    IntMul,
    /// `/`
    IntDiv,
    /// `%`
    IntRem,
    /// `Int.and` (bitwise and)
    IntAnd,
    /// `Int.or` (bitwise or)
    IntOr,
    /// `Int.xor` (bitwise xor)
    IntXor,
    /// `Int.shiftLeft`
    IntShl,
    /// `Int.shiftRight` (arithmetic, sign-extending)
    IntShr,
    /// `Int.shiftRightLogical` (logical, zero-filling)
    IntShrLogical,
    /// `Int.complement` (bitwise not, unary)
    IntComplement,
    /// `<`
    IntLt,
    /// `<=`
    IntLe,
    /// `>`
    IntGt,
    /// `>=`
    IntGe,
    /// `+` on `Float`
    FloatAdd,
    /// `-` on `Float`
    FloatSub,
    /// `*` on `Float`
    FloatMul,
    /// `/` on `Float`
    FloatDiv,
    /// `<` on `Float`
    FloatLt,
    /// `<=` on `Float`
    FloatLe,
    /// `>` on `Float`
    FloatGt,
    /// `>=` on `Float`
    FloatGe,
    /// Structural ordering: returns `-1`/`0`/`1` (used for non-numeric `< <= > >=`).
    Compare,
    /// Structural hash: returns a non-negative `Int` (the `HashDict`/`HashSet`
    /// containers build on it). Agrees with `Eq`.
    Hash,
    /// `=` (structural equality)
    Eq,
    /// `++` (string concatenation)
    StrConcat,
    /// `intToString`
    IntToString,
    /// `floatToString`
    FloatToString,
    /// `intToFloat`
    IntToFloat,
    /// `floatToInt`
    FloatToInt,
    /// `sqrt`
    Sqrt,
    /// `floatFromBits` (reinterpret an Int's bits as a Float)
    FloatFromBits,
    /// `floatToBits` (reinterpret a Float's bits as an Int)
    FloatToBits,
    /// `charToString` (a one-character `String`)
    CharToString,
    /// `charToCode` (a Char's Unicode scalar value as an Int)
    CharToCode,
    /// `charFromCode` (an Int code point as a Char; valid code guaranteed by the caller)
    CharFromCode,
    /// `isValidCharCode` (whether an Int is a Unicode scalar value)
    IsValidCharCode,
    /// `stringLength`
    StringLength,
    /// `toUpper`
    ToUpper,
    /// `toLower`
    ToLower,
    /// `trim`
    Trim,
    /// `stringContains`
    StringContains,
    /// `split`
    StringSplit,
    /// `join`
    StringJoin,
    /// `substring` (a `len`-char substring from a char index; a view or a copy)
    StringSubstring,
    /// `take` (the first `n` chars; a view or a copy)
    StringTake,
    /// `drop` (all but the first `n` chars; a view or a copy)
    StringDrop,
    /// `not`
    Not,
    /// `Console.writeLine`: write a line (the host for the `Console` capability).
    ConsoleWriteLine,
    /// `Clock.now`: milliseconds since the epoch.
    ClockNow,
    /// `Random.nextInt`: a pseudo-random `Int` in `[0, n)`.
    RandomNextInt,
    /// `FileSystem` read host: `String -> (Bool * String)` (ok?, contents-or-error).
    FileRead,
    /// `FileSystem` write host: `String -> String -> (Bool * String)`.
    FileWrite,
    /// `Env` lookup host: `String -> (Bool * String)` (found?, value).
    EnvGet,
    /// `Env` arguments host: `Unit -> List String`.
    EnvArgs,
    /// Row-polymorphic record update: clone a record (by runtime size), replacing
    /// the field at a runtime index. Internal to lowering, never a source name.
    RecordUpdate,
    /// `Array.withCapacity`: a fresh empty array with room for `n` elements.
    ArrayWithCapacity,
    /// `Array` length (the live element count).
    ArrayLength,
    /// `Array` element access by index (unchecked; out-of-bounds aborts).
    ArrayGet,
    /// `Array` element update by index (in place when unique; out-of-bounds aborts).
    ArraySet,
    /// `Array` append (in place when unique with spare capacity, else grows/copies).
    ArrayPush,
    /// `arraySplit`: split a `String` on a separator into an `Array String`.
    ArraySplit,
    /// `arrayJoin`: join an `Array String` with a separator into a `String`.
    ArrayJoin,
}

/// Whether values of `ty` are boxed, reference-counted heap values, so lending
/// rather than consuming one saves a duplicate/drop. Strings, floats, records,
/// tuples, ADTs, lists, and interface dictionaries qualify; immediates
/// (`Int`/`Bool`/`Char`/`Unit`), functions, and type variables do not.
fn is_boxed_rc(ty: &Ty) -> bool {
    fn boxed_head(ty: &Ty) -> bool {
        match ty {
            Ty::Adt(_) | Ty::Interface(_) | Ty::Con(Con::List | Con::Array) => true,
            Ty::App(head, _) => boxed_head(head),
            _ => false,
        }
    }
    matches!(ty, Ty::Record(_) | Ty::Tuple(_) | Ty::Con(Con::String) | Ty::Con(Con::Float))
        || boxed_head(ty)
}

impl Prim {
    /// Whether this primitive only *inspects* an operand of `operand_ty`, so the
    /// operand may be borrowed (the caller keeps ownership and the runtime's
    /// borrowed variant does not consume it) rather than consumed.
    ///
    /// True only for the inspect-only primitives — `=`, structural `compare`, and
    /// the `String` readers — and only when the operand is a reliably boxed,
    /// reference-counted type. Immediate operands (notably the hot `match`
    /// tag-test path) stay consumed, since lending them would only add a no-op
    /// drop. Both reference counting and code generation consult this, so they
    /// agree on whether the operand is borrowed.
    #[must_use]
    pub fn borrows_operand(self, operand_ty: &Ty) -> bool {
        matches!(
            self,
            Prim::Eq
                | Prim::Compare
                | Prim::Hash
                | Prim::StringLength
                | Prim::StringContains
                | Prim::ToUpper
                | Prim::ToLower
                | Prim::Trim
                | Prim::StringSplit
                | Prim::StringJoin
                | Prim::ArrayLength
                | Prim::ArrayGet
                | Prim::ArraySplit
                | Prim::ArrayJoin
        ) && is_boxed_rc(operand_ty)
    }

    /// The runtime symbol of this primitive's non-consuming variant, for the
    /// inspect-only primitives that have one (see [`Prim::borrows_operand`]).
    #[must_use]
    pub fn borrowed_runtime_symbol(self) -> Option<&'static str> {
        match self {
            Prim::Eq => Some("fai_equal_borrowed"),
            Prim::Compare => Some("fai_compare_borrowed"),
            Prim::Hash => Some("fai_hash_borrowed"),
            Prim::StringLength => Some("fai_string_length_borrowed"),
            Prim::StringContains => Some("fai_string_contains_borrowed"),
            Prim::ToUpper => Some("fai_to_upper_borrowed"),
            Prim::ToLower => Some("fai_to_lower_borrowed"),
            Prim::Trim => Some("fai_trim_borrowed"),
            Prim::StringSplit => Some("fai_string_split_borrowed"),
            Prim::ArrayLength => Some("fai_array_length_borrowed"),
            Prim::ArrayGet => Some("fai_array_get_borrowed"),
            Prim::StringJoin => Some("fai_string_join_borrowed"),
            Prim::ArraySplit => Some("fai_array_split_borrowed"),
            Prim::ArrayJoin => Some("fai_array_join_borrowed"),
            _ => None,
        }
    }

    /// The runtime symbol implementing this primitive.
    #[must_use]
    pub fn runtime_symbol(self) -> &'static str {
        match self {
            Prim::IntAdd => "fai_int_add",
            Prim::IntSub => "fai_int_sub",
            Prim::IntMul => "fai_int_mul",
            Prim::IntDiv => "fai_int_div",
            Prim::IntRem => "fai_int_rem",
            Prim::IntAnd => "fai_int_and",
            Prim::IntOr => "fai_int_or",
            Prim::IntXor => "fai_int_xor",
            Prim::IntShl => "fai_int_shl",
            Prim::IntShr => "fai_int_shr",
            Prim::IntShrLogical => "fai_int_shr_logical",
            Prim::IntComplement => "fai_int_complement",
            Prim::IntLt => "fai_int_lt",
            Prim::IntLe => "fai_int_le",
            Prim::IntGt => "fai_int_gt",
            Prim::IntGe => "fai_int_ge",
            Prim::FloatAdd => "fai_float_add",
            Prim::FloatSub => "fai_float_sub",
            Prim::FloatMul => "fai_float_mul",
            Prim::FloatDiv => "fai_float_div",
            Prim::FloatLt => "fai_float_lt",
            Prim::FloatLe => "fai_float_le",
            Prim::FloatGt => "fai_float_gt",
            Prim::FloatGe => "fai_float_ge",
            Prim::Compare => "fai_compare",
            Prim::Hash => "fai_hash",
            Prim::Eq => "fai_equal",
            Prim::StrConcat => "fai_string_concat",
            Prim::IntToString => "fai_int_to_string",
            Prim::FloatToString => "fai_float_to_string",
            Prim::IntToFloat => "fai_int_to_float",
            Prim::FloatToInt => "fai_float_to_int",
            Prim::Sqrt => "fai_sqrt",
            Prim::FloatFromBits => "fai_float_from_bits",
            Prim::FloatToBits => "fai_float_to_bits",
            Prim::CharToString => "fai_char_to_string",
            Prim::CharToCode => "fai_char_to_code",
            Prim::CharFromCode => "fai_char_from_code",
            Prim::IsValidCharCode => "fai_is_valid_char_code",
            Prim::StringLength => "fai_string_length",
            Prim::ToUpper => "fai_to_upper",
            Prim::ToLower => "fai_to_lower",
            Prim::Trim => "fai_trim",
            Prim::StringContains => "fai_string_contains",
            Prim::StringSplit => "fai_string_split",
            Prim::StringJoin => "fai_string_join",
            Prim::StringSubstring => "fai_string_substring",
            Prim::StringTake => "fai_string_take",
            Prim::StringDrop => "fai_string_drop",
            Prim::Not => "fai_not",
            Prim::ConsoleWriteLine => "fai_console_write_line",
            Prim::ClockNow => "fai_clock_now",
            Prim::RandomNextInt => "fai_random_next_int",
            Prim::FileRead => "fai_file_read",
            Prim::FileWrite => "fai_file_write",
            Prim::EnvGet => "fai_env_get",
            Prim::EnvArgs => "fai_env_args",
            Prim::RecordUpdate => "fai_record_update",
            Prim::ArrayWithCapacity => "fai_array_with_capacity",
            Prim::ArrayLength => "fai_array_length",
            Prim::ArrayGet => "fai_array_get",
            Prim::ArraySet => "fai_array_set",
            Prim::ArrayPush => "fai_array_push",
            Prim::ArraySplit => "fai_array_split",
            Prim::ArrayJoin => "fai_array_join",
        }
    }

    /// How many operands the primitive takes.
    #[must_use]
    pub fn arity(self) -> usize {
        match self {
            Prim::IntToString
            | Prim::FloatToString
            | Prim::IntToFloat
            | Prim::FloatToInt
            | Prim::Sqrt
            | Prim::FloatFromBits
            | Prim::FloatToBits
            | Prim::CharToString
            | Prim::CharToCode
            | Prim::CharFromCode
            | Prim::IsValidCharCode
            | Prim::StringLength
            | Prim::ToUpper
            | Prim::ToLower
            | Prim::Trim
            | Prim::Not
            | Prim::ConsoleWriteLine
            | Prim::ClockNow
            | Prim::RandomNextInt
            | Prim::EnvGet
            | Prim::EnvArgs
            | Prim::IntComplement
            | Prim::Hash
            | Prim::ArrayWithCapacity
            | Prim::ArrayLength
            | Prim::FileRead => 1,
            Prim::RecordUpdate | Prim::ArraySet | Prim::StringSubstring => 3,
            _ => 2,
        }
    }

    /// The primitive a named builtin maps to, if it is one (operators are lowered
    /// from `Binary` nodes, not via this table).
    #[must_use]
    pub fn from_builtin(name: &str) -> Option<Prim> {
        Some(match name {
            "intAnd" => Prim::IntAnd,
            "intOr" => Prim::IntOr,
            "intXor" => Prim::IntXor,
            "intShiftLeft" => Prim::IntShl,
            "intShiftRight" => Prim::IntShr,
            "intShiftRightLogical" => Prim::IntShrLogical,
            "intComplement" => Prim::IntComplement,
            "intToString" => Prim::IntToString,
            "floatToString" => Prim::FloatToString,
            "intToFloat" => Prim::IntToFloat,
            "floatToInt" => Prim::FloatToInt,
            "sqrt" => Prim::Sqrt,
            "floatFromBits" => Prim::FloatFromBits,
            "floatToBits" => Prim::FloatToBits,
            "charToString" => Prim::CharToString,
            "charToCode" => Prim::CharToCode,
            "charFromCode" => Prim::CharFromCode,
            "isValidCharCode" => Prim::IsValidCharCode,
            "stringLength" => Prim::StringLength,
            "toUpper" => Prim::ToUpper,
            "toLower" => Prim::ToLower,
            "trim" => Prim::Trim,
            "stringContains" => Prim::StringContains,
            "stringConcat" => Prim::StrConcat,
            "split" => Prim::StringSplit,
            "join" => Prim::StringJoin,
            "substring" => Prim::StringSubstring,
            "take" => Prim::StringTake,
            "drop" => Prim::StringDrop,
            "not" => Prim::Not,
            "compare" => Prim::Compare,
            "hash" => Prim::Hash,
            "consoleWriteLine" => Prim::ConsoleWriteLine,
            "clockNow" => Prim::ClockNow,
            "randomNextInt" => Prim::RandomNextInt,
            "fileRead" => Prim::FileRead,
            "fileWrite" => Prim::FileWrite,
            "envGet" => Prim::EnvGet,
            "envArgs" => Prim::EnvArgs,
            "arrayWithCapacity" => Prim::ArrayWithCapacity,
            "arrayLength" => Prim::ArrayLength,
            "arrayGet" => Prim::ArrayGet,
            "arraySet" => Prim::ArraySet,
            "arrayPush" => Prim::ArrayPush,
            "arraySplit" => Prim::ArraySplit,
            "arrayJoin" => Prim::ArrayJoin,
            _ => return None,
        })
    }
}

/// A typed Core expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CExpr {
    /// The expression form.
    pub kind: ExprKind,
    /// The expression's type (from `body_types`, or derived for synthesized
    /// nodes). Carried for downstream phases; M3 codegen relies on tagging.
    pub ty: Ty,
}

impl CExpr {
    /// Builds a typed expression.
    #[must_use]
    pub fn new(kind: ExprKind, ty: Ty) -> Self {
        Self { kind, ty }
    }
}

/// The slot of a projected record/dictionary field.
///
/// Monomorphic access is a compile-time constant. Row-polymorphic access is a
/// runtime sum `base + evidence`, where `evidence` is an integer local — a
/// leading offset-evidence parameter holding the count of the row's hidden
/// fields that precede this one (see [`fai_types::evidence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldIndex {
    /// A statically known slot (monomorphic record or interface dictionary).
    Const(u32),
    /// A row-polymorphic slot: `base` (the statically known preceding fields)
    /// plus the value of the `evidence` local.
    Dyn {
        /// The statically known fields preceding this one.
        base: u32,
        /// The evidence local holding the count of preceding hidden fields.
        evidence: LocalId,
    },
}

/// How a [`ExprKind::MakeClosure`] is realized at code generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClosureAlloc {
    /// A non-capturing lambda: it shares one immortal static closure (no
    /// per-activation environment), so it allocates nothing. Fixed at lowering
    /// from an empty capture list.
    Static,
    /// A capturing lambda that provably does not escape its creating activation:
    /// a stack-allocated cell, reclaimed when the frame returns. Reference
    /// counting still releases its captures when it dies (the cell itself is not
    /// freed). Set by escape analysis; never escapes, so a stack pointer is never
    /// stored anywhere outliving the frame.
    Stack,
    /// A capturing lambda that may escape: a heap-allocated, reference-counted
    /// cell (the conservative default).
    Heap,
}

/// The forms of a Core expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A literal.
    Lit(Lit),
    /// A use of a local (parameter, `let`-binding, or capture); consumes it.
    Local(LocalId),
    /// A top-level definition referenced as a value (its static closure).
    Global(DefId),
    /// A saturated primitive application.
    Prim {
        /// The primitive.
        op: Prim,
        /// The operands (consumed).
        args: Vec<CExpr>,
    },
    /// A general application, routed through the runtime `apply_n`.
    App {
        /// The function value (consumed).
        func: Box<CExpr>,
        /// The arguments (consumed).
        args: Vec<CExpr>,
        /// Reuse tokens forwarded into the callee's token-taking entry (inserted by
        /// `fai-rc` at a saturated direct call whose callee accepts them). Empty for
        /// an ordinary call. A non-empty list means the call targets the callee's
        /// `{base}__reuse` entry: its length is the callee's token-slot count, each
        /// slot either a forwarded token (`Some`, consumed here — the callee reuses
        /// it in a construction or frees it) or a null-token pad (`None`) for a slot
        /// this caller has no cell for.
        reuse: Vec<Option<LocalId>>,
        /// How a partial application built by this call is allocated: `Stack` when
        /// the call **under-applies** a known function and the resulting closure
        /// provably does not escape (escape analysis), else `Heap` (the default).
        /// Ignored for a saturated or over-application (which builds no partial
        /// application). `Static` is never used here.
        alloc: ClosureAlloc,
    },
    /// A conditional.
    If {
        /// The `Bool` condition.
        cond: Box<CExpr>,
        /// The then-branch.
        then: Box<CExpr>,
        /// The else-branch.
        els: Box<CExpr>,
    },
    /// A non-recursive local binding: `let local = value in body`.
    Let {
        /// The bound slot.
        local: LocalId,
        /// The bound value (owned).
        value: Box<CExpr>,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// The exploded components of a **fixed-shape float aggregate** (FFA), as a
    /// multi-value unit (scalar replacement of aggregates). Produced by the SROA
    /// pass (see [`crate::sroa`]) in two positions: a function's tail when its
    /// result ABI is [`Repr::Spread`] (lowered to a multi-result `return`), and a
    /// call argument whose callee parameter is spread (the components flow into
    /// consecutive registers). `components` are atoms (component locals, each typed
    /// `Float`) in canonical field order; the node's [`CExpr::ty`] is the FFA type.
    Spread {
        /// The component values, in canonical field order.
        components: Vec<CExpr>,
    },
    /// Binds several slots at once from a multi-result `value`: the destructuring
    /// of a [`Repr::Spread`]-result call. `value` is the call (an [`ExprKind::App`]
    /// whose callee returns an FFA); `locals` receive its components in canonical
    /// field order, then `body` is evaluated. Produced by the SROA pass.
    LetMany {
        /// The slots bound to the multi-result value's components, in order.
        locals: Vec<LocalId>,
        /// The multi-result value (a spread-returning call).
        value: Box<CExpr>,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// Builds a closure for a lifted function, capturing the given slots.
    MakeClosure {
        /// The lifted function.
        func: FnId,
        /// The captured slots, in the lifted function's `captures` order.
        captures: Vec<LocalId>,
        /// How the closure is allocated: a shared static cell (no captures), a
        /// stack cell (captures, non-escaping), or a heap cell (the default).
        alloc: ClosureAlloc,
    },
    /// Constructs a data value (constructor, record, or tuple): a tag plus its
    /// fields. A nullary constructor (no fields) is an immediate carrying its tag.
    MakeData {
        /// The constructor's tag (variant index; 0 for records/tuples).
        tag: u32,
        /// The field values, consumed into the new object.
        args: Vec<CExpr>,
        /// An optional reuse token (from a [`ExprKind::Reset`]) to build into in
        /// place when it is non-null and the right size, instead of allocating
        /// (inserted by `fai-rc`). The token is consumed here.
        reuse: Option<LocalId>,
        /// Which fields are stored as a raw unboxed `f64` (bit `i` ⇒ field `i`):
        /// the constructor's fields **declared** `Float` (a record/tuple field of
        /// type `Float`, or a constructor field declared `Float` — not a type
        /// variable instantiated to `Float`). Computed during lowering so the
        /// representation is consistent everywhere the constructor is used; zero
        /// for an all-uniform cell.
        scalars: u64,
        /// The niche scheme when this constructs a niche `Option` (`Some`/`None`),
        /// else `None` for a standard data value. A niche `Some` is built without a
        /// wrapper cell (its argument *is* the value); a niche `None` is the scheme's
        /// sentinel. Decided at lowering (see [`crate::niche`]).
        niche: Option<NicheKind>,
    },
    /// Reads the tag of a data value (consuming `base`), as an immediate `Int`.
    DataTag {
        /// The data value (consumed).
        base: Box<CExpr>,
        /// The niche scheme when `base` is a niche `Option`, so the tag is computed
        /// from the niche encoding (sentinel vs payload) rather than a header read;
        /// `None` for a standard data value.
        niche: Option<NicheKind>,
    },
    /// Projects field `index` of a data value, consuming `base` and yielding an
    /// owned reference to the field.
    DataField {
        /// The data value (consumed).
        base: Box<CExpr>,
        /// The field slot (constant, or row-polymorphic `base + evidence`).
        index: FieldIndex,
        /// Whether the projected slot is a raw unboxed `f64` (the field is declared
        /// `Float`), so the projection reads its bits directly rather than a uniform
        /// word. Mirrors the producing constructor's [`MakeData::scalars`] bit.
        scalar: bool,
        /// The niche scheme when `base` is a niche `Option` (so a `Some` projection
        /// is the identity — the value already *is* the payload); `None` for a
        /// standard data value.
        niche: Option<NicheKind>,
    },
    /// Release `value` for reuse: drop its reference-counted children and, if it
    /// was unique, bind `token` to its raw memory (else to a null token); then
    /// evaluate `body`. The token is consumed by exactly one [`ExprKind::MakeData`]
    /// on each path (inserted by `fai-rc`).
    Reset {
        /// The data value to release (consumed); a local after A-normal form.
        value: Box<CExpr>,
        /// The reuse token bound for `body`.
        token: LocalId,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// Free an unconsumed reuse `token` (from a [`ExprKind::Reset`]) on a path that
    /// builds nothing into it: reclaim the held cell's memory — a no-op on the null
    /// token — then evaluate `body`. Inserted by `fai-rc` when a reset cell's token
    /// reaches a branch with no reusing construction (so every path still consumes
    /// the token exactly once). The token is consumed here.
    FreeReuse {
        /// The reuse token to free (consumed).
        token: LocalId,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// Increment a variable's reference count, then evaluate `body` (inserted by
    /// `fai-rc`).
    Dup {
        /// The variable to duplicate.
        local: LocalId,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// Release a variable, then evaluate `body` (inserted by `fai-rc`).
    Drop {
        /// The variable to drop.
        local: LocalId,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// A loop header (a join point): the loop-carried `params` are already in scope
    /// holding their initial values; evaluate `body`. Each [`ExprKind::Recur`] in
    /// tail position of `body` reassigns the params and re-enters the loop. The
    /// loop's value is that of its non-`Recur` tails. Produced only by the
    /// tail-call transform in `fai-rc`.
    Join {
        /// The loop-carried locals, reassigned positionally by each `Recur`.
        params: Vec<LocalId>,
        /// The loop body.
        body: Box<CExpr>,
    },
    /// A tail back-edge to the enclosing [`ExprKind::Join`]: reassign its params to
    /// `args` (positionally; consuming each) and re-enter the loop. Valid only in
    /// tail position of a `Join` body.
    Recur {
        /// The new values for the enclosing `Join`'s params, in order.
        args: Vec<CExpr>,
    },
    /// Begin destination-passing construction of a spine: bind `hole` to a fresh,
    /// empty destination and evaluate `body`. The hole is a **non-reference-counted
    /// linear token** (like a reuse token): advanced by exactly one
    /// [`ExprKind::HoleFill`] per spine-extending tail and consumed by exactly one
    /// [`ExprKind::HoleClose`] per base case along each path.
    HoleStart {
        /// The destination token bound for `body`.
        hole: LocalId,
        /// The continuation.
        body: Box<CExpr>,
    },
    /// Link `cell` into the spine through `hole`'s destination — store `cell` where
    /// the hole points, then advance the destination to `cell`'s field `field` (the
    /// recursive field, the next hole). Consumes `hole` and `cell`; yields the new
    /// destination token. The recursive field of `cell` is built with a placeholder
    /// immediate that the next fill or close overwrites.
    HoleFill {
        /// The current destination token (consumed).
        hole: LocalId,
        /// The freshly built cell to link in (consumed).
        cell: Box<CExpr>,
        /// The index of `cell`'s recursive field (the next hole).
        field: u32,
    },
    /// Finish the spine: write the base-case value `base` where `hole` points and
    /// yield the completed structure. Consumes `hole` and `base`.
    HoleClose {
        /// The destination token (consumed).
        hole: LocalId,
        /// The base value written into the final hole (consumed).
        base: Box<CExpr>,
    },
    /// A lowering error placeholder (an unsupported construct was reported).
    Error,
}
