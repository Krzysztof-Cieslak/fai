//! A Wadler/Lindig pretty-printing document.
//!
//! A [`Doc`] is lowered from the AST, then [`print`]ed at a target width. A
//! [`Doc::Group`] renders flat (its [`Doc::Line`]s become spaces) if it fits on
//! the current line, otherwise broken (its `Line`s become newlines). A
//! [`Doc::Hardline`] is always a newline and forces every enclosing group to
//! break.

/// A layout document.
#[derive(Debug)]
pub enum Doc {
    /// Literal text (may contain newlines, e.g. a block comment).
    Text(String),
    /// A line comment printed after pending punctuation, before the next newline.
    LineSuffix(String),
    /// A space when flat, a newline + indent when broken.
    Line,
    /// Nothing when flat, a newline + indent when broken. Used at the inside
    /// edges of a collection so `[a, b]` stays tight when it fits but each
    /// element gets its own line once the group breaks.
    Softline,
    /// Always a newline + indent; forces the enclosing group to break.
    Hardline,
    /// Increases the indentation of its contents by `n`.
    Nest(usize, Box<Doc>),
    /// A sequence of documents.
    Concat(Vec<Doc>),
    /// A group that renders flat if it fits, else broken.
    Group(Box<Doc>),
}

impl Doc {
    pub(crate) fn has_line_suffix(&self) -> bool {
        match self {
            Self::LineSuffix(_) => true,
            Self::Concat(docs) => docs.iter().any(Self::has_line_suffix),
            Self::Nest(_, inner) | Self::Group(inner) => inner.has_line_suffix(),
            _ => false,
        }
    }
}

/// Literal text.
#[must_use]
pub fn text(s: impl Into<String>) -> Doc {
    Doc::Text(s.into())
}

/// A sequence of documents.
#[must_use]
pub fn concat(docs: Vec<Doc>) -> Doc {
    Doc::Concat(docs)
}

/// Indents `doc` by `n` columns.
#[must_use]
pub fn nest(n: usize, doc: Doc) -> Doc {
    Doc::Nest(n, Box::new(doc))
}

/// Groups `doc` (flat if it fits, else broken).
#[must_use]
pub fn group(doc: Doc) -> Doc {
    Doc::Group(Box::new(doc))
}

#[derive(Clone, Copy)]
enum Mode {
    Flat,
    Break,
}

/// Renders `doc` to a string, breaking groups that do not fit in `width`.
///
/// A broken line's indentation is written **lazily** — only when real text
/// follows on that line — so an otherwise-blank line carries no trailing
/// whitespace (e.g. the blank line between two groups inside a nested module).
#[must_use]
pub fn print(doc: &Doc, width: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    // When a line break is emitted, its indentation is deferred here and flushed
    // by the next non-empty `Text` (or a flat `Line`). A line that ends with no
    // text written keeps no trailing spaces.
    let mut pending_indent: Option<usize> = None;
    let mut suffixes = Vec::new();
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((indent, mode, doc)) = stack.pop() {
        match doc {
            Doc::LineSuffix(s) => suffixes.push(s.as_str()),
            Doc::Text(s) => {
                if !s.is_empty() {
                    flush_indent(&mut out, &mut pending_indent);
                    out.push_str(s);
                }
                match s.rfind('\n') {
                    Some(nl) => col = s[nl + 1..].chars().count(),
                    None => col += s.chars().count(),
                }
            }
            Doc::Concat(docs) => {
                for doc in docs.iter().rev() {
                    stack.push((indent, mode, doc));
                }
            }
            Doc::Nest(n, inner) => stack.push((indent + n, mode, inner)),
            Doc::Line if !suffixes.is_empty() => {
                flush_suffixes(&mut out, &mut suffixes, indent);
                out.push('\n');
                pending_indent = Some(indent);
                col = indent;
            }
            Doc::Line => match mode {
                Mode::Flat => {
                    flush_indent(&mut out, &mut pending_indent);
                    out.push(' ');
                    col += 1;
                }
                Mode::Break => {
                    out.push('\n');
                    pending_indent = Some(indent);
                    col = indent;
                }
            },
            Doc::Softline if !suffixes.is_empty() => {
                flush_suffixes(&mut out, &mut suffixes, indent);
                out.push('\n');
                pending_indent = Some(indent);
                col = indent;
            }
            Doc::Softline => match mode {
                Mode::Flat => {}
                Mode::Break => {
                    out.push('\n');
                    pending_indent = Some(indent);
                    col = indent;
                }
            },
            Doc::Hardline => {
                flush_suffixes(&mut out, &mut suffixes, indent);
                out.push('\n');
                pending_indent = Some(indent);
                col = indent;
            }
            Doc::Group(inner) => {
                let remaining = width as i32 - col as i32;
                let mode = if fits(remaining, inner, &stack) { Mode::Flat } else { Mode::Break };
                stack.push((indent, mode, inner));
            }
        }
    }
    flush_suffixes(&mut out, &mut suffixes, 0);
    out
}

fn flush_suffixes(out: &mut String, suffixes: &mut Vec<&str>, indent: usize) {
    for (i, suffix) in suffixes.drain(..).enumerate() {
        if i == 0 {
            out.push(' ');
        } else {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', indent));
        }
        out.push_str(suffix);
    }
}

/// Writes any deferred line indentation before real text is emitted.
fn flush_indent(out: &mut String, pending: &mut Option<usize>) {
    if let Some(indent) = pending.take() {
        for _ in 0..indent {
            out.push(' ');
        }
    }
}

/// Whether `doc` (flat), followed by the pending `rest`, fits before the next
/// newline within `remaining` columns.
fn fits(mut remaining: i32, doc: &Doc, rest: &[(usize, Mode, &Doc)]) -> bool {
    if remaining < 0 {
        return false;
    }
    let mut work: Vec<(Mode, &Doc)> = vec![(Mode::Flat, doc)];
    let mut rest = rest.iter().rev();
    loop {
        let (mode, doc) = match work.pop() {
            Some(item) => item,
            None => match rest.next() {
                Some(&(_, mode, doc)) => (mode, doc),
                None => return true,
            },
        };
        match doc {
            Doc::LineSuffix(s) => {
                remaining -= s.chars().count() as i32 + 1;
                if remaining < 0 {
                    return false;
                }
            }
            Doc::Text(s) => {
                if s.contains('\n') {
                    return true;
                }
                remaining -= s.chars().count() as i32;
                if remaining < 0 {
                    return false;
                }
            }
            Doc::Concat(docs) => {
                for doc in docs.iter().rev() {
                    work.push((mode, doc));
                }
            }
            Doc::Nest(_, inner) => work.push((mode, inner)),
            Doc::Group(inner) => work.push((Mode::Flat, inner)),
            Doc::Line => match mode {
                Mode::Flat => {
                    remaining -= 1;
                    if remaining < 0 {
                        return false;
                    }
                }
                Mode::Break => return true,
            },
            Doc::Softline => match mode {
                Mode::Flat => {}
                Mode::Break => return true,
            },
            // A hard line inside the group being tested (Flat) means it cannot be
            // rendered flat; a hard line in the trailing context ends the line.
            Doc::Hardline => match mode {
                Mode::Flat => return false,
                Mode::Break => return true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Doc, concat, group, nest, print, text};

    #[test]
    fn flat_when_it_fits() {
        let doc = group(concat(vec![text("a"), Doc::Line, text("b")]));
        assert_eq!(print(&doc, 80), "a b");
    }

    #[test]
    fn breaks_when_too_wide() {
        let doc = group(concat(vec![text("aaaa"), Doc::Line, text("bbbb")]));
        assert_eq!(print(&doc, 5), "aaaa\nbbbb");
    }

    #[test]
    fn nest_indents_broken_lines() {
        let doc = group(concat(vec![
            text("("),
            nest(2, concat(vec![Doc::Line, text("x")])),
            Doc::Line,
            text(")"),
        ]));
        assert_eq!(print(&doc, 3), "(\n  x\n)");
    }

    #[test]
    fn hardline_forces_break() {
        let doc = group(concat(vec![text("a"), Doc::Hardline, text("b")]));
        assert_eq!(print(&doc, 80), "a\nb");
    }

    #[test]
    fn multiline_text_tracks_column() {
        let doc = concat(vec![text("a\nbb"), text("c")]);
        assert_eq!(print(&doc, 80), "a\nbbc");
    }
}
