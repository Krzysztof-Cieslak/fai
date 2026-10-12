//! Package ownership, target-local source visibility, and dependency environments.

use std::{collections::BTreeMap, sync::Arc};

use crate::{Db, SourceFile, SourceOrigin};

/// A database-local package instantiation, registered under a stable source key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId(pub(crate) u32);

impl PackageId {
    /// Its index in the database's package registry.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A source's role within its package, independent of its filesystem location.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum SourceRole {
    /// A shared production module.
    #[default]
    Library,
    /// A named executable's private source scope.
    Executable(String),
    /// Additional package tests, with access to development dependencies.
    Test,
}

/// Dependency bindings and stable identity of one package instantiation.
#[salsa::input(debug)]
pub struct PackageEnvironment {
    /// The registry identity used by source ownership.
    pub id: PackageId,
    /// Stable source/instantiation key, independent of registration order.
    #[returns(ref)]
    pub key: String,
    /// Explicitly declared normal dependency aliases.
    #[returns(ref)]
    pub dependencies: BTreeMap<String, PackageId>,
    /// Aliases available only in additional test sources.
    #[returns(ref)]
    pub development: BTreeMap<String, PackageId>,
}

#[salsa::input(singleton, debug)]
pub(crate) struct PackageRegistry {
    #[returns(ref)]
    pub entries: Vec<PackageEnvironment>,
}

#[salsa::tracked]
fn registered_package(
    db: &dyn Db,
    registry: PackageRegistry,
    id: PackageId,
) -> Option<PackageEnvironment> {
    registry.entries(db).get(id.index()).copied()
}

/// Retrieves a registered package with a per-id incremental projection.
pub fn package_environment(db: &dyn Db, id: PackageId) -> Option<PackageEnvironment> {
    registered_package(db, PackageRegistry::get(db), id)
}

/// Package environments in registration order, for faithful scratch-database copies.
pub fn package_environments(db: &dyn Db) -> Vec<PackageEnvironment> {
    PackageRegistry::get(db).entries(db).clone()
}

/// Looks up a direct dependency in the referring file's own environment.
pub fn dependency_package(db: &dyn Db, file: SourceFile, alias: &str) -> Option<PackageId> {
    let SourceOrigin::Package(id) = file.origin(db) else { return None };
    let package = package_environment(db, id)?;
    package.dependencies(db).get(alias).copied().or_else(|| {
        matches!(file.role(db), SourceRole::Test)
            .then(|| package.development(db).get(alias).copied())
            .flatten()
    })
}

/// Whether `target` is automatically visible to `from`, before external imports.
pub fn automatically_visible(db: &dyn Db, from: SourceFile, target: SourceFile) -> bool {
    if target.is_std(db) {
        return true;
    }
    if from.origin(db) != target.origin(db) {
        return false;
    }
    match target.role(db) {
        SourceRole::Library => true,
        SourceRole::Executable(name) => {
            matches!(from.role(db), SourceRole::Executable(other) if name == other)
        }
        SourceRole::Test => matches!(from.role(db), SourceRole::Test),
    }
}

/// A visibility firewall: unrelated package membership changes stop here.
#[salsa::tracked]
pub fn automatically_visible_sources(db: &dyn Db, file: SourceFile) -> Arc<Vec<SourceFile>> {
    Arc::new(
        db.all_source_files()
            .into_iter()
            .filter(|&target| automatically_visible(db, file, target))
            .collect(),
    )
}

/// Shared library files owned by an explicitly imported dependency.
#[salsa::tracked]
pub fn package_library_sources(db: &dyn Db, package: PackageEnvironment) -> Arc<Vec<SourceFile>> {
    let origin = SourceOrigin::Package(package.id(db));
    Arc::new(
        db.all_source_files()
            .into_iter()
            .filter(|file| {
                file.origin(db) == origin && matches!(file.role(db), SourceRole::Library)
            })
            .collect(),
    )
}
