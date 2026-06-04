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
    CodeInfo { code: SYNTAX_ERROR, title: "syntax error", default_severity: Severity::Error },
    CodeInfo {
        code: LAYOUT_ERROR,
        title: "layout/indentation error",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: MODULE_HEADER,
        title: "malformed module header",
        default_severity: Severity::Error,
    },
    CodeInfo {
        code: UNSUPPORTED,
        title: "construct not yet supported",
        default_severity: Severity::Error,
    },
];
