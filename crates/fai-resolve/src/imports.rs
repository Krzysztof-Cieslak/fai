//! Identity-preserving external module bindings and public re-export traversal.

use std::sync::Arc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::{Diagnostic, DiagnosticCode};
use fai_span::{SourceId, Span};
use fai_syntax::{ImportSummary, ItemTreeKind, Symbol, ast::Visibility};

use crate::{
    CYCLIC_IMPORT, IMPORT_NAME_CONFLICT, INVALID_IMPORT, ModuleName, UNKNOWN_DEPENDENCY,
    module_defs, module_file_from, module_name, qualify,
};

/// A module namespace anchored in its original defining file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleTarget {
    /// The original source, retained across import aliases and re-exports.
    pub file: SourceFile,
    /// Nested module path within the file; empty for its top-level module.
    pub path: Vec<Symbol>,
}

/// A position-independent import failure, located by the caller's declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportFailure {
    /// Stable diagnostic category.
    pub code: DiagnosticCode,
    /// Explanation independent of the source declaration's byte position.
    pub message: String,
}

/// A file-local binding, with its original target or an explanatory failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBinding {
    /// Name introduced into this file's module namespace.
    pub name: Symbol,
    /// Export visibility of the alias itself.
    pub visibility: Visibility,
    /// Original module identity.
    pub target: Result<ModuleTarget, ImportFailure>,
}

fn failure(code: DiagnosticCode, message: impl Into<String>) -> ImportFailure {
    ImportFailure { code, message: message.into() }
}

fn binding_conflict(db: &dyn Db, file: SourceFile, name: Symbol) -> bool {
    let tree = fai_syntax::item_tree(db, file);
    if name.as_str() == crate::intrinsics::PRIM_MODULE
        || tree.imports.iter().filter(|import| import.name == name).count() > 1
        || fai_db::automatically_visible_sources(db, file)
            .iter()
            .any(|&source| module_name(db, source) == Some(ModuleName(name)))
    {
        return true;
    }
    tree.items.iter().any(|item| {
        (matches!(item.kind, ItemTreeKind::Module | ItemTreeKind::Type | ItemTreeKind::Interface)
            && item.name == Some(name))
            || item.ctors.contains(&name)
    })
}

/// A memoized import map whose values are free of source positions.
#[fai_db::salsa::tracked]
pub fn import_bindings(db: &dyn Db, file: SourceFile) -> Arc<Vec<ImportBinding>> {
    let tree = fai_syntax::item_tree(db, file);
    Arc::new(
        tree.imports
            .iter()
            .enumerate()
            .map(|(index, import)| ImportBinding {
                name: import.name,
                visibility: import.visibility,
                target: resolve_declaration(db, file, index, &mut Vec::new()),
            })
            .collect(),
    )
}

fn resolve_declaration(
    db: &dyn Db,
    file: SourceFile,
    index: usize,
    stack: &mut Vec<(SourceId, usize)>,
) -> Result<ModuleTarget, ImportFailure> {
    let site = (file.source(db), index);
    if stack.len() >= 128 || stack.contains(&site) {
        return Err(failure(
            CYCLIC_IMPORT,
            "module re-exports form a cycle or exceed 128 import layers",
        ));
    }
    let tree = fai_syntax::item_tree(db, file);
    let Some(import) = tree.imports.get(index) else {
        return Err(failure(INVALID_IMPORT, "invalid module import declaration"));
    };
    if binding_conflict(db, file, import.name) {
        return Err(failure(
            IMPORT_NAME_CONFLICT,
            format!(
                "module import `{}` conflicts with an existing name; choose another alias with `as`",
                import.name
            ),
        ));
    }
    stack.push(site);
    let result = resolve_import_path(db, file, import, stack);
    stack.pop();
    result
}

fn resolve_import_path(
    db: &dyn Db,
    file: SourceFile,
    import: &ImportSummary,
    stack: &mut Vec<(SourceId, usize)>,
) -> Result<ModuleTarget, ImportFailure> {
    let Some((&alias, rest)) = import.path.split_first() else {
        return Err(failure(INVALID_IMPORT, "an import needs a dependency alias and module"));
    };
    let package = fai_db::dependency_package(db, file, alias.as_str())
        .and_then(|id| fai_db::package_environment(db, id))
        .ok_or_else(|| {
            failure(
                UNKNOWN_DEPENDENCY,
                format!("`{alias}` is not a declared dependency in this source scope"),
            )
        })?;
    let Some((&head, nested)) = rest.split_first() else {
        return Err(failure(INVALID_IMPORT, "an import needs a module after the dependency alias"));
    };
    let sources = fai_db::package_library_sources(db, package);
    let mut matches =
        sources.iter().copied().filter(|&source| module_name(db, source) == Some(ModuleName(head)));
    let target = matches.next().ok_or_else(|| {
        failure(INVALID_IMPORT, format!("dependency `{alias}` has no library module `{head}`"))
    })?;
    if matches.next().is_some() {
        return Err(failure(
            INVALID_IMPORT,
            format!("dependency `{alias}` has more than one library module `{head}`"),
        ));
    }
    let mut target = ModuleTarget { file: target, path: Vec::new() };
    for &segment in nested {
        target = child_module(db, file, &target, segment, stack)?.ok_or_else(|| {
            failure(INVALID_IMPORT, format!("module path has no public module `{segment}`"))
        })?;
    }
    Ok(target)
}

fn child_module(
    db: &dyn Db,
    observer: SourceFile,
    target: &ModuleTarget,
    child: Symbol,
    stack: &mut Vec<(SourceId, usize)>,
) -> Result<Option<ModuleTarget>, ImportFailure> {
    if module_defs(db, target.file).is_module(qualify(&target.path, child)) {
        let mut path = target.path.clone();
        path.push(child);
        return Ok(Some(ModuleTarget { file: target.file, path }));
    }
    if !target.path.is_empty() {
        return Ok(None);
    }
    let tree = fai_syntax::item_tree(db, target.file);
    let Some((index, declaration)) =
        tree.imports.iter().enumerate().find(|(_, import)| import.name == child)
    else {
        return Ok(None);
    };
    if declaration.visibility != Visibility::Public && target.file != observer {
        return Err(failure(
            INVALID_IMPORT,
            format!("module alias `{child}` is private to its importing file"),
        ));
    }
    resolve_declaration(db, target.file, index, stack).map(Some)
}

/// Resolves an import binding or an automatically available top-level module.
pub fn module_target(
    db: &dyn Db,
    observer: SourceFile,
    name: Symbol,
) -> Result<Option<ModuleTarget>, ImportFailure> {
    if let Some(binding) = import_bindings(db, observer).iter().find(|binding| binding.name == name)
    {
        return binding.target.clone().map(Some);
    }
    Ok(module_file_from(db, observer, ModuleName(name))
        .map(|file| ModuleTarget { file, path: Vec::new() }))
}

/// Resolves the longest module prefix, leaving a value/type member to its caller.
pub fn module_prefix(
    db: &dyn Db,
    observer: SourceFile,
    segments: &[Symbol],
) -> Result<Option<(ModuleTarget, usize)>, ImportFailure> {
    let Some(&head) = segments.first() else {
        return Ok(None);
    };
    let Some(mut target) = module_target(db, observer, head)? else {
        return Ok(None);
    };
    let mut consumed = 1;
    while let Some(&segment) = segments.get(consumed) {
        let Some(child) = child_module(db, observer, &target, segment, &mut Vec::new())? else {
            break;
        };
        target = child;
        consumed += 1;
    }
    Ok(Some((target, consumed)))
}

/// Resolves a path consisting entirely of module/re-export names.
pub fn module_path_target(
    db: &dyn Db,
    observer: SourceFile,
    segments: &[Symbol],
) -> Option<ModuleTarget> {
    module_prefix(db, observer, segments)
        .ok()
        .flatten()
        .and_then(|(target, consumed)| (consumed == segments.len()).then_some(target))
}

/// Module bindings that can be offered by completion or qualification fixes.
pub fn visible_module_bindings(db: &dyn Db, observer: SourceFile) -> Vec<(Symbol, ModuleTarget)> {
    let mut bindings = Vec::new();
    for &source in fai_db::automatically_visible_sources(db, observer).iter() {
        let Some(ModuleName(name)) = module_name(db, source) else {
            continue;
        };
        if module_file_from(db, observer, ModuleName(name)) == Some(source) {
            bindings.push((name, ModuleTarget { file: source, path: Vec::new() }));
        }
    }
    for binding in import_bindings(db, observer).iter() {
        if let Ok(target) = &binding.target {
            bindings.push((binding.name, target.clone()));
        }
    }
    bindings.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    bindings
}

/// Emits located import failures after semantic binding results have been cached.
pub fn emit_import_diagnostics(db: &dyn Db, file: SourceFile) {
    let parsed = fai_syntax::parse(db, file);
    for (index, binding) in import_bindings(db, file).iter().enumerate() {
        let Err(error) = &binding.target else {
            continue;
        };
        let Some(import) = parsed.module.imports.get(index) else {
            continue;
        };
        let range = if error.code == UNKNOWN_DEPENDENCY {
            import.path_spans.first().copied().unwrap_or(import.span)
        } else if error.code == IMPORT_NAME_CONFLICT {
            import.alias_span.or_else(|| import.path_spans.last().copied()).unwrap_or(import.span)
        } else {
            import.span
        };
        emit(
            db,
            Diagnostic::error(error.code, error.message.clone(), Span::new(file.source(db), range)),
        );
    }
}
