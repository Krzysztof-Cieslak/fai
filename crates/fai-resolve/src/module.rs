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

use crate::{
    BINDING_VISIBILITY_MARKER, DUPLICATE_DEFINITION, DUPLICATE_MODULE, MULTIPLE_SIGNATURES,
    ORPHAN_SIGNATURE,
};

/// A module's declared header name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleName(pub Symbol);

/// One paired top-level definition in a module.
///
/// Produced by [`module_defs`]: a binding, optionally paired with a signature of
/// the same name. All ids are arena indices (span-free, stable under reformat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefInfo {
    /// The definition's name.
    pub name: Symbol,
    /// Effective visibility (from the signature when present, else the binding).
    pub visibility: Visibility,
    /// The signature item, if the definition has one.
    pub signature: Option<ItemId>,
    /// The binding item.
    pub binding: ItemId,
}

/// The paired definitions of a module, in source order.
///
/// This is a `salsa` value: it is `Eq`/`Update` and free of byte offsets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleDefs {
    /// The definitions, in source order.
    pub defs: Vec<DefInfo>,
}

impl ModuleDefs {
    /// Looks up a definition by name.
    #[must_use]
    pub fn get(&self, name: Symbol) -> Option<&DefInfo> {
        self.defs.iter().find(|d| d.name == name)
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
/// Derived from declared signatures only, sorted by name, span-free and `Eq`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleInterface {
    /// Public exports, sorted by name text.
    pub exports: Vec<Export>,
}

impl ModuleInterface {
    /// Looks up a public export by name.
    #[must_use]
    pub fn get(&self, name: Symbol) -> Option<&Export> {
        self.exports.iter().find(|e| e.name == name)
    }
}

/// Pairs each binding with its same-name signature, reporting pairing errors.
///
/// Errors: a signature with no binding ([`ORPHAN_SIGNATURE`]); two signatures for
/// one name ([`MULTIPLE_SIGNATURES`]); two bindings for one name
/// ([`DUPLICATE_DEFINITION`]); a visibility marker on a binding that already has
/// a signature ([`BINDING_VISIBILITY_MARKER`]).
#[salsa::tracked]
pub fn module_defs(db: &dyn Db, file: SourceFile) -> ModuleDefs {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let source = file.source(db);

    // Collect signatures and bindings by name, in order, tracking duplicates.
    let mut sig_by_name: FxHashMap<Symbol, ItemId> = FxHashMap::default();
    let mut binding_by_name: FxHashMap<Symbol, ItemId> = FxHashMap::default();
    let mut binding_order: Vec<(Symbol, ItemId)> = Vec::new();

    for (index, item) in module.items.iter().enumerate() {
        let id = ItemId::from_index(index);
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
                    // A binding may not carry a visibility marker when a
                    // signature exists; visibility lives on the signature.
                    if *visibility == Visibility::Public && sig_by_name.contains_key(name) {
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
            ItemKind::Example { .. } | ItemKind::Forall { .. } | ItemKind::Error => {}
        }
    }

    // Build the paired definitions, in binding source order.
    let mut defs = Vec::new();
    for (name, binding) in &binding_order {
        let signature = sig_by_name.get(name).copied();
        let visibility = effective_visibility(module, signature, *binding);
        defs.push(DefInfo { name: *name, visibility, signature, binding: *binding });
    }

    // Any signature without a matching binding is an orphan.
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

    ModuleDefs { defs }
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
    let defs = module_defs(db, file);
    let mut exports: Vec<Export> = defs
        .defs
        .iter()
        .filter(|d| d.visibility == Visibility::Public)
        .map(|d| Export { name: d.name, signature: d.signature })
        .collect();
    exports.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    ModuleInterface { exports }
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
