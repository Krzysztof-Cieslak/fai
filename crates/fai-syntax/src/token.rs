//! Tokens and comment trivia produced by the lexer.
//!
//! Tokens carry a [`TextRange`] (file-relative) rather than the lexeme itself;
//! callers recover the text from the source. Comments are kept separately as
//! trivia so the parser and layout pass can ignore them while the formatter can
//! still reproduce them.

use fai_span::TextRange;

/// A lexical token: a [`TokenKind`] tagged with its source range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    /// What the token is.
    pub kind: TokenKind,
    /// The token's byte range in its source file.
    pub range: TextRange,
}

impl Token {
    /// Creates a token of `kind` covering `range`.
    #[must_use]
    pub fn new(kind: TokenKind, range: TextRange) -> Self {
        Self { kind, range }
    }
}

/// The kind of a [`Token`].
///
/// `true`/`false` are intentionally **not** keywords: they lex as
/// [`TokenKind::LowerIdent`] and resolve to prelude values later, matching the
/// reserved-keyword set in `AGENTS.md` §11.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    // Identifiers & names.
    LowerIdent,
    UpperIdent,
    TypeVar,
    Underscore,

    // Literals.
    Int,
    Float,
    String,
    Char,

    // Keywords.
    Module,
    Let,
    Type,
    Interface,
    Match,
    With,
    If,
    Then,
    Else,
    Fun,
    Public,
    Internal,
    Opaque,
    Example,
    Forall,
    As,
    Foreign,

    // Grouping & punctuation.
    LParen,
    RParen,
    LBracket,
    RBracket,
    /// `[|` — opens an array literal.
    LArrayBracket,
    /// `|]` — closes an array literal.
    RArrayBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    // Reserved symbols carved out of the operator-character runs: the type/sig
    // colon, the function arrow, the binding/equality `=`, the match/row `|`,
    // and the list-cons `::`. Everything else made of operator characters is an
    // `Operator`.
    Colon,
    Arrow,
    Equals,
    Pipe,
    ColonColon,

    /// A symbolic operator: a maximal run of operator characters (e.g. `+`,
    /// `|>`, `<$>`). Its lexeme is recovered from the token's range.
    Operator,

    // Virtual layout tokens, inserted by the layout pass (never produced by the
    // lexer). They delimit indentation-derived blocks for the parser.
    LayoutOpen,
    LayoutSep,
    LayoutClose,

    // End of input.
    Eof,
}

impl TokenKind {
    /// Returns the keyword token kind for `text`, if it is reserved.
    #[must_use]
    pub fn keyword(text: &str) -> Option<TokenKind> {
        Some(match text {
            "module" => TokenKind::Module,
            "let" => TokenKind::Let,
            "type" => TokenKind::Type,
            "interface" => TokenKind::Interface,
            "match" => TokenKind::Match,
            "with" => TokenKind::With,
            "if" => TokenKind::If,
            "then" => TokenKind::Then,
            "else" => TokenKind::Else,
            "fun" => TokenKind::Fun,
            "public" => TokenKind::Public,
            "internal" => TokenKind::Internal,
            "opaque" => TokenKind::Opaque,
            "example" => TokenKind::Example,
            "forall" => TokenKind::Forall,
            "as" => TokenKind::As,
            "foreign" => TokenKind::Foreign,
            _ => return None,
        })
    }
}

/// The kind of a [`Comment`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommentKind {
    /// `// ...`
    Line,
    /// `(* ... *)` (nestable)
    Block,
    /// `/// ...`
    Doc,
}

/// A comment, retained as trivia for the formatter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Comment {
    /// What kind of comment this is.
    pub kind: CommentKind,
    /// The comment's byte range in its source file.
    pub range: TextRange,
}
