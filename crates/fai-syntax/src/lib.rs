//! The Fai surface syntax: interning, lexer, and tokens.
//!
//! This crate owns the compiler front end. It provides string interning
//! ([`Symbol`]), the hand-written [`lex`]er producing [`Token`]s and [`Comment`]
//! trivia, the [`layout`] pass that turns indentation into explicit block tokens,
//! and the recursive-descent [`parse_module`] producing the [`mod@ast`] tree;
//! comment attachment and incremental queries land in later stages.
//!
//! Diagnostics use the `FAI1xxx` range; every code is catalogued in [`CODES`].

pub mod ast;
mod attach;
mod layout;
mod lexer;
mod parser;
// salsa's `tracked`/`Update` macros emit `unsafe impl`s; this module is the only
// place in the crate that carries them (we write no `unsafe` by hand). The scoped
// allow mirrors the one on `fai-db`.
#[allow(unsafe_code)]
mod query;
mod symbol;
mod token;

pub use attach::{CommentId, CommentMap, NodeId, attach_comments};
pub use layout::{Layout, layout};
pub use lexer::{Lexed, lex};
pub use parser::{Parsed, parse_module};
pub use query::{
    ItemSummary, ItemTree, ItemTreeKind, ParsedModule, build_item_tree, item_tree, parse,
    public_item_count,
};
pub use symbol::Symbol;
pub use token::{Comment, CommentKind, Token, TokenKind};

use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};

/// An unexpected character that cannot begin any token.
pub const UNEXPECTED_CHAR: DiagnosticCode = DiagnosticCode::new("FAI1001");
/// A string literal with no closing quote.
pub const UNTERMINATED_STRING: DiagnosticCode = DiagnosticCode::new("FAI1002");
/// A block comment with no closing `*)`.
pub const UNTERMINATED_BLOCK_COMMENT: DiagnosticCode = DiagnosticCode::new("FAI1003");
/// A malformed character literal.
pub const INVALID_CHAR_LITERAL: DiagnosticCode = DiagnosticCode::new("FAI1004");
/// A malformed numeric literal (bad digits or an invalid suffix).
pub const INVALID_NUMBER: DiagnosticCode = DiagnosticCode::new("FAI1005");
/// An unrecognized escape sequence in a string or character literal.
pub const INVALID_ESCAPE: DiagnosticCode = DiagnosticCode::new("FAI1006");
/// A generic syntax error (an unexpected token, or a missing expected one).
pub const SYNTAX_ERROR: DiagnosticCode = DiagnosticCode::new("FAI1020");
/// Indentation that does not fit the offside rule (e.g. an un-indented block body).
pub const LAYOUT_ERROR: DiagnosticCode = DiagnosticCode::new("FAI1021");
/// A malformed or missing module header.
pub const MODULE_HEADER: DiagnosticCode = DiagnosticCode::new("FAI1022");
/// A construct that is reserved but not implemented yet (`type`, records, etc.).
pub const UNSUPPORTED: DiagnosticCode = DiagnosticCode::new("FAI1030");

/// Diagnostic codes owned by the lexer/parser layer (the `FAI1xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: UNEXPECTED_CHAR,
        title: "unexpected character",
        default_severity: Severity::Error,
        explanation: "The lexer met a character that cannot begin any token. Remove or correct \
                      the stray character.",
    },
    CodeInfo {
        code: UNTERMINATED_STRING,
        title: "unterminated string literal",
        default_severity: Severity::Error,
        explanation: "A string literal reached the end of the line or file without a closing \
                      double quote. Add the missing `\"`.",
    },
    CodeInfo {
        code: UNTERMINATED_BLOCK_COMMENT,
        title: "unterminated block comment",
        default_severity: Severity::Error,
        explanation: "A `(*` block comment was never closed. Add the matching `*)` (block \
                      comments nest, so each `(*` needs its own `*)`).",
    },
    CodeInfo {
        code: INVALID_CHAR_LITERAL,
        title: "invalid character literal",
        default_severity: Severity::Error,
        explanation: "A character literal is malformed — empty, multi-character, or missing its \
                      closing quote. A char literal holds exactly one character, e.g. `'a'`.",
    },
    CodeInfo {
        code: INVALID_NUMBER,
        title: "invalid numeric literal",
        default_severity: Severity::Error,
        explanation: "A numeric literal has invalid digits for its base or a trailing \
                      identifier character. Check the digits and remove any stray suffix.",
    },
    CodeInfo {
        code: INVALID_ESCAPE,
        title: "invalid escape sequence",
        default_severity: Severity::Error,
        explanation: "A string or character literal contains an unrecognized `\\` escape. The \
                      supported escapes are `\\n \\t \\r \\0 \\\\ \\\" \\' \\u{…}`.",
    },
    CodeInfo {
        code: SYNTAX_ERROR,
        title: "syntax error",
        default_severity: Severity::Error,
        explanation: "The parser found an unexpected token or a token it expected was missing. \
                      The message names what was expected; the parser recovers and continues so \
                      later errors are still reported.",
    },
    CodeInfo {
        code: LAYOUT_ERROR,
        title: "layout/indentation error",
        default_severity: Severity::Error,
        explanation: "Indentation does not fit the offside rule — typically a block body that \
                      is not indented past its enclosing block. `fai fmt` produces the canonical \
                      layout.",
    },
    CodeInfo {
        code: MODULE_HEADER,
        title: "malformed module header",
        default_severity: Severity::Error,
        explanation: "Every file must begin with a `module Name` header naming an upper-case \
                      module; it is missing or malformed.",
    },
    CodeInfo {
        code: UNSUPPORTED,
        title: "construct not yet supported",
        default_severity: Severity::Error,
        explanation: "A reserved construct that the parser recognizes but does not yet \
                      implement. It is rejected and recovered from until the feature lands.",
    },
];
