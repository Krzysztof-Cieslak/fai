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
    EffectAnnot, Expr, ExprId, ExprKind, FieldInit, FieldPat, FieldType, Item, ItemId, ItemKind,
    LetStmt, MatchArm, MethodImpl, MethodSig, Module, Pat, PatId, PatKind, RowTail, Type, TypeDef,
    TypeId, TypeKind, Variant, Visibility,
};
use crate::token::{Token, TokenKind};
use crate::{Comment, MODULE_HEADER, SYNTAX_ERROR, Symbol, layout, lex};

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

    /// Consumes a dotted upper-case path `Upper(.Upper)*` and interns it as one
    /// qualified symbol (e.g. `Outer.Inner.Shape`). Used for qualified type and
    /// constructor references; resolution splits on `.` to walk the module path.
    /// The leading `UpperIdent` must already be current.
    fn parse_dotted_upper(&mut self) -> Symbol {
        let head = self.bump();
        let mut name = self.lexeme(head).to_owned();
        while self.at(TokenKind::Dot) && self.peek_at(1) == TokenKind::UpperIdent {
            self.bump(); // `.`
            let seg = self.bump();
            name.push('.');
            name.push_str(self.lexeme(seg));
        }
        Symbol::intern(&name)
    }

    /// The operator symbol an operator-ish token denotes. The reserved `=` and
    /// `::` carry fixed lexemes; a general `Operator` interns its run.
    fn op_symbol(&self, token: Token) -> Symbol {
        match token.kind {
            TokenKind::Equals => Symbol::intern("="),
            TokenKind::ColonColon => Symbol::intern("::"),
            _ => self.symbol(token),
        }
    }

    /// The symbol of the current token if it can be used as an *infix* operator
    /// (`Operator`, the equality `=`, or the list-cons `::`).
    fn infix_op_symbol(&self) -> Option<Symbol> {
        match self.peek() {
            TokenKind::Operator | TokenKind::Equals | TokenKind::ColonColon => {
                Some(self.op_symbol(self.cur()))
            }
            _ => None,
        }
    }

    /// The symbol of the current token if it can be used as a *prefix* operator.
    /// Only a general `Operator` may be prefix (`=`/`::` are infix-only).
    fn prefix_op_symbol(&self) -> Option<Symbol> {
        match self.peek() {
            TokenKind::Operator => Some(self.op_symbol(self.cur())),
            _ => None,
        }
    }

    /// Whether the current token is the operator with lexeme `op`.
    fn at_operator(&self, op: &str) -> bool {
        self.at(TokenKind::Operator) && self.lexeme(self.cur()) == op
    }

    /// Consumes the current token if it is the operator with lexeme `op`.
    fn eat_operator(&mut self, op: &str) -> bool {
        if self.at_operator(op) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Whether the cursor is at a parenthesized operator name `( op )`.
    fn at_op_name(&self) -> bool {
        self.at(TokenKind::LParen)
            && matches!(
                self.peek_at(1),
                TokenKind::Operator | TokenKind::Equals | TokenKind::ColonColon
            )
            && self.peek_at(2) == TokenKind::RParen
    }

    /// Consumes a parenthesized operator name `( op )` and returns its symbol, or
    /// `None` (cursor unchanged) when the cursor is not at one.
    fn parse_op_name(&mut self) -> Option<Symbol> {
        if !self.at_op_name() {
            return None;
        }
        self.bump(); // `(`
        let op = self.op_symbol(self.cur());
        self.bump(); // operator
        self.bump(); // `)`
        Some(op)
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

    fn alloc_item(&mut self, item: Item) -> ItemId {
        let id = ItemId::from_index(self.module.items.len());
        self.module.items.push(item);
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
            let id = self.alloc_item(item);
            self.module.roots.push(id);
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
            TokenKind::LParen if self.at_op_name() => self.parse_signature(Visibility::Private),
            TokenKind::Example => self.parse_example(),
            TokenKind::Forall => self.parse_forall(),
            TokenKind::Type => self.parse_type_decl(Visibility::Private, false),
            TokenKind::Interface => self.parse_interface_decl(Visibility::Private),
            TokenKind::Module => self.parse_nested_module(),
            TokenKind::Opaque => {
                // `opaque` only marks a `public` type; written alone it is an
                // error. Recover by parsing it as `public opaque`, the intent.
                let span = self.cur().range;
                self.error(
                    SYNTAX_ERROR,
                    span,
                    "an `opaque` type must be `public`; write `public opaque type`",
                );
                self.bump(); // `opaque`
                if self.at(TokenKind::Type) {
                    self.parse_type_decl(Visibility::Public, true)
                } else {
                    let span = self.cur().range;
                    self.error(SYNTAX_ERROR, span, "expected `type` after `opaque`");
                    ItemKind::Error
                }
            }
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
            TokenKind::LParen if self.at_op_name() => self.parse_signature(Visibility::Public),
            TokenKind::Type => self.parse_type_decl(Visibility::Public, false),
            TokenKind::Opaque => {
                self.bump(); // `opaque`
                if self.at(TokenKind::Type) {
                    self.parse_type_decl(Visibility::Public, true)
                } else {
                    let span = self.cur().range;
                    self.error(SYNTAX_ERROR, span, "expected `type` after `public opaque`");
                    ItemKind::Error
                }
            }
            TokenKind::Interface => self.parse_interface_decl(Visibility::Public),
            TokenKind::Module => {
                let span = self.cur().range;
                self.error(
                    SYNTAX_ERROR,
                    span,
                    "a nested module cannot be marked `public`; mark its members `public` \
                     to expose them across files",
                );
                // Recover by parsing it as an ordinary (unmarked) nested module.
                self.parse_nested_module()
            }
            _ => {
                let span = self.cur().range;
                self.error(
                    SYNTAX_ERROR,
                    span,
                    "expected `let`, a signature, `type`, `opaque type`, or `interface` after `public`",
                );
                ItemKind::Error
            }
        }
    }

    /// Parses a nested module `module Name = <indented items>`. The body is an
    /// item list (mirroring the top level) bounded by a layout block; its child
    /// items are allocated into the shared arena and referenced by `ItemId`.
    fn parse_nested_module(&mut self) -> ItemKind {
        self.bump(); // `module`
        let name = if self.at(TokenKind::UpperIdent) {
            Some(self.bump_symbol())
        } else {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected an upper-case module name after `module`");
            None
        };
        self.expect(TokenKind::Equals, "`=` in the nested module declaration");
        // The `=` opens a layout block when the body starts on a new line.
        let opened = self.eat(TokenKind::LayoutOpen);
        let mut body = Vec::new();
        if opened {
            loop {
                while self.eat(TokenKind::LayoutSep) {}
                if self.at(TokenKind::LayoutClose) || self.at_eof() {
                    break;
                }
                let before = self.pos;
                let item = self.parse_item();
                let id = self.alloc_item(item);
                body.push(id);
                self.resync(0);
                if self.pos == before {
                    self.bump(); // guarantee forward progress
                }
            }
            self.expect(TokenKind::LayoutClose, "the end of the nested module");
        } else if !self.at_terminator() {
            // A single-line nested module `module M = let x = 1` (no block).
            let item = self.parse_item();
            let id = self.alloc_item(item);
            body.push(id);
        }
        match name {
            Some(name) => ItemKind::Module { name, body },
            None => ItemKind::Error,
        }
    }

    fn parse_signature(&mut self, visibility: Visibility) -> ItemKind {
        // A `LowerIdent` name, or a parenthesized operator name `(+++)`.
        let name = match self.parse_op_name() {
            Some(op) => op,
            None => self.bump_symbol(),
        };
        self.expect(TokenKind::Colon, "`:` in the signature");
        let ty = self.parse_type();
        ItemKind::Signature { visibility, name, ty }
    }

    fn parse_binding(&mut self, visibility: Visibility) -> ItemKind {
        self.bump(); // `let`
        let name = if let Some(op) = self.parse_op_name() {
            Some(op)
        } else if self.at(TokenKind::LowerIdent) {
            Some(self.bump_symbol())
        } else {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected a binding name after `let`");
            None
        };
        let mut params = Vec::new();
        while !self.at(TokenKind::Equals) && !self.at_terminator() {
            let before = self.pos;
            params.push(self.parse_pattern());
            if self.pos == before {
                break; // a non-pattern token (e.g. a stray `:`); avoid spinning
            }
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
            let range = self.cur().range;
            let sym = self.bump_symbol();
            binders.push(self.alloc_pat(PatKind::Var(sym), range));
        }
        if binders.is_empty() {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected at least one binder after `forall`");
        }
        self.expect(TokenKind::Colon, "`:` after the `forall` binders");
        let body = self.parse_expr();
        ItemKind::Forall { binders, body }
    }

    fn parse_type_decl(&mut self, visibility: Visibility, opaque: bool) -> ItemKind {
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
        // A leading `|` marks a discriminated union. Without one, parse a type
        // expression: if a `|` follows it, it is a union written without a
        // leading pipe (`type T = A | B`), whose first variant is that type
        // expression; otherwise it is a transparent alias. (`|` is a layout
        // continuation token, so this also covers the multi-line spellings.)
        let def = if self.at(TokenKind::Pipe) {
            let mut variants = Vec::new();
            self.parse_union_variants(&mut variants);
            TypeDef::Union(variants)
        } else {
            let ty = self.parse_type();
            if self.at(TokenKind::Pipe) {
                let mut variants = Vec::new();
                variants.extend(self.type_to_variant(ty));
                self.parse_union_variants(&mut variants);
                TypeDef::Union(variants)
            } else {
                TypeDef::Alias(ty)
            }
        };
        if opened {
            while self.eat(TokenKind::LayoutSep) {}
            self.expect(TokenKind::LayoutClose, "the end of the type declaration");
        }
        match name {
            Some(name) => ItemKind::Type { visibility, opaque, name, params, def },
            None => ItemKind::Error,
        }
    }

    fn parse_interface_decl(&mut self, visibility: Visibility) -> ItemKind {
        self.bump(); // `interface`
        let name = if self.at(TokenKind::UpperIdent) {
            Some(self.bump_symbol())
        } else {
            let span = self.cur().range;
            self.error(
                SYNTAX_ERROR,
                span,
                "expected an upper-case interface name after `interface`",
            );
            None
        };
        let mut params = Vec::new();
        while self.at(TokenKind::TypeVar) {
            params.push(self.bump_symbol());
        }
        self.expect(TokenKind::Equals, "`=` in the interface declaration");
        // The `=` opens a layout block when the methods start on a new line.
        let opened = self.eat(TokenKind::LayoutOpen);
        while self.eat(TokenKind::LayoutSep) {}
        let mut methods = Vec::new();
        if opened {
            while !matches!(self.peek(), TokenKind::LayoutClose | TokenKind::Eof) {
                let Some(m) = self.parse_method_sig() else { break };
                methods.push(m);
                while self.eat(TokenKind::LayoutSep) {}
            }
            while self.eat(TokenKind::LayoutSep) {}
            self.expect(TokenKind::LayoutClose, "the end of the interface declaration");
        } else if let Some(m) = self.parse_method_sig() {
            methods.push(m);
        }
        match name {
            Some(name) => ItemKind::Interface { visibility, name, params, methods },
            None => ItemKind::Error,
        }
    }

    /// Parses one interface method signature `name : ty` (the name may be a
    /// parenthesized operator), or `None` on a malformed method.
    fn parse_method_sig(&mut self) -> Option<MethodSig> {
        let start = self.start();
        let name = if let Some(op) = self.parse_op_name() {
            op
        } else if self.at(TokenKind::LowerIdent) {
            self.bump_symbol()
        } else {
            self.error(SYNTAX_ERROR, self.cur().range, "expected a method name");
            return None;
        };
        self.expect(TokenKind::Colon, "`:` in the method signature");
        let ty = self.parse_type();
        Some(MethodSig { name, ty, span: self.span_from(start) })
    }

    /// Parses the `| A | B 'a …` variants of a discriminated union, appending to
    /// `variants` (the cursor is at a leading `|`). A union written without a
    /// leading pipe seeds `variants` with its first variant before calling this.
    fn parse_union_variants(&mut self, variants: &mut Vec<Variant>) {
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
    }

    /// Reinterprets a type expression parsed for the leading variant of a
    /// `|`-less union (`type T = A | B`) as a [`Variant`]: the application spine
    /// `Con atom…` becomes the constructor name and its field types. A
    /// qualified or non-constructor head is a recoverable error.
    fn type_to_variant(&mut self, ty: TypeId) -> Option<Variant> {
        enum Head {
            Con(Symbol),
            Qualified,
            NotConstructor,
        }
        let span = self.module.types[ty.index()].span;
        let mut fields = Vec::new();
        let mut cur = ty;
        // Peel the spine without mutating, so the borrow ends before any `error`.
        let head = loop {
            match &self.module.types[cur.index()].kind {
                TypeKind::App { func, arg } => {
                    fields.push(*arg);
                    cur = *func;
                }
                // A redundant paren around the head (`(A) | B`) is unwrapped.
                TypeKind::Paren(inner) => cur = *inner,
                TypeKind::Con(name) if name.as_str().contains('.') => break Head::Qualified,
                TypeKind::Con(name) => break Head::Con(*name),
                _ => break Head::NotConstructor,
            }
        };
        match head {
            Head::Con(name) => {
                fields.reverse();
                Some(Variant { name, fields, span })
            }
            Head::Qualified => {
                self.error(SYNTAX_ERROR, span, "a union constructor name cannot be qualified");
                None
            }
            Head::NotConstructor => {
                self.error(
                    SYNTAX_ERROR,
                    span,
                    "expected a constructor name before `|` in the union",
                );
                None
            }
        }
    }

    // --- expressions (Pratt) ---------------------------------------------

    fn parse_expr(&mut self) -> ExprId {
        self.parse_expr_bp(0)
    }

    fn parse_expr_bp(&mut self, min_bp: u8) -> ExprId {
        let start = self.start();
        let mut lhs = self.parse_unary();
        while let Some(op_sym) = self.infix_op_symbol() {
            let (left_bp, right_bp) = binding_power(op_sym.as_str());
            if left_bp < min_bp {
                break;
            }
            let op_token = self.bump(); // operator
            // The operator is a `Var` node, so it resolves and types like a name.
            let op = self.alloc_expr(ExprKind::Var(op_sym), op_token.range);
            let rhs = self.parse_expr_bp(right_bp);
            lhs = self.alloc_expr(ExprKind::Infix { op, lhs, rhs }, self.span_from(start));
        }
        lhs
    }

    fn parse_unary(&mut self) -> ExprId {
        if let Some(op_sym) = self.prefix_op_symbol() {
            let start = self.start();
            let op_token = self.bump();
            let op = self.alloc_expr(ExprKind::Var(op_sym), op_token.range);
            let operand = self.parse_unary();
            self.alloc_expr(ExprKind::Prefix { op, operand }, self.span_from(start))
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
            // A field may be lower (record field / module value) or upper (a
            // nested-module segment or a qualified constructor); resolution
            // disambiguates the `A.B.c` / `Inner.MyCtor` chains by casing.
            let field = if self.at(TokenKind::LowerIdent) || self.at(TokenKind::UpperIdent) {
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
        // `(op)`: an operator in value position (e.g. `(+)`, `(::)`, `(|>)`).
        if let Some(op_sym) = self.infix_op_symbol()
            && self.peek_at(1) == TokenKind::RParen
        {
            self.bump(); // operator
            self.bump(); // `)`
            return self.alloc_expr(ExprKind::Var(op_sym), self.span_from(start));
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
        // `{ Name with … }` (upper-case head) is an interface instance.
        if self.at(TokenKind::UpperIdent) && self.peek_at(1) == TokenKind::With {
            let name = self.bump_symbol();
            self.bump(); // `with`
            let methods = self.parse_method_impls();
            self.expect(TokenKind::RBrace, "`}` to close the interface instance");
            return self.alloc_expr(ExprKind::Instance { name, methods }, self.span_from(start));
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

    /// Parses the comma-separated `m args = body` methods of an interface instance
    /// (the cursor is just past `with`; stops at `}`).
    fn parse_method_impls(&mut self) -> Vec<MethodImpl> {
        let mut methods = Vec::new();
        loop {
            if self.at(TokenKind::RBrace) || self.at_eof() {
                break;
            }
            let start = self.start();
            let name = if let Some(op) = self.parse_op_name() {
                op
            } else if self.at(TokenKind::LowerIdent) {
                self.bump_symbol()
            } else {
                self.error(SYNTAX_ERROR, self.cur().range, "expected a method name");
                Symbol::intern("")
            };
            let mut params = Vec::new();
            while !matches!(self.peek(), TokenKind::Equals | TokenKind::Comma | TokenKind::RBrace)
                && !self.at_eof()
            {
                params.push(self.parse_pattern());
            }
            self.expect(TokenKind::Equals, "`=` after the method parameters");
            let body = self.parse_expr();
            methods.push(MethodImpl { name, params, body, span: self.span_from(start) });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        methods
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
            let before = self.pos;
            params.push(self.parse_pattern());
            if self.pos == before {
                break; // a non-pattern token (e.g. a stray `:`); avoid spinning
            }
        }
        self.expect(TokenKind::Equals, "`=` in the let binding");
        let value = self.parse_expr();
        LetStmt { pat, params, value, span: self.span_from(start) }
    }

    // --- patterns ---------------------------------------------------------

    fn parse_pattern(&mut self) -> PatId {
        let start = self.start();
        let pat = self.parse_pattern_or();
        // `as` binds looser than everything: `p as name` aliases the whole `p`.
        if self.eat(TokenKind::As) {
            let name = if self.at(TokenKind::LowerIdent) {
                self.bump_symbol()
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a lower-case name after `as`");
                Symbol::intern("")
            };
            self.alloc_pat(PatKind::As { pat, name }, self.span_from(start))
        } else {
            pat
        }
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
            let name = self.parse_dotted_upper();
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
                PatKind::Constructor { name: self.parse_dotted_upper(), args: Vec::new() }
            }
            TokenKind::Int => PatKind::Int(self.bump_symbol()),
            TokenKind::Float => PatKind::Float(self.bump_symbol()),
            TokenKind::String => PatKind::String(self.bump_symbol()),
            TokenKind::Char => PatKind::Char(self.bump_symbol()),
            TokenKind::Operator if self.lexeme(self.cur()) == "-" => {
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
            // An effect annotation binds the innermost arrow: the right-recursive
            // `to` has already consumed any inner `/ …`, so a `/` here is ours.
            let effect = self.parse_effect_annot();
            self.alloc_ty(TypeKind::Arrow { from, to, effect }, self.span_from(start))
        } else {
            from
        }
    }

    /// Parses an optional arrow effect annotation: `/ 'e` (a lone tail, sugar for
    /// `{ | 'e }`) or `/ { Atom, … | tail }`. Returns `None` for a bare arrow.
    fn parse_effect_annot(&mut self) -> Option<EffectAnnot> {
        if !self.at_operator("/") {
            return None;
        }
        let start = self.start();
        self.bump(); // `/`
        // Lone effect variable: `/ 'e`.
        if self.at(TokenKind::TypeVar) {
            let tail = RowTail::Named(self.bump_symbol());
            return Some(EffectAnnot { labels: Vec::new(), tail, span: self.span_from(start) });
        }
        if !self.eat(TokenKind::LBrace) {
            let span = self.cur().range;
            self.error(SYNTAX_ERROR, span, "expected `{` or an effect variable after `/`");
            return Some(EffectAnnot {
                labels: Vec::new(),
                tail: RowTail::Closed,
                span: self.span_from(start),
            });
        }
        // `{ Atom, … | tail }` — atoms are capability interface names (upper).
        let mut labels = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Pipe) && !self.at_eof() {
            if self.at(TokenKind::UpperIdent) {
                labels.push(self.parse_dotted_upper());
            } else {
                let span = self.cur().range;
                self.error(SYNTAX_ERROR, span, "expected a capability name in the effect row");
                break;
            }
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
                self.error(SYNTAX_ERROR, span, "expected `_` or an effect variable after `|`");
                RowTail::Closed
            }
        } else {
            RowTail::Closed
        };
        self.expect(TokenKind::RBrace, "`}` to close the effect row");
        Some(EffectAnnot { labels, tail, span: self.span_from(start) })
    }

    fn parse_type_tuple(&mut self) -> TypeId {
        let start = self.start();
        let first = self.parse_type_app();
        if self.at_operator("*") {
            let mut elems = vec![first];
            while self.eat_operator("*") {
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
            TokenKind::UpperIdent => TypeKind::Con(self.parse_dotted_upper()),
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
                    && !matches!(self.peek(), TokenKind::Arrow | TokenKind::Equals)
                    && !self.at_operator("*")
                {
                    self.bump();
                }
                TypeKind::Error
            }
        };
        self.alloc_ty(kind, self.span_from(start))
    }
}

/// The left/right binding powers for an operator symbol (higher binds tighter),
/// a pure function of the lexeme. The known built-ins keep their canonical
/// levels; any other (user-defined) operator is placed F#-style by its leading
/// character. Left-associative operators use `(2n, 2n+1)`; right-associative ones
/// (`::`, `++`, `^…`) use `(2n+1, 2n)`.
fn binding_power(op: &str) -> (u8, u8) {
    match op {
        "|>" => (2, 3),
        ">>" => (4, 5),
        "||" => (6, 7),
        "&&" => (8, 9),
        "=" | "<>" | "<" | "<=" | ">" | ">=" => (10, 11),
        "::" | "++" => (13, 12),
        "+" | "-" => (14, 15),
        "*" | "/" | "%" => (16, 17),
        _ => leading_char_binding_power(op),
    }
}

/// The F#-style fallback precedence for a user-defined operator, keyed on its
/// leading character.
fn leading_char_binding_power(op: &str) -> (u8, u8) {
    match op.chars().next() {
        Some('|') => (2, 3),                     // pipe-like (left)
        Some('&') => (8, 9),                     // and-like (left)
        Some('=' | '<' | '>' | '!') => (10, 11), // comparison-like (left)
        Some(':' | '@' | '^') => (13, 12),       // cons/append-like (right)
        Some('+' | '-') => (14, 15),             // additive (left)
        Some('*' | '/' | '%') => (16, 17),       // multiplicative (left)
        _ => (10, 11),                           // default: comparison level
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
    use indoc::indoc;

    use super::{Parsed, parse_module};
    use crate::ast::{
        ExprId, ExprKind, Item, ItemKind, LetStmt, Module, PatId, PatKind, TypeId, TypeKind,
    };

    fn parse(src: &str) -> Parsed {
        parse_module(SourceId::new(0), src)
    }

    /// The operator symbol held in an operator `Var` node, for dumps.
    fn dump_op(m: &Module, op: ExprId) -> &str {
        match &m.expr(op).kind {
            ExprKind::Var(s) => s.as_str(),
            _ => "?",
        }
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
            ExprKind::Infix { op, lhs, rhs } => {
                format!("(infix {} {} {})", dump_op(m, *op), dump_expr(m, *lhs), dump_expr(m, *rhs))
            }
            ExprKind::Prefix { op, operand } => {
                format!("(prefix {} {})", dump_op(m, *op), dump_expr(m, *operand))
            }
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
            ExprKind::Instance { name, methods } => {
                let ms = methods
                    .iter()
                    .map(|meth| {
                        format!(
                            "({} [{}] {})",
                            meth.name.as_str(),
                            dump_pats(m, &meth.params),
                            dump_expr(m, meth.body)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("(instance {} [{ms}])", name.as_str())
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
            PatKind::As { pat, name } => format!("(pas {} {})", dump_pat(m, *pat), name.as_str()),
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

    fn dump_effect(e: &crate::ast::EffectAnnot) -> String {
        let mut s = String::from("{");
        for (i, l) in e.labels.iter().enumerate() {
            s.push_str(if i == 0 { " " } else { ", " });
            s.push_str(l.as_str());
        }
        match e.tail {
            crate::ast::RowTail::Closed => {}
            crate::ast::RowTail::Open => s.push_str(" | _"),
            crate::ast::RowTail::Named(r) => {
                s.push_str(" | ");
                s.push_str(r.as_str());
            }
        }
        s.push_str(" }");
        s
    }

    fn dump_type(m: &Module, id: TypeId) -> String {
        match &m.ty(id).kind {
            TypeKind::Var(s) => format!("(tvar {})", s.as_str()),
            TypeKind::Con(s) => format!("(tcon {})", s.as_str()),
            TypeKind::App { func, arg } => {
                format!("(tapp {} {})", dump_type(m, *func), dump_type(m, *arg))
            }
            TypeKind::Arrow { from, to, effect } => {
                let eff = match effect {
                    Some(e) => format!(" / {}", dump_effect(e)),
                    None => String::new(),
                };
                format!("(arrow {} {}{})", dump_type(m, *from), dump_type(m, *to), eff)
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
        for &id in &m.roots {
            out.push_str(&dump_item(m, &m.items[id.index()]));
            out.push('\n');
        }
        out
    }

    fn dump_item(m: &Module, item: &Item) -> String {
        match &item.kind {
            ItemKind::Signature { visibility, name, ty } => {
                format!("(sig {visibility:?} {} {})", name.as_str(), dump_type(m, *ty))
            }
            ItemKind::Binding { visibility, name, params, body } => format!(
                "(let {visibility:?} {} [{}] {})",
                name.as_str(),
                dump_pats(m, params),
                dump_expr(m, *body)
            ),
            ItemKind::Type { visibility, opaque, name, params, def } => {
                let ps = params.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" ");
                let op = if *opaque { " opaque" } else { "" };
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
                format!("(type {visibility:?}{op} {} [{}] {})", name.as_str(), ps, body)
            }
            ItemKind::Interface { visibility, name, params, methods } => {
                let ps = params.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" ");
                let ms = methods
                    .iter()
                    .map(|meth| format!("({} : {})", meth.name.as_str(), dump_type(m, meth.ty)))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("(interface {visibility:?} {} [{}] [{}])", name.as_str(), ps, ms)
            }
            ItemKind::Example { body } => format!("(example {})", dump_expr(m, *body)),
            ItemKind::Forall { binders, body } => {
                format!("(forall [{}] {})", dump_pats(m, binders), dump_expr(m, *body))
            }
            ItemKind::Module { name, body } => {
                let children = body
                    .iter()
                    .map(|&id| dump_item(m, &m.items[id.index()]))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("(module {} [{children}])", name.as_str())
            }
            ItemKind::Error => "(item-error)".to_owned(),
        }
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
        assert_eq!(expr("a + b * c"), "(infix + (var a) (infix * (var b) (var c)))");
        assert_eq!(expr("a * b + c"), "(infix + (infix * (var a) (var b)) (var c))");
    }

    #[test]
    fn left_and_right_associativity() {
        assert_eq!(expr("a - b - c"), "(infix - (infix - (var a) (var b)) (var c))");
        assert_eq!(expr("a :: b :: c"), "(infix :: (var a) (infix :: (var b) (var c)))");
    }

    #[test]
    fn application_is_left_nested_and_tighter_than_operators() {
        assert_eq!(expr("f a b"), "(app (app (var f) (var a)) (var b))");
        assert_eq!(expr("f a + g b"), "(infix + (app (var f) (var a)) (app (var g) (var b)))");
    }

    #[test]
    fn unary_minus_binds_tighter_than_multiply_but_looser_than_application() {
        assert_eq!(expr("-a * b"), "(infix * (prefix - (var a)) (var b))");
        assert_eq!(expr("-f x"), "(prefix - (app (var f) (var x)))");
        assert_eq!(expr("abs (-3)"), "(app (var abs) (paren (prefix - (int 3))))");
    }

    #[test]
    fn pipe_is_loosest_and_left_associative() {
        assert_eq!(expr("a |> f |> g"), "(infix |> (infix |> (var a) (var f)) (var g))");
    }

    #[test]
    fn comparison_tighter_than_boolean_and_equality_is_an_operator() {
        assert_eq!(expr("a < b && c"), "(infix && (infix < (var a) (var b)) (var c))");
        assert_eq!(expr("count % 2 = 0"), "(infix = (infix % (var count) (int 2)) (int 0))");
    }

    #[test]
    fn user_operators_define_use_and_value() {
        // Definition: the operator is the binding's name.
        assert_eq!(
            dump("module M\nlet (+++) a b = a").lines().nth(1).unwrap(),
            "(let Private +++ [(pvar a) (pvar b)] (var a))"
        );
        // A public operator signature.
        assert_eq!(
            dump("module M\npublic (+++) : Int -> Int -> Int").lines().nth(1).unwrap(),
            "(sig Public +++ (arrow (tcon Int) (arrow (tcon Int) (tcon Int))))"
        );
        // Infix use carries the operator symbol; a user prefix operator works too.
        assert_eq!(expr("a +++ b"), "(infix +++ (var a) (var b))");
        assert_eq!(expr("!x"), "(prefix ! (var x))");
        // Operator in value position: user, built-in, cons, and equality.
        assert_eq!(expr("(+++)"), "(var +++)");
        assert_eq!(expr("(+)"), "(var +)");
        assert_eq!(expr("(::)"), "(var ::)");
        assert_eq!(expr("(=)"), "(var =)");
    }

    #[test]
    fn user_operator_precedence_follows_leading_char() {
        // `+++` (leading `+`) is additive; `<+>` (leading `<`) is comparison-level,
        // so `+++` binds tighter.
        assert_eq!(expr("a <+> b +++ c"), "(infix <+> (var a) (infix +++ (var b) (var c)))");
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
    fn char_literals_keep_escape_and_multibyte_lexemes() {
        assert_eq!(expr("'\\n'"), "(char '\\n')");
        assert_eq!(expr("'\\u{1F600}'"), "(char '\\u{1F600}')");
        assert_eq!(expr("'😀'"), "(char '😀')");
        assert_eq!(expr("' '"), "(char ' ')");
    }

    #[test]
    fn char_literal_is_an_application_argument() {
        // A char literal can start an application argument (no parens needed).
        assert_eq!(expr("f 'a' 'b'"), "(app (app (var f) (char 'a')) (char 'b'))");
    }

    #[test]
    fn char_literal_in_a_list() {
        assert_eq!(expr("['a', 'b']"), "(list (char 'a') (char 'b'))");
    }

    #[test]
    fn char_pattern_in_match_arms() {
        let src = indoc! {r#"
            module M
            let f c =
              match c with
              | 'a' -> 1
              | '\n' -> 2
              | _ -> 0"#};
        assert_eq!(
            body(src),
            "(block [] (match (var c) [((pchar 'a') -> (int 1)) ((pchar '\\n') -> (int 2)) \
             ((pwild) -> (int 0))]))"
        );
    }

    #[test]
    fn char_or_pattern() {
        let src = indoc! {r#"
            module M
            let f c =
              match c with
              | 'a' | 'e' | 'i' -> 1
              | _ -> 0"#};
        assert_eq!(
            body(src),
            "(block [] (match (var c) [((por (pchar 'a') (pchar 'e') (pchar 'i')) -> (int 1)) \
             ((pwild) -> (int 0))]))"
        );
    }

    #[test]
    fn local_let_block_with_destructuring() {
        let src = indoc! {r#"
            module M
            let swap p =
              let (x, y) = p
              (y, x)"#};
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
    fn arrow_effect_closed_atoms() {
        let parsed = parse("module M\npublic save : String -> Unit / { Console }");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(sig Public save (arrow (tcon String) (tcon Unit) / { Console }))"
        );
    }

    #[test]
    fn arrow_effect_lone_variable_sugar() {
        // `/ 'e` parses to no atoms with a named tail (sugar for `/ { | 'e }`).
        let parsed = parse("module M\npublic run : Int -> Int / 'e");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(sig Public run (arrow (tcon Int) (tcon Int) / { | 'e }))"
        );
    }

    #[test]
    fn arrow_effect_open_with_atom() {
        let parsed = parse("module M\npublic f : Unit -> Unit / { Console | 'e }");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(sig Public f (arrow (tcon Unit) (tcon Unit) / { Console | 'e }))"
        );
    }

    #[test]
    fn arrow_effect_binds_innermost_arrow() {
        // In a curried type the effect attaches to the last (saturating) arrow.
        let parsed = parse("module M\npublic f : Int -> Int -> Unit / { Console }");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(sig Public f (arrow (tcon Int) (arrow (tcon Int) (tcon Unit) / { Console })))"
        );
    }

    #[test]
    fn arrow_effect_on_parenthesized_inner_arrow() {
        // Parens place the effect on the inner arrow; the outer arrow is pure.
        let parsed = parse("module M\npublic f : (Int -> Int / { Console }) -> Int");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(sig Public f (arrow (tparen (arrow (tcon Int) (tcon Int) / { Console })) (tcon Int)))"
        );
    }

    #[test]
    fn arrow_without_effect_has_none() {
        // A bare arrow carries no effect annotation (renders without ` / …`).
        let parsed = parse("module M\npublic f : Int -> Int");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(sig Public f (arrow (tcon Int) (tcon Int)))"
        );
    }

    #[test]
    fn example_and_forall_items() {
        assert_eq!(
            dump("module M\nexample: f 1 = 2").lines().nth(1).unwrap(),
            "(example (infix = (app (var f) (int 1)) (int 2)))"
        );
        assert_eq!(
            dump("module M\nforall xs ys: f xs = g ys").lines().nth(1).unwrap(),
            "(forall [(pvar xs) (pvar ys)] (infix = (app (var f) (var xs)) (app (var g) (var ys))))"
        );
    }

    #[test]
    fn binding_equals_is_consumed_so_inner_equals_is_equality() {
        // The first `=` binds; the second is the equality operator.
        let parsed = parse("module M\nlet isEven = count % 2 = 0");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(
            body("module M\nlet isEven = count % 2 = 0"),
            "(infix = (infix % (var count) (int 2)) (int 0))"
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
    fn let_binding_with_stray_colon_does_not_hang() {
        // A malformed `let name : …` (a signature written with `let`) must not
        // spin the parameter loop; it reports a syntax error and recovers.
        let parsed = parse("module M\n\nlet h : Int\nlet h x = x");
        assert!(parsed.diagnostics.iter().any(|d| d.code == crate::SYNTAX_ERROR));
    }

    #[test]
    fn as_patterns_bind_loosest() {
        // `as` binds looser than `|`, `::`, and constructor application.
        let out = dump(indoc! {r#"
            module M

            let f x =
              match x with
              | Circle r as whole -> whole
              | a :: rest as all -> all
              | _ -> x
        "#});
        // The whole constructor pattern is aliased.
        assert!(out.contains("(pas (pctor Circle [(pvar r)]) whole)"), "got: {out}");
        // The whole cons pattern is aliased.
        assert!(out.contains("(pas (pcons (pvar a) (pvar rest)) all)"), "got: {out}");
    }

    #[test]
    fn nested_modules_parse_cleanly() {
        let src = indoc! {r#"
            module M

            module Inner =
              let pi = 3
              let square x = x * x

            let area r =
              Inner.pi * Inner.square r"#};
        let parsed = parse(src);
        assert!(
            parsed.diagnostics.is_empty(),
            "nested modules should parse without diagnostics: {:?}",
            parsed.diagnostics,
        );
        // Two top-level roots: the nested module and `area`.
        assert_eq!(parsed.module.roots.len(), 2);
        // The nested module groups its two children.
        let out = dump(src);
        assert!(out.contains("(module Inner [(let Private pi"), "got: {out}");
        assert!(out.contains("square"), "got: {out}");
    }

    #[test]
    fn public_nested_module_is_rejected() {
        let parsed = parse(indoc! {r#"
            module M
            public module Inner =
              let x = 1"#});
        assert!(
            parsed.diagnostics.iter().any(|d| d.code == crate::SYNTAX_ERROR),
            "expected a syntax error for `public module`: {:?}",
            parsed.diagnostics,
        );
    }

    #[test]
    fn interface_declarations_parse() {
        assert_eq!(
            dump(indoc! {r#"
                module M
                interface Console =
                  writeLine : String -> Unit"#})
            .lines()
            .nth(1)
            .unwrap(),
            "(interface Private Console [] [(writeLine : (arrow (tcon String) (tcon Unit)))])"
        );
    }

    #[track_caller]
    fn parses_clean(src: &str) {
        let parsed = parse(src);
        assert!(
            parsed.diagnostics.is_empty(),
            "expected clean parse for {src}: {:?}",
            parsed.diagnostics
        );
    }

    #[test]
    fn parses_discriminated_union() {
        parses_clean(indoc! {r#"
            module M

            type T =
              | A
              | B Int
        "#});
    }

    #[test]
    fn parses_match_expression() {
        parses_clean(indoc! {r#"
            module M

            let f x =
              match x with
              | A -> 1
              | _ -> 2
        "#});
    }

    #[test]
    fn parses_transparent_type_alias() {
        parses_clean(indoc! {r#"
            module M

            type Celsius = Float
        "#});
    }

    #[test]
    fn parses_record_type() {
        parses_clean(indoc! {r#"
            module M

            type Vec2 = { x : Float, y : Float }
        "#});
    }

    #[test]
    fn parses_record_literal() {
        parses_clean(indoc! {r#"
            module M

            let origin = { x = 0, y = 0 }
        "#});
    }

    #[test]
    fn parses_record_update() {
        parses_clean(indoc! {r#"
            module M

            let f r = { r with x = 1 }
        "#});
    }

    #[test]
    fn parses_record_patterns() {
        parses_clean(indoc! {r#"
            module M

            let f v =
              match v with
              | { x = 0 | _ } -> 1
              | { x, y } -> x
        "#});
    }

    // --- data types, match, and records: AST shape -----------------------

    #[test]
    fn record_literal_update_and_field_shape() {
        assert_eq!(expr("{ x = 1, y = 2 }"), "(record [x = (int 1), y = (int 2)])");
        assert_eq!(expr("{ r with x = 1 }"), "(update (var r) [x = (int 1)])");
        assert_eq!(expr("{ r with x = 1, y = 2 }"), "(update (var r) [x = (int 1), y = (int 2)])");
        // Field labels keep their source order in the AST (the formatter and the
        // type renderer are what sort them).
        assert_eq!(expr("{ y = 2, x = 1 }"), "(record [y = (int 2), x = (int 1)])");
    }

    #[test]
    fn match_with_constructor_and_wildcard_shape() {
        assert_eq!(
            expr("match x with | Some n -> n | None -> 0"),
            "(match (var x) [((pctor Some [(pvar n)]) -> (var n)) ((pctor None []) -> (int 0))])"
        );
    }

    #[test]
    fn match_with_list_and_cons_patterns_shape() {
        assert_eq!(
            expr("match xs with | [] -> 0 | x :: rest -> x"),
            "(match (var xs) [((plist ) -> (int 0)) ((pcons (pvar x) (pvar rest)) -> (var x))])"
        );
    }

    #[test]
    fn match_with_literal_and_or_patterns_shape() {
        assert_eq!(
            expr("match n with | 0 | 1 -> 1 | _ -> 2"),
            "(match (var n) [((por (pint 0) (pint 1)) -> (int 1)) ((pwild) -> (int 2))])"
        );
    }

    #[test]
    fn match_with_record_patterns_open_and_closed_shape() {
        assert_eq!(
            expr("match r with | { x = 0 | _ } -> 0 | { x, y } -> x"),
            "(match (var r) [((precord [x = (pint 0)] open=true) -> (int 0)) ((precord [x, y] open=false) -> (var x))])"
        );
    }

    #[test]
    fn match_with_bool_and_string_patterns_shape() {
        assert_eq!(
            expr("match b with | true -> \"y\" | false -> \"n\""),
            "(match (var b) [((pbool true) -> (string \"y\")) ((pbool false) -> (string \"n\"))])"
        );
    }

    #[test]
    fn union_type_declaration_shape() {
        let nullary = dump(indoc! {r#"
            module M
            type Color =
              | Red
              | Green"#});
        assert_eq!(
            nullary.lines().nth(1).unwrap(),
            "(type Private Color [] = (| Red []) (| Green []))"
        );
        let with_fields = dump(indoc! {r#"
            module M
            type Shape =
              | Circle Float
              | Rect Float Float"#});
        assert_eq!(
            with_fields.lines().nth(1).unwrap(),
            "(type Private Shape [] = (| Circle [(tcon Float)]) (| Rect [(tcon Float) (tcon Float)]))"
        );
    }

    #[test]
    fn public_opaque_union_declaration_shape() {
        let parsed = dump(indoc! {r#"
            module M
            public opaque type T =
              | MkT Int"#});
        assert_eq!(
            parsed.lines().nth(1).unwrap(),
            "(type Public opaque T [] = (| MkT [(tcon Int)]))"
        );
        assert!(!parsed.contains("diag"), "clean parse: {parsed}");
    }

    #[test]
    fn public_opaque_alias_declaration_shape() {
        let parsed = dump("module M\npublic opaque type Id = Int");
        assert_eq!(parsed.lines().nth(1).unwrap(), "(type Public opaque Id [] = (tcon Int))");
    }

    #[test]
    fn opaque_without_public_is_an_error_and_recovers_as_public_opaque() {
        let parsed = dump(indoc! {r#"
            module M
            opaque type T =
              | MkT Int"#});
        // Recovered as the intended `public opaque`, so downstream sees it.
        assert!(parsed.contains("(type Public opaque T [] = (| MkT [(tcon Int)]))"), "{parsed}");
        // ...but the misuse is still reported.
        assert!(parsed.contains("diag"), "expected a diagnostic: {parsed}");
    }

    #[test]
    fn parametric_union_declaration_shape() {
        let parsed = dump(indoc! {r#"
            module M
            type Opt 'a =
              | None
              | Some 'a"#});
        assert_eq!(
            parsed.lines().nth(1).unwrap(),
            "(type Private Opt ['a] = (| None []) (| Some [(tvar 'a)]))"
        );
    }

    #[test]
    fn single_line_union_without_leading_pipe_is_a_union() {
        // `type T = A | B` (no leading `|`) lowers to the same shape as the
        // canonical multi-line `| A | B` form.
        assert_eq!(
            dump("module M\ntype T = A | B").lines().nth(1).unwrap(),
            "(type Private T [] = (| A []) (| B []))"
        );
    }

    #[test]
    fn single_line_union_without_leading_pipe_carries_fields() {
        assert_eq!(
            dump("module M\ntype Shape = Circle Float | Rect Float Float").lines().nth(1).unwrap(),
            "(type Private Shape [] = (| Circle [(tcon Float)]) (| Rect [(tcon Float) (tcon Float)]))"
        );
    }

    #[test]
    fn multiline_union_without_leading_pipe_is_a_union() {
        // `|` is a layout continuation token, so the no-leading-pipe form also
        // works across lines.
        let parsed = dump(indoc! {r#"
            module M
            type T =
              A
              | B"#});
        assert_eq!(parsed.lines().nth(1).unwrap(), "(type Private T [] = (| A []) (| B []))");
    }

    #[test]
    fn redundant_paren_around_leading_variant_is_unwrapped() {
        assert_eq!(
            dump("module M\ntype T = (A) | B").lines().nth(1).unwrap(),
            "(type Private T [] = (| A []) (| B []))"
        );
    }

    #[test]
    fn qualified_head_before_pipe_is_an_error_but_recovers() {
        let parsed = parse("module M\ntype T = Mod.A | B");
        let diag = parsed
            .diagnostics
            .iter()
            .find(|d| d.code == crate::SYNTAX_ERROR)
            .expect("expected a syntax error for the qualified constructor name");
        assert_eq!(diag.message, "a union constructor name cannot be qualified");
        // The span points at the offending `Mod.A`.
        assert_eq!(diag.primary.start().to_usize(), 18);
        assert_eq!(diag.primary.end().to_usize(), 23);
        // Recovery still yields a union from the remaining `| B`.
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(type Private T [] = (| B []))"
        );
    }

    #[test]
    fn non_constructor_head_before_pipe_is_an_error_but_recovers() {
        let parsed = parse("module M\ntype T = 'a | B");
        let diag = parsed
            .diagnostics
            .iter()
            .find(|d| d.code == crate::SYNTAX_ERROR)
            .expect("expected a syntax error for the non-constructor head");
        assert_eq!(diag.message, "expected a constructor name before `|` in the union");
        assert_eq!(
            dump_module(&parsed.module).lines().nth(1).unwrap(),
            "(type Private T [] = (| B []))"
        );
    }

    #[test]
    fn alias_and_record_type_declaration_shape() {
        assert_eq!(
            dump("module M\ntype Celsius = Int").lines().nth(1).unwrap(),
            "(type Private Celsius [] = (tcon Int))"
        );
        assert_eq!(
            dump("module M\ntype Vec2 = { x : Float, y : Float }").lines().nth(1).unwrap(),
            "(type Private Vec2 [] = (trecord [x : (tcon Float), y : (tcon Float)]))"
        );
    }

    #[test]
    fn open_and_named_record_types_in_signatures_shape() {
        assert_eq!(
            dump("module M\npublic getX : { x : 'a | _ } -> 'a").lines().nth(1).unwrap(),
            "(sig Public getX (arrow (trecord [x : (tvar 'a)] | _) (tvar 'a)))"
        );
        assert_eq!(
            dump("module M\npublic setX : { x : 'a | 'r } -> { x : 'a | 'r }")
                .lines()
                .nth(1)
                .unwrap(),
            "(sig Public setX (arrow (trecord [x : (tvar 'a)] | 'r) (trecord [x : (tvar 'a)] | 'r)))"
        );
    }

    #[test]
    fn one_bad_item_does_not_hide_the_next() {
        // A garbage item (a stray `)`) between two good ones: the parser reports
        // it and still parses both bindings.
        let parsed = parse(indoc! {r#"
            module M
            let a = 1
            )
            let b = 2"#});
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
        let parsed = parse(indoc! {r#"
            module M
            let f =
              let a = 1"#});
        assert!(parsed.diagnostics.iter().any(|d| d.code == crate::SYNTAX_ERROR));
    }

    #[test]
    fn interface_method_without_a_type_recovers() {
        let parsed = parse(indoc! {r#"
            module M
            interface Foo =
              bar :
            let after = 1"#});
        assert!(!parsed.diagnostics.is_empty(), "expected a diagnostic");
        // Recovery: the following binding still parses.
        assert!(
            parsed.module.items.iter().any(
                |i| matches!(&i.kind, ItemKind::Binding { name, .. } if name.as_str() == "after")
            ),
            "did not recover the following binding"
        );
    }

    #[test]
    fn instance_method_without_a_body_recovers() {
        let parsed = parse(indoc! {r#"
            module M
            let x = { Foo with greet }
            let after = 1"#});
        assert!(!parsed.diagnostics.is_empty(), "expected a diagnostic");
        assert!(
            parsed.module.items.iter().any(
                |i| matches!(&i.kind, ItemKind::Binding { name, .. } if name.as_str() == "after")
            ),
            "did not recover the following binding"
        );
    }

    #[test]
    fn operator_definition_without_a_body_recovers() {
        let parsed = parse(indoc! {r#"
            module M
            let (+-+) a b =
            let after = 1"#});
        assert!(!parsed.diagnostics.is_empty(), "expected a diagnostic");
        assert!(
            parsed.module.items.iter().any(
                |i| matches!(&i.kind, ItemKind::Binding { name, .. } if name.as_str() == "after")
            ),
            "did not recover the following binding"
        );
    }

    // --- snapshots --------------------------------------------------------

    #[test]
    fn snapshot_function_with_pipes() {
        insta::assert_snapshot!(
            "function_with_pipes",
            dump(indoc! {r#"
                    module Funcs
                    public describe : Int -> String
                    let describe n =
                      n
                      |> inc
                      |> Int.toString"#})
        );
    }

    #[test]
    fn snapshot_local_bindings() {
        insta::assert_snapshot!(
            "local_bindings",
            dump(indoc! {r#"
                    module Locals
                    let hypotenuse a b =
                      let a2 = a * a
                      let b2 = b * b
                      sqrt (a2 + b2)"#})
        );
    }

    #[test]
    fn snapshot_if_else_chain() {
        insta::assert_snapshot!(
            "if_else_chain",
            dump(indoc! {r#"
                    module Math
                    let classify n =
                      if n < 0 then "neg"
                      else if n = 0 then "zero"
                      else "pos""#})
        );
    }

    #[test]
    fn snapshot_contract_group() {
        insta::assert_snapshot!(
            "contract_group",
            dump(indoc! {r#"
                    module Math
                    public abs : Int -> Int
                    let abs n =
                      if n < 0 then 0 - n else n
                    example: abs (-3) = 3
                    forall n: abs n >= 0"#})
        );
    }

    #[test]
    fn snapshot_recovery() {
        insta::assert_snapshot!(
            "recovery",
            dump(indoc! {r#"
                module M
                let a = 1
                )
                let b = 2"#})
        );
    }

    #[test]
    fn snapshot_union_match_and_records() {
        insta::assert_snapshot!(
            "union_match_and_records",
            dump(indoc! {r#"
                    module Cards
                    type Suit =
                      | Red
                      | Black
                    type Card = { rank : Int, suit : Suit }
                    public describe : Card -> String
                    let describe c =
                      match c with
                      | { rank = 1 | _ } -> "ace"
                      | { rank, suit } -> Int.toString rank"#})
        );
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
            ExprKind::Infix { op, lhs, rhs } => {
                walk_expr(m, *op);
                walk_expr(m, *lhs);
                walk_expr(m, *rhs);
            }
            ExprKind::Prefix { op, operand } => {
                walk_expr(m, *op);
                walk_expr(m, *operand);
            }
            ExprKind::Paren(operand) => walk_expr(m, *operand),
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
            P::As { pat, .. } => walk_pat(m, *pat),
            _ => {}
        }
    }

    fn walk_type(m: &Module, id: TypeId) {
        match &m.ty(id).kind {
            crate::ast::TypeKind::App { func, arg } => {
                walk_type(m, *func);
                walk_type(m, *arg);
            }
            crate::ast::TypeKind::Arrow { from, to, .. } => {
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
                    ItemKind::Interface { methods, .. } => {
                        for meth in methods {
                            walk_type(&parsed.module, meth.ty);
                        }
                    }
                    ItemKind::Example { body } => walk_expr(&parsed.module, *body),
                    ItemKind::Forall { body, .. } => walk_expr(&parsed.module, *body),
                    // A nested module's children are themselves entries in the
                    // `items` arena, so this same loop walks them.
                    ItemKind::Module { .. } => {}
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

        /// A union declaration of any width plus a `match` covering every
        /// constructor is valid data-layer syntax, so it parses with no
        /// diagnostics and no recovery nodes.
        #[test]
        fn valid_union_and_match_parses_clean(n in 1usize..8) {
            let variants =
                (0..n).map(|i| format!("  | C{i} Int")).collect::<Vec<_>>().join("\n");
            let arms =
                (0..n).map(|i| format!("  | C{i} v -> v + {i}")).collect::<Vec<_>>().join("\n");
            let src =
                format!("module M\ntype T =\n{variants}\nlet eval t =\n  match t with\n{arms}\n");
            let parsed = parse_module(SourceId::new(0), &src);
            prop_assert!(parsed.diagnostics.is_empty(), "diagnostics: {:?}\n{}", parsed.diagnostics, src);
            let m = &parsed.module;
            prop_assert!(m.exprs.iter().all(|e| !matches!(e.kind, ExprKind::Error)));
            prop_assert!(m.pats.iter().all(|p| !matches!(p.kind, crate::ast::PatKind::Error)));
            prop_assert!(m.types.iter().all(|t| !matches!(t.kind, crate::ast::TypeKind::Error)));
            prop_assert!(m.items.iter().all(|i| !matches!(i.kind, ItemKind::Error)));
        }

        /// A record literal of indexed labels (always distinct, never a keyword)
        /// parses cleanly with no recovery nodes.
        #[test]
        fn valid_record_literal_parses_clean(n in 1usize..8) {
            let fields = (0..n).map(|i| format!("l{i} = {i}")).collect::<Vec<_>>().join(", ");
            let src = format!("module M\nlet r = {{ {fields} }}\n");
            let parsed = parse_module(SourceId::new(0), &src);
            prop_assert!(parsed.diagnostics.is_empty(), "diagnostics: {:?}", parsed.diagnostics);
            let m = &parsed.module;
            prop_assert!(m.exprs.iter().all(|e| !matches!(e.kind, ExprKind::Error)));
        }
    }
}
