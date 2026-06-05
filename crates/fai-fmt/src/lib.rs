//! Canonical formatter for Fai.
//!
//! [`format`] lowers a parsed [`Module`] (plus its comment trivia and the source
//! text) to a [`doc`] document and prints it at the canonical width. The output
//! is a deterministic function of the tree — input line breaks are ignored — so
//! formatting is idempotent. Explicit parentheses are preserved in the AST, so
//! the printer never has to reason about precedence: it prints flat and the
//! `Paren` nodes carry any grouping.

mod doc;

use doc::{Doc, concat, group, nest, print, text};
use fai_span::LineIndex;
use fai_syntax::ast::{
    BinOp, ExprId, ExprKind, FieldInit, FieldPat, FieldType, Item, ItemId, ItemKind, LetStmt,
    Module, PatId, PatKind, RowTail, TypeDef, TypeId, TypeKind, Visibility,
};
use fai_syntax::{Comment, CommentMap, NodeId, attach_comments};

/// The canonical line width.
const WIDTH: usize = 100;

/// Formats `module` (with its `comments`, from source `src`) into canonical text.
///
/// The result always ends with exactly one newline.
#[must_use]
pub fn format(module: &Module, comments: &[Comment], src: &str) -> String {
    let line_index = LineIndex::new(src);
    let map = attach_comments(module, comments, &line_index);
    let printer = Printer { module, comments, src, map };
    let mut out = print(&printer.module_doc(), WIDTH);
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

struct Printer<'a> {
    module: &'a Module,
    comments: &'a [Comment],
    src: &'a str,
    map: CommentMap,
}

impl Printer<'_> {
    fn module_doc(&self) -> Doc {
        let mut parts = Vec::new();
        let header = match self.module.name {
            Some(name) => format!("module {}", name.as_str()),
            None => "module".to_owned(),
        };
        parts.push(text(header));

        for (index, item) in self.module.items.iter().enumerate() {
            let id = ItemId::from_index(index);
            if index == 0 {
                parts.push(Doc::Hardline);
                parts.push(Doc::Hardline);
            } else {
                parts.push(Doc::Hardline);
                if !same_group(&self.module.items[index - 1].kind, &item.kind) {
                    parts.push(Doc::Hardline);
                }
            }
            parts.extend(self.leading_docs(NodeId::Item(id)));
            parts.push(self.item_doc(item));
            parts.extend(self.trailing_docs(NodeId::Item(id)));
        }

        for &id in self.map.dangling() {
            parts.push(Doc::Hardline);
            parts.push(text(self.comment_text(id)));
        }
        concat(parts)
    }

    fn item_doc(&self, item: &Item) -> Doc {
        match &item.kind {
            ItemKind::Signature { visibility, name, ty } => concat(vec![
                text(visibility_prefix(*visibility)),
                text(name.as_str()),
                text(" : "),
                self.type_doc(*ty),
            ]),
            ItemKind::Binding { visibility, name, params, body } => {
                let mut parts =
                    vec![text(visibility_prefix(*visibility)), text("let "), text(name.as_str())];
                for &param in params {
                    parts.push(text(" "));
                    parts.push(self.pat_doc(param));
                }
                parts.push(text(" ="));
                parts.push(self.body_doc(*body));
                concat(parts)
            }
            ItemKind::Type { visibility, name, params, def } => {
                self.type_decl_doc(*visibility, *name, params, def)
            }
            ItemKind::Example { body } => concat(vec![text("example: "), self.expr_doc(*body)]),
            ItemKind::Forall { binders, body } => {
                let bound = binders.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(" ");
                concat(vec![text(format!("forall {bound}: ")), self.expr_doc(*body)])
            }
            ItemKind::Error => text(self.span_src(item.span)),
        }
    }

    fn type_decl_doc(
        &self,
        visibility: Visibility,
        name: fai_syntax::Symbol,
        params: &[fai_syntax::Symbol],
        def: &TypeDef,
    ) -> Doc {
        let mut header = String::from(visibility_prefix(visibility));
        header.push_str("type ");
        header.push_str(name.as_str());
        for p in params {
            header.push(' ');
            header.push_str(p.as_str());
        }
        match def {
            TypeDef::Alias(ty) => concat(vec![text(header), text(" = "), self.type_doc(*ty)]),
            TypeDef::Union(variants) => {
                let mut parts = vec![text(header), text(" =")];
                let mut arms = vec![Doc::Hardline];
                for (index, variant) in variants.iter().enumerate() {
                    if index > 0 {
                        arms.push(Doc::Hardline);
                    }
                    let mut arm = vec![text("| "), text(variant.name.as_str())];
                    for &field in &variant.fields {
                        arm.push(text(" "));
                        arm.push(self.type_doc(field));
                    }
                    arms.push(concat(arm));
                }
                parts.push(nest(2, concat(arms)));
                concat(parts)
            }
        }
    }

    /// A binding/lambda body: a forced-multiline block, or an inline-or-broken
    /// expression in its own group.
    fn body_doc(&self, id: ExprId) -> Doc {
        if let Some((stmts, tail)) = self.multiline_block(id) {
            return nest(2, concat(vec![Doc::Hardline, self.block_inner(stmts, tail)]));
        }
        group(nest(2, concat(vec![Doc::Line, self.expr_doc(self.collapsed(id))])))
    }

    /// An `if` branch: shares the `if`'s break decision (no inner group).
    fn branch_doc(&self, id: ExprId) -> Doc {
        if let Some((stmts, tail)) = self.multiline_block(id) {
            return nest(2, concat(vec![Doc::Hardline, self.block_inner(stmts, tail)]));
        }
        nest(2, concat(vec![Doc::Line, self.expr_doc(self.collapsed(id))]))
    }

    /// The statements and tail of a block with at least one local `let`. A block
    /// that is only a tail expression collapses to that expression.
    fn multiline_block(&self, id: ExprId) -> Option<(&[LetStmt], ExprId)> {
        match &self.module.expr(id).kind {
            ExprKind::Block { stmts, tail } if !stmts.is_empty() => Some((stmts, *tail)),
            _ => None,
        }
    }

    /// Unwraps a tail-only block to its tail; otherwise the id itself.
    fn collapsed(&self, id: ExprId) -> ExprId {
        match &self.module.expr(id).kind {
            ExprKind::Block { stmts, tail } if stmts.is_empty() => *tail,
            _ => id,
        }
    }

    fn block_inner(&self, stmts: &[LetStmt], tail: ExprId) -> Doc {
        let mut parts = Vec::new();
        for stmt in stmts {
            parts.push(self.stmt_doc(stmt));
            parts.push(Doc::Hardline);
        }
        parts.push(self.expr_doc(tail));
        concat(parts)
    }

    fn stmt_doc(&self, stmt: &LetStmt) -> Doc {
        let mut parts = vec![text("let "), self.pat_doc(stmt.pat)];
        for &param in &stmt.params {
            parts.push(text(" "));
            parts.push(self.pat_doc(param));
        }
        parts.push(text(" ="));
        parts.push(self.body_doc(stmt.value));
        concat(parts)
    }

    fn expr_doc(&self, id: ExprId) -> Doc {
        let core = self.expr_core(id);
        let trailing = self.trailing_docs(NodeId::Expr(id));
        if trailing.is_empty() {
            core
        } else {
            let mut parts = vec![core];
            parts.extend(trailing);
            concat(parts)
        }
    }

    fn expr_core(&self, id: ExprId) -> Doc {
        let expr = self.module.expr(id);
        match &expr.kind {
            ExprKind::Int(s)
            | ExprKind::Float(s)
            | ExprKind::String(s)
            | ExprKind::Char(s)
            | ExprKind::Var(s) => text(s.as_str()),
            ExprKind::Unit => text("()"),
            ExprKind::App { func, arg } => {
                concat(vec![self.expr_doc(*func), text(" "), self.expr_doc(*arg)])
            }
            ExprKind::Binary { op, lhs, rhs } => concat(vec![
                self.expr_doc(*lhs),
                text(format!(" {} ", binop_str(*op))),
                self.expr_doc(*rhs),
            ]),
            ExprKind::Unary { operand, .. } => concat(vec![text("-"), self.expr_doc(*operand)]),
            ExprKind::If { .. } => self.if_doc(id),
            ExprKind::Lambda { params, body } => {
                let mut parts = vec![text("fun")];
                for &param in params {
                    parts.push(text(" "));
                    parts.push(self.pat_doc(param));
                }
                parts.push(text(" ->"));
                parts.push(self.body_doc(*body));
                concat(parts)
            }
            ExprKind::Match { .. } => self.match_doc(id),
            ExprKind::Block { .. } => self.body_doc(id),
            ExprKind::Field { base, field } => {
                concat(vec![self.expr_doc(*base), text("."), text(field.as_str())])
            }
            ExprKind::Record(fields) => self.record_literal_doc(None, fields),
            ExprKind::RecordUpdate { base, fields } => self.record_literal_doc(Some(*base), fields),
            ExprKind::Paren(inner) => concat(vec![text("("), self.expr_doc(*inner), text(")")]),
            ExprKind::Tuple(xs) => self.delimited("(", ")", xs),
            ExprKind::List(xs) => self.delimited("[", "]", xs),
            ExprKind::Error => text(self.span_src(expr.span)),
        }
    }

    fn if_doc(&self, id: ExprId) -> Doc {
        let ExprKind::If { cond, then_branch, else_branch } = &self.module.expr(id).kind else {
            unreachable!("if_doc on a non-if expression");
        };
        let else_tail = if matches!(self.module.expr(*else_branch).kind, ExprKind::If { .. }) {
            concat(vec![text(" "), self.if_doc(*else_branch)])
        } else {
            self.branch_doc(*else_branch)
        };
        group(concat(vec![
            text("if "),
            self.expr_doc(*cond),
            text(" then"),
            self.branch_doc(*then_branch),
            Doc::Line,
            text("else"),
            else_tail,
        ]))
    }

    fn match_doc(&self, id: ExprId) -> Doc {
        let ExprKind::Match { scrutinee, arms } = &self.module.expr(id).kind else {
            unreachable!("match_doc on a non-match expression");
        };
        // Arms align with `match` (no extra indent); each body collapses or breaks
        // independently via `body_doc`.
        let mut parts = vec![text("match "), self.expr_doc(*scrutinee), text(" with")];
        for arm in arms {
            parts.push(Doc::Hardline);
            parts.push(concat(vec![
                text("| "),
                self.pat_doc(arm.pat),
                text(" ->"),
                self.body_doc(arm.body),
            ]));
        }
        concat(parts)
    }

    /// Renders a record literal `{ x = a, … }` or update `{ base with x = a, … }`.
    /// Fields are sorted by label (canonical, low-entropy).
    fn record_literal_doc(&self, base: Option<ExprId>, fields: &[FieldInit]) -> Doc {
        let mut order: Vec<&FieldInit> = fields.iter().collect();
        order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let mut parts = vec![text("{")];
        if let Some(base) = base {
            parts.push(text(" "));
            parts.push(self.expr_doc(base));
            parts.push(text(" with"));
        }
        for (index, field) in order.iter().enumerate() {
            parts.push(text(if index == 0 { " " } else { ", " }));
            parts.push(text(field.name.as_str()));
            parts.push(text(" = "));
            parts.push(self.expr_doc(field.value));
        }
        if base.is_none() && order.is_empty() {
            return text("{}");
        }
        parts.push(text(" }"));
        concat(parts)
    }

    fn delimited(&self, open: &str, close: &str, xs: &[ExprId]) -> Doc {
        if xs.is_empty() {
            return text(format!("{open}{close}"));
        }
        let mut parts = vec![text(open.to_owned())];
        for (index, &x) in xs.iter().enumerate() {
            if index > 0 {
                parts.push(text(", "));
            }
            parts.push(self.expr_doc(x));
        }
        parts.push(text(close.to_owned()));
        concat(parts)
    }

    fn pat_doc(&self, id: PatId) -> Doc {
        let pat = self.module.pat(id);
        match &pat.kind {
            PatKind::Var(s) => text(s.as_str()),
            PatKind::Wildcard => text("_"),
            PatKind::Unit => text("()"),
            PatKind::Tuple(xs) => {
                let mut parts = vec![text("(")];
                for (index, &x) in xs.iter().enumerate() {
                    if index > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.pat_doc(x));
                }
                parts.push(text(")"));
                concat(parts)
            }
            PatKind::Paren(inner) => concat(vec![text("("), self.pat_doc(*inner), text(")")]),
            PatKind::Constructor { name, args } => {
                let mut parts = vec![text(name.as_str())];
                for &arg in args {
                    parts.push(text(" "));
                    parts.push(self.pat_doc(arg));
                }
                concat(parts)
            }
            PatKind::Int(s) | PatKind::Float(s) | PatKind::String(s) | PatKind::Char(s) => {
                text(s.as_str())
            }
            PatKind::Bool(b) => text(if *b { "true" } else { "false" }),
            PatKind::List(xs) => {
                let mut parts = vec![text("[")];
                for (index, &x) in xs.iter().enumerate() {
                    if index > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.pat_doc(x));
                }
                parts.push(text("]"));
                concat(parts)
            }
            PatKind::Cons { head, tail } => {
                concat(vec![self.pat_doc(*head), text(" :: "), self.pat_doc(*tail)])
            }
            PatKind::Or(alts) => {
                let mut parts = Vec::new();
                for (index, &alt) in alts.iter().enumerate() {
                    if index > 0 {
                        parts.push(text(" | "));
                    }
                    parts.push(self.pat_doc(alt));
                }
                concat(parts)
            }
            PatKind::Record { fields, open } => self.record_pat_doc(fields, *open),
            PatKind::Error => text(self.span_src(pat.span)),
        }
    }

    /// Renders a record pattern `{ x = p, y }` (fields sorted), open with `| _`.
    fn record_pat_doc(&self, fields: &[FieldPat], open: bool) -> Doc {
        let mut order: Vec<&FieldPat> = fields.iter().collect();
        order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let mut parts = vec![text("{")];
        for (index, field) in order.iter().enumerate() {
            parts.push(text(if index == 0 { " " } else { ", " }));
            parts.push(text(field.name.as_str()));
            if !field.punned {
                parts.push(text(" = "));
                parts.push(self.pat_doc(field.pat));
            }
        }
        if open {
            parts.push(text(" | _"));
        }
        if order.is_empty() && !open {
            return text("{}");
        }
        parts.push(text(" }"));
        concat(parts)
    }

    fn type_doc(&self, id: TypeId) -> Doc {
        let ty = self.module.ty(id);
        match &ty.kind {
            TypeKind::Var(s) | TypeKind::Con(s) => text(s.as_str()),
            TypeKind::App { func, arg } => {
                concat(vec![self.type_doc(*func), text(" "), self.type_doc(*arg)])
            }
            TypeKind::Arrow { from, to } => {
                concat(vec![self.type_doc(*from), text(" -> "), self.type_doc(*to)])
            }
            TypeKind::Tuple(xs) => {
                let mut parts = Vec::new();
                for (index, &x) in xs.iter().enumerate() {
                    if index > 0 {
                        parts.push(text(" * "));
                    }
                    parts.push(self.type_doc(x));
                }
                concat(parts)
            }
            TypeKind::Record { fields, tail } => self.record_type_doc(fields, *tail),
            TypeKind::Unit => text("()"),
            TypeKind::Paren(inner) => concat(vec![text("("), self.type_doc(*inner), text(")")]),
            TypeKind::Error => text(self.span_src(ty.span)),
        }
    }

    /// Renders a record type `{ x : T, … }` (fields sorted) with its tail.
    fn record_type_doc(&self, fields: &[FieldType], tail: RowTail) -> Doc {
        let mut order: Vec<&FieldType> = fields.iter().collect();
        order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let mut parts = vec![text("{")];
        for (index, field) in order.iter().enumerate() {
            parts.push(text(if index == 0 { " " } else { ", " }));
            parts.push(text(field.name.as_str()));
            parts.push(text(" : "));
            parts.push(self.type_doc(field.ty));
        }
        match tail {
            RowTail::Closed => {}
            RowTail::Open => parts.push(text(" | _")),
            RowTail::Named(r) => parts.push(text(format!(" | {}", r.as_str()))),
        }
        parts.push(text(" }"));
        concat(parts)
    }

    fn leading_docs(&self, node: NodeId) -> Vec<Doc> {
        self.map
            .leading(node)
            .iter()
            .flat_map(|&id| [text(self.comment_text(id)), Doc::Hardline])
            .collect()
    }

    fn trailing_docs(&self, node: NodeId) -> Vec<Doc> {
        self.map
            .trailing(node)
            .iter()
            .map(|&id| text(format!(" {}", self.comment_text(id))))
            .collect()
    }

    fn comment_text(&self, id: usize) -> &str {
        let range = self.comments[id].range;
        self.src[range.start().to_usize()..range.end().to_usize()].trim_end()
    }

    fn span_src(&self, span: fai_span::TextRange) -> &str {
        &self.src[span.start().to_usize()..span.end().to_usize()]
    }
}

fn same_group(prev: &ItemKind, next: &ItemKind) -> bool {
    match next {
        ItemKind::Example { .. } | ItemKind::Forall { .. } => true,
        ItemKind::Binding { name, .. } => {
            matches!(prev, ItemKind::Signature { name: prev_name, .. } if prev_name == name)
        }
        _ => false,
    }
}

fn visibility_prefix(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public ",
        Visibility::Private => "",
    }
}

fn binop_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Concat => "++",
        BinOp::Cons => "::",
        BinOp::Pipe => "|>",
        BinOp::Compose => ">>",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Eq => "=",
        BinOp::Ne => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
    }
}
