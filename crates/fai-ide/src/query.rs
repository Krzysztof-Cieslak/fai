//! The eight `fai query` commands, built on resolution + inference.
//!
//! Every result is a serde envelope carrying `schemaVersion` (CLI.md §8). Lists
//! are deterministically sorted; spans are resolved late via a [`SpanResolver`].
//! Results are best-effort: partial answers are returned even when the workspace
//! has errors.

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, Res, ResolvedBodies, module_defs, resolve};
use fai_span::{Span, SpanResolver};
use fai_syntax::Symbol;
use fai_syntax::ast::{ExprId, ExprKind, ItemKind, Module, PatKind, Visibility as AstVis};
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
