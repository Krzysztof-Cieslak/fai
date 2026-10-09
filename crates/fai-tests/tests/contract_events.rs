//! Contract event keys distinguish sources independently of labels and batch positions.

use fai_db::{Db, FaiDatabase, SourceOrigin};
use fai_driver::{ContractSource, ContractSourceOrigin, TestConfig, build_test_plan};

fn source(file: &str, origin: ContractSourceOrigin) -> ContractSource {
    ContractSource { file: file.to_owned(), origin }
}

#[test]
fn subjectless_contracts_in_matching_basenames_have_distinct_keys() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let a = db.add_source("left/Same.fai".into(), "module A\nexample: true\n".into());
    let b = db.add_source("right/Same.fai".into(), "module B\nexample: true\n".into());
    let files = [db.source_file(a).unwrap(), db.source_file(b).unwrap()];
    let plan = build_test_plan(&db, &files, None, TestConfig::default());
    assert!(!plan.blocked, "{:?}", plan.pre_diagnostics);
    let keys: Vec<_> = plan
        .runnable_meta
        .iter()
        .map(|m| (m.source.clone(), m.ordinal, m.symbol.clone()))
        .collect();
    assert_eq!(
        keys,
        vec![
            (source("left/Same.fai", ContractSourceOrigin::User), 0, None),
            (source("right/Same.fai", ContractSourceOrigin::User), 0, None),
        ]
    );
}

#[test]
fn filtering_preserves_file_local_ordinals_and_repeated_subjects() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), "module Main\nlet other x = x\nexample: other 1 = 1\nlet selected x = x\nexample: selected 1 = 1\nexample: selected 2 = 2\n".into());
    let files = [db.source_file(id).unwrap()];
    let all = build_test_plan(&db, &files, None, TestConfig::default());
    let selected = build_test_plan(&db, &files, Some("selected"), TestConfig::default());
    assert!(!all.blocked, "{:?}", all.pre_diagnostics);
    assert_eq!(selected.runnable_meta.len(), 2);
    let all_keys: Vec<_> =
        all.runnable_meta.iter().map(|m| (m.source.clone(), m.ordinal, m.symbol.clone())).collect();
    let selected_keys: Vec<_> = selected
        .runnable_meta
        .iter()
        .map(|m| (m.source.clone(), m.ordinal, m.symbol.clone()))
        .collect();
    assert_eq!(selected_keys, all_keys[1..]);
    assert_eq!(selected.runnable_meta[0].symbol, selected.runnable_meta[1].symbol);
    assert_ne!(selected.runnable_meta[0].ordinal, selected.runnable_meta[1].ordinal);
}

#[test]
fn embedded_and_user_sources_with_the_same_path_remain_distinct() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let embedded = db.add_source_with_origin(
        "<std>/Collision.fai".into(),
        "module Embedded\nexample: true\n".into(),
        SourceOrigin::StandardLibrary,
    );
    let user = db.add_source("<std>/Collision.fai".into(), "module User\nexample: true\n".into());
    let files = [db.source_file(embedded).unwrap(), db.source_file(user).unwrap()];
    let plan = build_test_plan(&db, &files, None, TestConfig::default());
    assert!(!plan.blocked, "{:?}", plan.pre_diagnostics);
    assert_eq!(
        plan.runnable_meta.iter().map(|m| (m.source.clone(), m.ordinal)).collect::<Vec<_>>(),
        vec![
            (source("<std>/Collision.fai", ContractSourceOrigin::StandardLibrary), 0),
            (source("<std>/Collision.fai", ContractSourceOrigin::User), 0),
        ]
    );
}
