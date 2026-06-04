//! The Fai surface syntax: interning, lexer, and tokens.
//!
//! This crate owns the compiler front end. It currently provides string
//! interning ([`Symbol`]) and the hand-written [`lex`]er producing [`Token`]s and
//! [`Comment`] trivia; the layout pass, parser, AST, and incremental queries land
//! in later stages.
//!
//! Diagnostics use the `FAI1xxx` range; every code is catalogued in [`CODES`].

mod lexer;
mod symbol;
mod token;

pub use lexer::{Lexed, lex};
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

/// Diagnostic codes owned by the lexer/parser layer (the `FAI1xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: UNEXPECTED_CHAR,
        title: "unexpected character",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNTERMINATED_STRING,
        title: "unterminated string literal",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNTERMINATED_BLOCK_COMMENT,
        title: "unterminated block comment",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: INVALID_CHAR_LITERAL,
        title: "invalid character literal",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: INVALID_NUMBER,
        title: "invalid numeric literal",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: INVALID_ESCAPE,
        title: "invalid escape sequence",
        default_severity: Severity::Error,
    },
];
