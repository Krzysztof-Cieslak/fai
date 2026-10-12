//! Package and target ownership determine lookup before source-level imports.

use std::collections::BTreeMap;

use fai_db::{Db, FaiDatabase, PackageId, SourceFile, SourceOrigin, SourceRole};
use fai_syntax::Symbol;

fn source(
    db: &mut FaiDatabase,
    package: PackageId,
    role: SourceRole,
    path: &str,
    text: &str,
) -> SourceFile {
    let id = db.add_source_scoped(path.into(), text.into(), SourceOrigin::Package(package), role);
    db.source_file(id).unwrap()
}

fn codes(db: &FaiDatabase, file: SourceFile) -> Vec<&'static str> {
    fai_driver::check(db, &[file])
        .diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == fai_diagnostics::Severity::Error)
        .map(|diagnostic| diagnostic.code.as_str())
        .collect()
}

#[test]
fn same_path_in_different_packages_has_distinct_source_identity() {
    let mut db = FaiDatabase::new();
    let a = db.register_package("a".into());
    let b = db.register_package("b".into());
    let left = source(&mut db, a, SourceRole::Library, "Same.fai", "module Same\nlet value = 1\n");
    let right =
        source(&mut db, b, SourceRole::Library, "Same.fai", "module Same\nlet value = true\n");
    assert_ne!(left.source(&db), right.source(&db));
    assert!(left.text(&db).contains("value = 1"));
    assert!(right.text(&db).contains("value = true"));
    assert_eq!(db.id_for_path("Same.fai".as_ref()), None);
}

#[test]
fn same_named_modules_resolve_within_their_packages() {
    let mut db = FaiDatabase::new();
    let a = db.register_package("a".into());
    let b = db.register_package("b".into());
    source(
        &mut db,
        a,
        SourceRole::Library,
        "a/Helper.fai",
        "module Helper\npublic value : Int\nlet value = 1\n",
    );
    source(
        &mut db,
        b,
        SourceRole::Library,
        "b/Helper.fai",
        "module Helper\npublic value : Bool\nlet value = true\n",
    );
    let left = source(
        &mut db,
        a,
        SourceRole::Library,
        "a/Use.fai",
        "module Use\nlet value = Helper.value\n",
    );
    let right = source(
        &mut db,
        b,
        SourceRole::Library,
        "b/Use.fai",
        "module Use\nlet value = Helper.value\n",
    );
    assert!(codes(&db, left).is_empty());
    assert!(codes(&db, right).is_empty());
    assert_eq!(
        fai_types::render_scheme(&fai_types::def_type(&db, left, Symbol::intern("value"))),
        "Int"
    );
    assert_eq!(
        fai_types::render_scheme(&fai_types::def_type(&db, right, Symbol::intern("value"))),
        "Bool"
    );
}

#[test]
fn another_packages_public_module_is_not_automatically_imported() {
    let mut db = FaiDatabase::new();
    let a = db.register_package("a".into());
    let b = db.register_package("b".into());
    source(
        &mut db,
        b,
        SourceRole::Library,
        "b/Other.fai",
        "module Other\npublic value : Int\nlet value = 1\n",
    );
    let caller = source(
        &mut db,
        a,
        SourceRole::Library,
        "a/Use.fai",
        "module Use\nlet value = Other.value\n",
    );
    assert!(codes(&db, caller).contains(&"FAI2008"));
}

#[test]
fn tests_can_reach_library_internal_values() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    source(
        &mut db,
        package,
        SourceRole::Library,
        "Library.fai",
        "module Library\ninternal value : Int\nlet value = 1\n",
    );
    let test = source(
        &mut db,
        package,
        SourceRole::Test,
        "Test.fai",
        "module Checks\nlet value = Library.value\n",
    );
    assert!(codes(&db, test).is_empty());
}

#[test]
fn production_does_not_acquire_test_module_visibility() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    source(
        &mut db,
        package,
        SourceRole::Test,
        "Helper.fai",
        "module Helper\npublic value : Int\nlet value = 1\n",
    );
    let library = source(
        &mut db,
        package,
        SourceRole::Library,
        "Library.fai",
        "module Library\nlet value = Helper.value\n",
    );
    assert!(codes(&db, library).contains(&"FAI2008"));
}

#[test]
fn executable_entries_are_mutually_isolated() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    let a = source(
        &mut db,
        package,
        SourceRole::Executable("a".into()),
        "a/Main.fai",
        "module Main\nlet value = 1\n",
    );
    let b = source(
        &mut db,
        package,
        SourceRole::Executable("b".into()),
        "b/Main.fai",
        "module Main\nlet value = true\n",
    );
    assert!(codes(&db, a).is_empty());
    assert!(codes(&db, b).is_empty());
    assert_eq!(
        fai_resolve::module_file_from(&db, a, fai_resolve::ModuleName(Symbol::intern("Main"))),
        Some(a)
    );
    assert_eq!(
        fai_resolve::module_file_from(&db, b, fai_resolve::ModuleName(Symbol::intern("Main"))),
        Some(b)
    );
}

#[test]
fn test_collision_does_not_hide_the_production_module_from_production() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    let library = source(
        &mut db,
        package,
        SourceRole::Library,
        "Library.fai",
        "module Shared\nlet value = 1\n",
    );
    let test =
        source(&mut db, package, SourceRole::Test, "Test.fai", "module Shared\nlet value = true\n");
    assert!(codes(&db, library).is_empty());
    assert!(codes(&db, test).contains(&"FAI2007"));
}

#[test]
fn dependency_aliases_are_owned_by_the_referring_package_and_role() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let normal = db.register_package("normal".into());
    let development = db.register_package("development".into());
    db.set_package_dependencies(
        app,
        BTreeMap::from([("lib".into(), normal)]),
        BTreeMap::from([("test".into(), development)]),
    );
    let library = source(&mut db, app, SourceRole::Library, "Library.fai", "module Library\n");
    let test = source(&mut db, app, SourceRole::Test, "Test.fai", "module TestCases\n");
    assert_eq!(fai_db::dependency_package(&db, library, "lib"), Some(normal));
    assert_eq!(fai_db::dependency_package(&db, library, "test"), None);
    assert_eq!(fai_db::dependency_package(&db, test, "test"), Some(development));
}

#[test]
fn removed_scoped_source_reactivates_with_the_same_identity() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    let first = source(&mut db, package, SourceRole::Library, "File.fai", "module File\n");
    let id = first.source(&db);
    db.remove_source(id);
    let second =
        source(&mut db, package, SourceRole::Library, "File.fai", "module File\nlet value = 1\n");
    assert_eq!(id, second.source(&db));
}

#[test]
fn unrelated_package_membership_does_not_reinfer_an_existing_package() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    let file =
        source(&mut db, package, SourceRole::Library, "File.fai", "module File\nlet value = 1\n");
    fai_types::check_file(&db, file);
    db.enable_event_log();
    let other = db.register_package("other".into());
    source(
        &mut db,
        other,
        SourceRole::Library,
        "other/File.fai",
        "module File\nlet value = true\n",
    );
    fai_types::check_file(&db, file);
    let events = db.take_events();
    assert!(!events.iter().any(|event| event.contains("infer_scc_query")), "{events:?}");
}

#[test]
fn package_context_survives_checked_rename_scratch_validation() {
    let mut db = FaiDatabase::new();
    let package = db.register_package("app".into());
    let text = "module Library\nlet value = 1\nlet answer = value\n";
    let file = source(&mut db, package, SourceRole::Library, "Library.fai", text);
    let edits = fai_ide::checked_rename_at(
        &db,
        &[file],
        file,
        text.find("value").unwrap() as u32,
        "renamed",
        &fai_db::DbSpanResolver::new(&db),
    )
    .unwrap();
    assert_eq!(edits.len(), 2);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn set_helper(db: &mut FaiDatabase, package: PackageId, present: bool, test_only: bool) {
        for role in [SourceRole::Library, SourceRole::Test] {
            if let Some(id) =
                db.id_for_scoped_path("Helper.fai".as_ref(), SourceOrigin::Package(package), &role)
            {
                db.remove_source(id);
            }
        }
        if present {
            source(
                db,
                package,
                if test_only { SourceRole::Test } else { SourceRole::Library },
                "Helper.fai",
                "module Helper\npublic value : Int\nlet value = 1\n",
            );
        }
    }

    proptest! {
        #[test]
        fn scoped_membership_incremental_matches_clean(states in prop::collection::vec((any::<bool>(), any::<bool>()), 1..8)) {
            let mut incremental = FaiDatabase::new();
            let package = incremental.register_package("app".into());
            let caller_text = "module Caller\nlet value = Helper.value\n";
            let caller = source(&mut incremental, package, SourceRole::Library, "Caller.fai", caller_text);
            for (present, test_only) in states {
                set_helper(&mut incremental, package, present, test_only);
                let mut clean = FaiDatabase::new();
                let clean_package = clean.register_package("app".into());
                let clean_caller = source(&mut clean, clean_package, SourceRole::Library, "Caller.fai", caller_text);
                set_helper(&mut clean, clean_package, present, test_only);
                prop_assert_eq!(codes(&incremental, caller), codes(&clean, clean_caller));
                prop_assert_eq!(
                    fai_types::render_scheme(&fai_types::def_type(&incremental, caller, Symbol::intern("value"))),
                    fai_types::render_scheme(&fai_types::def_type(&clean, clean_caller, Symbol::intern("value")))
                );
            }
        }
    }
}
