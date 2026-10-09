//! Prepared test spans retain their original coordinates independently of later inputs.

use fai_db::{Db, DbSpanResolver};
use fai_span::SpanResolver;

fn plan() -> (fai_db::FaiDatabase, fai_db::SourceFile, fai_driver::TestPlan) {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), "module M\nexample: true\nexample: false\n".into());
    let file = db.source_file(id).unwrap();
    let plan = fai_driver::build_test_plan(&db, &[file], None, fai_driver::TestConfig::default());
    (db, file, plan)
}

#[test]
fn edited_inputs_do_not_reinterpret_prepared_test_spans() {
    let (mut db, _, plan) = plan();
    let span = plan.runnable_meta[1].span;
    let before = plan.render_spans.resolve(span).unwrap();
    assert_eq!(before.start.line, 3);
    db.add_source(
        "M.fai".into(),
        "module M\n// 🌍 changed layout\n\n\nexample: true\nexample: false\n".into(),
    );
    assert_eq!(plan.render_spans.resolve(span), Some(before.clone()));
    assert_ne!(DbSpanResolver::new(&db).resolve(span), Some(before));
}

#[test]
fn deleting_a_file_keeps_its_test_report_coordinates() {
    let (mut db, file, plan) = plan();
    let span = plan.runnable_meta[1].span;
    let before = plan.render_spans.resolve(span);
    db.remove_sources([file.source(&db)]);
    assert_eq!(plan.render_spans.resolve(span), before);
}
