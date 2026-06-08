//! The eight `fai query` commands, built on resolution + inference.
//!
//! Every result is a serde envelope carrying `schemaVersion` (CLI.md §8). Lists
//! are deterministically sorted; spans are resolved late via a [`SpanResolver`].
//! Results are best-effort: partial answers are returned even when the workspace
//! has errors.

use fai_db::{Db, SourceFile};
use fai_resolve::{CtorRef, DefId, LocalId, Res, ResolvedBodies, module_defs, resolve, type_decls};
use fai_span::{ByteOffset, LineIndex, Span, SpanResolver, TextRange};
use fai_syntax::ast::{
    ExprId, ExprKind, ItemKind, Module, PatId, PatKind, RowTail, TypeDef, TypeId, TypeKind,
    Visibility as AstVis,
};
use fai_syntax::{CommentKind, NodeId, Symbol, attach_comments};
use fai_types::Ty;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

use crate::repr::{
    CapOrigin, Capability, Contract, Doc, Location, SCHEMA_VERSION, SpanJson, SymbolKind,
    SymbolRef, TypeRepr, Visibility,
};
use crate::target::{module_label, resolve_target};

/// Options shared by list-producing commands.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOpts {
    /// Maximum number of results (`None` = unbounded).
    pub limit: Option<usize>,
}

fn truncate<T>(mut items: Vec<T>, opts: ListOpts) -> (Vec<T>, bool) {
    if let Some(limit) = opts.limit
        && items.len() > limit
    {
        items.truncate(limit);
        return (items, true);
    }
    (items, false)
}

/// Builds a [`SymbolRef`] for a definition.
fn symbol_ref(
    db: &dyn Db,
    file: SourceFile,
    name: Symbol,
    resolver: &dyn SpanResolver,
) -> Option<SymbolRef> {
    let defs = module_defs(db, file);
    let def = defs.get(name)?;
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let span = module.items[def.binding.index()].span;
    let module_name = module_label(db, file);
    let scheme = fai_types::def_type(db, file, name);
    let kind = if matches!(scheme.ty, fai_types::Ty::Arrow(_, _)) {
        SymbolKind::Function
    } else {
        SymbolKind::Value
    };
    let visibility = match def.visibility {
        AstVis::Public => Visibility::Public,
        AstVis::Private => Visibility::Private,
    };
    let signature = Some(fai_types::render_scheme(&scheme));
    Some(SymbolRef {
        path: format!("{module_name}.{name}"),
        name: name.as_str().to_owned(),
        kind,
        module: module_name,
        visibility,
        signature,
        span: SpanJson::resolve(Span::new(file.source(db), span), resolver)?,
    })
}

/// `fai query symbols`.
#[derive(Debug, Serialize)]
pub struct SymbolsResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub symbols: Vec<SymbolRef>,
    pub truncated: bool,
}

/// Lists symbols across the given user files (optionally filtered to a module).
#[must_use]
pub fn symbols(
    db: &dyn Db,
    files: &[SourceFile],
    module_filter: Option<&str>,
    resolver: &dyn SpanResolver,
    opts: ListOpts,
) -> SymbolsResult {
    let mut out = Vec::new();
    for &file in files {
        let label = module_label(db, file);
        if let Some(m) = module_filter
            && m != label
        {
            continue;
        }
        let defs = module_defs(db, file);
        for d in &defs.defs {
            if let Some(sr) = symbol_ref(db, file, d.name, resolver) {
                out.push(sr);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    let (symbols, truncated) = truncate(out, opts);
    SymbolsResult { schema_version: SCHEMA_VERSION, symbols, truncated }
}

/// Symbols across the workspace whose name matches `query` (case-insensitive
/// substring; an empty query matches everything). Powers LSP `workspace/symbol`.
#[must_use]
pub fn workspace_symbols(
    db: &dyn Db,
    files: &[SourceFile],
    query: &str,
    resolver: &dyn SpanResolver,
    opts: ListOpts,
) -> SymbolsResult {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for &file in files {
        for d in &module_defs(db, file).defs {
            if !needle.is_empty() && !d.name.as_str().to_lowercase().contains(&needle) {
                continue;
            }
            if let Some(sr) = symbol_ref(db, file, d.name, resolver) {
                out.push(sr);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    let (symbols, truncated) = truncate(out, opts);
    SymbolsResult { schema_version: SCHEMA_VERSION, symbols, truncated }
}

/// `fai query def`.
#[derive(Debug, Serialize)]
pub struct DefResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub definitions: Vec<Location>,
}

/// Resolves a target to its definition site(s).
#[must_use]
pub fn def(db: &dyn Db, target: &str, resolver: &dyn SpanResolver) -> DefResult {
    let Some(t) = resolve_target(db, target) else {
        return DefResult { schema_version: SCHEMA_VERSION, target: None, definitions: vec![] };
    };
    let symbol = symbol_ref(db, t.file, t.name, resolver);
    let definitions = symbol
        .as_ref()
        .map(|s| vec![Location { span: s.span.clone(), preview: None }])
        .unwrap_or_default();
    DefResult { schema_version: SCHEMA_VERSION, target: symbol, definitions }
}

/// `fai query type`.
#[derive(Debug, Serialize)]
pub struct TypeResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    #[serde(rename = "type")]
    pub ty: TypeRepr,
}

/// The type at a target.
#[must_use]
pub fn type_at(db: &dyn Db, target: &str, resolver: &dyn SpanResolver) -> TypeResult {
    let Some(t) = resolve_target(db, target) else {
        return TypeResult {
            schema_version: SCHEMA_VERSION,
            target: None,
            ty: TypeRepr { display: "{unknown}".to_owned() },
        };
    };
    let scheme = fai_types::def_type(db, t.file, t.name);
    TypeResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        ty: TypeRepr { display: fai_types::render_scheme(&scheme) },
    }
}

// --- position-based queries (hover / go-to-definition) -----------------------
//
// `fai query` addresses a definition by name; an editor addresses a *byte offset*
// inside a file, and wants the most specific subexpression there. These two
// queries answer at offset granularity: they find the innermost expression
// containing the cursor and report its type (hover) or jump to what its reference
// resolves to (go-to-definition). They power the LSP.

/// The expressions whose span contains `offset`, smallest (innermost) first.
///
/// Half-open containment (`start <= offset < end`) so an offset is "the character
/// under the cursor"; ties break by arena order, which is stable.
fn exprs_containing(module: &Module, offset: u32) -> Vec<ExprId> {
    let mut hits: Vec<(u32, ExprId)> = module
        .exprs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let (start, end) = (e.span.start().raw(), e.span.end().raw());
            (start <= offset && offset < end).then(|| (end - start, ExprId::from_index(i)))
        })
        .collect();
    hits.sort_by_key(|&(width, _)| width);
    hits.into_iter().map(|(_, id)| id).collect()
}

/// The qualified name of the smallest top-level/nested binding whose item span
/// contains `offset` — the definition whose body the cursor sits in (for keying
/// the per-definition `body_types`).
pub(crate) fn enclosing_def(db: &dyn Db, file: SourceFile, offset: u32) -> Option<Symbol> {
    let parsed = fai_syntax::parse(db, file);
    let mut best: Option<(u32, Symbol)> = None;
    for d in &module_defs(db, file).defs {
        let r = parsed.module.items[d.binding.index()].span;
        if r.start().raw() <= offset && offset < r.end().raw() {
            let width = r.end().raw() - r.start().raw();
            if best.is_none_or(|(w, _)| width < w) {
                best = Some((width, d.name));
            }
        }
    }
    best.map(|(_, name)| name)
}

/// The name a referencing expression refers to (for the hover label), or `None`
/// when the expression is not a name reference.
fn reference_name(module: &Module, resolved: &ResolvedBodies, expr: ExprId) -> Option<String> {
    match resolved.get(expr)? {
        Res::Def(d) => Some(d.name.as_str().to_owned()),
        Res::Ctor(c) => Some(c.name.as_str().to_owned()),
        Res::Builtin(s) => Some(s.as_str().to_owned()),
        Res::Local(_) => match &module.expr(expr).kind {
            ExprKind::Var(name) => Some(name.as_str().to_owned()),
            _ => None,
        },
        Res::Error => None,
    }
}

/// The declaration site of a data constructor: the variant's span within its
/// `type` declaration (falling back to the whole declaration).
fn ctor_location(db: &dyn Db, ctor: CtorRef, resolver: &dyn SpanResolver) -> Option<Location> {
    let file = db.source_file(ctor.file)?;
    let decls = type_decls(db, file);
    let info = decls.ctor(ctor.name)?;
    let type_info = decls.type_named(info.adt)?;
    let parsed = fai_syntax::parse(db, file);
    let item = &parsed.module.items[type_info.item.index()];
    let range = match &item.kind {
        ItemKind::Type { def: TypeDef::Union(variants), .. } => {
            variants.iter().find(|v| v.name == ctor.name).map_or(item.span, |v| v.span)
        }
        _ => item.span,
    };
    let span = SpanJson::resolve(Span::new(file.source(db), range), resolver)?;
    Some(Location { span, preview: None })
}

/// The binding site of a local (the pattern that introduced it). Local slots are
/// unique within a file's resolution, so the reverse lookup is unambiguous.
fn local_location(
    db: &dyn Db,
    file: SourceFile,
    local: LocalId,
    resolver: &dyn SpanResolver,
) -> Option<Location> {
    let resolved = resolve(db, file);
    let pat = resolved.pat_locals.iter().find_map(|(&p, &l)| (l == local).then_some(p))?;
    let parsed = fai_syntax::parse(db, file);
    let span =
        SpanJson::resolve(Span::new(file.source(db), parsed.module.pat(pat).span), resolver)?;
    Some(Location { span, preview: None })
}

/// The hover answer at a byte offset: the innermost typed subexpression's type,
/// labelled with the name it refers to when it is a reference.
#[derive(Debug, Serialize)]
pub struct HoverResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// The referenced name, when the subexpression is a name reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The subexpression's type.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<TypeRepr>,
    /// The subexpression's span (so an editor can underline what it described).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SpanJson>,
    /// The `///` doc prose of the referenced definition, when the subexpression
    /// is a reference to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<Doc>,
    /// The contracts attached to the referenced definition.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<Contract>,
}

/// The type at a byte offset: the innermost expression that has an inferred type,
/// rendered with the name it refers to when applicable. When that expression is a
/// reference to a definition, the definition's doc prose and attached contracts
/// are included too. Powers LSP hover.
#[must_use]
pub fn hover_at(
    db: &dyn Db,
    file: SourceFile,
    offset: u32,
    resolver: &dyn SpanResolver,
) -> HoverResult {
    let empty = || HoverResult {
        schema_version: SCHEMA_VERSION,
        name: None,
        ty: None,
        span: None,
        doc: None,
        contracts: vec![],
    };
    let parsed = fai_syntax::parse(db, file);
    let resolved = resolve(db, file);
    let Some(types) = enclosing_def(db, file, offset).map(|d| fai_types::body_types(db, file, d))
    else {
        return empty();
    };
    for expr in exprs_containing(&parsed.module, offset) {
        let Some(ty) = types.get(expr) else { continue };
        let span =
            SpanJson::resolve(Span::new(file.source(db), parsed.module.expr(expr).span), resolver);
        // When the subexpression references a definition, surface its docs and
        // contracts (resolving through to the defining file).
        let (doc, contracts) = match resolved.get(expr) {
            Some(Res::Def(d)) => match db.source_file(d.file) {
                Some(f) => (
                    doc_for(db, f, d.name),
                    contracts_by_subject(db, f, resolver).remove(&d.name).unwrap_or_default(),
                ),
                None => (None, vec![]),
            },
            _ => (None, vec![]),
        };
        return HoverResult {
            schema_version: SCHEMA_VERSION,
            name: reference_name(&parsed.module, &resolved, expr),
            ty: Some(TypeRepr { display: fai_types::render_canonical(ty) }),
            span,
            doc,
            contracts,
        };
    }
    empty()
}

/// The definition site(s) of the reference at a byte offset: a top-level
/// definition, a constructor, or a local binding. Powers LSP go-to-definition.
#[must_use]
pub fn definition_at(
    db: &dyn Db,
    file: SourceFile,
    offset: u32,
    resolver: &dyn SpanResolver,
) -> DefResult {
    let empty = || DefResult { schema_version: SCHEMA_VERSION, target: None, definitions: vec![] };
    let parsed = fai_syntax::parse(db, file);
    let resolved = resolve(db, file);
    for expr in exprs_containing(&parsed.module, offset) {
        let Some(res) = resolved.get(expr) else { continue };
        let result = match res {
            Res::Def(d) => db.source_file(d.file).and_then(|f| {
                let symbol = symbol_ref(db, f, d.name, resolver)?;
                let location = Location { span: symbol.span.clone(), preview: None };
                Some(DefResult {
                    schema_version: SCHEMA_VERSION,
                    target: Some(symbol),
                    definitions: vec![location],
                })
            }),
            Res::Ctor(c) => ctor_location(db, c, resolver).map(|location| DefResult {
                schema_version: SCHEMA_VERSION,
                target: None,
                definitions: vec![location],
            }),
            Res::Local(l) => local_location(db, file, l, resolver).map(|location| DefResult {
                schema_version: SCHEMA_VERSION,
                target: None,
                definitions: vec![location],
            }),
            Res::Builtin(_) | Res::Error => None,
        };
        if let Some(result) = result {
            return result;
        }
    }
    empty()
}

// --- signature help (LSP `textDocument/signatureHelp`) -----------------------
//
// While the cursor sits among a call's arguments, signature help shows the
// callee's type with the parameter currently being supplied highlighted. We find
// the enclosing application (or a bare function name with a trailing space),
// take the head's inferred function type, and split its arrow chain into
// parameters; the active parameter is the count of arguments already before the
// cursor.

/// One parameter of a signature, as a half-open `[start, end)` slice of the
/// signature label (so the editor can highlight it).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ParamInfo {
    /// Start offset into the label.
    pub start: u32,
    /// End offset into the label.
    pub end: u32,
}

/// The signature help at a byte offset: the callee's rendered type, its parameter
/// spans, and which parameter the cursor is currently supplying.
#[derive(Debug, Serialize)]
pub struct SignatureHelp {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// The full signature label (`name : T1 -> T2 -> R`, or just the type).
    pub label: String,
    /// The parameter slices of `label`, in order.
    pub parameters: Vec<ParamInfo>,
    /// The 0-based index of the parameter currently being supplied.
    pub active_parameter: u32,
}

/// The head of an application spine and the argument expressions applied to it
/// (`f a b` → `(f, [a, b])`). Returns `None` when `expr` is not an application.
fn application_spine(module: &Module, expr: ExprId) -> Option<(ExprId, Vec<ExprId>)> {
    let mut args = Vec::new();
    let mut cur = expr;
    while let ExprKind::App { func, arg } = &module.expr(cur).kind {
        args.push(*arg);
        cur = *func;
    }
    if args.is_empty() {
        return None;
    }
    args.reverse();
    Some((cur, args))
}

/// Whether `[from, offset)` is a non-empty run of whitespace — the cursor sits
/// just past `from` with only spaces between.
fn whitespace_gap(src: &str, from: u32, offset: u32) -> bool {
    from < offset
        && src
            .get(from as usize..offset as usize)
            .is_some_and(|s| s.chars().all(char::is_whitespace))
}

/// The signature help at `offset`: finds the enclosing call (or a function name
/// followed by whitespace) and reports the callee's parameters with the active
/// one. Powers LSP signature help.
#[must_use]
pub fn signature_help_at(db: &dyn Db, file: SourceFile, offset: u32) -> Option<SignatureHelp> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let src = file.text(db);
    let resolved = resolve(db, file);

    // The enclosing application: the widest `App` whose span contains the cursor
    // (inclusive), or that ends with only whitespace before it (a trailing
    // argument position, `f a |`).
    let app = module
        .exprs
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.kind, ExprKind::App { .. }))
        .filter(|(_, e)| {
            let (start, end) = (e.span.start().raw(), e.span.end().raw());
            (start <= offset && offset <= end) || whitespace_gap(src, end, offset)
        })
        .max_by_key(|(_, e)| e.span.len())
        .map(|(i, _)| ExprId::from_index(i));

    let (head, args_before) = match app.and_then(|a| application_spine(module, a)) {
        // An argument advances the active parameter once the cursor is strictly
        // past it (so jamming the cursor against an argument keeps editing it,
        // while a following space moves on to the next parameter).
        Some((head, args)) => {
            let before = args.iter().filter(|&&a| module.expr(a).span.end().raw() < offset).count();
            (head, before)
        }
        None => (head_with_trailing_space(module, src, offset)?, 0),
    };

    // The head expression sits inside its definition's body even when the cursor
    // is in trailing whitespace, so key the per-body types on the head's position.
    let def = enclosing_def(db, file, module.expr(head).span.start().raw())?;
    let types = fai_types::body_types(db, file, def);
    let head_ty = types.get(head)?;
    let (params, result) = decompose_arrow(head_ty);
    if params.is_empty() {
        return None;
    }

    let mut label = String::new();
    if let Some(name) = reference_name(module, &resolved, head) {
        label.push_str(&name);
        label.push_str(" : ");
    }
    let mut parameters = Vec::with_capacity(params.len());
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            label.push_str(" -> ");
        }
        let start = label.len() as u32;
        label.push_str(&render_param(p));
        parameters.push(ParamInfo { start, end: label.len() as u32 });
    }
    label.push_str(" -> ");
    label.push_str(&fai_types::render_canonical(result));

    let active_parameter = args_before.min(params.len() - 1) as u32;
    Some(SignatureHelp { schema_version: SCHEMA_VERSION, label, parameters, active_parameter })
}

/// A function-typed name reference whose span ends before `offset` with only
/// whitespace in between (`f |`, the first argument not yet typed).
fn head_with_trailing_space(module: &Module, src: &str, offset: u32) -> Option<ExprId> {
    module
        .exprs
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(e.kind, ExprKind::Var(_) | ExprKind::Field { .. })
                && whitespace_gap(src, e.span.end().raw(), offset)
        })
        .max_by_key(|(_, e)| e.span.end().raw())
        .map(|(i, _)| ExprId::from_index(i))
}

/// Splits a function type into its parameter types and result type.
fn decompose_arrow(ty: &Ty) -> (Vec<Ty>, &Ty) {
    let mut params = Vec::new();
    let mut cur = ty;
    while let Ty::Arrow(from, to) = cur {
        params.push((**from).clone());
        cur = to;
    }
    (params, cur)
}

/// Renders a parameter type, parenthesizing a function-typed parameter so the
/// signature reads unambiguously.
fn render_param(ty: &Ty) -> String {
    let rendered = fai_types::render_canonical(ty);
    if matches!(ty, Ty::Arrow(_, _)) { format!("({rendered})") } else { rendered }
}

// --- find-references at a position (LSP `textDocument/references`) ------------
//
// `fai query refs` addresses a target by name; an editor invokes find-references
// at a *byte offset*, which may sit on a use, a constructor pattern, a local's
// binding, or a definition's name. We first resolve what the offset refers to
// (a definition, a constructor, or a local), then collect every occurrence of
// that thing across the workspace.

/// What a cursor position refers to — the subject of find-references and rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefTarget {
    /// A top-level (possibly nested) definition.
    Def(DefId),
    /// A data constructor.
    Ctor(CtorRef),
    /// A local binding within a single file's body.
    Local(SourceFile, LocalId),
}

/// The patterns whose span contains `offset`, smallest (innermost) first — the
/// pattern-side analogue of [`exprs_containing`].
fn pats_containing(module: &Module, offset: u32) -> Vec<PatId> {
    let mut hits: Vec<(u32, PatId)> = module
        .pats
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let (start, end) = (p.span.start().raw(), p.span.end().raw());
            (start <= offset && offset < end).then(|| (end - start, PatId::from_index(i)))
        })
        .collect();
    hits.sort_by_key(|&(width, _)| width);
    hits.into_iter().map(|(_, id)| id).collect()
}

/// The source range of a definition's *name* within one of its items (the
/// binding or signature). The name is the last segment of the (possibly
/// nested-qualified) symbol; it is the first whole-word occurrence of that
/// identifier inside the item's text, which sits after the leading
/// `let`/`public`/`type` keyword and before the parameters/body. Returns `None`
/// when the text cannot be located (e.g. a recovered item).
fn def_name_range(text: &str, item_span: TextRange, name: Symbol) -> Option<TextRange> {
    let ident = name.as_str().rsplit('.').next().unwrap_or(name.as_str());
    if ident.is_empty() {
        return None;
    }
    let start = item_span.start().raw() as usize;
    let end = item_span.end().raw() as usize;
    let slice = text.get(start..end)?;
    let bytes = slice.as_bytes();
    let is_ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut from = 0usize;
    while let Some(rel) = slice[from..].find(ident) {
        let at = from + rel;
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after = at + ident.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            let abs = (start + at) as u32;
            return Some(TextRange::new(
                ByteOffset::new(abs),
                ByteOffset::new(abs + ident.len() as u32),
            ));
        }
        from = at + ident.len();
    }
    None
}

/// The name range of `name`'s definition (preferring the binding item, falling
/// back to the signature), for locating a declaration occurrence.
fn def_decl_range(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<TextRange> {
    let parsed = fai_syntax::parse(db, file);
    let text = file.text(db);
    let def = module_defs(db, file).get(name).copied()?;
    for item in [Some(def.binding), def.signature].into_iter().flatten() {
        let span = parsed.module.items[item.index()].span;
        if let Some(r) = def_name_range(text, span, name) {
            return Some(r);
        }
    }
    None
}

/// Whether `offset` falls on the *name* of a definition in `file` (its binding
/// or signature), returning that definition's qualified name.
fn def_name_at(db: &dyn Db, file: SourceFile, offset: u32) -> Option<Symbol> {
    let parsed = fai_syntax::parse(db, file);
    let text = file.text(db);
    for d in &module_defs(db, file).defs {
        for item in [Some(d.binding), d.signature].into_iter().flatten() {
            let span = parsed.module.items[item.index()].span;
            if let Some(r) = def_name_range(text, span, d.name)
                && r.start().raw() <= offset
                && offset < r.end().raw()
            {
                return Some(d.name);
            }
        }
    }
    None
}

/// Resolves what the reference at `offset` refers to — a use site, a constructor
/// or local pattern, or a definition's own name — together with the precise
/// source range of the *occurrence under the cursor* (the bare name, for
/// highlighting and rename).
fn target_at(db: &dyn Db, file: SourceFile, offset: u32) -> Option<(RefTarget, TextRange)> {
    let parsed = fai_syntax::parse(db, file);
    let resolved = resolve(db, file);
    let text = file.text(db);
    // A use site: the innermost referencing expression.
    for expr in exprs_containing(&parsed.module, offset) {
        let range = ref_expr_name_range(&parsed.module, expr);
        match resolved.get(expr) {
            Some(Res::Def(d)) => return Some((RefTarget::Def(d), range)),
            Some(Res::Ctor(c)) => return Some((RefTarget::Ctor(c), range)),
            Some(Res::Local(l)) => return Some((RefTarget::Local(file, l), range)),
            _ => {}
        }
    }
    // A pattern: a constructor head, or a local's binding occurrence.
    for pat in pats_containing(&parsed.module, offset) {
        if let Some(Res::Ctor(c)) = resolved.pat_res(pat) {
            return Some((RefTarget::Ctor(c), ctor_pat_name_range(&parsed.module, text, pat)));
        }
        if let Some(l) = resolved.local_of(pat) {
            return Some((RefTarget::Local(file, l), parsed.module.pat(pat).span));
        }
    }
    // A definition's own name (the declaration site).
    let name = def_name_at(db, file, offset)?;
    let range = def_decl_range(db, file, name)?;
    Some((RefTarget::Def(DefId::new(file.source(db), name)), range))
}

/// The precise source range of the *name* an expression reference occupies: the
/// whole span for a bare `Var`, or the trailing member segment for a qualified
/// `Field` reference (`A.inc` → just `inc`). Anchoring at the span's end is robust
/// to whitespace and to a module segment that repeats the member text.
fn ref_expr_name_range(module: &Module, expr: ExprId) -> TextRange {
    let span = module.expr(expr).span;
    if let ExprKind::Field { field, .. } = &module.expr(expr).kind {
        let flen = field.as_str().len() as u32;
        if flen > 0 && flen <= span.len() {
            return TextRange::new(ByteOffset::new(span.end().raw() - flen), span.end());
        }
    }
    span
}

/// The source range of a constructor pattern's *head* name (`Some x` → `Some`,
/// `Inner.MyCtor x` → `MyCtor`), excluding its arguments. Falls back to the whole
/// pattern span when the head cannot be located.
fn ctor_pat_name_range(module: &Module, text: &str, pat: PatId) -> TextRange {
    let span = module.pat(pat).span;
    if let PatKind::Constructor { name, .. } = &module.pat(pat).kind
        && let Some(r) = def_name_range(text, span, *name)
    {
        return r;
    }
    span
}

/// Pushes the resolved form of `range` (in `file`) into `out` as a keyed
/// location, for later deterministic sort + dedup.
fn push_location(
    db: &dyn Db,
    file: SourceFile,
    range: TextRange,
    resolver: &dyn SpanResolver,
    out: &mut Vec<(String, u32, Location)>,
) {
    if let Some(span) = SpanJson::resolve(Span::new(file.source(db), range), resolver) {
        out.push((span.file.clone(), span.byte_start, Location { span, preview: None }));
    }
}

/// Every occurrence of the symbol referenced at `offset` in `file`, across the
/// given files: its uses (expressions and patterns) and, when
/// `include_declaration`, its definition site. Powers LSP find-references.
#[must_use]
pub fn references_at(
    db: &dyn Db,
    files: &[SourceFile],
    file: SourceFile,
    offset: u32,
    resolver: &dyn SpanResolver,
    include_declaration: bool,
) -> Vec<Location> {
    let Some((target, _)) = target_at(db, file, offset) else {
        return vec![];
    };
    collect_references(db, files, target, resolver, include_declaration)
}

/// Every occurrence of a resolved [`RefTarget`] across `files` (uses in
/// expressions and patterns), plus its declaration when `include_declaration`.
/// Deterministically sorted and deduplicated by location. Shared by
/// find-references and rename.
fn collect_references(
    db: &dyn Db,
    files: &[SourceFile],
    target: RefTarget,
    resolver: &dyn SpanResolver,
    include_declaration: bool,
) -> Vec<Location> {
    let mut out: Vec<(String, u32, Location)> = Vec::new();
    match target {
        RefTarget::Def(def) => {
            for &f in files {
                let resolved = resolve(db, f);
                let parsed = fai_syntax::parse(db, f);
                for (expr, res) in &resolved.by_expr {
                    if *res == Res::Def(def) {
                        let range = ref_expr_name_range(&parsed.module, *expr);
                        push_location(db, f, range, resolver, &mut out);
                    }
                }
            }
            if include_declaration
                && let Some(decl_file) = db.source_file(def.file)
                && let Some(range) = def_decl_range(db, decl_file, def.name)
            {
                push_location(db, decl_file, range, resolver, &mut out);
            }
        }
        RefTarget::Ctor(ctor) => {
            for &f in files {
                let resolved = resolve(db, f);
                let parsed = fai_syntax::parse(db, f);
                for (expr, res) in &resolved.by_expr {
                    if *res == Res::Ctor(ctor) {
                        let range = ref_expr_name_range(&parsed.module, *expr);
                        push_location(db, f, range, resolver, &mut out);
                    }
                }
                for (pat, res) in &resolved.by_pat {
                    if *res == Res::Ctor(ctor) {
                        let range = ctor_pat_name_range(&parsed.module, f.text(db), *pat);
                        push_location(db, f, range, resolver, &mut out);
                    }
                }
            }
            if include_declaration && let Some(loc) = ctor_location(db, ctor, resolver) {
                out.push((loc.span.file.clone(), loc.span.byte_start, loc));
            }
        }
        RefTarget::Local(lfile, local) => {
            let resolved = resolve(db, lfile);
            let parsed = fai_syntax::parse(db, lfile);
            for (expr, res) in &resolved.by_expr {
                if *res == Res::Local(local) {
                    let range = ref_expr_name_range(&parsed.module, *expr);
                    push_location(db, lfile, range, resolver, &mut out);
                }
            }
            if include_declaration {
                for (pat, l) in &resolved.pat_locals {
                    if *l == local {
                        push_location(db, lfile, parsed.module.pat(*pat).span, resolver, &mut out);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out.into_iter().map(|(_, _, loc)| loc).collect()
}

// --- rename (LSP `textDocument/prepareRename` + `rename`) --------------------
//
// Rename is find-references with the declaration always included, rewriting each
// occurrence to the new name. Because the reference ranges are already the bare
// name (a qualified `A.inc` use yields just `inc`), the edits never disturb the
// surrounding module path or constructor arguments.

/// The renameable symbol under the cursor: the precise range to replace and the
/// current name (the editor's rename placeholder).
#[derive(Debug, Serialize)]
pub struct RenameTarget {
    /// The span of the name occurrence under the cursor.
    pub span: SpanJson,
    /// The current name (placeholder for the rename input).
    pub name: String,
}

/// Whether `name` is a plain identifier (a letter or `_` then letters/digits/`_`),
/// the only form rename can safely produce.
fn is_plain_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Whether a target may be renamed: its definition must live in user code (the
/// standard library is read-only), and a local always may.
fn target_is_renameable(db: &dyn Db, target: RefTarget) -> bool {
    let defining = match target {
        RefTarget::Def(d) => d.file,
        RefTarget::Ctor(c) => c.file,
        RefTarget::Local(..) => return true,
    };
    db.source_file(defining).is_some_and(|f| !fai_db::is_std_path(f.path(db)))
}

/// Whether `new_name` is a legal replacement for `target`: a plain identifier in
/// the same casing namespace (a constructor stays upper-case; a value or local
/// stays lower-case), so the rename cannot move a symbol between namespaces.
fn valid_new_name(target: RefTarget, new_name: &str) -> bool {
    if !is_plain_ident(new_name) {
        return false;
    }
    let upper = new_name.as_bytes()[0].is_ascii_uppercase();
    match target {
        RefTarget::Ctor(_) => upper,
        RefTarget::Def(_) | RefTarget::Local(..) => !upper,
    }
}

/// The renameable symbol at `offset`, or `None` when the cursor is not on a
/// user-defined name (a builtin, a standard-library symbol, or no symbol at all).
/// Powers LSP `textDocument/prepareRename`.
#[must_use]
pub fn prepare_rename_at(
    db: &dyn Db,
    file: SourceFile,
    offset: u32,
    resolver: &dyn SpanResolver,
) -> Option<RenameTarget> {
    let (target, range) = target_at(db, file, offset)?;
    if !target_is_renameable(db, target) {
        return None;
    }
    let name = file.text(db).get(range.start().raw() as usize..range.end().raw() as usize)?;
    let span = SpanJson::resolve(Span::new(file.source(db), range), resolver)?;
    Some(RenameTarget { span, name: name.to_owned() })
}

/// The edits that rename the symbol at `offset` to `new_name` across `files`:
/// one replacement per occurrence (uses and the declaration). Returns `None`
/// when the target is not renameable or `new_name` is not a valid replacement.
/// Powers LSP `textDocument/rename`.
#[must_use]
pub fn rename_at(
    db: &dyn Db,
    files: &[SourceFile],
    file: SourceFile,
    offset: u32,
    new_name: &str,
    resolver: &dyn SpanResolver,
) -> Option<Vec<Location>> {
    let (target, _) = target_at(db, file, offset)?;
    if !target_is_renameable(db, target) || !valid_new_name(target, new_name) {
        return None;
    }
    Some(collect_references(db, files, target, resolver, true))
}

/// `fai query refs`.
#[derive(Debug, Serialize)]
pub struct RefsResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub references: Vec<Location>,
    pub truncated: bool,
}

/// Finds all references to a target across the given files (on-demand reverse
/// lookup over each file's cached resolution).
#[must_use]
pub fn refs(
    db: &dyn Db,
    files: &[SourceFile],
    target: &str,
    resolver: &dyn SpanResolver,
    opts: ListOpts,
) -> RefsResult {
    let Some(t) = resolve_target(db, target) else {
        return RefsResult {
            schema_version: SCHEMA_VERSION,
            target: None,
            references: vec![],
            truncated: false,
        };
    };
    let target_def = DefId::new(t.file.source(db), t.name);
    let mut locations: Vec<(String, u32, Location)> = Vec::new();
    for &file in files {
        let resolved = resolve(db, file);
        let parsed = fai_syntax::parse(db, file);
        for (expr, res) in &resolved.by_expr {
            if *res == fai_resolve::Res::Def(target_def) {
                let span = parsed.module.expr(*expr).span;
                if let Some(sj) = SpanJson::resolve(Span::new(file.source(db), span), resolver) {
                    let key = (sj.file.clone(), sj.byte_start);
                    locations.push((key.0, key.1, Location { span: sj, preview: None }));
                }
            }
        }
    }
    locations.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    let refs_only: Vec<Location> = locations.into_iter().map(|(_, _, l)| l).collect();
    let (references, truncated) = truncate(refs_only, opts);
    RefsResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        references,
        truncated,
    }
}

/// `fai query dependents`.
#[derive(Debug, Serialize)]
pub struct DependentsResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub dependents: Vec<SymbolRef>,
    pub transitive: bool,
    pub truncated: bool,
}

/// The reverse dependency graph over `files`: each definition mapped to the
/// definitions that reference it (a body-edit assembled, deterministic-free map).
fn reverse_dep_graph(db: &dyn Db, files: &[SourceFile]) -> FxHashMap<DefId, FxHashSet<DefId>> {
    let mut rev: FxHashMap<DefId, FxHashSet<DefId>> = FxHashMap::default();
    for &file in files {
        let resolved = resolve(db, file);
        for (owner, edges) in &resolved.deps_by_def {
            for &callee in edges {
                rev.entry(callee).or_default().insert(*owner);
            }
        }
    }
    rev
}

/// Finds the definitions that reference the target (reverse dependency edges).
/// With `transitive`, follows the reverse graph to its transitive closure.
#[must_use]
pub fn dependents(
    db: &dyn Db,
    files: &[SourceFile],
    target: &str,
    resolver: &dyn SpanResolver,
    transitive: bool,
    opts: ListOpts,
) -> DependentsResult {
    let Some(t) = resolve_target(db, target) else {
        return DependentsResult {
            schema_version: SCHEMA_VERSION,
            target: None,
            dependents: vec![],
            transitive,
            truncated: false,
        };
    };
    let target_def = DefId::new(t.file.source(db), t.name);
    let rev = reverse_dep_graph(db, files);

    // Collect the dependent definitions: direct callers, or the transitive
    // closure of the reverse graph (breadth-first, excluding the target itself).
    let mut found: Vec<DefId> = Vec::new();
    let mut seen: FxHashSet<DefId> = FxHashSet::default();
    let mut stack: Vec<DefId> = rev.get(&target_def).into_iter().flatten().copied().collect();
    while let Some(d) = stack.pop() {
        if d == target_def || !seen.insert(d) {
            continue;
        }
        found.push(d);
        if transitive {
            stack.extend(rev.get(&d).into_iter().flatten().copied());
        }
    }

    let mut deps: Vec<SymbolRef> = found
        .into_iter()
        .filter_map(|d| db.source_file(d.file).and_then(|f| symbol_ref(db, f, d.name, resolver)))
        .collect();
    deps.sort_by(|a, b| a.path.cmp(&b.path));
    deps.dedup_by(|a, b| a.path == b.path);
    let (dependents, truncated) = truncate(deps, opts);
    DependentsResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        dependents,
        transitive,
        truncated,
    }
}

/// One edge of a call hierarchy: a related definition and the sites that realize
/// the edge (CLI.md `callers`/`callees`).
#[derive(Debug, Serialize)]
pub struct CallEdge {
    pub symbol: SymbolRef,
    pub sites: Vec<Location>,
}

/// `fai query callers` / `callees`.
#[derive(Debug, Serialize)]
pub struct CallHierarchyResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub edges: Vec<CallEdge>,
}

fn empty_hierarchy() -> CallHierarchyResult {
    CallHierarchyResult { schema_version: SCHEMA_VERSION, target: None, edges: vec![] }
}

/// The body expression of a (value) definition with qualified `name` in `file`.
fn def_body(module: &Module, defs: &fai_resolve::ModuleDefs, name: Symbol) -> Option<ExprId> {
    let binding = defs.get(name)?.binding;
    match &module.items[binding.index()].kind {
        ItemKind::Binding { body, .. } => Some(*body),
        _ => None,
    }
}

/// Collects every referencing expression in `expr`'s subtree (those resolution
/// recorded), with what it resolved to — the per-body reference sites.
fn collect_body_refs(
    module: &Module,
    resolved: &ResolvedBodies,
    expr: ExprId,
    out: &mut Vec<(ExprId, Res)>,
) {
    if let Some(res) = resolved.get(expr) {
        out.push((expr, res));
    }
    match &module.expr(expr).kind {
        ExprKind::App { func, arg } => {
            collect_body_refs(module, resolved, *func, out);
            collect_body_refs(module, resolved, *arg, out);
        }
        ExprKind::Infix { op, lhs, rhs } => {
            collect_body_refs(module, resolved, *op, out);
            collect_body_refs(module, resolved, *lhs, out);
            collect_body_refs(module, resolved, *rhs, out);
        }
        ExprKind::Prefix { op, operand } => {
            collect_body_refs(module, resolved, *op, out);
            collect_body_refs(module, resolved, *operand, out);
        }
        ExprKind::If { cond, then_branch, else_branch } => {
            collect_body_refs(module, resolved, *cond, out);
            collect_body_refs(module, resolved, *then_branch, out);
            collect_body_refs(module, resolved, *else_branch, out);
        }
        ExprKind::Lambda { body, .. } => collect_body_refs(module, resolved, *body, out),
        ExprKind::Match { scrutinee, arms } => {
            collect_body_refs(module, resolved, *scrutinee, out);
            for arm in arms {
                collect_body_refs(module, resolved, arm.body, out);
            }
        }
        ExprKind::Block { stmts, tail } => {
            for stmt in stmts {
                collect_body_refs(module, resolved, stmt.value, out);
            }
            collect_body_refs(module, resolved, *tail, out);
        }
        ExprKind::Field { base, .. } => collect_body_refs(module, resolved, *base, out),
        ExprKind::Record(fields) => {
            for f in fields {
                collect_body_refs(module, resolved, f.value, out);
            }
        }
        ExprKind::RecordUpdate { base, fields } => {
            collect_body_refs(module, resolved, *base, out);
            for f in fields {
                collect_body_refs(module, resolved, f.value, out);
            }
        }
        ExprKind::Instance { methods, .. } => {
            for m in methods {
                collect_body_refs(module, resolved, m.body, out);
            }
        }
        ExprKind::Paren(inner) => collect_body_refs(module, resolved, *inner, out),
        ExprKind::Tuple(xs) | ExprKind::List(xs) => {
            for &x in xs {
                collect_body_refs(module, resolved, x, out);
            }
        }
        ExprKind::Var(_)
        | ExprKind::Unit
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Error => {}
    }
}

/// Groups `(callee, site)` pairs into sorted call edges.
fn build_edges(
    db: &dyn Db,
    by_callee: FxHashMap<DefId, Vec<Location>>,
    resolver: &dyn SpanResolver,
) -> Vec<CallEdge> {
    let mut edges: Vec<CallEdge> = by_callee
        .into_iter()
        .filter_map(|(def, mut sites)| {
            let file = db.source_file(def.file)?;
            let symbol = symbol_ref(db, file, def.name, resolver)?;
            sites.sort_by(|a, b| {
                (a.span.file.as_str(), a.span.byte_start)
                    .cmp(&(b.span.file.as_str(), b.span.byte_start))
            });
            Some(CallEdge { symbol, sites })
        })
        .collect();
    edges.sort_by(|a, b| a.symbol.path.cmp(&b.symbol.path));
    edges
}

/// `fai query callees`: the definitions `target`'s body references, with sites.
#[must_use]
pub fn callees(db: &dyn Db, target: &str, resolver: &dyn SpanResolver) -> CallHierarchyResult {
    let Some(t) = resolve_target(db, target) else {
        return empty_hierarchy();
    };
    let parsed = fai_syntax::parse(db, t.file);
    let resolved = resolve(db, t.file);
    let defs = module_defs(db, t.file);
    let mut by_callee: FxHashMap<DefId, Vec<Location>> = FxHashMap::default();
    if let Some(body) = def_body(&parsed.module, &defs, t.name) {
        let mut refs = Vec::new();
        collect_body_refs(&parsed.module, &resolved, body, &mut refs);
        for (expr, res) in refs {
            if let Res::Def(callee) = res {
                let span = parsed.module.expr(expr).span;
                if let Some(sj) = SpanJson::resolve(Span::new(t.file.source(db), span), resolver) {
                    by_callee.entry(callee).or_default().push(Location { span: sj, preview: None });
                }
            }
        }
    }
    CallHierarchyResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        edges: build_edges(db, by_callee, resolver),
    }
}

/// `fai query callers`: the definitions whose body references `target`, with sites.
#[must_use]
pub fn callers(
    db: &dyn Db,
    files: &[SourceFile],
    target: &str,
    resolver: &dyn SpanResolver,
) -> CallHierarchyResult {
    let Some(t) = resolve_target(db, target) else {
        return empty_hierarchy();
    };
    let target_def = DefId::new(t.file.source(db), t.name);
    let rev = reverse_dep_graph(db, files);
    let mut by_caller: FxHashMap<DefId, Vec<Location>> = FxHashMap::default();
    for &caller in rev.get(&target_def).into_iter().flatten() {
        let Some(file) = db.source_file(caller.file) else { continue };
        let parsed = fai_syntax::parse(db, file);
        let resolved = resolve(db, file);
        let defs = module_defs(db, file);
        let Some(body) = def_body(&parsed.module, &defs, caller.name) else { continue };
        let mut refs = Vec::new();
        collect_body_refs(&parsed.module, &resolved, body, &mut refs);
        for (expr, res) in refs {
            if res == Res::Def(target_def) {
                let span = parsed.module.expr(expr).span;
                if let Some(sj) = SpanJson::resolve(Span::new(file.source(db), span), resolver) {
                    by_caller.entry(caller).or_default().push(Location { span: sj, preview: None });
                }
            }
        }
    }
    CallHierarchyResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        edges: build_edges(db, by_caller, resolver),
    }
}

/// `fai query caps`.
#[derive(Debug, Serialize)]
pub struct CapsResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    pub capabilities: Vec<Capability>,
}

/// Collects the capabilities a signature requests directly: a parameter that is
/// an interface, or a record parameter's interface-typed fields. Each is
/// `(name, interface)`.
fn param_caps(ty: &fai_types::Ty, out: &mut Vec<(String, String)>) {
    let mut cur = ty;
    while let fai_types::Ty::Arrow(from, to) = cur {
        match from.as_ref() {
            fai_types::Ty::Interface(iref) => {
                out.push((iref.name.as_str().to_owned(), iref.name.as_str().to_owned()));
            }
            fai_types::Ty::Record(row) => {
                for (label, fty) in &row.fields {
                    if let fai_types::Ty::Interface(iref) = fty {
                        out.push((label.as_str().to_owned(), iref.name.as_str().to_owned()));
                    }
                }
            }
            _ => {}
        }
        cur = to;
    }
}

/// `fai query caps`: the capability footprint of a function — the capabilities it
/// requests directly (its signature parameters) plus those reached through its
/// callees (the call graph).
#[must_use]
pub fn caps(
    db: &dyn Db,
    files: &[SourceFile],
    target: &str,
    resolver: &dyn SpanResolver,
) -> CapsResult {
    let _ = files;
    let Some(t) = resolve_target(db, target) else {
        return CapsResult { schema_version: SCHEMA_VERSION, target: None, capabilities: vec![] };
    };
    let target_def = DefId::new(t.file.source(db), t.name);

    // Keyed by (interface, name) for determinism and dedup; first origin wins
    // (a directly-requested capability is never downgraded to transitive).
    let mut found: std::collections::BTreeMap<(String, String), Capability> =
        std::collections::BTreeMap::new();

    let mut direct = Vec::new();
    param_caps(&fai_types::def_type(db, t.file, t.name).ty, &mut direct);
    for (name, ty) in direct {
        found.entry((ty.clone(), name.clone())).or_insert(Capability {
            name,
            ty,
            origin: CapOrigin::Parameter,
            via: vec![],
        });
    }

    // Transitive: walk the forward call graph and add any callee's directly
    // requested capabilities not already requested here.
    let mut seen: FxHashSet<DefId> = FxHashSet::default();
    seen.insert(target_def);
    let mut stack: Vec<DefId> = resolve(db, t.file).deps_of(target_def).to_vec();
    while let Some(d) = stack.pop() {
        if !seen.insert(d) {
            continue;
        }
        let Some(file) = db.source_file(d.file) else { continue };
        let mut callee_caps = Vec::new();
        param_caps(&fai_types::def_type(db, file, d.name).ty, &mut callee_caps);
        for (name, ty) in callee_caps {
            found
                .entry((ty.clone(), name.clone()))
                .and_modify(|c| {
                    if c.origin == CapOrigin::Transitive
                        && !c.via.contains(&d.name.as_str().to_owned())
                    {
                        c.via.push(d.name.as_str().to_owned());
                    }
                })
                .or_insert(Capability {
                    name,
                    ty,
                    origin: CapOrigin::Transitive,
                    via: vec![d.name.as_str().to_owned()],
                });
        }
        stack.extend(resolve(db, file).deps_of(d).iter().copied());
    }

    CapsResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        capabilities: found.into_values().collect(),
    }
}

/// One hit of a type search.
#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub symbol: SymbolRef,
    #[serde(rename = "type")]
    pub ty: TypeRepr,
    pub score: f64,
}

/// `fai query search`.
#[derive(Debug, Serialize)]
pub struct SearchResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub query: String,
    pub results: Vec<SearchHit>,
    pub truncated: bool,
}

/// A normalized type shape, for type-pattern matching (CLI.md `search`).
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    /// A type variable, numbered per side in first-seen order.
    Var(usize),
    /// A constructor / type / interface name.
    Name(String),
    /// Type application `f a`.
    App(Box<Shape>, Box<Shape>),
    /// A function type.
    Arrow(Box<Shape>, Box<Shape>),
    /// A tuple.
    Tuple(Vec<Shape>),
    /// A record (fields sorted by label) and whether its row is open.
    Record(Vec<(String, Shape)>, bool),
    /// Unit.
    Unit,
    /// Anything else (no useful structure).
    Other,
}

/// Builds a [`Shape`] from a written type (the search pattern's AST).
fn shape_from_ast(module: &Module, ty: TypeId, vars: &mut FxHashMap<Symbol, usize>) -> Shape {
    match &module.ty(ty).kind {
        TypeKind::Var(name) => {
            let n = vars.len();
            Shape::Var(*vars.entry(*name).or_insert(n))
        }
        TypeKind::Con(name) => Shape::Name(name.as_str().to_owned()),
        TypeKind::App { func, arg } => Shape::App(
            Box::new(shape_from_ast(module, *func, vars)),
            Box::new(shape_from_ast(module, *arg, vars)),
        ),
        TypeKind::Arrow { from, to } => Shape::Arrow(
            Box::new(shape_from_ast(module, *from, vars)),
            Box::new(shape_from_ast(module, *to, vars)),
        ),
        TypeKind::Tuple(elems) => {
            Shape::Tuple(elems.iter().map(|&e| shape_from_ast(module, e, vars)).collect())
        }
        TypeKind::Record { fields, tail } => {
            let mut fs: Vec<(String, Shape)> = fields
                .iter()
                .map(|f| (f.name.as_str().to_owned(), shape_from_ast(module, f.ty, vars)))
                .collect();
            fs.sort_by(|a, b| a.0.cmp(&b.0));
            Shape::Record(fs, !matches!(tail, RowTail::Closed))
        }
        TypeKind::Unit => Shape::Unit,
        TypeKind::Paren(inner) => shape_from_ast(module, *inner, vars),
        TypeKind::Error => Shape::Other,
    }
}

/// Builds a [`Shape`] from a reified candidate type.
fn shape_from_ty(ty: &fai_types::Ty, vars: &mut FxHashMap<u32, usize>) -> Shape {
    use fai_types::Ty;
    match ty {
        Ty::Var(v) => {
            let n = vars.len();
            Shape::Var(*vars.entry(v.0).or_insert(n))
        }
        Ty::Con(c) => Shape::Name(c.name().to_owned()),
        Ty::Adt(a) => Shape::Name(a.name.as_str().to_owned()),
        Ty::Interface(i) => Shape::Name(i.name.as_str().to_owned()),
        Ty::App(f, a) => {
            Shape::App(Box::new(shape_from_ty(f, vars)), Box::new(shape_from_ty(a, vars)))
        }
        Ty::Arrow(f, a) => {
            Shape::Arrow(Box::new(shape_from_ty(f, vars)), Box::new(shape_from_ty(a, vars)))
        }
        Ty::Tuple(elems) => Shape::Tuple(elems.iter().map(|e| shape_from_ty(e, vars)).collect()),
        Ty::Record(row) => {
            let mut fs: Vec<(String, Shape)> = row
                .fields
                .iter()
                .map(|(l, t)| (l.as_str().to_owned(), shape_from_ty(t, vars)))
                .collect();
            fs.sort_by(|a, b| a.0.cmp(&b.0));
            Shape::Record(fs, matches!(row.tail, fai_types::RowEnd::Open(_)))
        }
        Ty::Unit => Shape::Unit,
        Ty::Error => Shape::Other,
    }
}

/// The last `.`-separated segment of a (possibly qualified) name.
fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Matches a query shape against a candidate shape. A query variable is a hole
/// that binds (consistently) to a candidate subtree; an open query record allows
/// extra candidate fields (row polymorphism). Returns `None` on no match, else
/// whether the match is *exact* (alpha-equivalent: holes bound only to variables,
/// names identical, openness equal).
fn match_shape(pat: &Shape, cand: &Shape, subst: &mut FxHashMap<usize, Shape>) -> Option<bool> {
    match (pat, cand) {
        (Shape::Var(q), _) => {
            if let Some(bound) = subst.get(q) {
                return (bound == cand).then_some(matches!(cand, Shape::Var(_)));
            }
            subst.insert(*q, cand.clone());
            Some(matches!(cand, Shape::Var(_)))
        }
        (Shape::Name(a), Shape::Name(b)) => {
            if a == b {
                Some(true)
            } else if last_segment(a) == last_segment(b) {
                Some(false)
            } else {
                None
            }
        }
        (Shape::App(f1, a1), Shape::App(f2, a2)) | (Shape::Arrow(f1, a1), Shape::Arrow(f2, a2)) => {
            let e1 = match_shape(f1, f2, subst)?;
            let e2 = match_shape(a1, a2, subst)?;
            Some(e1 && e2)
        }
        (Shape::Tuple(xs), Shape::Tuple(ys)) if xs.len() == ys.len() => {
            let mut exact = true;
            for (x, y) in xs.iter().zip(ys) {
                exact &= match_shape(x, y, subst)?;
            }
            Some(exact)
        }
        (Shape::Record(pf, popen), Shape::Record(cf, copen)) => {
            let mut exact = popen == copen && pf.len() == cf.len();
            for (label, psh) in pf {
                let csh = cf.iter().find(|(l, _)| l == label).map(|(_, s)| s)?;
                exact &= match_shape(psh, csh, subst)?;
            }
            if !popen && pf.len() != cf.len() {
                return None; // a closed query record must name exactly these fields
            }
            Some(exact)
        }
        (Shape::Unit, Shape::Unit) => Some(true),
        _ => None,
    }
}

/// `fai query search`: find definitions whose type matches a type pattern
/// (unification up to variable renaming and row polymorphism), ranked by score.
#[must_use]
pub fn search(
    db: &dyn Db,
    files: &[SourceFile],
    pattern: &str,
    resolver: &dyn SpanResolver,
    opts: ListOpts,
) -> SearchResult {
    let empty = || SearchResult {
        schema_version: SCHEMA_VERSION,
        query: pattern.to_owned(),
        results: vec![],
        truncated: false,
    };
    // Parse the pattern by wrapping it in a synthetic signature.
    let synthetic = format!("module Q\n\nq : {pattern}\n");
    let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), &synthetic);
    if parsed.diagnostics.iter().any(|d| d.severity == fai_diagnostics::Severity::Error) {
        return empty();
    }
    let Some(pat_ty) = parsed.module.items.iter().find_map(|it| match &it.kind {
        ItemKind::Signature { ty, .. } => Some(*ty),
        _ => None,
    }) else {
        return empty();
    };
    let mut pvars = FxHashMap::default();
    let pat_shape = shape_from_ast(&parsed.module, pat_ty, &mut pvars);

    let mut hits: Vec<(f64, String, SearchHit)> = Vec::new();
    for &file in files {
        for d in &module_defs(db, file).defs {
            let scheme = fai_types::def_type(db, file, d.name);
            let mut cvars = FxHashMap::default();
            let cand_shape = shape_from_ty(&scheme.ty, &mut cvars);
            let mut subst = FxHashMap::default();
            if let Some(exact) = match_shape(&pat_shape, &cand_shape, &mut subst) {
                let score = if exact { 1.0 } else { 0.6 };
                if let Some(symbol) = symbol_ref(db, file, d.name, resolver) {
                    let key = symbol.path.clone();
                    hits.push((
                        score,
                        key,
                        SearchHit {
                            symbol,
                            ty: TypeRepr { display: fai_types::render_scheme(&scheme) },
                            score,
                        },
                    ));
                }
            }
        }
    }
    // Best score first, then by path for determinism.
    hits.sort_by(|a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1))
    });
    let results: Vec<SearchHit> = hits.into_iter().map(|(_, _, h)| h).collect();
    let (results, truncated) = truncate(results, opts);
    SearchResult { schema_version: SCHEMA_VERSION, query: pattern.to_owned(), results, truncated }
}

/// `fai query outline` / `api` node.
#[derive(Debug, Serialize)]
pub struct OutlineNode {
    pub symbol: SymbolRef,
    pub children: Vec<OutlineNode>,
}

/// `fai query outline`.
#[derive(Debug, Serialize)]
pub struct OutlineResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub outline: Vec<OutlineNode>,
}

/// The outline (top-level symbols) of a file or module.
#[must_use]
pub fn outline(
    db: &dyn Db,
    target: &str,
    files: &[SourceFile],
    resolver: &dyn SpanResolver,
) -> OutlineResult {
    let file =
        files.iter().copied().find(|&f| module_label(db, f) == target || f.path(db) == target);
    match file {
        Some(file) => document_symbols(db, file, resolver),
        None => OutlineResult { schema_version: SCHEMA_VERSION, outline: vec![] },
    }
}

/// The nested symbol outline of one file, keyed by [`SourceFile`] rather than a
/// target string. Powers LSP `textDocument/documentSymbol` (and is the core of
/// [`outline`]).
#[must_use]
pub fn document_symbols(
    db: &dyn Db,
    file: SourceFile,
    resolver: &dyn SpanResolver,
) -> OutlineResult {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let mut scope: Vec<Symbol> = Vec::new();
    let nodes = outline_items(db, file, module, &mut scope, &module.roots, resolver);
    OutlineResult { schema_version: SCHEMA_VERSION, outline: nodes }
}

/// Builds the outline nodes of one module scope, nesting child items under each
/// nested module (sorted by name within each level).
fn outline_items(
    db: &dyn Db,
    file: SourceFile,
    module: &fai_syntax::ast::Module,
    scope: &mut Vec<Symbol>,
    items: &[fai_syntax::ast::ItemId],
    resolver: &dyn SpanResolver,
) -> Vec<OutlineNode> {
    use fai_syntax::ast::ItemKind;
    let mut nodes = Vec::new();
    for &id in items {
        match &module.items[id.index()].kind {
            ItemKind::Binding { name, .. } => {
                let qual = fai_resolve::qualify(scope, *name);
                if let Some(symbol) = symbol_ref(db, file, qual, resolver) {
                    nodes.push(OutlineNode { symbol, children: vec![] });
                }
            }
            ItemKind::Module { name, body } => {
                let span = module.items[id.index()].span;
                scope.push(*name);
                let children = outline_items(db, file, module, scope, body, resolver);
                scope.pop();
                if let Some(symbol) = module_symbol_ref(db, file, scope, *name, span, resolver) {
                    nodes.push(OutlineNode { symbol, children });
                }
            }
            _ => {}
        }
    }
    nodes.sort_by(|a, b| a.symbol.name.cmp(&b.symbol.name));
    nodes
}

/// A `SymbolRef` for a nested module declaration (kind `Module`).
fn module_symbol_ref(
    db: &dyn Db,
    file: SourceFile,
    scope: &[Symbol],
    name: Symbol,
    span: fai_span::TextRange,
    resolver: &dyn SpanResolver,
) -> Option<SymbolRef> {
    let module_name = module_label(db, file);
    let qual = fai_resolve::qualify(scope, name);
    Some(SymbolRef {
        path: format!("{module_name}.{qual}"),
        name: name.as_str().to_owned(),
        kind: SymbolKind::Module,
        module: module_name,
        visibility: Visibility::Private,
        signature: None,
        span: SpanJson::resolve(Span::new(file.source(db), span), resolver)?,
    })
}

/// One export in an `api` result.
#[derive(Debug, Serialize)]
pub struct ApiExport {
    pub symbol: SymbolRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<Doc>,
    pub contracts: Vec<Contract>,
}

/// `fai query api`.
#[derive(Debug, Serialize)]
pub struct ApiResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub module: String,
    pub exports: Vec<ApiExport>,
}

/// The public interface of a module.
#[must_use]
pub fn api(
    db: &dyn Db,
    module: &str,
    files: &[SourceFile],
    resolver: &dyn SpanResolver,
) -> ApiResult {
    let file = files.iter().copied().find(|&f| module_label(db, f) == module);
    let mut exports = Vec::new();
    if let Some(file) = file {
        let defs = module_defs(db, file);
        let mut by_subject = contracts_by_subject(db, file, resolver);
        for d in &defs.defs {
            if d.visibility != AstVis::Public {
                continue;
            }
            if let Some(symbol) = symbol_ref(db, file, d.name, resolver) {
                let contracts = by_subject.remove(&d.name).unwrap_or_default();
                exports.push(ApiExport { symbol, doc: doc_for(db, file, d.name), contracts });
            }
        }
        exports.sort_by(|a, b| a.symbol.name.cmp(&b.symbol.name));
    }
    ApiResult { schema_version: SCHEMA_VERSION, module: module.to_owned(), exports }
}

/// Collects a file's contracts, grouped by the top-level binding they describe
/// (the nearest preceding one). Powers the contract lists in `api`/`docs`.
fn contracts_by_subject(
    db: &dyn Db,
    file: SourceFile,
    resolver: &dyn SpanResolver,
) -> FxHashMap<Symbol, Vec<Contract>> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let text = file.text(db);
    let mut by_subject: FxHashMap<Symbol, Vec<Contract>> = FxHashMap::default();
    let mut subject: Option<Symbol> = None;
    for item in &module.items {
        let (kind, binders) = match &item.kind {
            ItemKind::Binding { name, .. } => {
                subject = Some(*name);
                continue;
            }
            ItemKind::Example { .. } => ("example".to_owned(), Vec::new()),
            ItemKind::Forall { binders, .. } => (
                "forall".to_owned(),
                binders
                    .iter()
                    .filter_map(|&p| match module.pat(p).kind {
                        PatKind::Var(name) => Some(name.as_str().to_owned()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => continue,
        };
        let Some(subject) = subject else { continue };
        let start = item.span.start().raw() as usize;
        let end = item.span.end().raw() as usize;
        let source = text.get(start..end).unwrap_or("").to_owned();
        let Some(span) = SpanJson::resolve(Span::new(file.source(db), item.span), resolver) else {
            continue;
        };
        by_subject.entry(subject).or_default().push(Contract { kind, binders, source, span });
    }
    by_subject
}

/// `fai query docs`.
#[derive(Debug, Serialize)]
pub struct DocsResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<SymbolRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<Doc>,
    pub contracts: Vec<Contract>,
}

/// Docs and attached contracts for a target.
#[must_use]
pub fn docs(db: &dyn Db, target: &str, resolver: &dyn SpanResolver) -> DocsResult {
    let Some(t) = resolve_target(db, target) else {
        return DocsResult {
            schema_version: SCHEMA_VERSION,
            target: None,
            doc: None,
            contracts: vec![],
        };
    };
    let contracts = contracts_by_subject(db, t.file, resolver).remove(&t.name).unwrap_or_default();
    DocsResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        doc: doc_for(db, t.file, t.name),
        contracts,
    }
}

/// The `///` doc prose attached to a definition, if any.
///
/// Doc comments lead the definition; they are attached to the signature item
/// (preferred, since it appears first) or, failing that, the binding. The `///`
/// markers and one following space are stripped and the lines joined with
/// newlines.
pub(crate) fn doc_for(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<Doc> {
    let info = *module_defs(db, file).get(name)?;
    let parsed = fai_syntax::parse(db, file);
    let src = file.text(db);
    let line_index = LineIndex::new(src);
    let map = attach_comments(&parsed.module, &parsed.comments, &line_index);
    for item in [info.signature, Some(info.binding)].into_iter().flatten() {
        if let Some(doc) = item_doc(&map, &parsed.comments, NodeId::Item(item), src) {
            return Some(doc);
        }
    }
    None
}

/// Extracts the leading `///` doc comments of a node as joined markdown prose.
fn item_doc(
    map: &fai_syntax::CommentMap,
    comments: &[fai_syntax::Comment],
    node: NodeId,
    src: &str,
) -> Option<Doc> {
    let mut lines: Vec<String> = Vec::new();
    for &id in map.leading(node) {
        let comment = &comments[id];
        if comment.kind != CommentKind::Doc {
            continue;
        }
        let raw = src.get(comment.range.start().to_usize()..comment.range.end().to_usize())?;
        let body = raw.trim_end();
        let body = body.strip_prefix("///").unwrap_or(body);
        let body = body.strip_prefix(' ').unwrap_or(body);
        lines.push(body.to_owned());
    }
    (!lines.is_empty()).then(|| Doc { markdown: lines.join("\n") })
}
