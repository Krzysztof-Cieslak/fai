//! The layout (offside) pass.
//!
//! [`layout`] rewrites a token stream so that indentation becomes explicit block
//! structure — virtual [`TokenKind::LayoutOpen`], [`TokenKind::LayoutSep`], and
//! [`TokenKind::LayoutClose`] tokens — letting the parser stay layout-agnostic.
//!
//! The rule (a restricted offside; see the decision log in `docs/PLAN.md`):
//!
//! * The first token establishes the implicit top-level block's reference
//!   column. Top-level items are separated by `LayoutSep`; the top level is not
//!   wrapped in `LayoutOpen`/`LayoutClose`.
//! * A block **opens** after `=`, `->`, `then`, or `else` when the next token
//!   begins a new line and is indented further than the enclosing block; its
//!   first token's column becomes the block's reference column. A body that is
//!   not indented further is a [`crate::LAYOUT_ERROR`].
//! * On a new line at the reference column, a `LayoutSep` is emitted unless the
//!   first token is a *continuation token* (an infix operator, `else`, `then`, or
//!   `|`). Greater indentation continues the current item; lesser indentation
//!   closes blocks until the column fits.
//! * Inside brackets (`(`/`[`/`{`) the offside rule is suspended, so multi-line
//!   parenthesized expressions, lists, and records are not split.
//! * `Eof` closes every open block.

use fai_diagnostics::Diagnostic;
use fai_span::{ByteOffset, LineIndex, SourceId, Span, TextRange};

use crate::LAYOUT_ERROR;
use crate::token::{Token, TokenKind};

/// The result of the layout pass.
#[derive(Debug, Default)]
pub struct Layout {
    /// The tokens, with virtual layout tokens inserted, ending in `Eof`.
    pub tokens: Vec<Token>,
    /// Diagnostics produced while applying the offside rule.
    pub diagnostics: Vec<Diagnostic>,
}

/// Applies the offside rule to `tokens` (the lexer output for `text`).
#[must_use]
pub fn layout(source: SourceId, text: &str, tokens: &[Token]) -> Layout {
    Layouter {
        source,
        text,
        line_index: LineIndex::new(text),
        out: Vec::with_capacity(tokens.len()),
        diagnostics: Vec::new(),
        contexts: Vec::new(),
        bracket_depth: 0,
        pending_open: false,
        prev_line: 0,
        first: true,
    }
    .run(tokens)
}

struct Layouter<'a> {
    source: SourceId,
    text: &'a str,
    line_index: LineIndex,
    out: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    /// Reference columns of the open blocks; the first is the implicit top level.
    contexts: Vec<u32>,
    bracket_depth: u32,
    /// Set after a block-opening token, to inspect the following token.
    pending_open: bool,
    prev_line: u32,
    first: bool,
}

impl Layouter<'_> {
    fn run(mut self, tokens: &[Token]) -> Layout {
        for &token in tokens {
            let at = token.range.start();
            let (line, col) = self.line_col(at);

            if token.kind == TokenKind::Eof {
                while self.contexts.len() > 1 {
                    self.contexts.pop();
                    self.push_virtual(TokenKind::LayoutClose, at);
                }
                self.out.push(token);
                break;
            }

            if self.first {
                self.contexts.push(col);
                self.first = false;
            } else if self.bracket_depth == 0 {
                if self.pending_open {
                    self.pending_open = false;
                    self.open_or_continue(line, col, token, at);
                } else if line > self.prev_line {
                    self.line_transition(col, token.kind, at);
                }
            }

            match token.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => {
                    self.bracket_depth += 1;
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                }
                _ => {}
            }

            self.pending_open = self.bracket_depth == 0 && is_opener(token.kind);

            self.out.push(token);
            self.prev_line = line;
        }

        Layout { tokens: self.out, diagnostics: self.diagnostics }
    }

    /// Handles the token following a block opener (`=`/`->`/`then`/`else`).
    fn open_or_continue(&mut self, line: u32, col: u32, token: Token, at: ByteOffset) {
        if line <= self.prev_line {
            // Inline body on the same line as the opener: no block.
            return;
        }
        let enclosing = *self.contexts.last().expect("top-level context is always present");
        if col > enclosing {
            self.push_virtual(TokenKind::LayoutOpen, at);
            self.contexts.push(col);
        } else {
            self.error(token.range, "expected the block body to be indented further");
            self.line_transition(col, token.kind, at);
        }
    }

    /// Handles a token that begins a new line (outside any opener / brackets).
    fn line_transition(&mut self, col: u32, kind: TokenKind, at: ByteOffset) {
        while self.contexts.len() > 1 && *self.contexts.last().unwrap() > col {
            self.contexts.pop();
            self.push_virtual(TokenKind::LayoutClose, at);
        }
        let reference = *self.contexts.last().unwrap();
        if col < reference || (col == reference && !is_continuation(kind)) {
            // `col < reference` can only mean a dedent past the top level; treat
            // it leniently as a new top-level item (the formatter re-indents).
            self.push_virtual(TokenKind::LayoutSep, at);
        }
        // `col > reference` continues the current item; a continuation token at
        // the reference column also continues it.
    }

    fn line_col(&self, at: ByteOffset) -> (u32, u32) {
        let line_col = self.line_index.line_col(self.text, at);
        (line_col.line, line_col.column)
    }

    fn push_virtual(&mut self, kind: TokenKind, at: ByteOffset) {
        self.out.push(Token::new(kind, TextRange::empty(at)));
    }

    fn error(&mut self, range: TextRange, message: impl Into<String>) {
        let span = Span::new(self.source, range);
        self.diagnostics.push(Diagnostic::error(LAYOUT_ERROR, message, span));
    }
}

/// Tokens that open a layout block when followed by an indented new line.
fn is_opener(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Equals | TokenKind::Arrow | TokenKind::Then | TokenKind::Else)
}

/// Tokens that, at the reference column, continue the current item instead of
/// starting a new one.
fn is_continuation(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::PlusPlus
            | TokenKind::ColonColon
            | TokenKind::PipeGreater
            | TokenKind::GreaterGreater
            | TokenKind::AmpAmp
            | TokenKind::PipePipe
            | TokenKind::Less
            | TokenKind::LessEq
            | TokenKind::Greater
            | TokenKind::GreaterEq
            | TokenKind::NotEq
            | TokenKind::Equals
            | TokenKind::Arrow
            | TokenKind::Then
            | TokenKind::Else
            | TokenKind::Pipe
    )
}

#[cfg(test)]
mod tests {
    use fai_span::SourceId;

    use super::{Layout, layout};
    use crate::lex;
    use crate::token::TokenKind;

    fn run(src: &str) -> Layout {
        let lexed = lex(SourceId::new(0), src);
        layout(SourceId::new(0), src, &lexed.tokens)
    }

    fn count(src: &str, kind: TokenKind) -> usize {
        run(src).tokens.iter().filter(|t| t.kind == kind).count()
    }

    /// Renders the layout stream as a nested, `{ ; }`-style tree for snapshots.
    fn render(src: &str) -> String {
        fn line(depth: usize, s: &str, out: &mut String) {
            for _ in 0..depth {
                out.push_str("  ");
            }
            out.push_str(s);
            out.push('\n');
        }
        let result = run(src);
        let mut out = String::new();
        let mut depth = 0usize;
        for token in &result.tokens {
            match token.kind {
                TokenKind::LayoutOpen => {
                    line(depth, "{", &mut out);
                    depth += 1;
                }
                TokenKind::LayoutClose => {
                    depth = depth.saturating_sub(1);
                    line(depth, "}", &mut out);
                }
                TokenKind::LayoutSep => line(depth, ";", &mut out),
                TokenKind::Eof => line(depth, "<eof>", &mut out),
                _ => {
                    let text = &src[token.range.start().to_usize()..token.range.end().to_usize()];
                    line(depth, text, &mut out);
                }
            }
        }
        if !result.diagnostics.is_empty() {
            out.push_str("--\n");
            for diag in &result.diagnostics {
                out.push_str(&format!("diag {} {}\n", diag.code, diag.message));
            }
        }
        out
    }

    #[test]
    fn top_level_items_are_separated_by_sep() {
        // `module M`, then two bindings: two separators, no blocks.
        let src = "module M\nlet a = 1\nlet b = 2";
        assert_eq!(count(src, TokenKind::LayoutSep), 2);
        assert_eq!(count(src, TokenKind::LayoutOpen), 0);
        assert_eq!(count(src, TokenKind::LayoutClose), 0);
    }

    #[test]
    fn inline_body_opens_no_block() {
        // `=` followed by a same-line body: no LayoutOpen.
        let src = "module M\nlet add x y = x + y";
        assert_eq!(count(src, TokenKind::LayoutOpen), 0);
        assert_eq!(count(src, TokenKind::LayoutClose), 0);
        assert_eq!(count(src, TokenKind::LayoutSep), 1); // between header and binding
    }

    #[test]
    fn indented_body_opens_one_block() {
        let src = "module M\nlet f x =\n  body";
        assert_eq!(count(src, TokenKind::LayoutOpen), 1);
        assert_eq!(count(src, TokenKind::LayoutClose), 1);
    }

    #[test]
    fn pipe_chain_is_one_item() {
        // Same-column lines led by `|>` continue the item: no inner Sep.
        let src = "module M\nlet describe n =\n  n\n  |> inc\n  |> intToString";
        // One Sep (header -> binding); none inside the block.
        assert_eq!(count(src, TokenKind::LayoutSep), 1);
        assert_eq!(count(src, TokenKind::LayoutOpen), 1);
    }

    #[test]
    fn else_chain_is_one_item() {
        let src = "module M\nlet classify n =\n  if n < 0 then \"neg\"\n  else if n = 0 then \"zero\"\n  else \"pos\"";
        // `else` at the reference column continues the if-expression: no inner Sep.
        assert_eq!(count(src, TokenKind::LayoutSep), 1);
        assert_eq!(count(src, TokenKind::LayoutOpen), 1);
        assert_eq!(count(src, TokenKind::LayoutClose), 1);
    }

    #[test]
    fn local_lets_produce_inner_seps() {
        let src = "module M\nlet hyp a b =\n  let a2 = a * a\n  let b2 = b * b\n  sqrt (a2 + b2)";
        // header->binding (1) + two inner seps between the three block items (2).
        assert_eq!(count(src, TokenKind::LayoutSep), 3);
        assert_eq!(count(src, TokenKind::LayoutOpen), 1);
        assert_eq!(count(src, TokenKind::LayoutClose), 1);
    }

    #[test]
    fn brackets_suspend_layout() {
        // A multi-line list inside `[ ]` must not introduce Seps or Closes.
        let src = "module M\nlet xs =\n  [ 1\n  , 2\n  , 3 ]";
        assert_eq!(count(src, TokenKind::LayoutSep), 1); // only header -> binding
        assert_eq!(count(src, TokenKind::LayoutOpen), 1);
        assert_eq!(count(src, TokenKind::LayoutClose), 1);
    }

    #[test]
    fn unindented_body_is_a_layout_error() {
        let src = "module M\nlet f x =\nbody";
        let result = run(src);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, crate::LAYOUT_ERROR);
        assert_eq!(count(src, TokenKind::LayoutOpen), 0);
    }

    #[test]
    fn multiline_then_else_open_blocks() {
        let src = "module M\nlet f x =\n  if c then\n    a\n  else\n    b";
        // Outer binding block + then-block + else-block = 3 opens/closes.
        assert_eq!(count(src, TokenKind::LayoutOpen), 3);
        assert_eq!(count(src, TokenKind::LayoutClose), 3);
        // No spurious separator before `else` (it is a continuation token).
        assert_eq!(count(src, TokenKind::LayoutSep), 1);
    }

    #[test]
    fn empty_input_is_just_eof() {
        let result = run("");
        assert_eq!(result.tokens.len(), 1);
        assert_eq!(result.tokens[0].kind, TokenKind::Eof);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn every_open_is_balanced_by_a_close() {
        let src = "module M\nlet f x =\n  if c then\n    a\n  else\n    b\nlet g = 1";
        assert_eq!(
            count(src, TokenKind::LayoutOpen),
            count(src, TokenKind::LayoutClose),
            "opens and closes must balance",
        );
    }

    #[test]
    fn snapshot_binding_block() {
        insta::assert_snapshot!("binding_block", render("module M\nlet f x =\n  body"));
    }

    #[test]
    fn snapshot_local_lets() {
        insta::assert_snapshot!(
            "local_lets",
            render("module M\nlet hyp a b =\n  let a2 = a * a\n  let b2 = b * b\n  sqrt (a2 + b2)"),
        );
    }

    #[test]
    fn snapshot_pipe_chain() {
        insta::assert_snapshot!(
            "pipe_chain",
            render("module M\nlet describe n =\n  n\n  |> inc\n  |> intToString"),
        );
    }

    #[test]
    fn snapshot_if_else_chain() {
        insta::assert_snapshot!(
            "if_else_chain",
            render(
                "module M\nlet classify n =\n  if n < 0 then \"neg\"\n  else if n = 0 then \"zero\"\n  else \"pos\""
            ),
        );
    }

    #[test]
    fn snapshot_multiline_then_else() {
        insta::assert_snapshot!(
            "multiline_then_else",
            render("module M\nlet f x =\n  if c then\n    a\n  else\n    b"),
        );
    }

    #[test]
    fn snapshot_bracketed_list() {
        insta::assert_snapshot!(
            "bracketed_list",
            render("module M\nlet xs =\n  [ 1\n  , 2\n  , 3 ]")
        );
    }
}
