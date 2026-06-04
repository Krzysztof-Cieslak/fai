//! The hand-written lexer.
//!
//! [`lex`] turns source text into [`Token`]s, [`Comment`] trivia, and
//! diagnostics. It is a pure function (no database): the [`SourceId`] is supplied
//! so diagnostics can carry a file-qualified [`Span`], while tokens and comments
//! store file-relative [`TextRange`]s. Whitespace and newlines are dropped (the
//! layout pass recovers line/column from byte ranges); comments are kept as
//! trivia for the formatter. Lexing always terminates with an [`TokenKind::Eof`].

use fai_diagnostics::{Diagnostic, DiagnosticCode};
use fai_span::{ByteOffset, SourceId, Span, TextRange};

use crate::token::{Comment, CommentKind, Token, TokenKind};
use crate::{
    INVALID_CHAR_LITERAL, INVALID_ESCAPE, INVALID_NUMBER, UNEXPECTED_CHAR,
    UNTERMINATED_BLOCK_COMMENT, UNTERMINATED_STRING,
};

/// The result of lexing one source file.
#[derive(Debug, Default)]
pub struct Lexed {
    /// The significant tokens, ending with [`TokenKind::Eof`].
    pub tokens: Vec<Token>,
    /// Comment trivia, in source order.
    pub comments: Vec<Comment>,
    /// Diagnostics produced while lexing.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes `text` (belonging to `source`) into tokens, comment trivia, and
/// diagnostics.
#[must_use]
pub fn lex(source: SourceId, text: &str) -> Lexed {
    Lexer { source, text, pos: 0, out: Lexed::default() }.run()
}

struct Lexer<'a> {
    source: SourceId,
    text: &'a str,
    pos: usize,
    out: Lexed,
}

impl Lexer<'_> {
    fn run(mut self) -> Lexed {
        loop {
            self.skip_whitespace();
            let start = self.pos;
            let Some(c) = self.peek() else {
                self.push(TokenKind::Eof, TextRange::empty(self.offset()));
                break;
            };
            match c {
                '/' if self.starts_with("//") => self.line_comment(start),
                '(' if self.starts_with("(*") => self.block_comment(start),
                '"' => self.string(start),
                '\'' => self.quote(start),
                c if c.is_ascii_digit() => self.number(start),
                c if is_ident_start(c) => self.ident(start),
                _ => self.operator_or_unknown(start),
            }
        }
        self.out
    }

    // --- cursor -----------------------------------------------------------

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.rest().starts_with(prefix)
    }

    fn nth(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn peek(&self) -> Option<char> {
        self.nth(0)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn offset(&self) -> ByteOffset {
        ByteOffset::from_usize(self.pos)
    }

    fn range_from(&self, start: usize) -> TextRange {
        TextRange::new(ByteOffset::from_usize(start), self.offset())
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, ' ' | '\t' | '\n' | '\r') {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    // --- emit -------------------------------------------------------------

    fn push(&mut self, kind: TokenKind, range: TextRange) {
        self.out.tokens.push(Token::new(kind, range));
    }

    fn comment(&mut self, kind: CommentKind, range: TextRange) {
        self.out.comments.push(Comment { kind, range });
    }

    fn error(&mut self, code: DiagnosticCode, range: TextRange, message: impl Into<String>) {
        let span = Span::new(self.source, range);
        self.out.diagnostics.push(Diagnostic::error(code, message, span));
    }

    // --- comments ---------------------------------------------------------

    fn line_comment(&mut self, start: usize) {
        // `///` is a doc comment, but `////`+ is an ordinary line comment.
        let doc = self.starts_with("///") && !self.starts_with("////");
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.pos += c.len_utf8();
        }
        let kind = if doc { CommentKind::Doc } else { CommentKind::Line };
        let range = self.range_from(start);
        self.comment(kind, range);
    }

    fn block_comment(&mut self, start: usize) {
        self.pos += 2; // `(*`
        let mut depth = 1u32;
        while depth > 0 {
            if self.peek().is_none() {
                let range = self.range_from(start);
                self.error(UNTERMINATED_BLOCK_COMMENT, range, "unterminated block comment");
                break;
            }
            if self.starts_with("(*") {
                self.pos += 2;
                depth += 1;
            } else if self.starts_with("*)") {
                self.pos += 2;
                depth -= 1;
            } else if let Some(c) = self.peek() {
                self.pos += c.len_utf8();
            }
        }
        let range = self.range_from(start);
        self.comment(CommentKind::Block, range);
    }

    // --- strings & escapes ------------------------------------------------

    fn string(&mut self, start: usize) {
        self.pos += 1; // opening quote
        loop {
            match self.peek() {
                None | Some('\n') => {
                    let range = self.range_from(start);
                    self.error(UNTERMINATED_STRING, range, "unterminated string literal");
                    break;
                }
                Some('"') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    self.escape();
                }
                Some(c) => self.pos += c.len_utf8(),
            }
        }
        let range = self.range_from(start);
        self.push(TokenKind::String, range);
    }

    /// Validates an escape sequence; the cursor is positioned just after the `\`.
    fn escape(&mut self) {
        let backslash = self.pos - 1;
        match self.peek() {
            Some('n' | 't' | 'r' | '0' | '\\' | '"' | '\'') => self.pos += 1,
            Some('u') => {
                self.pos += 1;
                if !self.unicode_escape_body() {
                    let range = self.range_from(backslash);
                    self.error(
                        INVALID_ESCAPE,
                        range,
                        "invalid unicode escape; expected `\\u{...}`",
                    );
                }
            }
            Some(c) => {
                self.pos += c.len_utf8();
                let range = self.range_from(backslash);
                self.error(INVALID_ESCAPE, range, "unknown escape sequence");
            }
            None => {
                let range = self.range_from(backslash);
                self.error(INVALID_ESCAPE, range, "unterminated escape sequence");
            }
        }
    }

    /// Consumes `{ hex+ }` after a `\u`; returns whether it was well-formed.
    fn unicode_escape_body(&mut self) -> bool {
        if !self.eat('{') {
            return false;
        }
        let mut any = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_hexdigit() {
                any = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        any && self.eat('}')
    }

    // --- character literals vs type variables -----------------------------

    fn quote(&mut self, start: usize) {
        match (self.nth(1), self.nth(2)) {
            (Some('\\'), _) => self.char_literal(start),
            (Some('\''), _) => {
                self.pos += 2; // `''`
                let range = self.range_from(start);
                self.error(INVALID_CHAR_LITERAL, range, "empty character literal");
            }
            (Some(c), Some('\'')) if c != '\n' => self.char_literal(start),
            _ => self.type_var(start),
        }
    }

    fn char_literal(&mut self, start: usize) {
        self.pos += 1; // opening quote
        match self.peek() {
            Some('\\') => {
                self.pos += 1;
                self.escape();
            }
            Some(c) if c != '\'' && c != '\n' => self.pos += c.len_utf8(),
            _ => {}
        }
        if self.eat('\'') {
            let range = self.range_from(start);
            self.push(TokenKind::Char, range);
        } else {
            let range = self.range_from(start);
            self.error(
                INVALID_CHAR_LITERAL,
                range,
                "unterminated character literal; expected closing `'`",
            );
        }
    }

    fn type_var(&mut self, start: usize) {
        self.pos += 1; // the tick
        if self.peek().is_some_and(is_ident_start) {
            while self.peek().is_some_and(is_ident_continue) {
                self.bump();
            }
            let range = self.range_from(start);
            self.push(TokenKind::TypeVar, range);
        } else {
            let range = self.range_from(start);
            self.error(
                INVALID_CHAR_LITERAL,
                range,
                "expected a type-variable name or a character literal after `'`",
            );
        }
    }

    // --- numbers ----------------------------------------------------------

    fn number(&mut self, start: usize) {
        let mut is_float = false;
        let mut ok = true;

        if self.peek() == Some('0')
            && matches!(self.nth(1), Some('x' | 'X' | 'o' | 'O' | 'b' | 'B'))
        {
            self.pos += 1; // `0`
            let base = self.bump().unwrap_or('x').to_ascii_lowercase();
            let mut any = false;
            while let Some(c) = self.peek() {
                let valid = match base {
                    'x' => c.is_ascii_hexdigit() || c == '_',
                    'o' => ('0'..='7').contains(&c) || c == '_',
                    _ => c == '0' || c == '1' || c == '_',
                };
                if !valid {
                    break;
                }
                any |= c != '_';
                self.pos += 1;
            }
            ok = any;
        } else {
            self.consume_digits();
            if self.peek() == Some('.') && self.nth(1).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1; // `.`
                self.consume_digits();
                is_float = true;
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                let save = self.pos;
                self.pos += 1; // `e`
                if matches!(self.peek(), Some('+' | '-')) {
                    self.pos += 1;
                }
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.consume_digits();
                    is_float = true;
                } else {
                    self.pos = save; // not an exponent; let the suffix check flag it
                }
            }
        }

        // Any trailing identifier characters are an invalid suffix.
        if self.peek().is_some_and(is_ident_continue) {
            while self.peek().is_some_and(is_ident_continue) {
                self.bump();
            }
            ok = false;
        }

        if !ok {
            let range = self.range_from(start);
            self.error(INVALID_NUMBER, range, "invalid numeric literal");
        }
        let range = self.range_from(start);
        self.push(if is_float { TokenKind::Float } else { TokenKind::Int }, range);
    }

    fn consume_digits(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    // --- identifiers & keywords -------------------------------------------

    fn ident(&mut self, start: usize) {
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        let text = &self.text[start..self.pos];
        let kind = if text == "_" {
            TokenKind::Underscore
        } else if let Some(keyword) = TokenKind::keyword(text) {
            keyword
        } else if text.as_bytes()[0].is_ascii_uppercase() {
            TokenKind::UpperIdent
        } else {
            TokenKind::LowerIdent
        };
        let range = self.range_from(start);
        self.push(kind, range);
    }

    // --- operators & punctuation ------------------------------------------

    fn operator_or_unknown(&mut self, start: usize) {
        let Some(c) = self.bump() else { return };
        let kind = match c {
            '(' => Some(TokenKind::LParen),
            ')' => Some(TokenKind::RParen),
            '[' => Some(TokenKind::LBracket),
            ']' => Some(TokenKind::RBracket),
            '{' => Some(TokenKind::LBrace),
            '}' => Some(TokenKind::RBrace),
            ',' => Some(TokenKind::Comma),
            '.' => Some(TokenKind::Dot),
            '=' => Some(TokenKind::Equals),
            '*' => Some(TokenKind::Star),
            '/' => Some(TokenKind::Slash),
            '%' => Some(TokenKind::Percent),
            '+' => Some(if self.eat('+') { TokenKind::PlusPlus } else { TokenKind::Plus }),
            '-' => Some(if self.eat('>') { TokenKind::Arrow } else { TokenKind::Minus }),
            ':' => Some(if self.eat(':') { TokenKind::ColonColon } else { TokenKind::Colon }),
            '|' => Some(if self.eat('>') {
                TokenKind::PipeGreater
            } else if self.eat('|') {
                TokenKind::PipePipe
            } else {
                TokenKind::Pipe
            }),
            '>' => Some(if self.eat('>') {
                TokenKind::GreaterGreater
            } else if self.eat('=') {
                TokenKind::GreaterEq
            } else {
                TokenKind::Greater
            }),
            '<' => Some(if self.eat('=') {
                TokenKind::LessEq
            } else if self.eat('>') {
                TokenKind::NotEq
            } else {
                TokenKind::Less
            }),
            '&' if self.eat('&') => Some(TokenKind::AmpAmp),
            _ => None,
        };
        match kind {
            Some(kind) => {
                let range = self.range_from(start);
                self.push(kind, range);
            }
            None => {
                let range = self.range_from(start);
                self.error(UNEXPECTED_CHAR, range, format!("unexpected character `{c}`"));
            }
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use fai_span::SourceId;

    use super::{Lexed, lex};
    use crate::token::{Token, TokenKind};

    fn lexed(src: &str) -> Lexed {
        lex(SourceId::new(0), src)
    }

    fn kinds(src: &str) -> Vec<TokenKind> {
        lexed(src).tokens.iter().map(|t| t.kind).collect()
    }

    fn lexeme<'a>(src: &'a str, token: &Token) -> &'a str {
        &src[token.range.start().to_usize()..token.range.end().to_usize()]
    }

    fn render(src: &str) -> String {
        let result = lexed(src);
        let mut out = String::new();
        for token in &result.tokens {
            out.push_str(&format!("{:?} {:?}\n", token.kind, lexeme(src, token)));
        }
        for comment in &result.comments {
            let text = &src[comment.range.start().to_usize()..comment.range.end().to_usize()];
            out.push_str(&format!("comment {:?} {:?}\n", comment.kind, text));
        }
        for diag in &result.diagnostics {
            out.push_str(&format!("diag {} {}\n", diag.code, diag.message));
        }
        out
    }

    #[test]
    fn keywords_idents_and_underscore() {
        assert_eq!(
            kinds("module Hello let x _ _foo"),
            vec![
                TokenKind::Module,
                TokenKind::UpperIdent,
                TokenKind::Let,
                TokenKind::LowerIdent,
                TokenKind::Underscore,
                TokenKind::LowerIdent,
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn true_false_are_lower_idents() {
        assert_eq!(
            kinds("true false"),
            vec![TokenKind::LowerIdent, TokenKind::LowerIdent, TokenKind::Eof,]
        );
    }

    #[test]
    fn char_vs_type_variable_rule() {
        assert_eq!(kinds("'a'"), vec![TokenKind::Char, TokenKind::Eof]);
        assert_eq!(kinds("'F'"), vec![TokenKind::Char, TokenKind::Eof]);
        assert_eq!(kinds("'a"), vec![TokenKind::TypeVar, TokenKind::Eof]);
        assert_eq!(kinds("'acc"), vec![TokenKind::TypeVar, TokenKind::Eof]);
        // Escaped character literal.
        assert_eq!(kinds("'\\n'"), vec![TokenKind::Char, TokenKind::Eof]);
        // In a type signature context.
        assert_eq!(
            kinds("('a -> 'a)"),
            vec![
                TokenKind::LParen,
                TokenKind::TypeVar,
                TokenKind::Arrow,
                TokenKind::TypeVar,
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn type_variable_lexemes() {
        let result = lexed("'ok 'err");
        assert_eq!(lexeme("'ok 'err", &result.tokens[0]), "'ok");
        assert_eq!(lexeme("'ok 'err", &result.tokens[1]), "'err");
    }

    #[test]
    fn numeric_forms() {
        assert_eq!(
            kinds("3 3.0 0xFF 0o17 0b1010 1_000 1.5e10 1e10 0"),
            vec![
                TokenKind::Int,
                TokenKind::Float,
                TokenKind::Int,
                TokenKind::Int,
                TokenKind::Int,
                TokenKind::Int,
                TokenKind::Float,
                TokenKind::Float,
                TokenKind::Int,
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn dot_after_int_is_not_a_float() {
        // `1.foo` is Int Dot LowerIdent, not a float.
        assert_eq!(
            kinds("1.foo"),
            vec![TokenKind::Int, TokenKind::Dot, TokenKind::LowerIdent, TokenKind::Eof,]
        );
    }

    #[test]
    fn multi_char_operators() {
        assert_eq!(
            kinds("-> :: |> >> ++ <> <= >= && || |"),
            vec![
                TokenKind::Arrow,
                TokenKind::ColonColon,
                TokenKind::PipeGreater,
                TokenKind::GreaterGreater,
                TokenKind::PlusPlus,
                TokenKind::NotEq,
                TokenKind::LessEq,
                TokenKind::GreaterEq,
                TokenKind::AmpAmp,
                TokenKind::PipePipe,
                TokenKind::Pipe,
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn single_char_punctuation() {
        assert_eq!(
            kinds("+ - * / % = < > . , : ( ) [ ] { }"),
            vec![
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Equals,
                TokenKind::Less,
                TokenKind::Greater,
                TokenKind::Dot,
                TokenKind::Comma,
                TokenKind::Colon,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn comments_are_trivia() {
        let result = lexed("// line\n/// doc\n(* block *) x");
        let comment_kinds: Vec<_> = result.comments.iter().map(|c| c.kind).collect();
        assert_eq!(
            comment_kinds,
            vec![crate::CommentKind::Line, crate::CommentKind::Doc, crate::CommentKind::Block,]
        );
        assert_eq!(
            kinds("// line\n/// doc\n(* block *) x"),
            vec![TokenKind::LowerIdent, TokenKind::Eof,]
        );
    }

    #[test]
    fn four_slashes_is_a_line_comment() {
        let result = lexed("//// not a doc");
        assert_eq!(result.comments[0].kind, crate::CommentKind::Line);
    }

    #[test]
    fn nested_block_comments() {
        let result = lexed("(* a (* b *) c *) x");
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.diagnostics.len(), 0);
        assert_eq!(kinds("(* a (* b *) c *) x"), vec![TokenKind::LowerIdent, TokenKind::Eof]);
    }

    #[test]
    fn unterminated_string_reports() {
        let result = lexed("\"abc");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::UNTERMINATED_STRING);
        assert_eq!(result.tokens[0].kind, TokenKind::String);
    }

    #[test]
    fn unterminated_block_comment_reports() {
        let result = lexed("(* abc");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::UNTERMINATED_BLOCK_COMMENT);
    }

    #[test]
    fn empty_char_literal_reports() {
        let result = lexed("''");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::INVALID_CHAR_LITERAL);
    }

    #[test]
    fn invalid_escape_reports() {
        let result = lexed("\"a\\qb\"");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::INVALID_ESCAPE);
        assert_eq!(result.tokens[0].kind, TokenKind::String);
    }

    #[test]
    fn invalid_numbers_report() {
        assert_eq!(lexed("0x").diagnostics[0].code, crate::INVALID_NUMBER);
        assert_eq!(lexed("1abc").diagnostics[0].code, crate::INVALID_NUMBER);
        // exactly one diagnostic for a bad base literal
        assert_eq!(lexed("0x").diagnostics.len(), 1);
    }

    #[test]
    fn unexpected_character_reports() {
        let result = lexed("@");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::UNEXPECTED_CHAR);
        // Stray characters are skipped, leaving just EOF.
        assert_eq!(kinds("@"), vec![TokenKind::Eof]);
    }

    #[test]
    fn always_ends_with_eof() {
        assert_eq!(kinds(""), vec![TokenKind::Eof]);
        assert_eq!(*kinds("x").last().unwrap(), TokenKind::Eof);
    }

    #[test]
    fn snapshot_function_definition() {
        insta::assert_snapshot!("function_definition", render("let add x y = x + y"));
    }

    #[test]
    fn snapshot_signature_and_contract() {
        insta::assert_snapshot!(
            "signature_and_contract",
            render("public divMod : Int -> Int -> Int * Int\nexample: divMod 7 3 = (2, 1)"),
        );
    }

    #[test]
    fn snapshot_trivia_and_literals() {
        insta::assert_snapshot!(
            "trivia_and_literals",
            render("/// doc\nlet name = \"Fai\" // trailing\nlet c = 'F'"),
        );
    }
}
