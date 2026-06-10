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
use fai_types::{Con, Ty};

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
/// type signature: which runtime parameters carry an **unboxed** `Float` (raw
/// `f64` bits in the argument slot) and whether the result is an unboxed `Float`.
///
/// Direct, saturated callers marshal float arguments and the result as raw bits
/// per this shape; the first-class form (`apply_n` / the static closure) keeps the
/// uniform boxed representation, bridged by a wrapper. A definition with no scalar
/// `Float` parameter or result is *uniform* (all flags clear) and needs no
/// bridging. Derived from the stable signature so it is body-edit-independent.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct FnAbi {
    /// One flag per runtime parameter (offset-evidence parameters first, then
    /// source parameters), `true` when that parameter is an unboxed `Float`.
    pub float_params: Vec<bool>,
    /// Whether the result is an unboxed `Float`.
    pub float_return: bool,
}

impl FnAbi {
    /// Derives the calling-convention shape from a definition's type `scheme` and
    /// its `source_params` count (the syntactic `let f a b = …` binders). The
    /// leading offset-evidence parameters (integers) are non-float; each source
    /// parameter is unboxed when its declared type is exactly `Float`, and the
    /// result is unboxed when the residual return type is exactly `Float`. Reading
    /// the *signature* (not the body) keeps a caller's compiled code independent of
    /// a callee's body.
    #[must_use]
    pub fn from_scheme(scheme: &fai_types::Scheme, source_params: usize) -> FnAbi {
        let evidence = fai_types::evidence_count(scheme);
        let mut float_params = vec![false; evidence];
        let mut ty = &scheme.ty;
        for _ in 0..source_params {
            match ty {
                Ty::Arrow(from, to) => {
                    float_params.push(matches!(from.as_ref(), Ty::Con(Con::Float)));
                    ty = to;
                }
                // Fewer arrows than declared source parameters cannot happen for a
                // well-typed binding; stop defensively rather than panic.
                _ => break,
            }
        }
        FnAbi { float_params, float_return: matches!(ty, Ty::Con(Con::Float)) }
    }

    /// Whether runtime parameter `i` is passed as an unboxed `Float` (raw bits).
    #[must_use]
    pub fn float_param(&self, i: usize) -> bool {
        self.float_params.get(i).copied().unwrap_or(false)
    }

    /// Whether the entry uses the plain uniform ABI (no unboxed float parameter or
    /// result), so direct calls need no float marshalling and no bridging wrapper.
    #[must_use]
    pub fn is_uniform(&self) -> bool {
        !self.float_return && self.float_params.iter().all(|&f| !f)
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
        ExprKind::DataTag(base) => collect_globals(base, out),
        ExprKind::DataField { base, .. } => collect_globals(base, out),
        ExprKind::App { func, args } => {
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
        ExprKind::Reset { value, body, .. } => {
            collect_globals(value, out);
            collect_globals(body, out);
        }
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
}

/// Whether values of `ty` are boxed, reference-counted heap values, so lending
/// rather than consuming one saves a duplicate/drop. Strings, floats, records,
/// tuples, ADTs, lists, and interface dictionaries qualify; immediates
/// (`Int`/`Bool`/`Char`/`Unit`), functions, and type variables do not.
fn is_boxed_rc(ty: &Ty) -> bool {
    fn boxed_head(ty: &Ty) -> bool {
        match ty {
            Ty::Adt(_) | Ty::Interface(_) | Ty::Con(Con::List) => true,
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
                | Prim::StringLength
                | Prim::StringContains
                | Prim::ToUpper
                | Prim::ToLower
                | Prim::Trim
                | Prim::StrConcat
                | Prim::StringSplit
                | Prim::StringJoin
        ) && is_boxed_rc(operand_ty)
    }

    /// The runtime symbol of this primitive's non-consuming variant, for the
    /// inspect-only primitives that have one (see [`Prim::borrows_operand`]).
    #[must_use]
    pub fn borrowed_runtime_symbol(self) -> Option<&'static str> {
        match self {
            Prim::Eq => Some("fai_equal_borrowed"),
            Prim::Compare => Some("fai_compare_borrowed"),
            Prim::StringLength => Some("fai_string_length_borrowed"),
            Prim::StringContains => Some("fai_string_contains_borrowed"),
            Prim::ToUpper => Some("fai_to_upper_borrowed"),
            Prim::ToLower => Some("fai_to_lower_borrowed"),
            Prim::Trim => Some("fai_trim_borrowed"),
            Prim::StrConcat => Some("fai_string_concat_borrowed"),
            Prim::StringSplit => Some("fai_string_split_borrowed"),
            Prim::StringJoin => Some("fai_string_join_borrowed"),
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
            Prim::Not => "fai_not",
            Prim::ConsoleWriteLine => "fai_console_write_line",
            Prim::ClockNow => "fai_clock_now",
            Prim::RandomNextInt => "fai_random_next_int",
            Prim::FileRead => "fai_file_read",
            Prim::FileWrite => "fai_file_write",
            Prim::EnvGet => "fai_env_get",
            Prim::EnvArgs => "fai_env_args",
            Prim::RecordUpdate => "fai_record_update",
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
            | Prim::FileRead => 1,
            Prim::RecordUpdate => 3,
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
            "not" => Prim::Not,
            "consoleWriteLine" => Prim::ConsoleWriteLine,
            "clockNow" => Prim::ClockNow,
            "randomNextInt" => Prim::RandomNextInt,
            "fileRead" => Prim::FileRead,
            "fileWrite" => Prim::FileWrite,
            "envGet" => Prim::EnvGet,
            "envArgs" => Prim::EnvArgs,
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
    /// Builds a closure for a lifted function, capturing the given slots.
    MakeClosure {
        /// The lifted function.
        func: FnId,
        /// The captured slots, in the lifted function's `captures` order.
        captures: Vec<LocalId>,
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
    },
    /// Reads the tag of a data value (consuming `base`), as an immediate `Int`.
    DataTag(Box<CExpr>),
    /// Projects field `index` of a data value, consuming `base` and yielding an
    /// owned reference to the field.
    DataField {
        /// The data value (consumed).
        base: Box<CExpr>,
        /// The field slot (constant, or row-polymorphic `base + evidence`).
        index: FieldIndex,
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
