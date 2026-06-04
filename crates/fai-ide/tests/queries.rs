//! JSON snapshot tests for the `fai query` commands over a small workspace.

use fai_db::{Db, DbSpanResolver, FaiDatabase, SourceFile};
use fai_ide::{ListOpts, api, def, dependents, outline, refs, symbols, type_at};

fn workspace() -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    let a = db.add_source(
        "A.fai".into(),
        "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n\npublic twice : ('a -> 'a) -> 'a -> 'a\nlet twice f = f >> f\n".to_owned(),
    );
    let b = db.add_source(
        "B.fai".into(),
        "module B\n\npublic two : Int\nlet two = A.inc 1\n\nlet four = A.inc (A.inc two)\n"
            .to_owned(),
    );
    let files = vec![db.source_file(a).unwrap(), db.source_file(b).unwrap()];
    (db, files)
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

#[test]
fn symbols_snapshot() {
    let (db, files) = workspace();
    let r = symbols(&db, &files, None, &DbSpanResolver::new(&db), ListOpts::default());
    insta::assert_snapshot!("symbols", json(&r));
}

#[test]
fn def_snapshot() {
    let (db, _files) = workspace();
    let r = def(&db, "A.inc", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("def_A_inc", json(&r));
}

#[test]
fn type_snapshot() {
    let (db, _files) = workspace();
    let r = type_at(&db, "A.twice", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("type_A_twice", json(&r));
}

#[test]
fn refs_snapshot() {
    let (db, files) = workspace();
    let r = refs(&db, &files, "A.inc", &DbSpanResolver::new(&db), ListOpts::default());
    insta::assert_snapshot!("refs_A_inc", json(&r));
}

#[test]
fn dependents_snapshot() {
    let (db, files) = workspace();
    let r = dependents(&db, &files, "A.inc", &DbSpanResolver::new(&db), ListOpts::default());
    insta::assert_snapshot!("dependents_A_inc", json(&r));
}

#[test]
fn outline_snapshot() {
    let (db, files) = workspace();
    let r = outline(&db, "A", &files, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("outline_A", json(&r));
}

#[test]
fn api_snapshot() {
    let (db, files) = workspace();
    let r = api(&db, "A", &files, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("api_A", json(&r));
}

#[test]
fn type_under_error_is_best_effort() {
    // A workspace with a type error still answers a `type` query for a good def.
    let mut db = FaiDatabase::new();
    let id = db.add_source(
        "M.fai".into(),
        "module M\n\npublic good : Int -> Int\nlet good x = x + 1\n\npublic bad : Int -> Bool\nlet bad x = x + 1\n".to_owned(),
    );
    let _ = db.source_file(id).unwrap();
    let r = type_at(&db, "M.good", &DbSpanResolver::new(&db));
    assert_eq!(r.ty.display, "Int -> Int");
}
