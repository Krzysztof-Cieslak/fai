//! The surface abstract syntax tree.
//!
//! Nodes live in per-category arenas on [`Module`] and are referenced by
//! newtyped indices ([`ExprId`], [`PatId`], [`TypeId`], [`ItemId`]); there are no
//! `Box`/`Rc` graphs. Every node carries an inline file-relative [`TextRange`].
//! Identifiers are interned to [`Symbol`]; literals keep their raw lexeme
//! (interned) so the formatter can reproduce them verbatim and later phases can
//! decode the value.
//!
//! The tree is total under error recovery: each category has an `Error` variant
//! so a malformed fragment still yields a well-formed node.

use fai_span::TextRange;

use crate::Symbol;

macro_rules! arena_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub struct $name(u32);

        impl $name {
            /// Builds an id from a raw arena index.
            #[must_use]
            pub fn from_index(index: usize) -> Self {
                Self(u32::try_from(index).expect("AST arena index overflow"))
            }

            /// The backing arena index.
            #[must_use]
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

arena_id!(
    /// Identifies an [`Expr`] in [`Module::exprs`].
    ExprId
);
arena_id!(
    /// Identifies a [`Pat`] in [`Module::pats`].
    PatId
);
arena_id!(
    /// Identifies a [`Type`] in [`Module::types`].
    TypeId
);
arena_id!(
    /// Identifies an [`Item`] in [`Module::items`].
    ItemId
);

/// A parsed module: its header plus the node arenas.
#[derive(Debug, Default)]
pub struct Module {
    /// The declared module name, or `None` if the header was missing/malformed.
    pub name: Option<Symbol>,
    /// Span of the module header (`module Name`), for diagnostics.
    pub header: TextRange,
    /// Top-level items, in source order.
    pub items: Vec<Item>,
    /// Expression arena.
    pub exprs: Vec<Expr>,
    /// Pattern arena.
    pub pats: Vec<Pat>,
    /// Type-expression arena.
    pub types: Vec<Type>,
}

impl Module {
    /// Returns the expression for `id`.
    #[must_use]
    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.index()]
    }

    /// Returns the pattern for `id`.
    #[must_use]
    pub fn pat(&self, id: PatId) -> &Pat {
        &self.pats[id.index()]
    }

    /// Returns the type for `id`.
    #[must_use]
    pub fn ty(&self, id: TypeId) -> &Type {
        &self.types[id.index()]
    }
}

/// Visibility of a top-level binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Exported (`public`).
    Public,
    /// Module-private (the default).
    Private,
}

/// A top-level declaration.
#[derive(Debug)]
pub struct Item {
    /// What the item declares.
    pub kind: ItemKind,
    /// The item's source range.
    pub span: TextRange,
}

/// The kind of a top-level [`Item`].
#[derive(Debug)]
pub enum ItemKind {
    /// A type signature: `[public] name : ty`.
    Signature { visibility: Visibility, name: Symbol, ty: TypeId },
    /// A value binding: `[public] let name params… = body`.
    Binding { visibility: Visibility, name: Symbol, params: Vec<PatId>, body: ExprId },
    /// An `example: body` contract.
    Example { body: ExprId },
    /// A `forall binders…: body` contract.
    Forall { binders: Vec<Symbol>, body: ExprId },
    /// An unparseable item (recovered).
    Error,
}

/// An expression.
#[derive(Debug)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// The expression's source range.
    pub span: TextRange,
}

/// The kind of an [`Expr`]. Literals store their raw (interned) lexeme.
#[derive(Debug)]
pub enum ExprKind {
    /// Integer literal (raw lexeme, e.g. `0xFF`, `1_000`).
    Int(Symbol),
    /// Float literal (raw lexeme).
    Float(Symbol),
    /// String literal (raw lexeme, including quotes and escapes).
    String(Symbol),
    /// Character literal (raw lexeme).
    Char(Symbol),
    /// A name reference (`true`/`false` included).
    Var(Symbol),
    /// The unit value `()`.
    Unit,
    /// Function application `func arg` (curried; one argument per node).
    App { func: ExprId, arg: ExprId },
    /// A binary operator application.
    Binary { op: BinOp, lhs: ExprId, rhs: ExprId },
    /// A prefix operator application (unary minus).
    Unary { op: UnOp, operand: ExprId },
    /// `if cond then then_branch else else_branch`.
    If { cond: ExprId, then_branch: ExprId, else_branch: ExprId },
    /// `fun params… -> body`.
    Lambda { params: Vec<PatId>, body: ExprId },
    /// A layout block: local `let`s followed by a tail expression.
    Block { stmts: Vec<LetStmt>, tail: ExprId },
    /// Record field access `base.field`.
    Field { base: ExprId, field: Symbol },
    /// A parenthesized expression (kept so the formatter is faithful).
    Paren(ExprId),
    /// A tuple `(a, b, …)` (two or more elements).
    Tuple(Vec<ExprId>),
    /// A list literal `[a, b, …]`.
    List(Vec<ExprId>),
    /// An unparseable expression (recovered).
    Error,
}

/// A local `let` statement inside a [`ExprKind::Block`].
#[derive(Debug)]
pub struct LetStmt {
    /// The bound pattern.
    pub pat: PatId,
    /// Parameters, for a local function binding (empty for a plain `let`).
    pub params: Vec<PatId>,
    /// The bound value.
    pub value: ExprId,
    /// The statement's source range.
    pub span: TextRange,
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `++`
    Concat,
    /// `::`
    Cons,
    /// `|>`
    Pipe,
    /// `>>`
    Compose,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A prefix (unary) operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// Arithmetic negation `-`.
    Neg,
}

/// A pattern.
#[derive(Debug)]
pub struct Pat {
    /// What the pattern is.
    pub kind: PatKind,
    /// The pattern's source range.
    pub span: TextRange,
}

/// The kind of a [`Pat`].
#[derive(Debug)]
pub enum PatKind {
    /// A variable binding.
    Var(Symbol),
    /// The wildcard `_`.
    Wildcard,
    /// The unit pattern `()`.
    Unit,
    /// A tuple pattern `(a, b, …)`.
    Tuple(Vec<PatId>),
    /// A parenthesized pattern.
    Paren(PatId),
    /// An unparseable pattern (recovered).
    Error,
}

/// A type expression.
#[derive(Debug)]
pub struct Type {
    /// What the type is.
    pub kind: TypeKind,
    /// The type's source range.
    pub span: TextRange,
}

/// The kind of a [`Type`].
#[derive(Debug)]
pub enum TypeKind {
    /// A type variable `'a`.
    Var(Symbol),
    /// A type constructor name `Int`, `List`, …
    Con(Symbol),
    /// Type application `func arg` (curried), e.g. `List 'a`.
    App { func: TypeId, arg: TypeId },
    /// A function type `from -> to` (right-associative).
    Arrow { from: TypeId, to: TypeId },
    /// A tuple type `a * b * …`.
    Tuple(Vec<TypeId>),
    /// The unit type `()`.
    Unit,
    /// A parenthesized type.
    Paren(TypeId),
    /// An unparseable type (recovered).
    Error,
}
