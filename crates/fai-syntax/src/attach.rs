//! Attaching comment trivia to AST nodes for the formatter.
//!
//! Comments are collected separately by the lexer; [`attach_comments`] decides,
//! for each one, which node it leads or trails, Prettier-style: the node ending
//! closest before the comment (`preceding`) and the one starting closest after
//! (`following`) are found, and the comment becomes a **trailing** comment of
//! `preceding` if it is on the same line, otherwise a **leading** comment of
//! `following`. Doc comments (`///`) always lead. The result is a side-table
//! keyed by [`NodeId`]; the AST nodes themselves are not changed.

use fai_span::{LineIndex, TextRange};
use rustc_hash::FxHashMap;

use crate::ast::{ExprId, ItemId, Module};
use crate::token::{Comment, CommentKind};

/// A comment, identified by its index in the lexer's comment list.
pub type CommentId = usize;

/// A node that can own attached comments.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NodeId {
    /// A top-level item.
    Item(ItemId),
    /// An expression.
    Expr(ExprId),
}

/// Comments attached to the tree, keyed by the node they belong to.
#[derive(Default, Debug)]
pub struct CommentMap {
    leading: FxHashMap<NodeId, Vec<CommentId>>,
    trailing: FxHashMap<NodeId, Vec<CommentId>>,
    dangling: Vec<CommentId>,
}

impl CommentMap {
    /// Comments that print on their own lines before `node`.
    #[must_use]
    pub fn leading(&self, node: NodeId) -> &[CommentId] {
        self.leading.get(&node).map_or(&[], Vec::as_slice)
    }

    /// Comments that print at the end of `node`'s line.
    #[must_use]
    pub fn trailing(&self, node: NodeId) -> &[CommentId] {
        self.trailing.get(&node).map_or(&[], Vec::as_slice)
    }

    /// Comments not attached to any node (e.g. trailing at end of file).
    #[must_use]
    pub fn dangling(&self) -> &[CommentId] {
        &self.dangling
    }
}

/// Attaches `comments` to the nodes of `module` (pure; `line_index` is for the
/// same file).
#[must_use]
pub fn attach_comments(
    module: &Module,
    comments: &[Comment],
    line_index: &LineIndex,
) -> CommentMap {
    let mut nodes: Vec<(NodeId, TextRange)> =
        Vec::with_capacity(module.items.len() + module.exprs.len());
    for (index, item) in module.items.iter().enumerate() {
        nodes.push((NodeId::Item(ItemId::from_index(index)), item.span));
    }
    for (index, expr) in module.exprs.iter().enumerate() {
        nodes.push((NodeId::Expr(ExprId::from_index(index)), expr.span));
    }

    let mut map = CommentMap::default();
    for (id, comment) in comments.iter().enumerate() {
        let start = comment.range.start();
        let end = comment.range.end();
        let comment_line = line_index.line(start);

        // The outermost node ending closest before the comment.
        let mut preceding: Option<(NodeId, TextRange)> = None;
        // The outermost node starting closest after the comment.
        let mut following: Option<(NodeId, TextRange)> = None;
        for &(node, range) in &nodes {
            if range.end() <= start {
                let better = preceding.is_none_or(|(_, best)| {
                    range.end() > best.end()
                        || (range.end() == best.end() && range.start() < best.start())
                });
                if better {
                    preceding = Some((node, range));
                }
            }
            if range.start() >= end {
                let better = following.is_none_or(|(_, best)| {
                    range.start() < best.start()
                        || (range.start() == best.start() && range.end() > best.end())
                });
                if better {
                    following = Some((node, range));
                }
            }
        }

        let same_line = comment.kind != CommentKind::Doc
            && preceding.is_some_and(|(_, range)| line_index.line(range.end()) == comment_line);

        if same_line {
            push(&mut map.trailing, preceding.unwrap().0, id);
        } else if let Some((node, _)) = following {
            push(&mut map.leading, node, id);
        } else if let Some((node, _)) = preceding {
            push(&mut map.trailing, node, id);
        } else {
            map.dangling.push(id);
        }
    }
    map
}

fn push(map: &mut FxHashMap<NodeId, Vec<CommentId>>, node: NodeId, id: CommentId) {
    map.entry(node).or_default().push(id);
}
