//! The recursive-descent parser with a Pratt expression sub-parser.
//!
//! [`parse_module`] runs the whole front end (lex → layout → parse) and returns
//! the [`ast::Module`] plus comment trivia and diagnostics. Parsing is **total**:
//! every malformed fragment becomes an `Error` node and one run reports many
//! diagnostics (it synchronizes on the layout `Sep`/`Close` tokens and item
//! keywords). The binding `=` is consumed by the declaration parsers, so `=` in
//! expression position is always equality.

use fai_diagnostics::{Diagnostic, DiagnosticCode};
use fai_span::{ByteOffset, SourceId, Span, TextRange};

use crate::ast::{
    BinOp, Expr, ExprId, ExprKind, FieldInit, FieldPat, FieldType, Item, ItemKind, LetStmt,
    MatchArm, Module, Pat, PatId, PatKind, RowTail, Type, TypeDef, TypeId, TypeKind, UnOp, Variant,
    Visibility,
};
use crate::token::{Token, TokenKind};
use crate::{Comment, MODULE_HEADER, SYNTAX_ERROR, Symbol, UNSUPPORTED, layout, lex};

/// The result of parsing one source file.
#[derive(Debug)]
pub struct Parsed {
    /// The parsed module.
    pub module: Module,
    /// Comment trivia, in source order (attached to the tree in a later stage).
    pub comments: Vec<Comment>,
    /// All diagnostics from lexing, layout, and parsing.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes, applies layout, and parses `text` into a [`Parsed`] module.
#[must_use]
pub fn parse_module(source: SourceId, text: &str) -> Parsed {
    let lexed = lex(source, text);
    let laid = layout(source, text, &lexed.tokens);
    let mut diagnostics = lexed.diagnostics;
    diagnostics.extend(laid.diagnostics);

    let mut parser = Parser {
        source,
        text,
        tokens: &laid.tokens,
        pos: 0,
        last_end: ByteOffset::ZERO,
        module: Module::default(),
        diagnostics,
    };
    parser.parse_top_level();
    Parsed { module: parser.module, comments: lexed.comments, diagnostics: parser.diagnostics }
}

struct Parser<'a> {
    source: SourceId,
    text: &'a str,
    tokens: &'a [Token],
    pos: usize,
    last_end: ByteOffset,
    module: Module,
    diagnostics: Vec<Diagnostic>,
}

impl Parser<'_> {
    // --- cursor -----------------------------------------------------------

    fn peek(&self) -> TokenKind {
        self.tokens[self.pos].kind
    }

    /// The kind of the token `n` positions ahead (clamped to `Eof`).
    fn peek_at(&self, n: usize) -> TokenKind {
        self.tokens.get(self.pos + n).map_or(TokenKind::Eof, |t| t.kind)
    }

    fn cur(&self) -> Token {
        self.tokens[self.pos]
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.peek() == kind
    }

    fn at_eof(&self) -> bool {
        self.at(TokenKind::Eof)
    }

    /// `true` at a token that terminates an item or block item.
    fn at_terminator(&self) -> bool {
        matches!(self.peek(), TokenKind::LayoutSep | TokenKind::LayoutClose | TokenKind::Eof)
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.pos];
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        self.last_end = token.range.end();
        token
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) {
        if !self.eat(kind) {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, format!("expected {what}"));
        }
    }

    fn start(&self) -> ByteOffset {
        self.cur().range.start()
    }

    fn span_from(&self, start: ByteOffset) -> TextRange {
        let end = if self.last_end.raw() >= start.raw() { self.last_end } else { start };
        TextRange::new(start, end)
    }

    fn lexeme(&self, token: Token) -> &str {
        &self.text[token.range.start().to_usize()..token.range.end().to_usize()]
    }

    fn symbol(&self, token: Token) -> Symbol {
        Symbol::intern(self.lexeme(token))
    }

    /// Consumes the current token and interns its lexeme.
    fn bump_symbol(&mut self) -> Symbol {
        let token = self.bump();
        self.symbol(token)
    }

    fn error(&mut self, code: DiagnosticCode, span: TextRange, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(code, message, Span::new(self.source, span)));
    }

    fn alloc_expr(&mut self, kind: ExprKind, span: TextRange) -> ExprId {
        let id = ExprId::from_index(self.module.exprs.len());
        self.module.exprs.push(Expr { kind, span });
        id
    }

    fn alloc_pat(&mut self, kind: PatKind, span: TextRange) -> PatId {
        let id = PatId::from_index(self.module.pats.len());
        self.module.pats.push(Pat { kind, span });
        id
    }

    fn alloc_ty(&mut self, kind: TypeKind, span: TextRange) -> TypeId {
        let id = TypeId::from_index(self.module.types.len());
        self.module.types.push(Type { kind, span });
        id
    }

    // --- top level --------------------------------------------------------

    fn parse_top_level(&mut self) {
        self.parse_header();
        loop {
            while self.eat(TokenKind::LayoutSep) {}
            if self.at_eof() {
                break;
            }
            let before = self.pos;
            let item = self.parse_item();
            self.module.items.push(item);
            self.resync(0);
            if self.pos == before {
                self.bump(); // guarantee forward progress
            }
        }
    }

    fn parse_header(&mut self) {
        let start = self.start();
        if self.eat(TokenKind::Module) {
            if self.at(TokenKind::UpperIdent) {
                let token = self.bump();
                self.module.name = Some(self.symbol(token));
            } else {
                let span = self.cur().range;
                self.error(MODULE_HEADER, span, "expected a module name after `module`");
            }
        } else {
            let span = self.cur().range;
            self.error(
                MODULE_HEADER,
                span,
                "expected a module header (`module Name`) at the start of the file",
            );
        }
        self.module.header = self.span_from(start);
    }

    /// Skips tokens until the next item separator at `target_depth`, balancing
    /// nested layout blocks so a whole malformed construct is discarded.
    fn resync(&mut self, target_depth: i32) {
        let mut depth = 0i32;
        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::LayoutSep if depth == target_depth => break,
                TokenKind::LayoutClose if depth == target_depth => break,
                TokenKind::LayoutOpen => {
                    depth += 1;
                    self.bump();
                }
                TokenKind::LayoutClose => {
                    depth -= 1;
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    // --- items ------------------------------------------------------------

    fn parse_item(&mut self) -> Item {
        let start = self.start();
        let kind = match self.peek() {
            TokenKind::Public => self.parse_public_item(),
            TokenKind::Let => self.parse_binding(Visibility::Private),
            TokenKind::LowerIdent => self.parse_signature(Visibility::Private),
            TokenKind::Example => self.parse_example(),
            TokenKind::Forall => self.parse_forall(),
            TokenKind::Type => self.parse_type_decl(Visibility::Private),
            TokenKind::Interface => self.unsupported("interface declarations"),
            TokenKind::Module => self.unsupported("nested modules"),
            _ => {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a declaration");
                self.bump();
                ItemKind::Error
            }
        };
        Item { kind, span: self.span_from(start) }
    }

    fn parse_public_item(&mut self) -> ItemKind {
        self.bump(); // `public`
        match self.peek() {
            TokenKind::Let => self.parse_binding(Visibility::Public),
            TokenKind::LowerIdent => self.parse_signature(Visibility::Public),
            TokenKind::Type => self.parse_type_decl(Visibility::Public),
            _ => {
                let span = self.cur().range;
                self.error(
                    SYNTAX_ERROR,
                    span,
                    "expected `let`, a signature, or `type` after `public`",
                );
                ItemKind::Error
            }
        }
    }

    fn unsupported(&mut self, what: &str) -> ItemKind {
        let token = self.bump(); // the keyword, so its span anchors the diagnostic
        self.error(UNSUPPORTED, token.range, format!("{what} are not supported yet"));
        ItemKind::Error
    }

    fn parse_signature(&mut self, visibility: Visibility) -> ItemKind {
        let name = self.bump_symbol(); // LowerIdent
        self.expect(TokenKind::Colon, "`:` in the signature");
        let ty = self.parse_type();
        ItemKind::Signature { visibility, name, ty }
    }

    fn parse_binding(&mut self, visibility: Visibility) -> ItemKind {
        self.bump(); // `let`
        let name = if self.at(TokenKind::LowerIdent) {
            Some(self.bump_symbol())
        } else {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected a binding name after `let`");
            None
        };
        let mut params = Vec::new();
        while !self.at(TokenKind::Equals) && !self.at_terminator() {
            params.push(self.parse_pattern());
        }
        self.expect(TokenKind::Equals, "`=` in the binding");
        let body = self.parse_expr();
        match name {
            Some(name) => ItemKind::Binding { visibility, name, params, body },
            None => ItemKind::Error,
        }
    }

    fn parse_example(&mut self) -> ItemKind {
        self.bump(); // `example`
        self.expect(TokenKind::Colon, "`:` after `example`");
        let body = self.parse_expr();
        ItemKind::Example { body }
    }

    fn parse_forall(&mut self) -> ItemKind {
        self.bump(); // `forall`
        let mut binders = Vec::new();
        while self.at(TokenKind::LowerIdent) {
            binders.push(self.bump_symbol());
        }
        if binders.is_empty() {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected at least one binder after `forall`");
        }
        self.expect(TokenKind::Colon, "`:` after the `forall` binders");
        let body = self.parse_expr();
        ItemKind::Forall { binders, body }
    }

    fn parse_type_decl(&mut self, visibility: Visibility) -> ItemKind {
        self.bump(); // `type`
        let name = if self.at(TokenKind::UpperIdent) {
            Some(self.bump_symbol())
        } else {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected an upper-case type name after `type`");
            None
        };
        let mut params = Vec::new();
        while self.at(TokenKind::TypeVar) {
            params.push(self.bump_symbol());
        }
        self.expect(TokenKind::Equals, "`=` in the type declaration");
        // The `=` opens a layout block when the body starts on a new line.
        let opened = self.eat(TokenKind::LayoutOpen);
        while self.eat(TokenKind::LayoutSep) {}
        // A leading `|` marks a discriminated union; anything else is a
        // transparent alias to a type expression.
        let def = if self.at(TokenKind::Pipe) {
            self.parse_union()
        } else {
            TypeDef::Alias(self.parse_type())
        };
        if opened {
            while self.eat(TokenKind::LayoutSep) {}
            self.expect(TokenKind::LayoutClose, "the end of the type declaration");
        }
        match name {
            Some(name) => ItemKind::Type { visibility, name, params, def },
            None => ItemKind::Error,
        }
    }

    /// Parses the `| A | B 'a …` variants of a discriminated union (the cursor is
    /// at the leading `|`).
    fn parse_union(&mut self) -> TypeDef {
        let mut variants = Vec::new();
        while self.eat(TokenKind::Pipe) {
            let start = self.start();
            let Some(name) = (if self.at(TokenKind::UpperIdent) {
                Some(self.bump_symbol())
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected an upper-case constructor name");
                None
            }) else {
                // Skip to the next `|` or the block end so recovery continues.
                while !self.at(TokenKind::Pipe) && !self.at_terminator() && !self.at_eof() {
                    self.bump();
                }
                continue;
            };
            let mut fields = Vec::new();
            while can_start_type_atom(self.peek()) {
                fields.push(self.parse_type_atom());
            }
            variants.push(Variant { name, fields, span: self.span_from(start) });
        }
        TypeDef::Union(variants)
    }

    // --- expressions (Pratt) ---------------------------------------------

    fn parse_expr(&mut self) -> ExprId {
        self.parse_expr_bp(0)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> ExprId {
        let start = self.start();
        let mut lhs = self.parse_unary();
        while let Some(op) = binop(self.peek()) {
            let (left_bp, right_bp) = binding_power(op);
            if left_bp < min_bp {
                break;
            }
            self.bump(); // operator
            let rhs = self.parse_expr_bp(right_bp);
            lhs = self.alloc_expr(ExprKind::Binary { op, lhs, rhs }, self.span_from(start));
        }
        lhs
    }

    fn parse_unary(&mut self) -> ExprId {
        if self.at(TokenKind::Minus) {
            let start = self.start();
            self.bump();
            let operand = self.parse_unary();
            self.alloc_expr(ExprKind::Unary { op: UnOp::Neg, operand }, self.span_from(start))
        } else {
            self.parse_app()
        }
    }

    fn parse_app(&mut self) -> ExprId {
        let start = self.start();
        let mut func = self.parse_postfix();
        while can_start_arg(self.peek()) {
            let arg = self.parse_postfix();
            func = self.alloc_expr(ExprKind::App { func, arg }, self.span_from(start));
        }
        func
    }

    fn parse_postfix(&mut self) -> ExprId {
        let start = self.start();
        let mut base = self.parse_atom();
        while self.at(TokenKind::Dot) {
            self.bump();
            let field = if self.at(TokenKind::LowerIdent) {
                self.bump_symbol()
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a field name after `.`");
                Symbol::intern("")
            };
            base = self.alloc_expr(ExprKind::Field { base, field }, self.span_from(start));
        }
        base
    }

    fn parse_atom(&mut self) -> ExprId {
        let start = self.start();
        let kind = match self.peek() {
            TokenKind::Int => ExprKind::Int(self.bump_symbol()),
            TokenKind::Float => ExprKind::Float(self.bump_symbol()),
            TokenKind::String => ExprKind::String(self.bump_symbol()),
            TokenKind::Char => ExprKind::Char(self.bump_symbol()),
            TokenKind::LowerIdent | TokenKind::UpperIdent => ExprKind::Var(self.bump_symbol()),
            TokenKind::LParen => return self.parse_paren(start),
            TokenKind::LBracket => return self.parse_list(start),
            TokenKind::Fun => return self.parse_lambda(start),
            TokenKind::If => return self.parse_if(start),
            TokenKind::LayoutOpen => return self.parse_block(start),
            TokenKind::Match => return self.parse_match(start),
            TokenKind::LBrace => return self.parse_record_expr(start),
            _ => {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected an expression");
                if !self.at_terminator() {
                    self.bump();
                }
                ExprKind::Error
            }
        };
        self.alloc_expr(kind, self.span_from(start))
    }

    fn parse_paren(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // `(`
        if self.eat(TokenKind::RParen) {
            return self.alloc_expr(ExprKind::Unit, self.span_from(start));
        }
        let first = self.parse_expr();
        if self.at(TokenKind::Comma) {
            let mut elems = vec![first];
            while self.eat(TokenKind::Comma) {
                elems.push(self.parse_expr());
            }
            self.expect(TokenKind::RParen, "`)` to close the tuple");
            self.alloc_expr(ExprKind::Tuple(elems), self.span_from(start))
        } else {
            self.expect(TokenKind::RParen, "`)`");
            self.alloc_expr(ExprKind::Paren(first), self.span_from(start))
        }
    }

    fn parse_list(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // `[`
        let mut elems = Vec::new();
        if !self.at(TokenKind::RBracket) {
            elems.push(self.parse_expr());
            while self.eat(TokenKind::Comma) {
                elems.push(self.parse_expr());
            }
        }
        self.expect(TokenKind::RBracket, "`]` to close the list");
        self.alloc_expr(ExprKind::List(elems), self.span_from(start))
    }

    /// Parses a record literal `{ x = a, … }` or update `{ base with x = a, … }`.
    fn parse_record_expr(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // `{`
        if self.eat(TokenKind::RBrace) {
            return self.alloc_expr(ExprKind::Record(Vec::new()), self.span_from(start));
        }
        // `{ ident = … }` is a literal; anything else is `{ expr with … }`.
        if self.at(TokenKind::LowerIdent) && self.peek_at(1) == TokenKind::Equals {
            let fields = self.parse_field_inits();
            self.expect(TokenKind::RBrace, "`}` to close the record");
            self.alloc_expr(ExprKind::Record(fields), self.span_from(start))
        } else {
            let base = self.parse_expr();
            self.expect(TokenKind::With, "`with` in the record update");
            let fields = self.parse_field_inits();
            self.expect(TokenKind::RBrace, "`}` to close the record update");
            self.alloc_expr(ExprKind::RecordUpdate { base, fields }, self.span_from(start))
        }
    }

    fn parse_field_inits(&mut self) -> Vec<FieldInit> {
        let mut fields = Vec::new();
        loop {
            if self.at(TokenKind::RBrace) || self.at_eof() {
                break;
            }
            let start = self.start();
            let name = if self.at(TokenKind::LowerIdent) {
                self.bump_symbol()
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a field name");
                Symbol::intern("")
            };
            self.expect(TokenKind::Equals, "`=` after the field name");
            let value = self.parse_expr();
            fields.push(FieldInit { name, value, span: self.span_from(start) });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        fields
    }

    fn parse_lambda(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // `fun`
        let mut params = Vec::new();
        while !self.at(TokenKind::Arrow) && !self.at_terminator() {
            params.push(self.parse_pattern());
        }
        if params.is_empty() {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected a parameter after `fun`");
        }
        self.expect(TokenKind::Arrow, "`->` after the lambda parameters");
        let body = self.parse_expr();
        self.alloc_expr(ExprKind::Lambda { params, body }, self.span_from(start))
    }

    fn parse_match(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // `match`
        let scrutinee = self.parse_expr();
        self.expect(TokenKind::With, "`with` after the match scrutinee");
        let mut arms = Vec::new();
        while self.at(TokenKind::Pipe) {
            let arm_start = self.start();
            self.bump(); // `|`
            let pat = self.parse_pattern();
            self.expect(TokenKind::Arrow, "`->` in the match arm");
            if self.at(TokenKind::Match) {
                // A bare nested match here would greedily swallow the outer arms.
                let span = self.cur().range;
                self.error(
                    SYNTAX_ERROR,
                    span,
                    "start a nested match on a new indented line, or parenthesize it",
                );
            }
            let body = self.parse_expr();
            arms.push(MatchArm { pat, body, span: self.span_from(arm_start) });
        }
        if arms.is_empty() {
            let span = self.span_from(start);
            self.error(
                SYNTAX_ERROR,
                span,
                "a match expression needs at least one `| pattern -> body` arm",
            );
        }
        self.alloc_expr(ExprKind::Match { scrutinee, arms }, self.span_from(start))
    }

    fn parse_if(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // `if`
        let cond = self.parse_expr();
        self.expect(TokenKind::Then, "`then`");
        let then_branch = self.parse_expr();
        self.expect(TokenKind::Else, "`else`");
        let else_branch = self.parse_expr();
        self.alloc_expr(ExprKind::If { cond, then_branch, else_branch }, self.span_from(start))
    }

    fn parse_block(&mut self, start: ByteOffset) -> ExprId {
        self.bump(); // LayoutOpen
        let mut stmts = Vec::new();
        let mut tail = None;
        loop {
            while self.eat(TokenKind::LayoutSep) {}
            if self.at(TokenKind::LayoutClose) || self.at_eof() {
                break;
            }
            if self.at(TokenKind::Let) {
                let stmt = self.parse_let_stmt();
                stmts.push(stmt);
            } else {
                tail = Some(self.parse_expr());
                while self.eat(TokenKind::LayoutSep) {}
                if !self.at(TokenKind::LayoutClose) && !self.at_eof() {
                    let span = self.cur().range;
                    self.error(SYNTAX_ERROR, span, "expected the end of the block");
                    self.resync(0);
                }
                break;
            }
        }
        self.expect(TokenKind::LayoutClose, "the end of the block");
        let tail = tail.unwrap_or_else(|| {
            let span = self.span_from(start);
            self.error(SYNTAX_ERROR, span, "a block must end in an expression");
            self.alloc_expr(ExprKind::Error, span)
        });
        self.alloc_expr(ExprKind::Block { stmts, tail }, self.span_from(start))
    }

    fn parse_let_stmt(&mut self) -> LetStmt {
        let start = self.start();
        self.bump(); // `let`
        let pat = self.parse_pattern();
        let mut params = Vec::new();
        while !self.at(TokenKind::Equals) && !self.at_terminator() {
            params.push(self.parse_pattern());
        }
        self.expect(TokenKind::Equals, "`=` in the let binding");
        let value = self.parse_expr();
        LetStmt { pat, params, value, span: self.span_from(start) }
    }

    // --- patterns ---------------------------------------------------------

    fn parse_pattern(&mut self) -> PatId {
        self.parse_pattern_or()
    }

    /// `p | p | …` — or-pattern alternatives (must bind the same variables). The
    /// loop stops at `->`, so an `|` inside a match arm is gathered here while the
    /// next-arm `|` is left for the arm loop.
    fn parse_pattern_or(&mut self) -> PatId {
        let start = self.start();
        let first = self.parse_pattern_cons();
        if self.at(TokenKind::Pipe) {
            let mut alts = vec![first];
            while self.eat(TokenKind::Pipe) {
                alts.push(self.parse_pattern_cons());
            }
            self.alloc_pat(PatKind::Or(alts), self.span_from(start))
        } else {
            first
        }
    }

    /// `head :: tail` — right-associative cons.
    fn parse_pattern_cons(&mut self) -> PatId {
        let start = self.start();
        let head = self.parse_pattern_app();
        if self.eat(TokenKind::ColonColon) {
            let tail = self.parse_pattern_cons();
            self.alloc_pat(PatKind::Cons { head, tail }, self.span_from(start))
        } else {
            head
        }
    }

    /// `Ctor arg…` — constructor applied to atom patterns (juxtaposition), or a
    /// bare atom when the head is not a constructor name.
    fn parse_pattern_app(&mut self) -> PatId {
        if self.at(TokenKind::UpperIdent) {
            let start = self.start();
            let name = self.bump_symbol();
            let mut args = Vec::new();
            while can_start_pattern_atom(self.peek()) {
                args.push(self.parse_pattern_atom());
            }
            self.alloc_pat(PatKind::Constructor { name, args }, self.span_from(start))
        } else {
            self.parse_pattern_atom()
        }
    }

    fn parse_pattern_atom(&mut self) -> PatId {
        let start = self.start();
        let kind = match self.peek() {
            TokenKind::LowerIdent => {
                let sym = self.bump_symbol();
                match sym.as_str() {
                    "true" => PatKind::Bool(true),
                    "false" => PatKind::Bool(false),
                    _ => PatKind::Var(sym),
                }
            }
            TokenKind::Underscore => {
                self.bump();
                PatKind::Wildcard
            }
            TokenKind::UpperIdent => {
                PatKind::Constructor { name: self.bump_symbol(), args: Vec::new() }
            }
            TokenKind::Int => PatKind::Int(self.bump_symbol()),
            TokenKind::Float => PatKind::Float(self.bump_symbol()),
            TokenKind::String => PatKind::String(self.bump_symbol()),
            TokenKind::Char => PatKind::Char(self.bump_symbol()),
            TokenKind::Minus => {
                self.bump(); // `-`
                let token = self.cur();
                match self.peek() {
                    TokenKind::Int => {
                        self.bump();
                        PatKind::Int(Symbol::intern(&format!("-{}", self.lexeme(token))))
                    }
                    TokenKind::Float => {
                        self.bump();
                        PatKind::Float(Symbol::intern(&format!("-{}", self.lexeme(token))))
                    }
                    _ => {
                        let span = self.cur().range;
                        self.error(SYNTAX_ERROR, span, "expected a number after `-` in a pattern");
                        PatKind::Error
                    }
                }
            }
            TokenKind::LParen => return self.parse_pattern_paren(start),
            TokenKind::LBracket => return self.parse_pattern_list(start),
            TokenKind::LBrace => return self.parse_record_pattern(start),
            _ => {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a pattern");
                if !self.at_terminator()
                    && !self.at(TokenKind::Equals)
                    && !self.at(TokenKind::Arrow)
                {
                    self.bump();
                }
                PatKind::Error
            }
        };
        self.alloc_pat(kind, self.span_from(start))
    }

    /// Parses a record pattern `{ x = p, y }` (closed) or `{ x = p | _ }` (open).
    /// Field values bind below the or-level, so a top-level `|` is the open tail.
    fn parse_record_pattern(&mut self, start: ByteOffset) -> PatId {
        self.bump(); // `{`
        let mut fields = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Pipe) && !self.at_eof() {
            let fstart = self.start();
            let name = if self.at(TokenKind::LowerIdent) {
                self.bump_symbol()
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a field name in the record pattern");
                Symbol::intern("")
            };
            let (pat, punned) = if self.eat(TokenKind::Equals) {
                (self.parse_pattern_cons(), false)
            } else {
                // Field punning: `{ x }` binds a variable `x`.
                (self.alloc_pat(PatKind::Var(name), self.span_from(fstart)), true)
            };
            fields.push(FieldPat { name, pat, punned, span: self.span_from(fstart) });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        let mut open = false;
        if self.eat(TokenKind::Pipe) {
            if self.eat(TokenKind::Underscore) {
                open = true;
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected `_` after `|` in the record pattern");
            }
        }
        self.expect(TokenKind::RBrace, "`}` to close the record pattern");
        self.alloc_pat(PatKind::Record { fields, open }, self.span_from(start))
    }

    fn parse_pattern_list(&mut self, start: ByteOffset) -> PatId {
        self.bump(); // `[`
        let mut elems = Vec::new();
        if !self.at(TokenKind::RBracket) {
            elems.push(self.parse_pattern());
            while self.eat(TokenKind::Comma) {
                elems.push(self.parse_pattern());
            }
        }
        self.expect(TokenKind::RBracket, "`]` to close the list pattern");
        self.alloc_pat(PatKind::List(elems), self.span_from(start))
    }

    fn parse_pattern_paren(&mut self, start: ByteOffset) -> PatId {
        self.bump(); // `(`
        if self.eat(TokenKind::RParen) {
            return self.alloc_pat(PatKind::Unit, self.span_from(start));
        }
        let first = self.parse_pattern();
        if self.at(TokenKind::Comma) {
            let mut elems = vec![first];
            while self.eat(TokenKind::Comma) {
                elems.push(self.parse_pattern());
            }
            self.expect(TokenKind::RParen, "`)` to close the tuple pattern");
            self.alloc_pat(PatKind::Tuple(elems), self.span_from(start))
        } else {
            self.expect(TokenKind::RParen, "`)`");
            self.alloc_pat(PatKind::Paren(first), self.span_from(start))
        }
    }

    // --- types ------------------------------------------------------------

    fn parse_type(&mut self) -> TypeId {
        self.parse_type_arrow()
    }

    fn parse_type_arrow(&mut self) -> TypeId {
        let start = self.start();
        let from = self.parse_type_tuple();
        if self.eat(TokenKind::Arrow) {
            let to = self.parse_type_arrow(); // right-associative
            self.alloc_ty(TypeKind::Arrow { from, to }, self.span_from(start))
        } else {
            from
        }
    }

    fn parse_type_tuple(&mut self) -> TypeId {
        let start = self.start();
        let first = self.parse_type_app();
        if self.at(TokenKind::Star) {
            let mut elems = vec![first];
            while self.eat(TokenKind::Star) {
                elems.push(self.parse_type_app());
            }
            self.alloc_ty(TypeKind::Tuple(elems), self.span_from(start))
        } else {
            first
        }
    }

    fn parse_type_app(&mut self) -> TypeId {
        let start = self.start();
        let mut func = self.parse_type_atom();
        while can_start_type_atom(self.peek()) {
            let arg = self.parse_type_atom();
            func = self.alloc_ty(TypeKind::App { func, arg }, self.span_from(start));
        }
        func
    }

    /// Parses a record type `{ x : T, … }` with a closed, `| _`, or `| 'r` tail.
    fn parse_record_type(&mut self, start: ByteOffset) -> TypeId {
        self.bump(); // `{`
        let mut fields = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Pipe) && !self.at_eof() {
            let fstart = self.start();
            let name = if self.at(TokenKind::LowerIdent) {
                self.bump_symbol()
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a field name in the record type");
                Symbol::intern("")
            };
            self.expect(TokenKind::Colon, "`:` after the field name");
            let ty = self.parse_type();
            fields.push(FieldType { name, ty, span: self.span_from(fstart) });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        let tail = if self.eat(TokenKind::Pipe) {
            if self.eat(TokenKind::Underscore) {
                RowTail::Open
            } else if self.at(TokenKind::TypeVar) {
                RowTail::Named(self.bump_symbol())
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected `_` or a row variable after `|`");
                RowTail::Closed
            }
        } else {
            RowTail::Closed
        };
        self.expect(TokenKind::RBrace, "`}` to close the record type");
        self.alloc_ty(TypeKind::Record { fields, tail }, self.span_from(start))
    }

    fn parse_type_atom(&mut self) -> TypeId {
        let start = self.start();
        let kind = match self.peek() {
            TokenKind::TypeVar => TypeKind::Var(self.bump_symbol()),
            TokenKind::UpperIdent => TypeKind::Con(self.bump_symbol()),
            TokenKind::LParen => {
                self.bump();
                if self.eat(TokenKind::RParen) {
                    TypeKind::Unit
                } else {
                    let inner = self.parse_type();
                    self.expect(TokenKind::RParen, "`)`");
                    TypeKind::Paren(inner)
                }
            }
            TokenKind::LBrace => return self.parse_record_type(start),
            _ => {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a type");
                if !self.at_terminator()
                    && !matches!(
                        self.peek(),
                        TokenKind::Arrow | TokenKind::Star | TokenKind::Equals
                    )
                {
                    self.bump();
                }
                TypeKind::Error
            }
        };
        self.alloc_ty(kind, self.span_from(start))
    }
}

/// Maps an operator token to its [`BinOp`], if it is a binary operator.
fn binop(kind: TokenKind) -> Option<BinOp> {
    Some(match kind {
        TokenKind::Plus => BinOp::Add,
        TokenKind::Minus => BinOp::Sub,
        TokenKind::Star => BinOp::Mul,
        TokenKind::Slash => BinOp::Div,
        TokenKind::Percent => BinOp::Rem,
        TokenKind::PlusPlus => BinOp::Concat,
        TokenKind::ColonColon => BinOp::Cons,
        TokenKind::PipeGreater => BinOp::Pipe,
        TokenKind::GreaterGreater => BinOp::Compose,
        TokenKind::AmpAmp => BinOp::And,
        TokenKind::PipePipe => BinOp::Or,
        TokenKind::Equals => BinOp::Eq,
        TokenKind::NotEq => BinOp::Ne,
        TokenKind::Less => BinOp::Lt,
        TokenKind::LessEq => BinOp::Le,
        TokenKind::Greater => BinOp::Gt,
        TokenKind::GreaterEq => BinOp::Ge,
        _ => return None,
    })
}

/// The left/right binding powers for `op` (higher binds tighter). Left-associative
/// operators use `(2n, 2n+1)`; the right-associative `::`/`++` use `(2n+1, 2n)`.
fn binding_power(op: BinOp) -> (u8, u8) {
    match op {
        BinOp::Pipe => (2, 3),
        BinOp::Compose => (4, 5),
        BinOp::Or => (6, 7),
        BinOp::And => (8, 9),
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (10, 11),
        BinOp::Cons | BinOp::Concat => (13, 12),
        BinOp::Add | BinOp::Sub => (14, 15),
        BinOp::Mul | BinOp::Div | BinOp::Rem => (16, 17),
    }
}

/// Whether `kind` can begin a function-application argument (a simple atom).
fn can_start_arg(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Int
            | TokenKind::Float
            | TokenKind::String
            | TokenKind::Char
            | TokenKind::LowerIdent
            | TokenKind::UpperIdent
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
    )
}

/// Whether `kind` can begin a constructor-argument pattern atom.
fn can_start_pattern_atom(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::LowerIdent
            | TokenKind::Underscore
            | TokenKind::UpperIdent
            | TokenKind::Int
            | TokenKind::Float
            | TokenKind::String
            | TokenKind::Char
            | TokenKind::LParen
            | TokenKind::LBracket
            | TokenKind::LBrace
    )
}

/// Whether `kind` can begin a type-application argument atom.
fn can_start_type_atom(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::TypeVar | TokenKind::UpperIdent | TokenKind::LParen)
}

#[cfg(test)]
mod tests {
    use fai_span::SourceId;

    use super::{Parsed, parse_module};
    use crate::ast::{
        ExprId, ExprKind, ItemKind, LetStmt, Module, PatId, PatKind, TypeId, TypeKind,
    };

    fn parse(src: &str) -> Parsed {
        parse_module(SourceId::new(0), src)
    }

    fn dump_expr(m: &Module, id: ExprId) -> String {
        match &m.expr(id).kind {
            ExprKind::Int(s) => format!("(int {})", s.as_str()),
            ExprKind::Float(s) => format!("(float {})", s.as_str()),
            ExprKind::String(s) => format!("(string {})", s.as_str()),
            ExprKind::Char(s) => format!("(char {})", s.as_str()),
            ExprKind::Var(s) => format!("(var {})", s.as_str()),
            ExprKind::Unit => "(unit)".to_owned(),
            ExprKind::App { func, arg } => {
                format!("(app {} {})", dump_expr(m, *func), dump_expr(m, *arg))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                format!("({op:?} {} {})", dump_expr(m, *lhs), dump_expr(m, *rhs))
            }
            ExprKind::Unary { op, operand } => format!("({op:?} {})", dump_expr(m, *operand)),
            ExprKind::If { cond, then_branch, else_branch } => format!(
                "(if {} {} {})",
                dump_expr(m, *cond),
                dump_expr(m, *then_branch),
                dump_expr(m, *else_branch)
            ),
            ExprKind::Lambda { params, body } => {
                format!("(fun [{}] {})", dump_pats(m, params), dump_expr(m, *body))
            }
            ExprKind::Match { scrutinee, arms } => {
                let arms = arms
                    .iter()
                    .map(|a| format!("({} -> {})", dump_pat(m, a.pat), dump_expr(m, a.body)))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("(match {} [{}])", dump_expr(m, *scrutinee), arms)
            }
            ExprKind::Block { stmts, tail } => {
                let stmts = stmts.iter().map(|s| dump_stmt(m, s)).collect::<Vec<_>>().join(" ");
                format!("(block [{}] {})", stmts, dump_expr(m, *tail))
            }
            ExprKind::Field { base, field } => {
                format!("(field {} {})", dump_expr(m, *base), field.as_str())
            }
            ExprKind::Record(fields) => {
                let fs = fields
                    .iter()
                    .map(|f| format!("{} = {}", f.name.as_str(), dump_expr(m, f.value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("(record [{fs}])")
            }
            ExprKind::RecordUpdate { base, fields } => {
                let fs = fields
                    .iter()
                    .map(|f| format!("{} = {}", f.name.as_str(), dump_expr(m, f.value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("(update {} [{fs}])", dump_expr(m, *base))
            }
            ExprKind::Paren(inner) => format!("(paren {})", dump_expr(m, *inner)),
            ExprKind::Tuple(xs) => format!("(tuple {})", dump_exprs(m, xs)),
            ExprKind::List(xs) => format!("(list {})", dump_exprs(m, xs)),
            ExprKind::Error => "(expr-error)".to_owned(),
        }
    }

    fn dump_exprs(m: &Module, ids: &[ExprId]) -> String {
        ids.iter().map(|id| dump_expr(m, *id)).collect::<Vec<_>>().join(" ")
    }

    fn dump_stmt(m: &Module, stmt: &LetStmt) -> String {
        format!(
            "(let {} [{}] {})",
            dump_pat(m, stmt.pat),
            dump_pats(m, &stmt.params),
            dump_expr(m, stmt.value)
        )
    }

    fn dump_pat(m: &Module, id: PatId) -> String {
        match &m.pat(id).kind {
            PatKind::Var(s) => format!("(pvar {})", s.as_str()),
            PatKind::Wildcard => "(pwild)".to_owned(),
            PatKind::Unit => "(punit)".to_owned(),
            PatKind::Tuple(xs) => {
                format!(
                    "(ptuple {})",
                    xs.iter().map(|p| dump_pat(m, *p)).collect::<Vec<_>>().join(" ")
                )
            }
            PatKind::Paren(inner) => format!("(pparen {})", dump_pat(m, *inner)),
            PatKind::Constructor { name, args } => {
                format!("(pctor {} [{}])", name.as_str(), dump_pats(m, args))
            }
            PatKind::Int(s) => format!("(pint {})", s.as_str()),
            PatKind::Float(s) => format!("(pfloat {})", s.as_str()),
            PatKind::String(s) => format!("(pstring {})", s.as_str()),
            PatKind::Char(s) => format!("(pchar {})", s.as_str()),
            PatKind::Bool(b) => format!("(pbool {b})"),
            PatKind::List(xs) => format!("(plist {})", dump_pats(m, xs)),
            PatKind::Cons { head, tail } => {
                format!("(pcons {} {})", dump_pat(m, *head), dump_pat(m, *tail))
            }
            PatKind::Or(alts) => format!("(por {})", dump_pats(m, alts)),
            PatKind::Record { fields, open } => {
                let fs = fields
                    .iter()
                    .map(|f| {
                        if f.punned {
                            f.name.as_str().to_owned()
                        } else {
                            format!("{} = {}", f.name.as_str(), dump_pat(m, f.pat))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("(precord [{fs}] open={open})")
            }
            PatKind::Error => "(pat-error)".to_owned(),
        }
    }

    fn dump_pats(m: &Module, ids: &[PatId]) -> String {
        ids.iter().map(|id| dump_pat(m, *id)).collect::<Vec<_>>().join(" ")
    }

    fn dump_type(m: &Module, id: TypeId) -> String {
        match &m.ty(id).kind {
            TypeKind::Var(s) => format!("(tvar {})", s.as_str()),
            TypeKind::Con(s) => format!("(tcon {})", s.as_str()),
            TypeKind::App { func, arg } => {
                format!("(tapp {} {})", dump_type(m, *func), dump_type(m, *arg))
            }
            TypeKind::Arrow { from, to } => {
                format!("(arrow {} {})", dump_type(m, *from), dump_type(m, *to))
            }
            TypeKind::Tuple(xs) => format!(
                "(ttuple {})",
                xs.iter().map(|t| dump_type(m, *t)).collect::<Vec<_>>().join(" ")
            ),
            TypeKind::Record { fields, tail } => {
                let fs = fields
                    .iter()
                    .map(|f| format!("{} : {}", f.name.as_str(), dump_type(m, f.ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let t = match tail {
                    crate::ast::RowTail::Closed => String::new(),
                    crate::ast::RowTail::Open => " | _".to_owned(),
                    crate::ast::RowTail::Named(r) => format!(" | {}", r.as_str()),
                };
                format!("(trecord [{fs}]{t})")
            }
            TypeKind::Unit => "(tunit)".to_owned(),
            TypeKind::Paren(inner) => format!("(tparen {})", dump_type(m, *inner)),
            TypeKind::Error => "(type-error)".to_owned(),
        }
    }

    fn dump_module(m: &Module) -> String {
        let mut out = format!("module {}\n", m.name.map_or("<none>", |s| s.as_str()));
        for item in &m.items {
            let line = match &item.kind {
                ItemKind::Signature { visibility, name, ty } => {
                    format!("(sig {visibility:?} {} {})", name.as_str(), dump_type(m, *ty))
                }
                ItemKind::Binding { visibility, name, params, body } => format!(
                    "(let {visibility:?} {} [{}] {})",
                    name.as_str(),
                    dump_pats(m, params),
                    dump_expr(m, *body)
                ),
                ItemKind::Type { visibility, name, params, def } => {
                    let ps = params.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" ");
                    let body = match def {
                        crate::ast::TypeDef::Alias(ty) => format!("= {}", dump_type(m, *ty)),
                        crate::ast::TypeDef::Union(variants) => {
                            let vs = variants
                                .iter()
                                .map(|v| {
                                    let fs = v
                                        .fields
                                        .iter()
                                        .map(|f| dump_type(m, *f))
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                    format!("(| {} [{}])", v.name.as_str(), fs)
                                })
                                .collect::<Vec<_>>()
                                .join(" ");
                            format!("= {vs}")
                        }
                    };
                    format!("(type {visibility:?} {} [{}] {})", name.as_str(), ps, body)
                }
                ItemKind::Example { body } => format!("(example {})", dump_expr(m, *body)),
                ItemKind::Forall { binders, body } => {
                    let bs = binders.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(" ");
                    format!("(forall [{}] {})", bs, dump_expr(m, *body))
                }
                ItemKind::Error => "(item-error)".to_owned(),
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    fn dump(src: &str) -> String {
        let parsed = parse(src);
        let mut out = dump_module(&parsed.module);
        for diag in &parsed.diagnostics {
            out.push_str(&format!("diag {} {}\n", diag.code, diag.message));
        }
        out
    }

    /// Returns the S-expression for the first binding's body.
    fn body(src: &str) -> String {
        let parsed = parse(src);
        for item in &parsed.module.items {
            if let ItemKind::Binding { body, .. } = &item.kind {
                return dump_expr(&parsed.module, *body);
            }
        }
        panic!("no binding found in: {src}");
    }

    /// Wraps an expression as the body of a binding for focused expr tests.
    fn expr(src: &str) -> String {
        body(&format!("module M\nlet it = {src}"))
    }

    #[test]
    fn module_header_and_simple_binding() {
        let parsed = parse("module Main\nlet x = 1");
        assert_eq!(parsed.module.name.unwrap().as_str(), "Main");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(dump_module(&parsed.module), "module Main\n(let Private x [] (int 1))\n");
    }

    #[test]
    fn precedence_arithmetic() {
        assert_eq!(expr("a + b * c"), "(Add (var a) (Mul (var b) (var c)))");
        assert_eq!(expr("a * b + c"), "(Add (Mul (var a) (var b)) (var c))");
    }

    #[test]
    fn left_and_right_associativity() {
        assert_eq!(expr("a - b - c"), "(Sub (Sub (var a) (var b)) (var c))");
        assert_eq!(expr("a :: b :: c"), "(Cons (var a) (Cons (var b) (var c)))");
    }

    #[test]
    fn application_is_left_nested_and_tighter_than_operators() {
        assert_eq!(expr("f a b"), "(app (app (var f) (var a)) (var b))");
        assert_eq!(expr("f a + g b"), "(Add (app (var f) (var a)) (app (var g) (var b)))");
    }

    #[test]
    fn unary_minus_binds_tighter_than_multiply_but_looser_than_application() {
        assert_eq!(expr("-a * b"), "(Mul (Neg (var a)) (var b))");
        assert_eq!(expr("-f x"), "(Neg (app (var f) (var x)))");
        assert_eq!(expr("abs (-3)"), "(app (var abs) (paren (Neg (int 3))))");
    }

    #[test]
    fn pipe_is_loosest_and_left_associative() {
        assert_eq!(expr("a |> f |> g"), "(Pipe (Pipe (var a) (var f)) (var g))");
    }

    #[test]
    fn comparison_tighter_than_boolean_and_equality_is_an_operator() {
        assert_eq!(expr("a < b && c"), "(And (Lt (var a) (var b)) (var c))");
        assert_eq!(expr("count % 2 = 0"), "(Eq (Rem (var count) (int 2)) (int 0))");
    }

    #[test]
    fn field_access_chains_tightest() {
        assert_eq!(expr("r.x.y"), "(field (field (var r) x) y)");
        assert_eq!(expr("a.b c"), "(app (field (var a) b) (var c))");
    }

    #[test]
    fn if_then_else_and_else_if_chain() {
        assert_eq!(expr("if c then a else b"), "(if (var c) (var a) (var b))");
        assert_eq!(
            expr("if a then b else if c then d else e"),
            "(if (var a) (var b) (if (var c) (var d) (var e)))"
        );
    }

    #[test]
    fn lambda_tuple_list_unit_paren() {
        assert_eq!(expr("fun x y -> x"), "(fun [(pvar x) (pvar y)] (var x))");
        assert_eq!(expr("(a, b)"), "(tuple (var a) (var b))");
        assert_eq!(expr("[1, 2, 3]"), "(list (int 1) (int 2) (int 3))");
        assert_eq!(expr("[]"), "(list )");
        assert_eq!(expr("()"), "(unit)");
        assert_eq!(expr("(a)"), "(paren (var a))");
    }

    #[test]
    fn literals_keep_their_raw_lexemes() {
        assert_eq!(expr("0xFF"), "(int 0xFF)");
        assert_eq!(expr("1_000"), "(int 1_000)");
        assert_eq!(expr("3.0"), "(float 3.0)");
        assert_eq!(expr("\"hi\""), "(string \"hi\")");
        assert_eq!(expr("'a'"), "(char 'a')");
    }

    #[test]
    fn local_let_block_with_destructuring() {
        let src = "module M\nlet swap p =\n  let (x, y) = p\n  (y, x)";
        assert_eq!(
            body(src),
            "(block [(let (ptuple (pvar x) (pvar y)) [] (var p))] (tuple (var y) (var x)))"
        );
    }

    #[test]
    fn signature_types_arrow_tuple_and_application() {
        let parsed = parse("module M\npublic divMod : Int -> Int -> Int * Int");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module),
            "module M\n(sig Public divMod (arrow (tcon Int) (arrow (tcon Int) (ttuple (tcon Int) (tcon Int)))))\n"
        );
        assert_eq!(
            dump("module M\npublic map : ('a -> 'b) -> List 'a -> List 'b").lines().nth(1).unwrap(),
            "(sig Public map (arrow (tparen (arrow (tvar 'a) (tvar 'b))) (arrow (tapp (tcon List) (tvar 'a)) (tapp (tcon List) (tvar 'b)))))"
        );
    }

    #[test]
    fn example_and_forall_items() {
        assert_eq!(
            dump("module M\nexample: f 1 = 2").lines().nth(1).unwrap(),
            "(example (Eq (app (var f) (int 1)) (int 2)))"
        );
        assert_eq!(
            dump("module M\nforall xs ys: f xs = g ys").lines().nth(1).unwrap(),
            "(forall [xs ys] (Eq (app (var f) (var xs)) (app (var g) (var ys))))"
        );
    }

    #[test]
    fn binding_equals_is_consumed_so_inner_equals_is_equality() {
        // The first `=` binds; the second is the equality operator.
        let parsed = parse("module M\nlet isEven = count % 2 = 0");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            body("module M\nlet isEven = count % 2 = 0"),
            "(Eq (Rem (var count) (int 2)) (int 0))"
        );
    }

    // --- error recovery ---------------------------------------------------

    #[test]
    fn missing_module_header_is_reported_but_items_still_parse() {
        let parsed = parse("let x = 1\nlet y = 2");
        assert!(parsed.diagnostics.iter().any(|d| d.code == crate::MODULE_HEADER));
        // Both bindings still parsed.
        assert_eq!(parsed.module.items.len(), 2);
    }

    #[test]
    fn unsupported_constructs_report_fai1030_and_recover() {
        // `interface` and nested modules remain unimplemented; `type`/`match`/
        // records no longer do.
        for src in ["module M\ninterface I =\n  m : Int", "module M\nmodule Inner =\n  let x = 1"] {
            let parsed = parse(src);
            assert!(
                parsed.diagnostics.iter().any(|d| d.code == crate::UNSUPPORTED),
                "expected FAI1030 for: {src}",
            );
        }
    }

    #[test]
    fn type_match_and_records_now_parse_cleanly() {
        for src in [
            "module M\n\ntype T =\n  | A\n  | B Int\n",
            "module M\n\nlet f x =\n  match x with\n  | A -> 1\n  | _ -> 2\n",
            "module M\n\ntype Celsius = Float\n",
            "module M\n\ntype Vec2 = { x : Float, y : Float }\n",
            "module M\n\nlet origin = { x = 0, y = 0 }\n",
            "module M\n\nlet f r = { r with x = 1 }\n",
            "module M\n\nlet f v =\n  match v with\n  | { x = 0 | _ } -> 1\n  | { x, y } -> x\n",
        ] {
            let parsed = parse(src);
            assert!(
                parsed.diagnostics.is_empty(),
                "expected clean parse for {src}: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn type_and_match_now_parse_cleanly() {
        for src in [
            "module M\n\ntype T =\n  | A\n  | B Int\n",
            "module M\n\nlet f x =\n  match x with\n  | A -> 1\n  | _ -> 2\n",
            "module M\n\ntype Celsius = Float\n",
        ] {
            let parsed = parse(src);
            assert!(
                parsed.diagnostics.is_empty(),
                "expected clean parse for {src}: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn one_bad_item_does_not_hide_the_next() {
        // A garbage item (a stray `)`) between two good ones: the parser reports
        // it and still parses both bindings.
        let parsed = parse("module M\nlet a = 1\n)\nlet b = 2");
        let bindings = parsed
            .module
            .items
            .iter()
            .filter(|i| matches!(i.kind, ItemKind::Binding { .. }))
            .count();
        assert_eq!(bindings, 2);
        assert!(parsed.diagnostics.iter().any(|d| d.code == crate::SYNTAX_ERROR));
    }

    #[test]
    fn unclosed_paren_recovers_with_an_error() {
        let parsed = parse("module M\nlet x = (a + b");
        assert!(parsed.diagnostics.iter().any(|d| d.code == crate::SYNTAX_ERROR));
        // Still produced a binding for `x`.
        assert!(parsed.module.items.iter().any(|i| matches!(i.kind, ItemKind::Binding { .. })));
    }

    #[test]
    fn block_must_end_in_an_expression() {
        let parsed = parse("module M\nlet f =\n  let a = 1");
        assert!(parsed.diagnostics.iter().any(|d| d.code == crate::SYNTAX_ERROR));
    }

    // --- snapshots --------------------------------------------------------

    #[test]
    fn snapshot_function_with_pipes() {
        insta::assert_snapshot!(
            "function_with_pipes",
            dump(
                "module Funcs\npublic describe : Int -> String\nlet describe n =\n  n\n  |> inc\n  |> intToString"
            )
        );
    }

    #[test]
    fn snapshot_local_bindings() {
        insta::assert_snapshot!(
            "local_bindings",
            dump(
                "module Locals\nlet hypotenuse a b =\n  let a2 = a * a\n  let b2 = b * b\n  sqrt (a2 + b2)"
            )
        );
    }

    #[test]
    fn snapshot_if_else_chain() {
        insta::assert_snapshot!(
            "if_else_chain",
            dump(
                "module Math\nlet classify n =\n  if n < 0 then \"neg\"\n  else if n = 0 then \"zero\"\n  else \"pos\""
            )
        );
    }

    #[test]
    fn snapshot_contract_group() {
        insta::assert_snapshot!(
            "contract_group",
            dump(
                "module Math\npublic abs : Int -> Int\nlet abs n =\n  if n < 0 then 0 - n else n\nexample: abs (-3) = 3\nforall n: abs n >= 0"
            )
        );
    }

    #[test]
    fn snapshot_recovery() {
        insta::assert_snapshot!("recovery", dump("module M\nlet a = 1\n)\nlet b = 2"));
    }
}

#[cfg(test)]
mod proptests {
    use fai_span::SourceId;
    use proptest::prelude::*;

    use super::parse_module;
    use crate::ast::{ExprId, ExprKind, ItemKind, Module, PatId, TypeId};
    use crate::token::TokenKind;

    // Walks every node via its id; panics (failing the test) if any id dangles.
    fn walk_expr(m: &Module, id: ExprId) {
        match &m.expr(id).kind {
            ExprKind::App { func, arg } => {
                walk_expr(m, *func);
                walk_expr(m, *arg);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                walk_expr(m, *lhs);
                walk_expr(m, *rhs);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Paren(operand) => walk_expr(m, *operand),
            ExprKind::If { cond, then_branch, else_branch } => {
                walk_expr(m, *cond);
                walk_expr(m, *then_branch);
                walk_expr(m, *else_branch);
            }
            ExprKind::Lambda { params, body } => {
                params.iter().for_each(|p| walk_pat(m, *p));
                walk_expr(m, *body);
            }
            ExprKind::Block { stmts, tail } => {
                for stmt in stmts {
                    walk_pat(m, stmt.pat);
                    stmt.params.iter().for_each(|p| walk_pat(m, *p));
                    walk_expr(m, stmt.value);
                }
                walk_expr(m, *tail);
            }
            ExprKind::Field { base, .. } => walk_expr(m, *base),
            ExprKind::Tuple(xs) | ExprKind::List(xs) => xs.iter().for_each(|x| walk_expr(m, *x)),
            ExprKind::Match { scrutinee, arms } => {
                walk_expr(m, *scrutinee);
                for arm in arms {
                    walk_pat(m, arm.pat);
                    walk_expr(m, arm.body);
                }
            }
            _ => {}
        }
    }

    fn walk_pat(m: &Module, id: PatId) {
        use crate::ast::PatKind as P;
        match &m.pat(id).kind {
            P::Tuple(xs) | P::List(xs) | P::Or(xs) => xs.iter().for_each(|p| walk_pat(m, *p)),
            P::Paren(inner) => walk_pat(m, *inner),
            P::Constructor { args, .. } => args.iter().for_each(|p| walk_pat(m, *p)),
            P::Cons { head, tail } => {
                walk_pat(m, *head);
                walk_pat(m, *tail);
            }
            _ => {}
        }
    }

    fn walk_type(m: &Module, id: TypeId) {
        match &m.ty(id).kind {
            crate::ast::TypeKind::App { func, arg } => {
                walk_type(m, *func);
                walk_type(m, *arg);
            }
            crate::ast::TypeKind::Arrow { from, to } => {
                walk_type(m, *from);
                walk_type(m, *to);
            }
            crate::ast::TypeKind::Tuple(xs) => xs.iter().for_each(|t| walk_type(m, *t)),
            crate::ast::TypeKind::Paren(inner) => walk_type(m, *inner),
            _ => {}
        }
    }

    /// Binary operator lexemes used by the expression generator.
    const OPS: &[&str] = &[
        "+", "-", "*", "/", "%", "++", "::", "|>", ">>", "&&", "||", "=", "<>", "<", "<=", ">",
        ">=",
    ];

    fn arb_ident() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9]*".prop_filter("reserved keyword", |s| TokenKind::keyword(s).is_none())
    }

    /// Generates well-formed, fully self-delimiting expressions: every produced
    /// value is a single atom (a literal/name or a bracketed form), so any
    /// composition of them parses cleanly. This lets us assert that genuinely
    /// valid programs never trigger error recovery.
    fn arb_expr() -> impl Strategy<Value = String> {
        let leaf = prop_oneof![
            arb_ident(),
            any::<u32>().prop_map(|n| n.to_string()),
            Just("()".to_owned()),
            "[a-z ]*".prop_map(|s| format!("\"{s}\"")),
        ];
        leaf.prop_recursive(4, 48, 3, |inner| {
            let op = proptest::sample::select(OPS.to_vec());
            prop_oneof![
                (inner.clone(), inner.clone()).prop_map(|(f, a)| format!("({f} {a})")),
                (inner.clone(), op, inner.clone()).prop_map(|(a, o, b)| format!("({a} {o} {b})")),
                (inner.clone(), inner.clone(), inner.clone())
                    .prop_map(|(c, t, e)| format!("(if {c} then {t} else {e})")),
                inner.clone().prop_map(|e| format!("({e})")),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a}, {b})")),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("[{a}, {b}]")),
                inner.clone().prop_map(|e| format!("(fun x -> {e})")),
                inner.prop_map(|e| format!("(-{e})")),
            ]
        })
    }

    /// A module of one or more plain bindings with generated bodies.
    fn arb_program() -> impl Strategy<Value = String> {
        proptest::collection::vec((arb_ident(), arb_expr()), 1..5).prop_map(|binds| {
            let mut src = "module M".to_owned();
            for (name, body) in binds {
                src.push_str(&format!("\nlet {name} = {body}"));
            }
            src
        })
    }

    proptest! {
        /// Parsing arbitrary input never panics, and the resulting tree has no
        /// dangling node ids.
        #[test]
        fn parsing_is_total(input in any::<String>()) {
            let parsed = parse_module(SourceId::new(0), &input);
            for item in &parsed.module.items {
                match &item.kind {
                    ItemKind::Signature { ty, .. } => walk_type(&parsed.module, *ty),
                    ItemKind::Binding { params, body, .. } => {
                        params.iter().for_each(|p| walk_pat(&parsed.module, *p));
                        walk_expr(&parsed.module, *body);
                    }
                    ItemKind::Type { def, .. } => match def {
                        crate::ast::TypeDef::Alias(ty) => walk_type(&parsed.module, *ty),
                        crate::ast::TypeDef::Union(variants) => {
                            for v in variants {
                                v.fields.iter().for_each(|f| walk_type(&parsed.module, *f));
                            }
                        }
                    },
                    ItemKind::Example { body } => walk_expr(&parsed.module, *body),
                    ItemKind::Forall { body, .. } => walk_expr(&parsed.module, *body),
                    ItemKind::Error => {}
                }
            }
            // Each item consumes at least one token, so item count is bounded.
            prop_assert!(parsed.module.items.len() <= input.len() + 1);
        }

        /// A minimal generated binding parses cleanly to exactly one binding.
        #[test]
        fn simple_binding_parses_clean(name in "[a-z][a-zA-Z0-9_]*") {
            prop_assume!(TokenKind::keyword(&name).is_none());
            let src = format!("module M\nlet {name} = 1");
            let parsed = parse_module(SourceId::new(0), &src);
            prop_assert!(parsed.diagnostics.is_empty());
            prop_assert_eq!(parsed.module.items.len(), 1);
            match &parsed.module.items[0].kind {
                ItemKind::Binding { name: bound, .. } => prop_assert_eq!(bound.as_str(), name.as_str()),
                other => prop_assert!(false, "expected a binding, got {:?}", other),
            }
        }

        /// Every node span (across all arenas and items) is ordered, in bounds,
        /// and on a `char` boundary. Spans are the foundation of every later
        /// phase and every diagnostic, so they must always be sliceable.
        #[test]
        fn node_spans_are_well_formed(input in any::<String>()) {
            let parsed = parse_module(SourceId::new(0), &input);
            let m = &parsed.module;
            let spans = m
                .exprs
                .iter()
                .map(|e| e.span)
                .chain(m.pats.iter().map(|p| p.span))
                .chain(m.types.iter().map(|t| t.span))
                .chain(m.items.iter().map(|i| i.span));
            for span in spans {
                let start = span.start().to_usize();
                let end = span.end().to_usize();
                prop_assert!(start <= end, "span start after end");
                prop_assert!(end <= input.len(), "span past end of input");
                prop_assert!(input.get(start..end).is_some(), "span off a char boundary");
            }
        }

        /// A generated valid program parses with no diagnostics and contains no
        /// recovery (`Error`) nodes in any arena — sound recovery never fires on
        /// well-formed input.
        #[test]
        fn valid_programs_parse_without_errors(src in arb_program()) {
            let parsed = parse_module(SourceId::new(0), &src);
            prop_assert!(
                parsed.diagnostics.is_empty(),
                "unexpected diagnostics for:\n{}\n{:?}",
                src,
                parsed.diagnostics,
            );
            let m = &parsed.module;
            prop_assert!(m.exprs.iter().all(|e| !matches!(e.kind, ExprKind::Error)));
            prop_assert!(m.pats.iter().all(|p| !matches!(p.kind, crate::ast::PatKind::Error)));
            prop_assert!(m.types.iter().all(|t| !matches!(t.kind, crate::ast::TypeKind::Error)));
            prop_assert!(m.items.iter().all(|i| !matches!(i.kind, ItemKind::Error)));
        }
    }
}
