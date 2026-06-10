//! Inlay hints and semantic tokens — the presentational code-intelligence the
//! editor renders inline (`textDocument/inlayHint` and `…/semanticTokens`).

use fai_db::{Db, SourceFile};
use fai_resolve::{Res, ResolvedBodies, is_upper, module_defs, resolve};
use fai_syntax::ast::{ExprId, ExprKind, Module, PatKind, TypeKind};
use fai_syntax::{TokenKind, lex};
use fai_types::{Ty, body_types, def_type, render_canonical};
use rustc_hash::FxHashMap;
use serde::Serialize;

// --- inlay hints ------------------------------------------------------------

/// An inferred-type hint to render at a byte `offset` (just after a binder),
/// e.g. `: Int`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InlayHint {
    /// Byte offset at which to show the hint (the end of the binder).
    pub offset: u32,
    /// The hint text, including its leading `: `.
    pub label: String,
}

/// Type hints for the variable binders within `[start, end]`: parameters, lambda
/// binders, local `let`s, and match binders. Fai binders carry no inline
/// annotation, so each is shown with its inferred type.
#[must_use]
pub fn inlay_hints(db: &dyn Db, file: SourceFile, start: u32, end: u32) -> Vec<InlayHint> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let mut out: Vec<InlayHint> = Vec::new();
    for d in &module_defs(db, file).defs {
        let types = body_types(db, file, d.name);
        for (&pat, ty) in &types.pat_types {
            if !matches!(module.pat(pat).kind, PatKind::Var(_)) {
                continue; // only named binders get a "name : type" hint
            }
            let pos = module.pat(pat).span.end().raw();
            if pos < start || pos > end {
                continue;
            }
            out.push(InlayHint { offset: pos, label: format!(": {}", render_canonical(ty)) });
        }
    }
    out.sort_by(|a, b| a.offset.cmp(&b.offset).then_with(|| a.label.cmp(&b.label)));
    out.dedup();
    out
}

// --- semantic tokens --------------------------------------------------------

/// The semantic token classes, in legend order (the index of each name is the
/// `tokenType` the editor receives).
pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "keyword",
    "function",
    "variable",
    "type",
    "typeParameter",
    "enumMember",
    "namespace",
    "number",
    "string",
    "operator",
    "comment",
];

/// The class of a semantic token.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SemKind {
    Keyword,
    Function,
    Variable,
    Type,
    TypeParameter,
    EnumMember,
    Namespace,
    Number,
    String,
    Operator,
    Comment,
}

impl SemKind {
    fn name(self) -> &'static str {
        match self {
            SemKind::Keyword => "keyword",
            SemKind::Function => "function",
            SemKind::Variable => "variable",
            SemKind::Type => "type",
            SemKind::TypeParameter => "typeParameter",
            SemKind::EnumMember => "enumMember",
            SemKind::Namespace => "namespace",
            SemKind::Number => "number",
            SemKind::String => "string",
            SemKind::Operator => "operator",
            SemKind::Comment => "comment",
        }
    }

    /// The token's index in [`SEMANTIC_TOKEN_TYPES`] (its LSP `tokenType`).
    #[must_use]
    pub fn index(self) -> u32 {
        SEMANTIC_TOKEN_TYPES.iter().position(|&t| t == self.name()).unwrap_or(0) as u32
    }
}

/// A classified source token: a byte range and its class.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SemToken {
    /// Start byte offset.
    pub offset: u32,
    /// Length in bytes.
    pub length: u32,
    /// The token's class.
    pub kind: SemKind,
}

/// The semantic tokens of `file`, in source order: every keyword, literal,
/// operator, comment, and identifier (classified by resolution where it is a
/// name reference, else by casing). Powers LSP semantic highlighting.
#[must_use]
pub fn semantic_tokens(db: &dyn Db, file: SourceFile) -> Vec<SemToken> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = resolve(db, file);
    let text = file.text(db);
    let kinds = build_kind_map(db, module, &resolved);

    let lexed = lex(file.source(db), text);
    let mut out: Vec<SemToken> = Vec::new();
    for token in &lexed.tokens {
        let start = token.range.start().raw();
        let kind = match token.kind {
            TokenKind::Module
            | TokenKind::Let
            | TokenKind::Type
            | TokenKind::Interface
            | TokenKind::Match
            | TokenKind::With
            | TokenKind::If
            | TokenKind::Then
            | TokenKind::Else
            | TokenKind::Fun
            | TokenKind::Public
            | TokenKind::Example
            | TokenKind::Forall
            | TokenKind::As => SemKind::Keyword,
            TokenKind::Int | TokenKind::Float => SemKind::Number,
            TokenKind::String | TokenKind::Char => SemKind::String,
            TokenKind::Operator
            | TokenKind::Arrow
            | TokenKind::Equals
            | TokenKind::Pipe
            | TokenKind::ColonColon => SemKind::Operator,
            TokenKind::TypeVar => SemKind::TypeParameter,
            // A name reference's class comes from resolution; otherwise a bare
            // identifier defaults by casing (a value vs. a type/constructor).
            TokenKind::LowerIdent => kinds.get(&start).copied().unwrap_or(SemKind::Variable),
            TokenKind::UpperIdent => kinds.get(&start).copied().unwrap_or(SemKind::Type),
            _ => continue, // punctuation, the wildcard, layout tokens, EOF
        };
        out.push(SemToken { offset: start, length: token.range.len(), kind });
    }
    for comment in &lexed.comments {
        out.push(SemToken {
            offset: comment.range.start().raw(),
            length: comment.range.len(),
            kind: SemKind::Comment,
        });
    }
    out.sort_by_key(|t| t.offset);
    out
}

/// Maps the byte-start of each classifiable identifier to its semantic class,
/// from resolution (references) and the AST (binders, type names).
fn build_kind_map(
    db: &dyn Db,
    module: &Module,
    resolved: &ResolvedBodies,
) -> FxHashMap<u32, SemKind> {
    let mut map: FxHashMap<u32, SemKind> = FxHashMap::default();
    for (i, expr) in module.exprs.iter().enumerate() {
        let id = ExprId::from_index(i);
        match &expr.kind {
            ExprKind::Var(_) => {
                if let Some(kind) = resolved.get(id).and_then(|res| classify_res(db, res)) {
                    map.insert(expr.span.start().raw(), kind);
                }
            }
            ExprKind::Field { base, field } => {
                // A qualified reference resolves on the `Field` node; classify the
                // trailing member token, and the leading bare module segment.
                if let Some(kind) = resolved.get(id).and_then(|res| classify_res(db, res)) {
                    let flen = field.as_str().len() as u32;
                    if flen > 0 && flen <= expr.span.len() {
                        map.insert(expr.span.end().raw() - flen, kind);
                    }
                    if let ExprKind::Var(bname) = &module.expr(*base).kind
                        && is_upper(*bname)
                    {
                        map.entry(module.expr(*base).span.start().raw())
                            .or_insert(SemKind::Namespace);
                    }
                }
            }
            _ => {}
        }
    }
    for pat in &module.pats {
        match &pat.kind {
            PatKind::Var(_) => {
                map.insert(pat.span.start().raw(), SemKind::Variable);
            }
            PatKind::Constructor { name, .. } if !name.as_str().contains('.') => {
                map.insert(pat.span.start().raw(), SemKind::EnumMember);
            }
            _ => {}
        }
    }
    for ty in &module.types {
        if let TypeKind::Con(name) = &ty.kind {
            let seg = name.as_str().rsplit('.').next().unwrap_or(name.as_str());
            let slen = seg.len() as u32;
            if slen > 0 && slen <= ty.span.len() {
                map.entry(ty.span.end().raw() - slen).or_insert(SemKind::Type);
            }
        }
    }
    map
}

/// The semantic class of a resolved reference (a function vs. value definition,
/// a constructor, a local, or a builtin). `None` for the error sentinel.
fn classify_res(db: &dyn Db, res: Res) -> Option<SemKind> {
    match res {
        Res::Def(d) => {
            let f = db.source_file(d.file)?;
            let is_fn = matches!(def_type(db, f, d.name).ty, Ty::Arrow(..));
            Some(if is_fn { SemKind::Function } else { SemKind::Variable })
        }
        Res::Ctor(_) => Some(SemKind::EnumMember),
        Res::Local(_) => Some(SemKind::Variable),
        Res::Builtin(_) => Some(SemKind::Function),
        Res::Error => None,
    }
}
