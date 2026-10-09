//! Comparison totality remains incremental and cuts off unchanged safety facts.

use fai_db::Db;
use fai_syntax::Symbol;

#[test]
fn comparison_safety_edits_match_clean_analysis() {
    let scalar = "module M\npublic same : Int -> Bool\nlet same x = x = x\n";
    let generic = "module M\npublic same : 'a -> Bool\nlet same x = x = x\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", scalar)], &[("M.fai", generic)], &[("M.fai", scalar)]],
        |db, ids| {
            fai_core::purity::application_pure_total(
                db,
                db.source_file(ids[0]).unwrap(),
                Symbol::intern("same"),
                1,
            )
        },
    );
}

#[test]
fn unchanged_comparison_totality_cuts_off_callers() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = "module M\ninner : Int -> Bool\nlet inner x = x = 0\nlet outer x = inner x\n";
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let name = Symbol::intern("outer");
    assert!(fai_core::purity::application_pure_total(&db, file, name, 1));
    db.enable_event_log();
    db.add_source("M.fai".into(), source.replace("x = 0", "x = 1"));
    assert!(fai_core::purity::application_pure_total(&db, file, name, 1));
    let events = db.take_events();
    assert_eq!(
        events.iter().filter(|event| event.contains("application_pure_total")).count(),
        1,
        "{events:?}"
    );
}
