//! Stable source-site keys for testing constructors; coordinates are resolved late.

use fai_db::{Db, SourceFile, SourceOrigin};
use fai_resolve::{DefId, ModuleName, module_name};
use fai_syntax::{Symbol, ast::ExprId};

/// A position-independent key carried through native test values.
#[must_use]
pub fn test_location_key(db: &dyn Db, file: SourceFile, expr: ExprId) -> String {
    let origin = match file.origin(db) {
        SourceOrigin::Package(id) => fai_db::package_environment(db, id)
            .map(|package| package.key(db).clone())
            .unwrap_or_default(),
        SourceOrigin::StandardLibrary => "std".to_owned(),
        SourceOrigin::User => "user".to_owned(),
    };
    let path = file.path(db);
    format!("{}:{origin}{}:{path}:{}", origin.len(), path.len(), expr.index())
}

/// The location-aware entry for a resolved testing constructor, if instrumented.
pub fn test_location_target(db: &dyn Db, caller: SourceFile, def: DefId) -> Option<DefId> {
    if caller.origin(db) == SourceOrigin::StandardLibrary {
        return None;
    }
    let file = db.source_file(def.file)?;
    if file.origin(db) != SourceOrigin::StandardLibrary {
        return None;
    }
    let module = module_name(db, file)?;
    let target = match (module.0.as_str(), def.name.as_str()) {
        ("Test", "test") => "testAt",
        ("Test", "property") => "propertyAt",
        ("Test", "describe") => "describeAt",
        ("Test", "error") => "errorAt",
        ("Expect", "equal") => "equalAt",
        ("Expect", "isTrue") => "isTrueAt",
        ("Expect", "fail") => "failAt",
        _ => return None,
    };
    Some(DefId::new(file.source(db), Symbol::intern(target)))
}

pub(crate) fn is_check(db: &dyn Db, ty: &fai_types::Ty) -> bool {
    let fai_types::Ty::Adt(adt) = ty else { return false };
    let Some(file) = db.source_file(adt.file) else { return false };
    file.origin(db) == SourceOrigin::StandardLibrary
        && module_name(db, file) == Some(ModuleName(Symbol::intern("Test")))
        && adt.name.as_str() == "Check"
}
