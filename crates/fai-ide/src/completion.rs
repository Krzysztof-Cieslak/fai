//! Code completion at a byte offset (LSP `textDocument/completion`).
//!
//! The candidate set depends on the *context* immediately before the cursor,
//! determined lexically (so a half-typed buffer with a trailing `.` still works):
//!
//! - after `Module.` — that module's members (cross-file public exports, or the
//!   members of a nested module in the same file);
//! - after `value.` — the fields of the value's record type;
//! - otherwise (a bare identifier) — the names in scope: the locals visible at the
//!   cursor, this module's visible definitions, the visible constructors, and the
//!   auto-imported prelude values.
//!
//! Each item carries a kind and a rendered type. Filtering by the typed prefix is
//! left to the editor, so the whole context-appropriate set is returned.

use fai_db::{Db, SourceFile};
use fai_resolve::{
    ModuleName, module_defs, module_file, module_interface, prelude_exports, type_decls,
};
use fai_syntax::Symbol;
use fai_syntax::ast::{ExprId, ExprKind, ItemKind, Module, PatId, PatKind};
use fai_types::{
    Scheme, Ty, body_types, constructor_scheme, def_type, render_canonical, render_scheme,
};
use rustc_hash::FxHashSet;
use serde::{Deserialize, Serialize};

use crate::query::enclosing_def;
use crate::repr::SCHEMA_VERSION;

/// The kind of a completion candidate.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CompletionKind {
    /// A function binding (its type is an arrow).
    Function,
    /// A non-function value binding.
    Value,
    /// A data constructor.
    Constructor,
    /// A record field.
    Field,
    /// A module.
    Module,
}

/// A resolvable item's stable identity: the defining file and the definition's
/// qualified name. A serializable mirror of `fai_resolve::DefId`, carried through
/// the LSP `completionItem/resolve` round trip so the chosen item's `///` docs and
/// contracts can be fetched lazily without a cursor position. `file` is the raw
/// [`fai_span::SourceId`]; reconstruct with `db.source_file(SourceId::new(file))`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionData {
    /// The defining file's source id (raw index).
    pub file: u32,
    /// The definition's fully-qualified name.
    pub name: String,
}

/// One completion candidate.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompletionItem {
    /// The text to insert.
    pub label: String,
    /// The candidate's kind.
    pub kind: CompletionKind,
    /// A rendered type (or other detail), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The definition's identity, when it has one (value/function/constructor
    /// items); `None` for record fields and locals, which have no addressable
    /// definition. Lets `completionItem/resolve` fetch docs lazily.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<CompletionData>,
}

/// `textDocument/completion` result.
#[derive(Debug, Serialize)]
pub struct CompletionResult {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    /// The candidates, deduplicated and sorted by label.
    pub items: Vec<CompletionItem>,
}

/// The completions available at `offset` in `file`.
#[must_use]
pub fn completions_at(db: &dyn Db, file: SourceFile, offset: u32) -> CompletionResult {
    let mut items = match member_context(db, file, offset) {
        Some(MemberContext::Module(path)) => module_members(db, file, &path),
        Some(MemberContext::Record(base)) => record_fields(db, file, offset, base),
        None => bare_candidates(db, file, offset),
    };
    items.sort_by(|a, b| a.label.cmp(&b.label).then(a.kind.cmp(&b.kind)));
    items.dedup_by(|a, b| a.label == b.label && a.kind == b.kind);
    CompletionResult { schema_version: SCHEMA_VERSION, items }
}

// `CompletionKind` needs a total order only for the dedup/sort above.
impl PartialOrd for CompletionKind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CompletionKind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

/// The member-access context just before `offset`, if any.
enum MemberContext {
    /// `Module.` / `Outer.Inner.` — a (dotted, all-upper-case) module path.
    Module(String),
    /// `value.` — the base expression whose record fields to offer.
    Record(ExprId),
}

fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Detects whether the cursor sits after a `path.` and, if so, classifies the
/// base as a module path (all segments upper-case) or a value field access.
fn member_context(db: &dyn Db, file: SourceFile, offset: u32) -> Option<MemberContext> {
    let text = file.text(db);
    let bytes = text.as_bytes();
    let mut i = (offset as usize).min(bytes.len());
    // Skip the partial identifier currently being typed.
    while i > 0 && is_ident_byte(bytes[i - 1]) {
        i -= 1;
    }
    // A member access requires a `.` immediately before the partial.
    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    let dot = i - 1;
    // Walk back over the dotted base path (identifier segments joined by dots).
    let mut start = dot;
    loop {
        let seg_end = start;
        while start > 0 && is_ident_byte(bytes[start - 1]) {
            start -= 1;
        }
        if start == seg_end {
            break; // not a segment (e.g. a method-chain on `)` — unsupported)
        }
        if start > 0 && bytes[start - 1] == b'.' {
            start -= 1;
            continue;
        }
        break;
    }
    let path = text.get(start..dot)?;
    if path.is_empty() {
        return None;
    }
    let all_upper =
        path.split('.').all(|s| s.as_bytes().first().is_some_and(u8::is_ascii_uppercase));
    if all_upper {
        return Some(MemberContext::Module(path.to_owned()));
    }
    let parsed = fai_syntax::parse(db, file);
    let base = expr_ending_at(&parsed.module, dot as u32)?;
    Some(MemberContext::Record(base))
}

/// The widest expression whose span ends exactly at `end` (the base of a field
/// access, which ends at the `.`).
fn expr_ending_at(module: &Module, end: u32) -> Option<ExprId> {
    module
        .exprs
        .iter()
        .enumerate()
        .filter(|(_, e)| e.span.end().raw() == end)
        .max_by_key(|(_, e)| e.span.len())
        .map(|(i, _)| ExprId::from_index(i))
}

/// The last `.`-separated segment of a (possibly qualified) name.
fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// A value/function candidate from a binding's (qualified) name and scheme. The
/// displayed label is the name's last segment; the resolve payload keeps the full
/// qualified name and defining file so its docs can be fetched lazily.
fn value_item(db: &dyn Db, file: SourceFile, name: Symbol, scheme: &Scheme) -> CompletionItem {
    let kind = if matches!(scheme.ty, Ty::Arrow(..)) {
        CompletionKind::Function
    } else {
        CompletionKind::Value
    };
    CompletionItem {
        label: last_segment(name.as_str()).to_owned(),
        kind,
        detail: Some(render_scheme(scheme)),
        data: Some(completion_data(db, file, name)),
    }
}

/// The resolve payload identifying a definition: its file's source id and its
/// fully-qualified name.
fn completion_data(db: &dyn Db, file: SourceFile, name: Symbol) -> CompletionData {
    CompletionData { file: file.source(db).raw(), name: name.as_str().to_owned() }
}

// --- member access: `Module.` -----------------------------------------------

/// The members offered after `path.`: a cross-file module's public exports and
/// constructors, or a same-file nested module's (visible) members.
fn module_members(db: &dyn Db, file: SourceFile, path: &str) -> Vec<CompletionItem> {
    let sym = Symbol::intern(path);
    if let Some(mfile) = module_file(db, ModuleName(sym)) {
        let interface = module_interface(db, mfile);
        let mut out = Vec::new();
        for export in &interface.exports {
            out.push(value_item(db, mfile, export.name, &def_type(db, mfile, export.name)));
        }
        for &ctor in &interface.ctors {
            out.push(ctor_item(db, mfile, ctor));
        }
        return out;
    }
    // A nested module declared in this file (addressed by its full path).
    let defs = module_defs(db, file);
    if defs.modules.contains(&sym) {
        let prefix = format!("{path}.");
        let mut out = Vec::new();
        for d in &defs.defs {
            if let Some(child) = d.name.as_str().strip_prefix(&prefix)
                && !child.contains('.')
            {
                out.push(value_item(db, file, d.name, &def_type(db, file, d.name)));
            }
        }
        for ctor in type_decls(db, file).ctors.values() {
            if let Some(child) = ctor.name.as_str().strip_prefix(&prefix)
                && !child.contains('.')
            {
                out.push(ctor_item(db, file, ctor.name));
            }
        }
        return out;
    }
    Vec::new()
}

/// A constructor candidate (its label is the bare name; its detail the scheme).
fn ctor_item(db: &dyn Db, file: SourceFile, name: Symbol) -> CompletionItem {
    let detail = constructor_scheme(db, file, name).map(|s| render_scheme(&s));
    CompletionItem {
        label: last_segment(name.as_str()).to_owned(),
        kind: CompletionKind::Constructor,
        detail,
        data: Some(completion_data(db, file, name)),
    }
}

// --- member access: `value.` ------------------------------------------------

/// The fields of the base value's record type (empty when it is not a record or
/// has no inferred type).
fn record_fields(db: &dyn Db, file: SourceFile, offset: u32, base: ExprId) -> Vec<CompletionItem> {
    let Some(def) = enclosing_def(db, file, offset) else {
        return Vec::new();
    };
    let types = body_types(db, file, def);
    let Some(Ty::Record(row)) = types.get(base) else {
        return Vec::new();
    };
    row.fields
        .iter()
        .map(|(label, fty)| CompletionItem {
            label: label.as_str().to_owned(),
            kind: CompletionKind::Field,
            detail: Some(render_canonical(fty)),
            data: None,
        })
        .collect()
}

// --- bare identifier --------------------------------------------------------

/// The names in scope at `offset`: visible locals, this module's visible
/// definitions and constructors, and the auto-imported prelude values.
fn bare_candidates(db: &dyn Db, file: SourceFile, offset: u32) -> Vec<CompletionItem> {
    let mut out = local_candidates(db, file, offset);
    out.extend(module_def_candidates(db, file, offset));
    out.extend(ctor_candidates(db, file, offset));
    out.extend(prelude_value_candidates(db));
    out
}

/// The locals visible at `offset`, innermost binding winning on shadowing.
fn local_candidates(db: &dyn Db, file: SourceFile, offset: u32) -> Vec<CompletionItem> {
    let Some(def) = enclosing_def(db, file, offset) else {
        return Vec::new();
    };
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let Some(info) = module_defs(db, file).get(def).copied() else {
        return Vec::new();
    };
    let ItemKind::Binding { params, body, .. } = &module.items[info.binding.index()].kind else {
        return Vec::new();
    };
    let mut binders: Vec<PatId> = Vec::new();
    for &p in params {
        collect_pat_binders(module, p, &mut binders);
    }
    walk_scope(module, *body, offset, &mut binders);

    let types = body_types(db, file, def);
    let mut seen: FxHashSet<Symbol> = FxHashSet::default();
    let mut out = Vec::new();
    // Innermost binders come last; iterate in reverse so they win on shadowing.
    for &pat in binders.iter().rev() {
        let Some(name) = pat_binder_name(module, pat) else { continue };
        if !seen.insert(name) {
            continue;
        }
        let detail = types.pat_type(pat).map(render_canonical);
        out.push(CompletionItem {
            label: name.as_str().to_owned(),
            kind: CompletionKind::Value,
            detail,
            data: None,
        });
    }
    out
}

/// This module's definitions visible at `offset` (the enclosing nested-module
/// scope and every scope outward, including the top level).
fn module_def_candidates(db: &dyn Db, file: SourceFile, offset: u32) -> Vec<CompletionItem> {
    let parsed = fai_syntax::parse(db, file);
    let scope = enclosing_module_scope(&parsed.module, offset);
    let defs = module_defs(db, file);
    let mut out = Vec::new();
    for d in &defs.defs {
        if visible_in_scope(d.name.as_str(), &scope) {
            out.push(value_item(db, file, d.name, &def_type(db, file, d.name)));
        }
    }
    out
}

/// The constructors visible at `offset`: this file's (scope-visible) constructors
/// plus the auto-imported prelude constructors.
fn ctor_candidates(db: &dyn Db, file: SourceFile, offset: u32) -> Vec<CompletionItem> {
    let parsed = fai_syntax::parse(db, file);
    let scope = enclosing_module_scope(&parsed.module, offset);
    let mut out = Vec::new();
    for ctor in type_decls(db, file).ctors.values() {
        if visible_in_scope(ctor.name.as_str(), &scope) {
            out.push(ctor_item(db, file, ctor.name));
        }
    }
    for (name, cref) in &prelude_exports(db).ctors {
        if let Some(cfile) = db.source_file(cref.file) {
            out.push(ctor_item(db, cfile, *name));
        }
    }
    out
}

/// The auto-imported prelude value bindings (operators excluded — their symbolic
/// names are not completed by typing letters).
fn prelude_value_candidates(db: &dyn Db) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    for (name, def) in &prelude_exports(db).values {
        if !name.as_str().as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
            continue;
        }
        if let Some(dfile) = db.source_file(def.file) {
            out.push(value_item(db, dfile, *name, &def_type(db, dfile, *name)));
        }
    }
    out
}

/// Whether a definition with qualified `name` is visible from `scope`: its owning
/// module path must be `scope` or an enclosing prefix of it.
fn visible_in_scope(name: &str, scope: &[Symbol]) -> bool {
    // The module path is the name without its last segment.
    let segments: Vec<&str> = name.split('.').collect();
    let module_path = &segments[..segments.len() - 1];
    if module_path.len() > scope.len() {
        return false;
    }
    module_path.iter().zip(scope).all(|(m, s)| *m == s.as_str())
}

/// The nested-module path whose body contains `offset` (empty at the top level).
fn enclosing_module_scope(module: &Module, offset: u32) -> Vec<Symbol> {
    let mut scope = Vec::new();
    descend_scope(module, &module.roots, offset, &mut scope);
    scope
}

fn descend_scope(
    module: &Module,
    items: &[fai_syntax::ast::ItemId],
    offset: u32,
    scope: &mut Vec<Symbol>,
) {
    for &id in items {
        let item = &module.items[id.index()];
        if let ItemKind::Module { name, body } = &item.kind
            && item.span.start().raw() <= offset
            && offset < item.span.end().raw()
        {
            scope.push(*name);
            descend_scope(module, body, offset, scope);
            return;
        }
    }
}

// --- pattern binders & scope walk -------------------------------------------

/// The binder name of a `Var`/`As` pattern.
fn pat_binder_name(module: &Module, pat: PatId) -> Option<Symbol> {
    match &module.pat(pat).kind {
        PatKind::Var(name) | PatKind::As { name, .. } => Some(*name),
        _ => None,
    }
}

/// Collects the `Var`/`As` binder patterns nested anywhere in `pat`.
fn collect_pat_binders(module: &Module, pat: PatId, out: &mut Vec<PatId>) {
    match &module.pat(pat).kind {
        PatKind::Var(_) => out.push(pat),
        PatKind::As { pat: inner, .. } => {
            out.push(pat);
            collect_pat_binders(module, *inner, out);
        }
        PatKind::Tuple(ps) | PatKind::List(ps) | PatKind::Or(ps) => {
            for &p in ps {
                collect_pat_binders(module, p, out);
            }
        }
        PatKind::Paren(p) => collect_pat_binders(module, *p, out),
        PatKind::Constructor { args, .. } => {
            for &p in args {
                collect_pat_binders(module, p, out);
            }
        }
        PatKind::Cons { head, tail } => {
            collect_pat_binders(module, *head, out);
            collect_pat_binders(module, *tail, out);
        }
        PatKind::Record { fields, .. } => {
            for f in fields {
                collect_pat_binders(module, f.pat, out);
            }
        }
        _ => {}
    }
}

fn contains(module: &Module, expr: ExprId, offset: u32) -> bool {
    let span = module.expr(expr).span;
    span.start().raw() <= offset && offset <= span.end().raw()
}

/// Descends the expression tree toward `offset`, accumulating the binders whose
/// scope includes it: lambda/match-arm/local-`let` bindings along the path.
fn walk_scope(module: &Module, expr: ExprId, offset: u32, acc: &mut Vec<PatId>) {
    if !contains(module, expr, offset) {
        return;
    }
    match &module.expr(expr).kind {
        ExprKind::Lambda { params, body } => {
            for &p in params {
                collect_pat_binders(module, p, acc);
            }
            walk_scope(module, *body, offset, acc);
        }
        ExprKind::Block { stmts, tail } => {
            for stmt in stmts {
                // A local binding is in scope in the statements after it (and the
                // tail); its own parameters are in scope inside its value.
                if offset >= module.expr(stmt.value).span.end().raw() {
                    collect_pat_binders(module, stmt.pat, acc);
                }
                if contains(module, stmt.value, offset) {
                    for &p in &stmt.params {
                        collect_pat_binders(module, p, acc);
                    }
                    walk_scope(module, stmt.value, offset, acc);
                }
            }
            walk_scope(module, *tail, offset, acc);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_scope(module, *scrutinee, offset, acc);
            for arm in arms {
                if contains(module, arm.body, offset) {
                    collect_pat_binders(module, arm.pat, acc);
                    walk_scope(module, arm.body, offset, acc);
                }
            }
        }
        ExprKind::App { func, arg } => {
            walk_scope(module, *func, offset, acc);
            walk_scope(module, *arg, offset, acc);
        }
        ExprKind::Infix { lhs, rhs, .. } => {
            walk_scope(module, *lhs, offset, acc);
            walk_scope(module, *rhs, offset, acc);
        }
        ExprKind::Prefix { operand, .. } => walk_scope(module, *operand, offset, acc),
        ExprKind::If { cond, then_branch, else_branch } => {
            walk_scope(module, *cond, offset, acc);
            walk_scope(module, *then_branch, offset, acc);
            walk_scope(module, *else_branch, offset, acc);
        }
        ExprKind::Field { base, .. } => walk_scope(module, *base, offset, acc),
        ExprKind::Record(fields) => {
            for f in fields {
                walk_scope(module, f.value, offset, acc);
            }
        }
        ExprKind::RecordUpdate { base, fields } => {
            walk_scope(module, *base, offset, acc);
            for f in fields {
                walk_scope(module, f.value, offset, acc);
            }
        }
        ExprKind::Instance { methods, .. } => {
            for m in methods {
                if contains(module, m.body, offset) {
                    for &p in &m.params {
                        collect_pat_binders(module, p, acc);
                    }
                    walk_scope(module, m.body, offset, acc);
                }
            }
        }
        ExprKind::Paren(inner) => walk_scope(module, *inner, offset, acc),
        ExprKind::Tuple(xs) | ExprKind::List(xs) => {
            for &x in xs {
                walk_scope(module, x, offset, acc);
            }
        }
        _ => {}
    }
}
