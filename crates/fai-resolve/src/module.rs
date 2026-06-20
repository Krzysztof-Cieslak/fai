//! Per-module structure: signature/binding pairing, visibility, and the public
//! interface (`module_exports`) plus the workspace name index.
//!
//! Pairing and the interface are **salsa queries** keyed on a file. The interface
//! is derived from declared signatures only (never bodies), so editing a private
//! body cannot change it — the cross-module firewall. `ItemId`s are arena indices
//! (position-independent), so they are stable under reformatting and travel
//! safely inside cached values.

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::{Diagnostic, Severity};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemId, ItemKind, Visibility};
use rustc_hash::FxHashMap;

use crate::ids::{CtorRef, DefId};
use crate::{
    BINDING_VISIBILITY_MARKER, DUPLICATE_DEFINITION, DUPLICATE_MODULE, DUPLICATE_PRELUDE_EXPORT,
    MODULE_NAME_CONFLICT, MULTIPLE_SIGNATURES, ORPHAN_SIGNATURE, PUBLIC_FOREIGN,
};

/// A module's declared header name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleName(pub Symbol);

/// One paired definition in a module (at any nesting depth).
///
/// Produced by [`module_defs`]: a binding, optionally paired with a signature of
/// the same name. `name` is the **qualified** name (a nested binding's name is
/// prefixed by its module path, e.g. `Internal.pi`; a top-level binding keeps its
/// bare name). All ids are arena indices (span-free, stable under reformat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefInfo {
    /// The definition's qualified name (bare for a top-level binding).
    pub name: Symbol,
    /// Effective visibility (from the signature when present, else the binding).
    pub visibility: Visibility,
    /// The signature item, if the definition has one.
    pub signature: Option<ItemId>,
    /// The binding item.
    pub binding: ItemId,
}

/// The paired definitions of a module, in source order (pre-order across nesting).
///
/// This is a `salsa` value: it is `Eq`/`Update` and free of byte offsets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleDefs {
    /// The definitions, in source order (a nested module's defs follow its
    /// declaration). Names are qualified.
    pub defs: Vec<DefInfo>,
    /// The qualified paths of every nested module declared in the file (e.g.
    /// `Internal`, `Outer.Inner`), in declaration order. Used to recognize a
    /// module segment during qualified-path resolution.
    pub modules: Vec<Symbol>,
}

impl ModuleDefs {
    /// Looks up a definition by its qualified name.
    #[must_use]
    pub fn get(&self, name: Symbol) -> Option<&DefInfo> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// Whether `name` is the qualified path of a nested module in this file.
    #[must_use]
    pub fn is_module(&self, name: Symbol) -> bool {
        self.modules.contains(&name)
    }
}

/// One public export of a module: its name and (optional) signature item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Export {
    /// The exported name.
    pub name: Symbol,
    /// The signature item declaring its type, if any. (A public binding without
    /// a signature is an error, reported in the types phase; the export still
    /// appears so dependents see a name rather than a spurious "unbound".)
    pub signature: Option<ItemId>,
}

/// A module's public interface — the cross-module firewall value.
///
/// Derived from declared signatures and public `type` declarations only, sorted
/// by name, span-free and `Eq`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleInterface {
    /// Public value exports, sorted by name text.
    pub exports: Vec<Export>,
    /// Public type names, sorted by name text.
    pub types: Vec<Symbol>,
    /// Public data-constructor names, sorted by name text.
    pub ctors: Vec<Symbol>,
    /// Public interface names, sorted by name text.
    pub interfaces: Vec<Symbol>,
}

impl ModuleInterface {
    /// Looks up a public value export by name.
    #[must_use]
    pub fn get(&self, name: Symbol) -> Option<&Export> {
        self.exports.iter().find(|e| e.name == name)
    }

    /// Whether `name` is a public type of this module.
    #[must_use]
    pub fn has_type(&self, name: Symbol) -> bool {
        self.types.binary_search_by(|t| t.as_str().cmp(name.as_str())).is_ok()
    }

    /// Whether `name` is a public constructor of this module.
    #[must_use]
    pub fn has_ctor(&self, name: Symbol) -> bool {
        self.ctors.binary_search_by(|c| c.as_str().cmp(name.as_str())).is_ok()
    }
}

/// Pairs each binding with its same-name signature, reporting pairing errors —
/// recursively, per nested-module scope.
///
/// Errors (each per scope): a signature with no binding ([`ORPHAN_SIGNATURE`]);
/// two signatures for one name ([`MULTIPLE_SIGNATURES`]); two bindings for one
/// name ([`DUPLICATE_DEFINITION`]); a visibility marker on a binding that already
/// has a signature ([`BINDING_VISIBILITY_MARKER`]); a nested-module name that
/// collides with another module/type/interface/constructor in the same scope
/// ([`MODULE_NAME_CONFLICT`]). Names in the result are qualified by their module
/// path.
#[salsa::tracked]
pub fn module_defs(db: &dyn Db, file: SourceFile) -> ModuleDefs {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let source = file.source(db);

    let mut out = ModuleDefs::default();
    let mut scope: Vec<Symbol> = Vec::new();
    collect_scope(db, module, source, &mut scope, &module.roots, &mut out);
    out
}

/// Pairs and qualifies the definitions of one module scope (`items`), then
/// recurses into its nested modules.
fn collect_scope(
    db: &dyn Db,
    module: &fai_syntax::ast::Module,
    source: fai_span::SourceId,
    scope: &mut Vec<Symbol>,
    items: &[ItemId],
    out: &mut ModuleDefs,
) {
    let mut sig_by_name: FxHashMap<Symbol, ItemId> = FxHashMap::default();
    let mut binding_by_name: FxHashMap<Symbol, ItemId> = FxHashMap::default();
    let mut binding_order: Vec<(Symbol, ItemId)> = Vec::new();
    // Upper-namespace names declared in this scope (types, interfaces, and union
    // constructors) — a nested module may not reuse one of these.
    let mut upper_names: FxHashMap<Symbol, ()> = FxHashMap::default();
    // Nested modules in this scope: (name, declaring item, body), recursed last.
    let mut nested: Vec<(Symbol, ItemId)> = Vec::new();

    for &id in items {
        let item = &module.items[id.index()];
        match &item.kind {
            ItemKind::Signature { name, .. } => {
                if sig_by_name.insert(*name, id).is_some() {
                    emit(
                        db,
                        Diagnostic::error(
                            MULTIPLE_SIGNATURES,
                            format!("`{name}` has more than one signature"),
                            Span::new(source, item.span),
                        ),
                    );
                }
            }
            ItemKind::Binding { name, visibility, .. } => {
                if binding_by_name.insert(*name, id).is_some() {
                    emit(
                        db,
                        Diagnostic::error(
                            DUPLICATE_DEFINITION,
                            format!("`{name}` is defined more than once"),
                            Span::new(source, item.span),
                        ),
                    );
                } else {
                    binding_order.push((*name, id));
                    if *visibility != Visibility::Private && sig_by_name.contains_key(name) {
                        emit(
                            db,
                            Diagnostic::error(
                                BINDING_VISIBILITY_MARKER,
                                format!(
                                    "`{name}` has a signature, so its visibility must \
                                     be declared there, not on the binding"
                                ),
                                Span::new(source, item.span),
                            ),
                        );
                    }
                }
            }
            ItemKind::Foreign { name, visibility, .. } => {
                // A `foreign` decl is a value definition that carries its own
                // signature, so it occupies both the binding and signature slots
                // (its `DefInfo` then has `signature == binding == this item`, and
                // the declared scheme reads its written type). It is always
                // module-private: a `public`/`internal foreign` is rejected.
                if *visibility != Visibility::Private {
                    emit(
                        db,
                        Diagnostic::error(
                            PUBLIC_FOREIGN,
                            format!(
                                "the foreign declaration `{name}` cannot be `{}`",
                                if *visibility == Visibility::Public {
                                    "public"
                                } else {
                                    "internal"
                                }
                            ),
                            Span::new(source, item.span),
                        )
                        .with_help(
                            "expose a foreign function through a capability interface, \
                             not directly",
                        ),
                    );
                }
                if binding_by_name.insert(*name, id).is_some() {
                    emit(
                        db,
                        Diagnostic::error(
                            DUPLICATE_DEFINITION,
                            format!("`{name}` is defined more than once"),
                            Span::new(source, item.span),
                        ),
                    );
                } else {
                    binding_order.push((*name, id));
                    sig_by_name.insert(*name, id);
                }
            }
            ItemKind::Type { name, def, .. } => {
                upper_names.insert(*name, ());
                if let fai_syntax::ast::TypeDef::Union(variants) = def {
                    for v in variants {
                        upper_names.insert(v.name, ());
                    }
                }
            }
            ItemKind::Interface { name, .. } => {
                upper_names.insert(*name, ());
            }
            ItemKind::Module { name, .. } => nested.push((*name, id)),
            ItemKind::Example { .. } | ItemKind::Forall { .. } | ItemKind::Error => {}
        }
    }

    for (name, binding) in &binding_order {
        let signature = sig_by_name.get(name).copied();
        let visibility = effective_visibility(module, signature, *binding);
        out.defs.push(DefInfo {
            name: crate::qualify(scope, *name),
            visibility,
            signature,
            binding: *binding,
        });
    }

    for (name, sig) in &sig_by_name {
        if !binding_by_name.contains_key(name) {
            let span = module.items[sig.index()].span;
            emit(
                db,
                Diagnostic::error(
                    ORPHAN_SIGNATURE,
                    format!("`{name}` has a signature but no binding"),
                    Span::new(source, span),
                ),
            );
        }
    }

    // A nested module's name must not collide with another module, type,
    // interface, or constructor in the same scope.
    let mut module_count: FxHashMap<Symbol, usize> = FxHashMap::default();
    for (name, _) in &nested {
        *module_count.entry(*name).or_insert(0) += 1;
    }
    for &(name, id) in &nested {
        if upper_names.contains_key(&name) || module_count[&name] > 1 {
            emit(
                db,
                Diagnostic::error(
                    MODULE_NAME_CONFLICT,
                    format!("`{name}` is already declared in this module"),
                    Span::new(source, module.items[id.index()].span),
                )
                .with_help("a module shares the upper-case namespace with types, interfaces, and constructors"),
            );
        }
    }

    // Record and recurse into nested modules (in declaration order).
    for &(name, id) in &nested {
        out.modules.push(crate::qualify(scope, name));
        if let ItemKind::Module { body, .. } = &module.items[id.index()].kind {
            scope.push(name);
            collect_scope(db, module, source, scope, body, out);
            scope.pop();
        }
    }
}

/// The effective visibility of a definition: the signature's when present, else
/// the binding's own marker.
fn effective_visibility(
    module: &fai_syntax::ast::Module,
    signature: Option<ItemId>,
    binding: ItemId,
) -> Visibility {
    if let Some(sig) = signature
        && let ItemKind::Signature { visibility, .. } = &module.items[sig.index()].kind
    {
        return *visibility;
    }
    if let ItemKind::Binding { visibility, .. } = &module.items[binding.index()].kind {
        return *visibility;
    }
    Visibility::Private
}

/// The module's public interface (the cross-module firewall value).
///
/// Depends only on signatures and visibility, so private-body edits leave it
/// unchanged. Exports are sorted by name text for deterministic output.
#[salsa::tracked]
pub fn module_interface(db: &dyn Db, file: SourceFile) -> ModuleInterface {
    interface_at_visibility(db, file, Visibility::Public)
}

/// A module's `internal` interface — its same-origin cross-file surface.
///
/// The peer of [`module_interface`] for `internal` members: the bindings, types,
/// constructors, and interfaces a same-origin file may reference. It feeds
/// origin-aware tooling (completion, code actions, `fai query api`); name
/// resolution gates `internal` references directly against `module_defs`/
/// `type_decls`, so this query is not on the resolution firewall and an
/// `internal` edit never invalidates a cross-origin importer.
#[salsa::tracked]
pub fn module_internal_interface(db: &dyn Db, file: SourceFile) -> ModuleInterface {
    interface_at_visibility(db, file, Visibility::Internal)
}

/// The exported surface of `file` at exactly `visibility` (the shared body of
/// [`module_interface`] and [`module_internal_interface`]). Derived only from
/// signatures and visibility, sorted by name text, so private-body edits leave it
/// unchanged.
fn interface_at_visibility(
    db: &dyn Db,
    file: SourceFile,
    visibility: Visibility,
) -> ModuleInterface {
    let defs = module_defs(db, file);
    let mut exports: Vec<Export> = defs
        .defs
        .iter()
        .filter(|d| d.visibility == visibility)
        .map(|d| Export { name: d.name, signature: d.signature })
        .collect();
    exports.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    // `type` declarations of this visibility export their type name and (for
    // unions) every constructor. An **opaque** type exports its name but not its
    // constructors: it is named but not constructed, deconstructed, or seen
    // through from other files.
    let decls = crate::decls::type_decls(db, file);
    let mut types: Vec<Symbol> = Vec::new();
    let mut ctors: Vec<Symbol> = Vec::new();
    for info in decls.types.values() {
        if info.visibility == visibility {
            types.push(info.name);
            if !info.opaque {
                ctors.extend(info.ctors.iter().copied());
            }
        }
    }
    types.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ctors.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    let mut interfaces: Vec<Symbol> = crate::decls::interface_decls(db, file)
        .interfaces
        .values()
        .filter(|info| info.visibility == visibility)
        .map(|info| info.name)
        .collect();
    interfaces.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    ModuleInterface { exports, types, ctors, interfaces }
}

/// The module's declared header name, if it has one.
#[salsa::tracked]
pub fn module_name(db: &dyn Db, file: SourceFile) -> Option<ModuleName> {
    fai_syntax::parse(db, file).module.name.map(ModuleName)
}

/// The set of files whose header name collides with another file's, each of
/// which receives a duplicate-module error and is excluded from name lookup.
///
/// Computed once per workspace; depends only on each file's (cheap, stable)
/// header name, so body edits never recompute it.
#[salsa::tracked]
pub fn duplicate_module_files(db: &dyn Db) -> Arc<Vec<SourceFile>> {
    let mut by_name: FxHashMap<Symbol, Vec<SourceFile>> = FxHashMap::default();
    for file in db.all_source_files() {
        if let Some(ModuleName(name)) = module_name(db, file) {
            by_name.entry(name).or_default().push(file);
        }
    }
    let mut duplicates: Vec<SourceFile> =
        by_name.into_values().filter(|files| files.len() > 1).flatten().collect();
    duplicates.sort_by_key(|f| f.source(db));
    Arc::new(duplicates)
}

/// Resolves a module *name* to its file, honoring uniqueness.
///
/// Returns `None` if no module declares `name`, or if `name` is duplicated
/// (duplicated names are excluded from lookup). Duplicate-module diagnostics are
/// emitted by [`emit_duplicate_module_errors`].
///
/// Not a tracked query (its key is a plain value); it is a thin scan over the
/// memoized [`module_name`] of each file, so it stays cheap and incremental.
#[must_use]
pub fn module_file(db: &dyn Db, name: ModuleName) -> Option<SourceFile> {
    let mut found = None;
    for file in db.all_source_files() {
        if module_name(db, file) == Some(name) {
            if found.is_some() {
                return None; // duplicated => not uniquely resolvable
            }
            found = Some(file);
        }
    }
    found
}

/// The reserved module whose public interface is auto-imported everywhere.
pub const PRELUDE_MODULE: &str = "Prelude";

/// The standard-library modules whose public interface is visible unqualified in
/// every module (the one exception to the qualified-only cross-module rule).
///
/// Kept as a set so the auto-import machinery and the duplicate-export check
/// already generalize beyond a single module; today it is just `Prelude`.
const AUTO_IMPORTED: &[&str] = &[PRELUDE_MODULE];

/// The embedded standard-library files currently loaded (recognized by their
/// synthetic `<std>/` path), in [`SourceId`] order.
#[must_use]
pub fn std_files(db: &dyn Db) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> =
        db.all_source_files().into_iter().filter(|f| fai_db::is_std_path(f.path(db))).collect();
    files.sort_by_key(|f| f.source(db));
    files
}

/// The auto-imported `Prelude` module's file, located **among the standard-library
/// files** so a user's own `module Prelude` can neither hijack nor collapse
/// auto-import (it still gets [`DUPLICATE_MODULE`] and is excluded from lookup).
#[must_use]
pub fn prelude_module_file(db: &dyn Db) -> Option<SourceFile> {
    let name = ModuleName(Symbol::intern(PRELUDE_MODULE));
    std_files(db).into_iter().find(|&f| module_name(db, f) == Some(name))
}

/// The [`SourceId`] of the auto-imported `Prelude` module — where `Option` and
/// `Result` are declared — or `None` if the standard library is not loaded.
/// Tracked so a representation classifier can identify the prelude `Option` (and
/// distinguish it from a user-defined `Option`) without rescanning the source set.
#[salsa::tracked]
pub fn prelude_source(db: &dyn Db) -> Option<fai_span::SourceId> {
    prelude_module_file(db).map(|f| f.source(db))
}

/// Which namespace a duplicated auto-imported export lives in (for its message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// A value binding.
    Value,
    /// A data constructor.
    Ctor,
    /// A type name.
    Type,
    /// An interface name.
    Interface,
}

/// A name exported by more than one auto-imported module (recorded against the
/// later-declaring file, which is reported by [`emit_duplicate_prelude_export_errors`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateExport {
    /// The clashing name.
    pub name: Symbol,
    /// The file that re-declares an already-auto-imported name.
    pub file: SourceFile,
    /// The namespace the clash is in.
    pub kind: ExportKind,
}

/// The merged public interface of the auto-imported modules — the names visible
/// unqualified everywhere.
///
/// Keyed on names (each entry carries its declaring identity), so this value is
/// stable under body edits and reformatting: only a change to the auto-imported
/// *name set* invalidates dependents. Sorted for determinism.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreludeExports {
    /// Auto-imported value bindings, sorted by name.
    pub values: Vec<(Symbol, DefId)>,
    /// Auto-imported data constructors, sorted by name.
    pub ctors: Vec<(Symbol, CtorRef)>,
    /// Auto-imported type names with their declaring file, sorted by name.
    pub types: Vec<(Symbol, SourceFile)>,
    /// Auto-imported interface names with their declaring file, sorted by name.
    pub interfaces: Vec<(Symbol, SourceFile)>,
    /// Names declared by more than one auto-imported module.
    pub duplicates: Vec<DuplicateExport>,
}

/// Merges the public interfaces of `modules` into the auto-imported name set,
/// recording any name a later module redeclares.
///
/// Pure (no diagnostics): the first module to declare a name owns it; a later
/// redeclaration is pushed to `duplicates` for per-file emission. Exposed so the
/// duplicate detection is unit-testable with more than one module even while the
/// production set is a single `Prelude`.
#[must_use]
pub fn merge_auto_imports(db: &dyn Db, modules: &[SourceFile]) -> PreludeExports {
    use rustc_hash::FxHashSet;

    let mut value_names: FxHashSet<Symbol> = FxHashSet::default();
    let mut ctor_names: FxHashSet<Symbol> = FxHashSet::default();
    let mut type_names: FxHashSet<Symbol> = FxHashSet::default();
    let mut interface_names: FxHashSet<Symbol> = FxHashSet::default();
    let mut out = PreludeExports::default();

    for &file in modules {
        let source = file.source(db);
        let interface = module_interface(db, file);
        for export in &interface.exports {
            if value_names.insert(export.name) {
                out.values.push((export.name, DefId::new(source, export.name)));
            } else {
                out.duplicates.push(DuplicateExport {
                    name: export.name,
                    file,
                    kind: ExportKind::Value,
                });
            }
        }
        for &ctor in &interface.ctors {
            if ctor_names.insert(ctor) {
                out.ctors.push((ctor, CtorRef::new(source, ctor)));
            } else {
                out.duplicates.push(DuplicateExport { name: ctor, file, kind: ExportKind::Ctor });
            }
        }
        for &ty in &interface.types {
            if type_names.insert(ty) {
                out.types.push((ty, file));
            } else {
                out.duplicates.push(DuplicateExport { name: ty, file, kind: ExportKind::Type });
            }
        }
        for &iface in &interface.interfaces {
            if interface_names.insert(iface) {
                out.interfaces.push((iface, file));
            } else {
                out.duplicates.push(DuplicateExport {
                    name: iface,
                    file,
                    kind: ExportKind::Interface,
                });
            }
        }
    }

    out.values.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out.ctors.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out.types.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out.interfaces.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out
}

/// The auto-imported name set (the merged `Prelude` interface).
///
/// Tracked so the merge is computed once per revision and shared by every
/// module's resolution and the type-name fallback; its early cutoff means a
/// Prelude body edit (which leaves the name set unchanged) recomputes nothing
/// downstream.
#[salsa::tracked]
pub fn prelude_exports(db: &dyn Db) -> Arc<PreludeExports> {
    let modules: Vec<SourceFile> = AUTO_IMPORTED
        .iter()
        .filter_map(|n| {
            let name = ModuleName(Symbol::intern(n));
            std_files(db).into_iter().find(|&f| module_name(db, f) == Some(name))
        })
        .collect();
    Arc::new(merge_auto_imports(db, &modules))
}

/// Emits [`DUPLICATE_PRELUDE_EXPORT`] for any auto-imported name that `file`
/// redeclares, attributing it to `file` so it is reported once (when the
/// standard library itself is checked) rather than under every user module.
pub fn emit_duplicate_prelude_export_errors(db: &dyn Db, file: SourceFile) {
    let exports = prelude_exports(db);
    if exports.duplicates.iter().all(|d| d.file != file) {
        return;
    }
    let defs = module_defs(db, file);
    let decls = crate::decls::type_decls(db, file);
    let parsed = fai_syntax::parse(db, file);
    let source = file.source(db);
    let items = &parsed.module.items;
    for dup in exports.duplicates.iter().filter(|d| d.file == file) {
        let span = match dup.kind {
            ExportKind::Value => defs.get(dup.name).map(|d| items[d.binding.index()].span),
            ExportKind::Ctor => decls
                .ctor(dup.name)
                .and_then(|ci| decls.type_named(ci.adt))
                .map(|ti| items[ti.item.index()].span),
            ExportKind::Type => decls.type_named(dup.name).map(|ti| items[ti.item.index()].span),
            ExportKind::Interface => crate::decls::interface_decls(db, file)
                .interface_named(dup.name)
                .map(|ii| items[ii.item.index()].span),
        };
        let span = span.unwrap_or(parsed.module.header);
        emit(
            db,
            Diagnostic::warning(
                DUPLICATE_PRELUDE_EXPORT,
                format!("`{}` is exported by more than one auto-imported module", dup.name),
                Span::new(source, span),
            )
            .with_help("auto-imported modules must export disjoint names"),
        );
    }
}

/// Emits a duplicate-module error for `file` if its header name collides.
///
/// Called from the per-file resolution pass so the error is reported on each
/// colliding file (with the others as context).
pub fn emit_duplicate_module_errors(db: &dyn Db, file: SourceFile) {
    let duplicates = duplicate_module_files(db);
    if !duplicates.contains(&file) {
        return;
    }
    let Some(ModuleName(name)) = module_name(db, file) else {
        return;
    };
    let source = file.source(db);
    let header = fai_syntax::parse(db, file).module.header;
    let mut diag = Diagnostic::new(
        DUPLICATE_MODULE,
        Severity::Error,
        format!("module `{name}` is declared in more than one file"),
        Span::new(source, header),
    )
    .with_help("top-level module names must be unique across the workspace");
    for other in duplicates.iter().copied() {
        if other == file {
            continue;
        }
        let other_source = other.source(db);
        let other_header = fai_syntax::parse(db, other).module.header;
        diag = diag.with_label(fai_diagnostics::Label::new(
            Span::new(other_source, other_header),
            "also declared here",
        ));
    }
    emit(db, diag);
}
