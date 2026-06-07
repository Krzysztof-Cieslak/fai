//! Type and constructor declarations: a per-file index of `type` declarations.
//!
//! [`type_decls`] is a `salsa` query keyed on a file. Its value, [`TypeDecls`],
//! is span-free (only names, visibility, arities, tags, and arena [`ItemId`]s),
//! so it travels safely inside cached values. The types phase reads the AST via
//! the stored `item` to recover field types and alias bodies.

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemId, ItemKind, TypeDef, Visibility};
use rustc_hash::FxHashMap;

/// A `type` declaration's position-independent summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDeclInfo {
    /// The type's name.
    pub name: Symbol,
    /// The type's visibility (constructors inherit it).
    pub visibility: Visibility,
    /// The declared type parameters, in order (e.g. `'a 'b`).
    pub params: Vec<Symbol>,
    /// The declaring item (to fetch field types / alias bodies from the AST).
    pub item: ItemId,
    /// `true` for a transparent alias, `false` for a discriminated union.
    pub is_alias: bool,
    /// The constructor names of a union, in declaration order (empty for alias).
    pub ctors: Vec<Symbol>,
}

/// A data constructor's position-independent summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtorInfo {
    /// The constructor's name.
    pub name: Symbol,
    /// The owning type's name.
    pub adt: Symbol,
    /// The runtime tag (declaration order within the union).
    pub tag: u32,
    /// The number of fields the constructor takes.
    pub arity: usize,
    /// The constructor's index among its union's variants.
    pub variant_index: usize,
    /// The constructor's visibility (inherited from its type).
    pub visibility: Visibility,
}

/// A file's declared types and constructors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeDecls {
    /// Declared types, by name.
    pub types: FxHashMap<Symbol, TypeDeclInfo>,
    /// Declared data constructors, by name.
    pub ctors: FxHashMap<Symbol, CtorInfo>,
}

impl TypeDecls {
    /// The declaration of type `name`, if any.
    #[must_use]
    pub fn type_named(&self, name: Symbol) -> Option<&TypeDeclInfo> {
        self.types.get(&name)
    }

    /// The constructor `name`, if any.
    #[must_use]
    pub fn ctor(&self, name: Symbol) -> Option<&CtorInfo> {
        self.ctors.get(&name)
    }
}

/// An `interface` declaration's position-independent summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceInfo {
    /// The interface's name.
    pub name: Symbol,
    /// The interface's visibility.
    pub visibility: Visibility,
    /// The declared type parameters, in order.
    pub params: Vec<Symbol>,
    /// The declaring item (to fetch method types from the AST).
    pub item: ItemId,
    /// The method names, in declaration order.
    pub methods: Vec<Symbol>,
}

/// A file's declared interfaces, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterfaceDecls {
    /// Declared interfaces, by name.
    pub interfaces: FxHashMap<Symbol, InterfaceInfo>,
}

impl InterfaceDecls {
    /// The declaration of interface `name`, if any.
    #[must_use]
    pub fn interface_named(&self, name: Symbol) -> Option<&InterfaceInfo> {
        self.interfaces.get(&name)
    }
}

/// Indexes the `interface` declarations of `file` (pure; no diagnostics).
///
/// Walks nested modules; an interface's `name` is qualified by its module path.
#[salsa::tracked]
pub fn interface_decls(db: &dyn Db, file: SourceFile) -> Arc<InterfaceDecls> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let mut decls = InterfaceDecls::default();
    let mut scope: Vec<Symbol> = Vec::new();
    collect_interfaces(module, &mut scope, &module.roots, &mut decls);
    Arc::new(decls)
}

fn collect_interfaces(
    module: &fai_syntax::ast::Module,
    scope: &mut Vec<Symbol>,
    items: &[ItemId],
    decls: &mut InterfaceDecls,
) {
    for &id in items {
        match &module.items[id.index()].kind {
            ItemKind::Interface { visibility, name, params, methods } => {
                let qual = crate::qualify(scope, *name);
                decls.interfaces.entry(qual).or_insert(InterfaceInfo {
                    name: qual,
                    visibility: *visibility,
                    params: params.clone(),
                    item: id,
                    methods: methods.iter().map(|m| m.name).collect(),
                });
            }
            ItemKind::Module { name, body } => {
                scope.push(*name);
                collect_interfaces(module, scope, body, decls);
                scope.pop();
            }
            _ => {}
        }
    }
}

/// Indexes the `type` declarations of `file` (pure; no diagnostics).
///
/// Walks nested modules; type and constructor `name`s (and a constructor's owning
/// `adt`) are qualified by their module path. Duplicate names keep the first
/// declaration (later duplicates are reported by the types phase, which has spans).
#[salsa::tracked]
pub fn type_decls(db: &dyn Db, file: SourceFile) -> Arc<TypeDecls> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let mut decls = TypeDecls::default();
    let mut scope: Vec<Symbol> = Vec::new();
    collect_types(module, &mut scope, &module.roots, &mut decls);
    Arc::new(decls)
}

fn collect_types(
    module: &fai_syntax::ast::Module,
    scope: &mut Vec<Symbol>,
    items: &[ItemId],
    decls: &mut TypeDecls,
) {
    for &id in items {
        match &module.items[id.index()].kind {
            ItemKind::Type { visibility, name, params, def } => {
                let qual = crate::qualify(scope, *name);
                let (is_alias, ctor_names) = match def {
                    TypeDef::Alias(_) => (true, Vec::new()),
                    TypeDef::Union(variants) => {
                        let mut names = Vec::with_capacity(variants.len());
                        for (variant_index, variant) in variants.iter().enumerate() {
                            let ctor_qual = crate::qualify(scope, variant.name);
                            names.push(ctor_qual);
                            let tag = u32::try_from(variant_index).unwrap_or(u32::MAX);
                            decls.ctors.entry(ctor_qual).or_insert(CtorInfo {
                                name: ctor_qual,
                                adt: qual,
                                tag,
                                arity: variant.fields.len(),
                                variant_index,
                                visibility: *visibility,
                            });
                        }
                        (false, names)
                    }
                };
                decls.types.entry(qual).or_insert(TypeDeclInfo {
                    name: qual,
                    visibility: *visibility,
                    params: params.clone(),
                    item: id,
                    is_alias,
                    ctors: ctor_names,
                });
            }
            ItemKind::Module { name, body } => {
                scope.push(*name);
                collect_types(module, scope, body, decls);
                scope.pop();
            }
            _ => {}
        }
    }
}
