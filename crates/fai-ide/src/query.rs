//! The eight `fai query` commands, built on resolution + inference.
//!
//! Every result is a serde envelope carrying `schemaVersion` (CLI.md §8). Lists
//! are deterministically sorted; spans are resolved late via a [`SpanResolver`].
//! Results are best-effort: partial answers are returned even when the workspace
//! has errors.

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, module_defs, resolve};
use fai_span::{Span, SpanResolver};
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemKind, PatKind, Visibility as AstVis};
use rustc_hash::FxHashMap;
use serde::Serialize;

use crate::repr::{
    Contract, Doc, Location, SCHEMA_VERSION, SpanJson, SymbolKind, SymbolRef, TypeRepr, Visibility,
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

/// Finds the definitions that reference the target (reverse dependency edges).
#[must_use]
pub fn dependents(
    db: &dyn Db,
    files: &[SourceFile],
    target: &str,
    resolver: &dyn SpanResolver,
    opts: ListOpts,
) -> DependentsResult {
    let Some(t) = resolve_target(db, target) else {
        return DependentsResult {
            schema_version: SCHEMA_VERSION,
            target: None,
            dependents: vec![],
            transitive: false,
            truncated: false,
        };
    };
    let target_def = DefId::new(t.file.source(db), t.name);
    let mut deps: Vec<SymbolRef> = Vec::new();
    for &file in files {
        let resolved = resolve(db, file);
        for (owner, edges) in &resolved.deps_by_def {
            if edges.contains(&target_def)
                && let Some(owner_file) = db.source_file(owner.file)
                && let Some(sr) = symbol_ref(db, owner_file, owner.name, resolver)
            {
                deps.push(sr);
            }
        }
    }
    deps.sort_by(|a, b| a.path.cmp(&b.path));
    deps.dedup_by(|a, b| a.path == b.path);
    let (dependents, truncated) = truncate(deps, opts);
    DependentsResult {
        schema_version: SCHEMA_VERSION,
        target: symbol_ref(db, t.file, t.name, resolver),
        dependents,
        transitive: false,
        truncated,
    }
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
    let mut nodes = Vec::new();
    if let Some(file) = file {
        let parsed = fai_syntax::parse(db, file);
        let module = &parsed.module;
        let mut scope: Vec<Symbol> = Vec::new();
        nodes = outline_items(db, file, module, &mut scope, &module.roots, resolver);
    }
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
                exports.push(ApiExport { symbol, doc: None, contracts });
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
        doc: None,
        contracts,
    }
}
