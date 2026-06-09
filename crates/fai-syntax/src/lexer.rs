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
            if c == '\n' || c == '\r' {
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
        // Single-character grouping and punctuation (none are operator chars).
        let punct = match c {
            '(' => Some(TokenKind::LParen),
            ')' => Some(TokenKind::RParen),
            '[' => Some(TokenKind::LBracket),
            ']' => Some(TokenKind::RBracket),
            '{' => Some(TokenKind::LBrace),
            '}' => Some(TokenKind::RBrace),
            ',' => Some(TokenKind::Comma),
            '.' => Some(TokenKind::Dot),
            _ => None,
        };
        if let Some(kind) = punct {
            let range = self.range_from(start);
            self.push(kind, range);
            return;
        }

        if is_operator_char(c) {
            // Maximal munch: an operator is the longest run of operator chars.
            while self.peek().is_some_and(is_operator_char) {
                self.pos += 1; // operator chars are all single-byte ASCII
            }
            let range = self.range_from(start);
            let lexeme = &self.text[start..self.pos];
            // A run that exactly matches a reserved symbol is that token; every
            // other run is a general operator (resolved as a name later).
            let kind = match lexeme {
                "=" => TokenKind::Equals,
                "|" => TokenKind::Pipe,
                ":" => TokenKind::Colon,
                "::" => TokenKind::ColonColon,
                "->" => TokenKind::Arrow,
                _ => TokenKind::Operator,
            };
            self.push(kind, range);
            return;
        }

        let range = self.range_from(start);
        self.error(UNEXPECTED_CHAR, range, format!("unexpected character `{c}`"));
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The operator-character class (F#'s set minus `.` and `#`). A maximal run of
/// these forms a single operator token; `=`, `|`, `:`, `::`, and `->` are carved
/// back out as reserved tokens.
fn is_operator_char(c: char) -> bool {
    matches!(
        c,
        '!' | '$'
            | '%'
            | '&'
            | '*'
            | '+'
            | '-'
            | '/'
            | ':'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '^'
            | '|'
            | '~'
    )
}

#[cfg(test)]
mod tests {
    use fai_span::{ByteOffset, SourceId, TextRange};
    use indoc::indoc;

    use super::{Lexed, lex};
    use crate::token::{Token, TokenKind};

    fn lexed(src: &str) -> Lexed {
        lex(SourceId::new(0), src)
    }

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(ByteOffset::new(start), ByteOffset::new(end))
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

    /// The `(kind, lexeme)` of every non-EOF token.
    fn token_pairs(src: &str) -> Vec<(TokenKind, &str)> {
        lexed(src)
            .tokens
            .iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .map(|t| (t.kind, lexeme(src, t)))
            .collect()
    }

    #[test]
    fn multi_char_operators() {
        // Operator-character runs munch maximally into one `Operator`; `->`, `::`,
        // and `|` carve back out as reserved tokens.
        assert_eq!(
            token_pairs("-> :: |> >> ++ <> <= >= && || |"),
            vec![
                (TokenKind::Arrow, "->"),
                (TokenKind::ColonColon, "::"),
                (TokenKind::Operator, "|>"),
                (TokenKind::Operator, ">>"),
                (TokenKind::Operator, "++"),
                (TokenKind::Operator, "<>"),
                (TokenKind::Operator, "<="),
                (TokenKind::Operator, ">="),
                (TokenKind::Operator, "&&"),
                (TokenKind::Operator, "||"),
                (TokenKind::Pipe, "|"),
            ],
        );
    }

    #[test]
    fn single_char_operators_and_punctuation() {
        assert_eq!(
            token_pairs("+ - * / % = < > . , : ( ) [ ] { }"),
            vec![
                (TokenKind::Operator, "+"),
                (TokenKind::Operator, "-"),
                (TokenKind::Operator, "*"),
                (TokenKind::Operator, "/"),
                (TokenKind::Operator, "%"),
                (TokenKind::Equals, "="),
                (TokenKind::Operator, "<"),
                (TokenKind::Operator, ">"),
                (TokenKind::Dot, "."),
                (TokenKind::Comma, ","),
                (TokenKind::Colon, ":"),
                (TokenKind::LParen, "("),
                (TokenKind::RParen, ")"),
                (TokenKind::LBracket, "["),
                (TokenKind::RBracket, "]"),
                (TokenKind::LBrace, "{"),
                (TokenKind::RBrace, "}"),
            ],
        );
    }

    #[test]
    fn user_operators_munch_maximally() {
        // Novel operator runs are single tokens; their lexeme is preserved.
        assert_eq!(
            token_pairs("<$> >>= +++ <|> !?"),
            vec![
                (TokenKind::Operator, "<$>"),
                (TokenKind::Operator, ">>="),
                (TokenKind::Operator, "+++"),
                (TokenKind::Operator, "<|>"),
                (TokenKind::Operator, "!?"),
            ],
        );
    }

    #[test]
    fn comments_are_trivia() {
        let src = indoc! {r#"
            // line
            /// doc
            (* block *) x"#};
        let result = lexed(src);
        let comment_kinds: Vec<_> = result.comments.iter().map(|c| c.kind).collect();
        assert_eq!(
            comment_kinds,
            vec![crate::CommentKind::Line, crate::CommentKind::Doc, crate::CommentKind::Block,]
        );
        assert_eq!(kinds(src), vec![TokenKind::LowerIdent, TokenKind::Eof,]);
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
        // `#` is not an operator character, so it is genuinely unexpected.
        let result = lexed("#");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::UNEXPECTED_CHAR);
        // Stray characters are skipped, leaving just EOF.
        assert_eq!(kinds("#"), vec![TokenKind::Eof]);
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
            render(indoc! {r#"
                /// doc
                let name = "Fai" // trailing
                let c = 'F'"#}),
        );
    }

    #[test]
    fn token_ranges_are_exact() {
        let result = lexed("let x");
        assert_eq!(result.tokens[0].range, range(0, 3)); // `let`
        assert_eq!(result.tokens[1].range, range(4, 5)); // `x`
        assert_eq!(result.tokens[2].range, range(5, 5)); // EOF (zero-width at end)
    }

    #[test]
    fn tokens_need_no_whitespace() {
        assert_eq!(
            kinds("x+y"),
            vec![TokenKind::LowerIdent, TokenKind::Operator, TokenKind::LowerIdent, TokenKind::Eof,]
        );
        assert_eq!(
            kinds("a->b"),
            vec![TokenKind::LowerIdent, TokenKind::Arrow, TokenKind::LowerIdent, TokenKind::Eof,]
        );
        assert_eq!(
            kinds("(a,b)"),
            vec![
                TokenKind::LParen,
                TokenKind::LowerIdent,
                TokenKind::Comma,
                TokenKind::LowerIdent,
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn identifiers_that_start_with_keywords() {
        // A keyword is only a keyword when it is the whole identifier.
        assert_eq!(
            kinds("lets ifx forallx letter"),
            vec![
                TokenKind::LowerIdent,
                TokenKind::LowerIdent,
                TokenKind::LowerIdent,
                TokenKind::LowerIdent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn multibyte_content_keeps_byte_offsets() {
        // `é` is two UTF-8 bytes; the token after the string must still slice
        // correctly, which only holds if byte offsets are tracked properly.
        let src = "\"café\" x";
        let result = lexed(src);
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.tokens[0].kind, TokenKind::String);
        assert_eq!(lexeme(src, &result.tokens[0]), "\"café\"");
        assert_eq!(result.tokens[1].kind, TokenKind::LowerIdent);
        assert_eq!(lexeme(src, &result.tokens[1]), "x");
    }

    #[test]
    fn diagnostic_points_at_the_offending_span() {
        let result = lexed("x # y");
        assert_eq!(result.diagnostics.len(), 1);
        let diag = &result.diagnostics[0];
        assert_eq!(diag.code, crate::UNEXPECTED_CHAR);
        assert_eq!(diag.primary.source(), SourceId::new(0));
        assert_eq!(diag.primary.start().to_usize(), 2); // the `#`
        assert_eq!(diag.primary.end().to_usize(), 3);
        // Lexing recovers and continues on both sides of the bad character.
        assert_eq!(
            kinds("x # y"),
            vec![TokenKind::LowerIdent, TokenKind::LowerIdent, TokenKind::Eof,]
        );
    }

    #[test]
    fn multiple_errors_are_all_reported() {
        // Neither `#` nor `\` is an operator character.
        let result = lexed("# \\");
        assert_eq!(result.diagnostics.len(), 2);
        assert!(result.diagnostics.iter().all(|d| d.code == crate::UNEXPECTED_CHAR));
    }

    #[test]
    fn valid_escapes_have_no_diagnostics() {
        assert!(lexed("\"a\\nb\"").diagnostics.is_empty());
        assert!(lexed("\"tab\\there\"").diagnostics.is_empty());
        assert!(lexed("\"q\\\"q\"").diagnostics.is_empty());
        assert!(lexed("'\\n'").diagnostics.is_empty());
    }

    #[test]
    fn unicode_escapes() {
        // Well-formed `\u{...}`.
        let ok = lexed("\"\\u{1F600}\"");
        assert!(ok.diagnostics.is_empty());
        assert_eq!(ok.tokens[0].kind, TokenKind::String);
        // Missing braces, empty braces, and non-hex digits are all errors.
        assert_eq!(lexed("\"\\u1234\"").diagnostics[0].code, crate::INVALID_ESCAPE);
        assert_eq!(lexed("\"\\u{}\"").diagnostics[0].code, crate::INVALID_ESCAPE);
        assert_eq!(lexed("\"\\u{zz}\"").diagnostics[0].code, crate::INVALID_ESCAPE);
    }

    #[test]
    fn whitespace_only_is_just_eof() {
        assert_eq!(kinds("  \n\t "), vec![TokenKind::Eof]);
    }

    #[test]
    fn line_comment_excludes_carriage_return() {
        // On CRLF input the trailing `\r` is not part of the comment text.
        let result = lexed("// c\r\nx");
        assert_eq!(result.comments.len(), 1);
        let c = result.comments[0];
        let text = &"// c\r\nx"[c.range.start().to_usize()..c.range.end().to_usize()];
        assert_eq!(text, "// c");
        assert_eq!(result.tokens[0].kind, TokenKind::LowerIdent);
    }

    #[test]
    fn comment_markers_inside_strings_are_content() {
        // `//` and `(*` inside a string must not start a comment.
        let result = lexed("\"a // b\"");
        assert_eq!(result.tokens[0].kind, TokenKind::String);
        assert!(result.comments.is_empty());
        assert!(result.diagnostics.is_empty());

        let block = lexed("\"a (* b *)\"");
        assert_eq!(block.tokens[0].kind, TokenKind::String);
        assert!(block.comments.is_empty());
    }

    #[test]
    fn quotes_inside_comments_are_content() {
        // A `"` inside a comment must not start a string literal.
        let line = lexed("// \"unclosed");
        assert_eq!(line.comments.len(), 1);
        assert_eq!(line.comments[0].kind, crate::CommentKind::Line);
        assert!(line.diagnostics.is_empty());

        let block = lexed("(* \"x\" // *)");
        assert_eq!(block.comments.len(), 1);
        assert_eq!(block.comments[0].kind, crate::CommentKind::Block);
        assert!(block.diagnostics.is_empty());
    }

    #[test]
    fn empty_string_is_valid() {
        let result = lexed("\"\"");
        assert_eq!(result.tokens[0].kind, TokenKind::String);
        assert_eq!(lexeme("\"\"", &result.tokens[0]), "\"\"");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn quote_character_literals() {
        assert_eq!(kinds("'\"'"), vec![TokenKind::Char, TokenKind::Eof]); // '"'
        assert_eq!(kinds("'('"), vec![TokenKind::Char, TokenKind::Eof]); // '('
        assert_eq!(kinds("' '"), vec![TokenKind::Char, TokenKind::Eof]); // a space
    }

    #[test]
    fn char_literal_lexeme_includes_both_quotes() {
        let result = lexed("'a'");
        assert_eq!(result.tokens[0].kind, TokenKind::Char);
        assert_eq!(lexeme("'a'", &result.tokens[0]), "'a'");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn char_literal_span_is_exact() {
        // A leading binding pins the char's byte range away from offset 0.
        let src = "x = 'a'";
        let result = lexed(src);
        let tok = result.tokens.iter().find(|t| t.kind == TokenKind::Char).unwrap();
        assert_eq!(tok.range, range(4, 7));
        assert_eq!(lexeme(src, tok), "'a'");
    }

    #[test]
    fn char_escape_forms_lex_clean() {
        for src in ["'\\t'", "'\\r'", "'\\0'", "'\\\\'", "'\\''", "'\\\"'"] {
            let result = lexed(src);
            assert_eq!(result.tokens[0].kind, TokenKind::Char, "{src} should lex as a Char");
            assert!(result.diagnostics.is_empty(), "{src} should have no diagnostics");
        }
    }

    #[test]
    fn char_unicode_escape_lexes_and_validates() {
        assert!(lexed("'\\u{41}'").diagnostics.is_empty());
        assert_eq!(lexed("'\\u{1F600}'").tokens[0].kind, TokenKind::Char);
        // Empty braces and non-hex digits are rejected, like in strings.
        assert_eq!(lexed("'\\u{}'").diagnostics[0].code, crate::INVALID_ESCAPE);
        assert_eq!(lexed("'\\u{zz}'").diagnostics[0].code, crate::INVALID_ESCAPE);
    }

    #[test]
    fn multibyte_char_literal_lexes_with_exact_span() {
        // A two-byte scalar value: `'é'` spans the quote, two content bytes, quote.
        let two = lexed("'é'");
        assert_eq!(two.tokens[0].kind, TokenKind::Char);
        assert_eq!(two.tokens[0].range, range(0, 4));
        assert!(two.diagnostics.is_empty());
        // A four-byte astral scalar value: `'😀'`.
        let four = lexed("'😀'");
        assert_eq!(four.tokens[0].kind, TokenKind::Char);
        assert_eq!(four.tokens[0].range, range(0, 6));
        assert!(four.diagnostics.is_empty());
    }

    #[test]
    fn adjacent_char_literals_are_two_tokens() {
        let src = "'a''b'";
        let result = lexed(src);
        assert_eq!(
            result.tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
            vec![TokenKind::Char, TokenKind::Char, TokenKind::Eof]
        );
        assert_eq!(result.tokens[0].range, range(0, 3));
        assert_eq!(result.tokens[1].range, range(3, 6));
    }

    #[test]
    fn char_literals_in_a_list_are_not_type_vars() {
        // `['a', 'b']`: each closed tick is a Char; an open tick (`'a`) would be a
        // type variable, but these all close.
        assert_eq!(
            kinds("['a', 'b']"),
            vec![
                TokenKind::LBracket,
                TokenKind::Char,
                TokenKind::Comma,
                TokenKind::Char,
                TokenKind::RBracket,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn non_ascii_outside_strings_is_unexpected() {
        // Identifiers are ASCII in v1; a stray multi-byte char is reported and
        // skipped wholesale, leaving following tokens at valid offsets.
        let result = lexed("é x");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::UNEXPECTED_CHAR);
        assert_eq!(kinds("é x"), vec![TokenKind::LowerIdent, TokenKind::Eof]);
    }

    #[test]
    fn lone_backslash_is_unexpected() {
        let result = lexed("\\");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::UNEXPECTED_CHAR);
    }

    #[test]
    fn operators_munch_maximally() {
        // A maximal run of operator characters is a single operator token.
        assert_eq!(token_pairs("||>"), vec![(TokenKind::Operator, "||>")]);
        assert_eq!(token_pairs(">>="), vec![(TokenKind::Operator, ">>=")]);
    }

    #[test]
    fn double_dot_is_two_dots_not_a_float() {
        assert_eq!(
            kinds("1..2"),
            vec![TokenKind::Int, TokenKind::Dot, TokenKind::Dot, TokenKind::Int, TokenKind::Eof,]
        );
    }

    #[test]
    fn exponent_without_digits_is_invalid() {
        // `1e` has no exponent digits, so the `e` is an invalid numeric suffix.
        let result = lexed("1e");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::INVALID_NUMBER);
        assert_eq!(result.tokens[0].kind, TokenKind::Int);
    }

    #[test]
    fn signed_exponents_are_floats() {
        assert_eq!(kinds("1e+5"), vec![TokenKind::Float, TokenKind::Eof]);
        assert_eq!(kinds("1.5e-3"), vec![TokenKind::Float, TokenKind::Eof]);
        assert_eq!(kinds("2E10"), vec![TokenKind::Float, TokenKind::Eof]);
        assert!(lexed("1e+5").diagnostics.is_empty());
    }

    #[track_caller]
    fn lexes_clean_int(src: &str) {
        let result = lexed(src);
        assert!(result.diagnostics.is_empty(), "unexpected diagnostic for {src}");
        assert_eq!(result.tokens[0].kind, TokenKind::Int, "{src}");
    }

    #[test]
    fn underscores_in_decimal() {
        lexes_clean_int("1_000");
    }

    #[test]
    fn underscores_in_hex() {
        lexes_clean_int("0xFF_FF");
    }

    #[test]
    fn underscores_in_octal() {
        lexes_clean_int("0o1_7");
    }

    #[test]
    fn underscores_in_binary() {
        lexes_clean_int("0b1010_1010");
    }

    /// A base prefix with no valid digits, or a digit outside the base, is a
    /// single invalid-number token.
    #[track_caller]
    fn first_diag_is_invalid_number(src: &str) {
        assert_eq!(lexed(src).diagnostics[0].code, crate::INVALID_NUMBER, "{src}");
    }

    #[test]
    fn bare_hex_prefix_is_invalid() {
        first_diag_is_invalid_number("0x");
    }

    #[test]
    fn bare_octal_prefix_is_invalid() {
        first_diag_is_invalid_number("0o");
    }

    #[test]
    fn bare_binary_prefix_is_invalid() {
        first_diag_is_invalid_number("0b");
    }

    #[test]
    fn binary_digit_out_of_range_is_invalid() {
        first_diag_is_invalid_number("0b2");
    }

    #[test]
    fn octal_digit_out_of_range_is_invalid() {
        first_diag_is_invalid_number("0o9");
    }

    #[test]
    fn float_then_field_access() {
        // `1.0.foo` is a float followed by a field access, not a malformed number.
        assert_eq!(
            kinds("1.0.foo"),
            vec![TokenKind::Float, TokenKind::Dot, TokenKind::LowerIdent, TokenKind::Eof,]
        );
    }

    #[test]
    fn type_var_without_a_name_is_an_error() {
        // A lone tick that neither closes a char nor starts an identifier.
        let result = lexed("' ");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::INVALID_CHAR_LITERAL);
    }
}

#[cfg(test)]
mod proptests {
    use fai_span::SourceId;
    use proptest::prelude::*;

    use super::lex;
    use crate::token::TokenKind;

    /// Operator and punctuation lexemes paired with the single token they form.
    const OPERATORS: &[(&str, TokenKind)] = &[
        ("+", TokenKind::Operator),
        ("-", TokenKind::Operator),
        ("*", TokenKind::Operator),
        ("/", TokenKind::Operator),
        ("%", TokenKind::Operator),
        ("++", TokenKind::Operator),
        ("|>", TokenKind::Operator),
        (">>", TokenKind::Operator),
        ("&&", TokenKind::Operator),
        ("||", TokenKind::Operator),
        ("<>", TokenKind::Operator),
        ("<", TokenKind::Operator),
        ("<=", TokenKind::Operator),
        (">", TokenKind::Operator),
        (">=", TokenKind::Operator),
        ("::", TokenKind::ColonColon),
        ("=", TokenKind::Equals),
        ("->", TokenKind::Arrow),
        ("|", TokenKind::Pipe),
        (":", TokenKind::Colon),
        (".", TokenKind::Dot),
        (",", TokenKind::Comma),
        ("(", TokenKind::LParen),
        (")", TokenKind::RParen),
        ("[", TokenKind::LBracket),
        ("]", TokenKind::RBracket),
        ("{", TokenKind::LBrace),
        ("}", TokenKind::RBrace),
    ];

    /// Every reserved keyword lexeme.
    const KEYWORDS: &[&str] = &[
        "module",
        "let",
        "type",
        "interface",
        "match",
        "with",
        "if",
        "then",
        "else",
        "fun",
        "public",
        "example",
        "forall",
        "as",
    ];

    proptest! {
        /// Arbitrary input never panics or hangs, and always ends with `Eof`.
        #[test]
        fn lexing_is_total(input in any::<String>()) {
            let result = lex(SourceId::new(0), &input);
            prop_assert_eq!(result.tokens.last().map(|t| t.kind), Some(TokenKind::Eof));
        }

        /// Token ranges are ordered, within bounds, and on `char` boundaries —
        /// so slicing the source by any token range is always valid.
        #[test]
        fn token_ranges_are_well_formed(input in any::<String>()) {
            let result = lex(SourceId::new(0), &input);
            let len = input.len();
            let mut prev_end = 0usize;
            for token in &result.tokens {
                let start = token.range.start().to_usize();
                let end = token.range.end().to_usize();
                prop_assert!(start <= end, "start after end");
                prop_assert!(end <= len, "range past end of input");
                prop_assert!(start >= prev_end, "ranges overlap or go backwards");
                prop_assert!(input.get(start..end).is_some(), "range not on a char boundary");
                prev_end = end;
            }
        }

        /// A generated identifier lexes back to exactly itself.
        #[test]
        fn identifiers_round_trip(name in "[a-z][a-zA-Z0-9_]*") {
            prop_assume!(TokenKind::keyword(&name).is_none());
            let result = lex(SourceId::new(0), &name);
            prop_assert_eq!(result.tokens.len(), 2); // identifier + Eof
            prop_assert_eq!(result.tokens[0].kind, TokenKind::LowerIdent);
            let token = result.tokens[0];
            let lexeme = &name[token.range.start().to_usize()..token.range.end().to_usize()];
            prop_assert_eq!(lexeme, name.as_str());
        }

        /// Every diagnostic's primary span is ordered, in bounds, and on a `char`
        /// boundary — diagnostics are an API, so their locations must be sliceable.
        #[test]
        fn diagnostic_spans_are_well_formed(input in any::<String>()) {
            let result = lex(SourceId::new(0), &input);
            for diag in &result.diagnostics {
                let start = diag.primary.start().to_usize();
                let end = diag.primary.end().to_usize();
                prop_assert!(start <= end, "diagnostic start after end");
                prop_assert!(end <= input.len(), "diagnostic span past end of input");
                prop_assert!(input.get(start..end).is_some(), "diagnostic span off a char boundary");
            }
        }

        /// Comment trivia ranges are ordered, in bounds, and on `char` boundaries.
        #[test]
        fn comment_ranges_are_well_formed(input in any::<String>()) {
            let result = lex(SourceId::new(0), &input);
            let mut prev_end = 0usize;
            for comment in &result.comments {
                let start = comment.range.start().to_usize();
                let end = comment.range.end().to_usize();
                prop_assert!(start <= end, "comment start after end");
                prop_assert!(end <= input.len(), "comment range past end of input");
                prop_assert!(start >= prev_end, "comment ranges overlap or go backwards");
                prop_assert!(input.get(start..end).is_some(), "comment range off a char boundary");
                prev_end = end;
            }
        }

        /// A decimal integer lexes to exactly one `Int` token with no diagnostics.
        #[test]
        fn decimal_integers_round_trip(n in any::<u64>()) {
            let src = n.to_string();
            let result = lex(SourceId::new(0), &src);
            prop_assert!(result.diagnostics.is_empty());
            prop_assert_eq!(
                result.tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
                vec![TokenKind::Int, TokenKind::Eof],
            );
        }

        /// A decimal float lexes to exactly one `Float` token with no diagnostics.
        #[test]
        fn decimal_floats_round_trip(whole in any::<u32>(), frac in any::<u32>()) {
            let src = format!("{whole}.{frac}");
            let result = lex(SourceId::new(0), &src);
            prop_assert!(result.diagnostics.is_empty());
            prop_assert_eq!(
                result.tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
                vec![TokenKind::Float, TokenKind::Eof],
            );
        }

        /// A string of escape-free, quote-free, newline-free characters lexes to
        /// one `String` token whose lexeme is the whole literal.
        #[test]
        fn simple_strings_round_trip(body in "[a-zA-Z0-9 !?.,;:()]*") {
            let src = format!("\"{body}\"");
            let result = lex(SourceId::new(0), &src);
            prop_assert!(result.diagnostics.is_empty());
            prop_assert_eq!(result.tokens.len(), 2);
            prop_assert_eq!(result.tokens[0].kind, TokenKind::String);
            let token = result.tokens[0];
            let lexeme = &src[token.range.start().to_usize()..token.range.end().to_usize()];
            prop_assert_eq!(lexeme, src.as_str());
        }

        /// Each operator/punctuation lexeme produces exactly its one token.
        #[test]
        fn operators_lex_to_their_kind(pair in proptest::sample::select(OPERATORS.to_vec())) {
            let (lexeme, kind) = pair;
            let result = lex(SourceId::new(0), lexeme);
            prop_assert!(result.diagnostics.is_empty());
            prop_assert_eq!(
                result.tokens.iter().map(|t| t.kind).collect::<Vec<_>>(),
                vec![kind, TokenKind::Eof],
            );
        }

        /// Each reserved word lexes as its keyword token (and nothing else).
        #[test]
        fn keywords_lex_as_keywords(kw in proptest::sample::select(KEYWORDS.to_vec())) {
            let result = lex(SourceId::new(0), kw);
            prop_assert_eq!(result.tokens.len(), 2);
            prop_assert_eq!(result.tokens[0].kind, TokenKind::keyword(kw).unwrap());
        }
    }
}
