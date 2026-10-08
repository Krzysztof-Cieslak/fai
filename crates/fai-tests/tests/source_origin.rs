//! Source trust is loader metadata, never a filesystem naming convention.

use fai_db::{Db, FaiDatabase, SourceOrigin};
use fai_syntax::Symbol;

#[test]
fn identical_paths_have_independent_user_and_library_identities() {
    let mut db = FaiDatabase::new();
    let path = "<std>/Same.fai";
    let standard = db.add_source_with_origin(
        path.into(),
        "module Same\nlet value = 1\n".into(),
        SourceOrigin::StandardLibrary,
    );
    let user = db.add_source(path.into(), "module User\nlet value = 2\n".into());
    assert_ne!(standard, user);
    let standard_file = db.source_file(standard).unwrap();
    let user_file = db.source_file(user).unwrap();
    assert!(standard_file.is_std(&db));
    assert!(!user_file.is_std(&db));
    assert_eq!(db.id_for_path(path.as_ref()), Some(user));
    let snapshot = db.clone();
    assert_eq!(
        snapshot.source_file(standard).unwrap().origin(&snapshot),
        SourceOrigin::StandardLibrary
    );
    assert_eq!(snapshot.source_file(user).unwrap().origin(&snapshot), SourceOrigin::User);
    drop(snapshot);
    db.add_source(path.into(), "module User\nlet value = 3\n".into());
    assert_eq!(standard_file.text(&db), "module Same\nlet value = 1\n");
    db.remove_source(user);
    assert!(db.source_file(standard).is_some());
    assert_eq!(db.id_for_path(path.as_ref()), None);
}

#[test]
fn high_durability_does_not_grant_library_origin() {
    let mut db = FaiDatabase::new();
    let id = db.add_source_with_durability(
        "<std>/User.fai".into(),
        "module User\n".into(),
        fai_db::Durability::HIGH,
    );
    assert_eq!(db.source_file(id).unwrap().origin(&db), SourceOrigin::User);
}

#[test]
fn a_std_looking_user_path_cannot_call_intrinsics() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = "module User\nlet value = Prim.not true\n";
    let id = db.add_source("<std>/User.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let errors = fai_resolve::resolve::accumulated::<fai_db::Diag>(&db, file);
    let error = errors
        .iter()
        .find(|error| error.0.code.as_str() == "FAI2014")
        .expect("intrinsic access rejected");
    assert_eq!(error.0.primary.source(), id);
    assert_eq!(
        &source[error.0.primary.start().raw() as usize..error.0.primary.end().raw() as usize],
        "Prim.not"
    );
}

#[test]
fn an_explicit_library_origin_does_not_require_a_path_prefix() {
    let mut db = FaiDatabase::new();
    let id = db.add_source_with_origin(
        "Trusted.fai".into(),
        "module Trusted\nlet value = Prim.not true\n".into(),
        SourceOrigin::StandardLibrary,
    );
    let file = db.source_file(id).unwrap();
    assert!(fai_resolve::resolve::accumulated::<fai_db::Diag>(&db, file).is_empty());
}

#[test]
fn a_std_looking_user_path_cannot_access_internal_library_members() {
    let mut db = FaiDatabase::new();
    db.add_source_with_origin(
        "Library.fai".into(),
        "module Library\ninternal value : Int\nlet value = 1\n".into(),
        SourceOrigin::StandardLibrary,
    );
    let id =
        db.add_source("<std>/User.fai".into(), "module User\nlet value = Library.value\n".into());
    let errors =
        fai_resolve::resolve::accumulated::<fai_db::Diag>(&db, db.source_file(id).unwrap());
    assert!(errors.iter().any(|error| error.0.code.as_str() == "FAI2020"));
}

#[test]
fn user_foreign_under_a_std_looking_path_keeps_its_marshaled_abi() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "<std>/Native.fai".into(),
        "module Native\nforeign \"increment\" increment : Int -> Int / { Console }\n".into(),
    );
    let lowered = fai_core::core(&db, db.source_file(id).unwrap(), Symbol::intern("increment"));
    assert!(matches!(
        lowered.entry().body.kind,
        fai_core::ir::ExprKind::Foreign { marshalled: true, .. }
    ));
}

#[test]
fn user_foreign_under_a_std_looking_path_still_checks_marshallable_types() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "<std>/Native.fai".into(),
        "module Native\nforeign \"native\" native : { value : Int } -> Int / { Console }\n".into(),
    );
    let errors =
        fai_types::check_file::accumulated::<fai_db::Diag>(&db, db.source_file(id).unwrap());
    assert!(errors.iter().any(|error| error.0.code.as_str() == "FAI5003"));
}

#[test]
fn user_edits_cannot_invalidate_the_same_named_embedded_input() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let library = db
        .all_source_files()
        .into_iter()
        .find(|file| file.path(&db).ends_with("/List.fai"))
        .unwrap();
    let path = library.path(&db).clone();
    db.add_source(path.clone().into(), "module User\nlet value = 1\n".into());
    let before = fai_types::def_type(&db, library, Symbol::intern("length"));
    db.enable_event_log();
    db.add_source(path.into(), "module User\nlet value = 2\n".into());
    assert_eq!(fai_types::def_type(&db, library, Symbol::intern("length")), before);
    let events = db.take_events();
    assert!(!events.iter().any(|event| event.contains("infer_scc_query")), "{events:?}");
}

#[test]
fn source_origin_survives_incremental_membership_edits() {
    let unsafe_source = "module User\nlet value = Prim.not true\n";
    let safe_source = "module User\nlet value = true\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[
            &[("<std>/User.fai", unsafe_source)],
            &[("<std>/User.fai", safe_source)],
            &[],
            &[("<std>/User.fai", unsafe_source)],
        ],
        |db, files| {
            let mut errors: Vec<_> = files
                .iter()
                .flat_map(|id| {
                    fai_resolve::resolve::accumulated::<fai_db::Diag>(
                        db,
                        db.source_file(*id).unwrap(),
                    )
                    .into_iter()
                    .map(|error| error.0.code.as_str().to_owned())
                })
                .collect();
            errors.sort();
            (db.all_source_files().into_iter().filter(|file| file.is_std(db)).count(), errors)
        },
    );
}

#[test]
fn rename_preserves_origin_and_edits_only_the_colliding_user_source() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let standard = db
        .all_source_files()
        .into_iter()
        .find(|file| file.path(&db).ends_with("/List.fai"))
        .unwrap();
    let original = standard.text(&db).clone();
    let text = "module Fake\nlet value = List.length [1]\nlet answer = value\n";
    let id = db.add_source(standard.path(&db).clone().into(), text.into());
    let user = db.source_file(id).unwrap();
    let edits = fai_ide::checked_rename_at(
        &db,
        &[user],
        user,
        text.find("value").unwrap() as u32,
        "renamed",
        &fai_db::DbSpanResolver::new(&db),
    )
    .unwrap();
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|edit| edit.span.source == id));
    assert_eq!(standard.text(&db), &original);
    assert_eq!(user.text(&db), text);
}

#[cfg(unix)]
#[test]
fn disk_files_cannot_replace_embedded_inputs_or_hide_from_selection() {
    let directory = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let mut session = fai_driver::Session::open(root.clone()).unwrap();
    let standard = session
        .db()
        .all_source_files()
        .into_iter()
        .find(|file| file.is_std(session.db()) && file.path(session.db()).ends_with("/List.fai"))
        .unwrap();
    let path = standard.path(session.db()).clone();
    let original = standard.text(session.db()).clone();
    let disk = root.join(&path);
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    std::fs::write(&disk, "module User\nlet value = Prim.not true\n").unwrap();
    session.sync_from_disk().unwrap();
    let users = session.select_files(Some(path.as_ref()));
    assert_eq!(users.len(), 1);
    assert!(!users[0].is_std(session.db()));
    assert_ne!(users[0].source(session.db()), standard.source(session.db()));
    assert_eq!(standard.text(session.db()), &original);
    let checked = fai_driver::check(session.db(), &users);
    assert!(checked.diagnostics.iter().any(|error| error.code.as_str() == "FAI2014"));
    std::fs::remove_file(&disk).unwrap();
    session.sync_from_disk().unwrap();
    assert!(session.user_files().is_empty());
    assert_eq!(standard.text(session.db()), &original);
}
