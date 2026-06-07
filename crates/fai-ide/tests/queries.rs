//! JSON snapshot tests for the `fai query` commands over a small workspace.

use fai_db::{Db, DbSpanResolver, FaiDatabase, SourceFile};
use fai_ide::{
    ListOpts, api, callees, callers, def, dependents, outline, refs, search, symbols, type_at,
};
use indoc::indoc;

fn workspace() -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    let a = db.add_source(
        "A.fai".into(),
        indoc! {r#"
            module A

            public inc : Int -> Int
            let inc x = x + 1

            public twice : ('a -> 'a) -> 'a -> 'a
            let twice f = f >> f
        "#}
        .to_owned(),
    );
    let b = db.add_source(
        "B.fai".into(),
        indoc! {r#"
            module B

            public two : Int
            let two = A.inc 1

            let four = A.inc (A.inc two)
        "#}
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
    let r = dependents(&db, &files, "A.inc", &DbSpanResolver::new(&db), false, ListOpts::default());
    insta::assert_snapshot!("dependents_A_inc", json(&r));
}

#[test]
fn dependents_transitive_follows_the_chain() {
    let mut db = FaiDatabase::new();
    let c = db.add_source(
        "C.fai".into(),
        indoc! {r#"
            module C

            public base : Int
            let base = 0

            let mid = base + 1

            let top = mid + 1
        "#}
        .to_owned(),
    );
    let files = vec![db.source_file(c).unwrap()];
    let names = |transitive: bool| -> Vec<String> {
        let r = dependents(
            &db,
            &files,
            "C.base",
            &DbSpanResolver::new(&db),
            transitive,
            ListOpts::default(),
        );
        r.dependents.iter().map(|s| s.name.clone()).collect()
    };
    // Direct: only `mid` references `base`.
    let direct = names(false);
    assert!(direct.contains(&"mid".to_owned()), "{direct:?}");
    assert!(!direct.contains(&"top".to_owned()), "{direct:?}");
    // Transitive: `top` reaches `base` through `mid`.
    let all = names(true);
    assert!(all.contains(&"mid".to_owned()) && all.contains(&"top".to_owned()), "{all:?}");
}

#[test]
fn callers_of_inc() {
    let (db, files) = workspace();
    let r = callers(&db, &files, "A.inc", &DbSpanResolver::new(&db));
    // Both `B.two` and `B.four` reference `A.inc` (edges sorted by path).
    let names: Vec<&str> = r.edges.iter().map(|e| e.symbol.name.as_str()).collect();
    assert_eq!(names, vec!["four", "two"], "{names:?}");
    let four = r.edges.iter().find(|e| e.symbol.name == "four").unwrap();
    assert_eq!(four.sites.len(), 2, "B.four calls A.inc twice");
}

#[test]
fn callees_of_four() {
    let (db, _files) = workspace();
    let r = callees(&db, "B.four", &DbSpanResolver::new(&db));
    let names: Vec<&str> = r.edges.iter().map(|e| e.symbol.name.as_str()).collect();
    assert!(names.contains(&"inc") && names.contains(&"two"), "{names:?}");
    let inc = r.edges.iter().find(|e| e.symbol.name == "inc").unwrap();
    assert_eq!(inc.sites.len(), 2, "B.four references A.inc twice");
}

#[test]
fn callers_snapshot() {
    let (db, files) = workspace();
    let r = callers(&db, &files, "A.inc", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("callers_A_inc", json(&r));
}

#[test]
fn callees_snapshot() {
    let (db, _files) = workspace();
    let r = callees(&db, "B.four", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("callees_B_four", json(&r));
}

#[test]
fn search_ranks_exact_above_hole() {
    let mut db = FaiDatabase::new();
    let m = db.add_source(
        "S.fai".into(),
        indoc! {r#"
            module S

            public len : List 'a -> Int
            let len xs = 0

            public sumInts : List Int -> Int
            let sumInts xs = 0

            public other : Int -> Int
            let other x = x
        "#}
        .to_owned(),
    );
    let files = vec![db.source_file(m).unwrap()];
    let r = search(&db, &files, "List 'a -> Int", &DbSpanResolver::new(&db), ListOpts::default());
    let names: Vec<&str> = r.results.iter().map(|h| h.symbol.name.as_str()).collect();
    // `len` matches exactly (var<->var); `sumInts` matches with the hole bound to
    // `Int`; `other` does not match at all.
    assert_eq!(names.first(), Some(&"len"), "exact match ranks first: {names:?}");
    assert!(names.contains(&"sumInts"), "{names:?}");
    assert!(!names.contains(&"other"), "{names:?}");
    let len_hit = r.results.iter().find(|h| h.symbol.name == "len").unwrap();
    let sum_hit = r.results.iter().find(|h| h.symbol.name == "sumInts").unwrap();
    assert!(len_hit.score > sum_hit.score, "exact score should exceed hole score");
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
        indoc! {r#"
            module M

            public good : Int -> Int
            let good x = x + 1

            public bad : Int -> Bool
            let bad x = x + 1
        "#}
        .to_owned(),
    );
    let _ = db.source_file(id).unwrap();
    let r = type_at(&db, "M.good", &DbSpanResolver::new(&db));
    assert_eq!(r.ty.display, "Int -> Int");
}
