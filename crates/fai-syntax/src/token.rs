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
/// reserved-keyword set in `Agents.md` §11.
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
    Example,
    Forall,

    // Grouping & punctuation.
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    Colon,
    Arrow,
    Equals,
    Pipe,

    // Operators.
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusPlus,
    ColonColon,
    PipeGreater,
    GreaterGreater,
    AmpAmp,
    PipePipe,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    NotEq,

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
            "example" => TokenKind::Example,
            "forall" => TokenKind::Forall,
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
