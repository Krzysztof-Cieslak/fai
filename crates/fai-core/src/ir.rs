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
use fai_types::Ty;

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

/// A lowered top-level definition: its entry function plus lifted lambdas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoweredDef {
    /// The definition this lowering belongs to.
    pub def: DefId,
    /// The functions; `fns[0]` is the definition's entry, the rest are lifted
    /// lambdas referenced by [`FnId`].
    pub fns: Vec<CoreFn>,
}

impl LoweredDef {
    /// The definition's entry function.
    #[must_use]
    pub fn entry(&self) -> &CoreFn {
        &self.fns[0]
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
        ExprKind::Dup { body, .. } | ExprKind::Drop { body, .. } => collect_globals(body, out),
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
    /// `Console.writeLine`
    ConsoleWriteLine,
}

impl Prim {
    /// The runtime symbol implementing this primitive.
    #[must_use]
    pub fn runtime_symbol(self) -> &'static str {
        match self {
            Prim::IntAdd => "fai_int_add",
            Prim::IntSub => "fai_int_sub",
            Prim::IntMul => "fai_int_mul",
            Prim::IntDiv => "fai_int_div",
            Prim::IntRem => "fai_int_rem",
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
            Prim::StringLength => "fai_string_length",
            Prim::ToUpper => "fai_to_upper",
            Prim::ToLower => "fai_to_lower",
            Prim::Trim => "fai_trim",
            Prim::StringContains => "fai_string_contains",
            Prim::StringSplit => "fai_string_split",
            Prim::StringJoin => "fai_string_join",
            Prim::Not => "fai_not",
            Prim::ConsoleWriteLine => "fai_console_write_line",
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
            | Prim::StringLength
            | Prim::ToUpper
            | Prim::ToLower
            | Prim::Trim
            | Prim::Not => 1,
            _ => 2,
        }
    }

    /// The primitive a named builtin maps to, if it is one (operators are lowered
    /// from `Binary` nodes, not via this table).
    #[must_use]
    pub fn from_builtin(name: &str) -> Option<Prim> {
        Some(match name {
            "intToString" => Prim::IntToString,
            "floatToString" => Prim::FloatToString,
            "intToFloat" => Prim::IntToFloat,
            "floatToInt" => Prim::FloatToInt,
            "sqrt" => Prim::Sqrt,
            "stringLength" => Prim::StringLength,
            "toUpper" => Prim::ToUpper,
            "toLower" => Prim::ToLower,
            "trim" => Prim::Trim,
            "stringContains" => Prim::StringContains,
            "stringConcat" => Prim::StrConcat,
            "split" => Prim::StringSplit,
            "join" => Prim::StringJoin,
            "not" => Prim::Not,
            "writeLine" => Prim::ConsoleWriteLine,
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
    },
    /// Reads the tag of a data value (consuming `base`), as an immediate `Int`.
    DataTag(Box<CExpr>),
    /// Projects field `index` of a data value, consuming `base` and yielding an
    /// owned reference to the field.
    DataField {
        /// The data value (consumed).
        base: Box<CExpr>,
        /// The field index.
        index: u32,
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
    /// A lowering error placeholder (an unsupported construct was reported).
    Error,
}
