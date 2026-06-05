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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lit {
    /// A 64-bit integer (decoded from its lexeme).
    Int(i64),
    /// A boolean.
    Bool(bool),
    /// A string's decoded UTF-8 bytes.
    Str(Vec<u8>),
    /// The unit value.
    Unit,
}

/// A primitive operation, lowered to a runtime call. Every primitive consumes
/// its operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// `=` (structural equality)
    Eq,
    /// `++` (string concatenation)
    StrConcat,
    /// `intToString`
    IntToString,
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
            Prim::Eq => "fai_equal",
            Prim::StrConcat => "fai_string_concat",
            Prim::IntToString => "fai_int_to_string",
            Prim::Not => "fai_not",
            Prim::ConsoleWriteLine => "fai_console_write_line",
        }
    }

    /// How many operands the primitive takes.
    #[must_use]
    pub fn arity(self) -> usize {
        match self {
            Prim::IntToString | Prim::Not => 1,
            _ => 2,
        }
    }

    /// The primitive a named builtin maps to, if it is one (operators are lowered
    /// from `Binary` nodes, not via this table).
    #[must_use]
    pub fn from_builtin(name: &str) -> Option<Prim> {
        Some(match name {
            "intToString" => Prim::IntToString,
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
