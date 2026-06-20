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
///
/// `items` is the arena of **all** items — top-level *and* those nested inside an
/// [`ItemKind::Module`] — each addressed by a stable single-index [`ItemId`].
/// `roots` lists the top-level items in source order; a nested module's children
/// are listed (by `ItemId`) on its [`ItemKind::Module`]. Walk `roots` (recursing
/// into module bodies) to visit items with their enclosing-module context; index
/// `items` by `ItemId` to fetch any one item directly.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Module {
    /// The declared module name, or `None` if the header was missing/malformed.
    pub name: Option<Symbol>,
    /// Span of the module header (`module Name`), for diagnostics.
    pub header: TextRange,
    /// The arena of all items (top-level and nested), addressed by [`ItemId`].
    pub items: Vec<Item>,
    /// The top-level items, in source order (indices into `items`).
    pub roots: Vec<ItemId>,
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

    /// The arrow arity of a type: the count of leading `->`, unwrapping parens
    /// (`A -> B -> C` is 2). Used to derive a `foreign` declaration's parameter
    /// count from its written signature (it has no parameter patterns).
    #[must_use]
    pub fn arrow_arity(&self, id: TypeId) -> usize {
        match &self.ty(id).kind {
            TypeKind::Arrow { to, .. } => 1 + self.arrow_arity(*to),
            TypeKind::Paren(inner) => self.arrow_arity(*inner),
            _ => 0,
        }
    }

    /// The contract items (`example`/`forall`) in source order.
    pub fn contracts(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|it| it.kind.is_contract())
    }

    /// The `ordinal`-th contract item (`example`/`forall`) in source order.
    #[must_use]
    pub fn contract(&self, ordinal: usize) -> Option<&Item> {
        self.contracts().nth(ordinal)
    }
}

/// Visibility of a top-level binding.
///
/// Three tiers, ordered `Public > Internal > Private`. `Internal` is visible
/// across files within the same *origin* (today the standard library vs. user
/// code; a package boundary later) but hidden from other origins — so a
/// standard-library module can share a binding with its siblings without
/// exposing it to user programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Exported to every file (`public`).
    Public,
    /// Exported only to same-origin files (`internal`).
    Internal,
    /// Module-private (the default).
    Private,
}

impl Visibility {
    /// A reach rank where a larger value is more widely visible
    /// (`Public` = 2, `Internal` = 1, `Private` = 0). Used to compare reach,
    /// e.g. an exported surface must not name a type of lower rank.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Visibility::Public => 2,
            Visibility::Internal => 1,
            Visibility::Private => 0,
        }
    }

    /// Whether a binding with this visibility is exported across files at all
    /// (`public` or `internal`, i.e. not module-private).
    #[must_use]
    pub fn is_exported(self) -> bool {
        self != Visibility::Private
    }
}

/// A top-level declaration.
#[derive(Debug, PartialEq, Eq)]
pub struct Item {
    /// What the item declares.
    pub kind: ItemKind,
    /// The item's source range.
    pub span: TextRange,
}

/// The kind of a top-level [`Item`].
#[derive(Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A type signature: `[public] name : ty`.
    Signature { visibility: Visibility, name: Symbol, ty: TypeId },
    /// A value binding: `[public] let name params… = body`.
    Binding { visibility: Visibility, name: Symbol, params: Vec<PatId>, body: ExprId },
    /// A foreign function declaration: `foreign "native_symbol" name : ty`. Binds
    /// `name` to a native runtime function (no Fai body); `symbol` is the native
    /// symbol the call links to. The written `ty` is the declaration's signature.
    /// Always module-private (a raw native function is reached only through a
    /// capability interface); `visibility` is recorded so resolution can reject a
    /// `public foreign`.
    Foreign { visibility: Visibility, symbol: Symbol, name: Symbol, ty: TypeId },
    /// A type declaration: `[public] [opaque] type Name 'p… = <definition>`.
    ///
    /// `opaque` exports the type's name but not its definition (a union's
    /// constructors / an alias's underlying type), so other files may name the
    /// type but cannot construct, deconstruct, or see through it. It is only
    /// meaningful on a `public` type (the parser rejects `opaque` otherwise) and
    /// is file-scoped: the type stays transparent within its declaring file.
    Type { visibility: Visibility, opaque: bool, name: Symbol, params: Vec<Symbol>, def: TypeDef },
    /// An interface declaration: `[public] interface Name 'p… = <methods>`.
    Interface { visibility: Visibility, name: Symbol, params: Vec<Symbol>, methods: Vec<MethodSig> },
    /// A nested module: `module Name = <body>`. The body lists its child items by
    /// `ItemId` (into [`Module::items`]). Nested modules group declarations under a
    /// qualified path; they carry no visibility marker (member-level visibility
    /// governs cross-file access).
    Module { name: Symbol, body: Vec<ItemId> },
    /// An `example: body` contract.
    Example { body: ExprId },
    /// A `forall binders…: body` contract. Each binder is a `PatKind::Var`
    /// pattern, so it flows through resolution/inference/lowering exactly like a
    /// function parameter (its local and type are recoverable downstream).
    Forall { binders: Vec<PatId>, body: ExprId },
    /// An unparseable item (recovered).
    Error,
}

impl ItemKind {
    /// Whether this item is a contract declaration (`example`/`forall`).
    #[must_use]
    pub fn is_contract(&self) -> bool {
        matches!(self, ItemKind::Example { .. } | ItemKind::Forall { .. })
    }
}

/// One method signature in an [`ItemKind::Interface`]: `name : ty` (no `self`).
#[derive(Debug, PartialEq, Eq)]
pub struct MethodSig {
    /// The method name.
    pub name: Symbol,
    /// The method's type.
    pub ty: TypeId,
    /// The method's source range.
    pub span: TextRange,
}

/// The body of a [`ItemKind::Type`] declaration.
#[derive(Debug, PartialEq, Eq)]
pub enum TypeDef {
    /// A discriminated union: `| A | B 'a` (nominal; may be recursive).
    Union(Vec<Variant>),
    /// A transparent alias to a type expression (must be acyclic).
    Alias(TypeId),
}

/// One variant of a discriminated union: a constructor name and its field types.
#[derive(Debug, PartialEq, Eq)]
pub struct Variant {
    /// The constructor name (an upper-case identifier).
    pub name: Symbol,
    /// The constructor's field types, in order (empty for a nullary constructor).
    pub fields: Vec<TypeId>,
    /// The variant's source range.
    pub span: TextRange,
}

/// An expression.
#[derive(Debug, PartialEq, Eq)]
pub struct Expr {
    /// What the expression is.
    pub kind: ExprKind,
    /// The expression's source range.
    pub span: TextRange,
}

/// The kind of an [`Expr`]. Literals store their raw (interned) lexeme.
#[derive(Debug, PartialEq, Eq)]
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
    /// An infix operator application `lhs op rhs`. `op` is a [`ExprKind::Var`]
    /// node holding the operator symbol, so it resolves and types like any name.
    Infix { op: ExprId, lhs: ExprId, rhs: ExprId },
    /// A prefix operator application `op operand` (e.g. unary minus). `op` is a
    /// [`ExprKind::Var`] node holding the operator symbol.
    Prefix { op: ExprId, operand: ExprId },
    /// `if cond then then_branch else else_branch`.
    If { cond: ExprId, then_branch: ExprId, else_branch: ExprId },
    /// `fun params… -> body`.
    Lambda { params: Vec<PatId>, body: ExprId },
    /// `match scrutinee with | pat -> body …`.
    Match { scrutinee: ExprId, arms: Vec<MatchArm> },
    /// A layout block: local `let`s followed by a tail expression.
    Block { stmts: Vec<LetStmt>, tail: ExprId },
    /// Record field access `base.field`.
    Field { base: ExprId, field: Symbol },
    /// A record literal `{ x = a, y = b }` (closed).
    Record(Vec<FieldInit>),
    /// A record update `{ base with x = a, … }`.
    RecordUpdate { base: ExprId, fields: Vec<FieldInit> },
    /// An interface instance `{ Name with m args = body, … }` (an existential
    /// value compiled to a dictionary of closures).
    Instance { name: Symbol, methods: Vec<MethodImpl> },
    /// A parenthesized expression (kept so the formatter is faithful).
    Paren(ExprId),
    /// A tuple `(a, b, …)` (two or more elements).
    Tuple(Vec<ExprId>),
    /// A list literal `[a, b, …]`.
    List(Vec<ExprId>),
    /// An array literal `[| a, b, … |]`.
    Array(Vec<ExprId>),
    /// An unparseable expression (recovered).
    Error,
}

/// One arm of a [`ExprKind::Match`]: `| pat -> body`.
#[derive(Debug, PartialEq, Eq)]
pub struct MatchArm {
    /// The arm pattern (an [`PatKind::Or`] when the arm lists alternatives).
    pub pat: PatId,
    /// The arm body.
    pub body: ExprId,
    /// The arm's source range.
    pub span: TextRange,
}

/// One field of a record literal or update: `name = value`.
#[derive(Debug, PartialEq, Eq)]
pub struct FieldInit {
    /// The field label.
    pub name: Symbol,
    /// The field's value.
    pub value: ExprId,
    /// The field's source range.
    pub span: TextRange,
}

/// One method implementation in an [`ExprKind::Instance`]: `name params… = body`
/// (ML method sugar). Sibling methods are not in scope in the body.
#[derive(Debug, PartialEq, Eq)]
pub struct MethodImpl {
    /// The method name.
    pub name: Symbol,
    /// The method's parameters (empty for a value-shaped method).
    pub params: Vec<PatId>,
    /// The method body.
    pub body: ExprId,
    /// The method's source range.
    pub span: TextRange,
}

/// A local `let` statement inside a [`ExprKind::Block`].
#[derive(Debug, PartialEq, Eq)]
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
    /// `::` (the built-in list constructor)
    Cons,
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

/// Classifies an operator symbol as a built-in binary operator, or `None` for a
/// user-defined operator (which resolves and types like an ordinary function).
/// This is a pure function of the lexeme; whether a built-in is *shadowed* is
/// decided by name resolution, not here.
#[must_use]
pub fn classify_op(symbol: Symbol) -> Option<BinOp> {
    Some(match symbol.as_str() {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "%" => BinOp::Rem,
        "::" => BinOp::Cons,
        "&&" => BinOp::And,
        "||" => BinOp::Or,
        "=" => BinOp::Eq,
        "<>" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        _ => return None,
    })
}

/// Classifies an operator symbol as a built-in prefix operator (only unary `-`),
/// or `None` for a user-defined prefix operator.
#[must_use]
pub fn classify_prefix(symbol: Symbol) -> Option<UnOp> {
    match symbol.as_str() {
        "-" => Some(UnOp::Neg),
        _ => None,
    }
}

/// A pattern.
#[derive(Debug, PartialEq, Eq)]
pub struct Pat {
    /// What the pattern is.
    pub kind: PatKind,
    /// The pattern's source range.
    pub span: TextRange,
}

/// The kind of a [`Pat`].
#[derive(Debug, PartialEq, Eq)]
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
    /// A constructor pattern `Name p…` (nullary when `args` is empty).
    Constructor { name: Symbol, args: Vec<PatId> },
    /// An integer literal pattern (raw lexeme).
    Int(Symbol),
    /// A float literal pattern (raw lexeme).
    Float(Symbol),
    /// A string literal pattern (raw lexeme).
    String(Symbol),
    /// A character literal pattern (raw lexeme).
    Char(Symbol),
    /// A boolean literal pattern.
    Bool(bool),
    /// A list pattern `[a, b, …]`.
    List(Vec<PatId>),
    /// A cons pattern `head :: tail`.
    Cons { head: PatId, tail: PatId },
    /// An or-pattern `a | b | …` (alternatives must bind the same variables).
    Or(Vec<PatId>),
    /// An as-pattern `p as name`: matches `p` and also binds the whole matched
    /// value to `name`. Binds looser than every other pattern form.
    As { pat: PatId, name: Symbol },
    /// A record pattern `{ x = p, y }` (closed) or `{ x = p | _ }` (open).
    Record { fields: Vec<FieldPat>, open: bool },
    /// An unparseable pattern (recovered).
    Error,
}

/// One field of a record pattern: `name = pat`, or `name` (field punning, which
/// binds a variable of the field's name). Punning carries a synthesized
/// `Var(name)` sub-pattern so later phases treat all fields uniformly.
#[derive(Debug, PartialEq, Eq)]
pub struct FieldPat {
    /// The field label.
    pub name: Symbol,
    /// The sub-pattern (`Var(name)` when punned).
    pub pat: PatId,
    /// Whether this field used punning (`{ x }`), for faithful formatting.
    pub punned: bool,
    /// The field's source range.
    pub span: TextRange,
}

/// A type expression.
#[derive(Debug, PartialEq, Eq)]
pub struct Type {
    /// What the type is.
    pub kind: TypeKind,
    /// The type's source range.
    pub span: TextRange,
}

/// The kind of a [`Type`].
#[derive(Debug, PartialEq, Eq)]
pub enum TypeKind {
    /// A type variable `'a`.
    Var(Symbol),
    /// A type constructor name `Int`, `List`, …
    Con(Symbol),
    /// Type application `func arg` (curried), e.g. `List 'a`.
    App { func: TypeId, arg: TypeId },
    /// A function type `from -> to` (right-associative), with an optional effect
    /// annotation (`a -> b / { Console | 'e }`). `None` is the pure (bare) arrow;
    /// the annotation binds this arrow (the innermost in a curried chain).
    Arrow { from: TypeId, to: TypeId, effect: Option<EffectAnnot> },
    /// A tuple type `a * b * …`.
    Tuple(Vec<TypeId>),
    /// A record type `{ x : T, … }` with a closed, anonymous-open, or named-open
    /// tail.
    Record { fields: Vec<FieldType>, tail: RowTail },
    /// An effect row written as an argument (`{ Console | 'e }`), valid only as an
    /// interface's effect argument (`Logger { Console }`). Distinguished from a
    /// record by its leading capability name (atoms are upper-case).
    EffectRow { labels: Vec<Symbol>, tail: RowTail },
    /// The unit type `()`.
    Unit,
    /// A parenthesized type.
    Paren(TypeId),
    /// An unparseable type (recovered).
    Error,
}

/// One field of a record type: `name : ty`.
#[derive(Debug, PartialEq, Eq)]
pub struct FieldType {
    /// The field label.
    pub name: Symbol,
    /// The field's type.
    pub ty: TypeId,
    /// The field's source range.
    pub span: TextRange,
}

/// A written effect annotation on an arrow: the capability atoms (interface
/// names) it uses plus a tail. A lone effect variable (`/ 'e`) is `labels` empty
/// with a `Named` tail; `/ { Console | _ }` is one label with an `Open` tail.
#[derive(Debug, PartialEq, Eq)]
pub struct EffectAnnot {
    /// The effect atoms (capability interface names), in written order.
    pub labels: Vec<Symbol>,
    /// The row's tail (closed, anonymous-open `_`, or a named variable).
    pub tail: RowTail,
    /// The annotation's source range (the `/ …` span).
    pub span: TextRange,
}

/// The tail of a written record type, governing its openness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTail {
    /// `{ x : T }` — exactly these fields.
    Closed,
    /// `{ x : T | _ }` — these fields and any others (a fresh anonymous row).
    Open,
    /// `{ x : T | 'r }` — these fields plus the named tail `'r`.
    Named(Symbol),
}
