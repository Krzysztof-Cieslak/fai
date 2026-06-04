//! Incremental behavior of the front-end queries: early cutoff at the span-free
//! item tree, and incremental-vs-clean agreement.

use fai_db::{Db, FaiDatabase};
use fai_syntax::{item_tree, public_item_count};
use fai_tests::assert_incremental_matches_clean;

#[test]
fn comment_edit_cuts_off_at_the_item_tree() {
    let mut db = FaiDatabase::new();
    let id = db.add_source(
        "M.fai".into(),
        "module M\npublic main : Runtime -> Unit\nlet main r = r".to_owned(),
    );
    let file = db.source_file(id).unwrap();

    // Prime the query chain.
    assert_eq!(public_item_count(&db, file), 1);

    db.enable_event_log();

    // Insert a comment: the text changes, so `parse` and `item_tree` re-run, but
    // the span-free item tree is unchanged, so `public_item_count` is cut off.
    db.add_source(
        "M.fai".into(),
        "module M\n// a fresh comment\npublic main : Runtime -> Unit\nlet main r = r".to_owned(),
    );
    assert_eq!(public_item_count(&db, file), 1);
    let log = db.take_events();
    assert!(log.iter().any(|e| e.contains("parse")), "parse should re-run: {log:?}");
    assert!(log.iter().any(|e| e.contains("item_tree")), "item_tree should re-run: {log:?}");
    assert!(
        !log.iter().any(|e| e.contains("public_item_count")),
        "public_item_count should be cut off after a comment edit: {log:?}",
    );

    // Rename the public binding: the item tree changes, so the dependent re-runs.
    db.add_source(
        "M.fai".into(),
        "module M\n// a fresh comment\npublic renamed : Runtime -> Unit\nlet renamed r = r"
            .to_owned(),
    );
    assert_eq!(public_item_count(&db, file), 1);
    let log = db.take_events();
    assert!(
        log.iter().any(|e| e.contains("public_item_count")),
        "public_item_count should re-run after a rename: {log:?}",
    );
}

#[test]
fn body_edit_cuts_off_at_the_item_tree() {
    let mut db = FaiDatabase::new();
    let id = db.add_source("M.fai".into(), "module M\npublic f : Int\nlet f = 1".to_owned());
    let file = db.source_file(id).unwrap();
    assert_eq!(public_item_count(&db, file), 1);

    db.enable_event_log();
    // Changing a binding *body* leaves names/kinds/visibility unchanged.
    db.add_source("M.fai".into(), "module M\npublic f : Int\nlet f = 9999".to_owned());
    assert_eq!(public_item_count(&db, file), 1);
    let log = db.take_events();
    assert!(
        !log.iter().any(|e| e.contains("public_item_count")),
        "a body edit must not invalidate the item-tree dependent: {log:?}",
    );
}

#[test]
fn item_tree_incremental_matches_clean() {
    assert_incremental_matches_clean(
        &[
            &[("a.fai", "module A\npublic f : Int\nlet f = 1")],
            // Trivia edit: same item tree.
            &[("a.fai", "module A\n// note\npublic f : Int\nlet f = 1")],
            // Body edit: same item tree.
            &[("a.fai", "module A\n// note\npublic f : Int\nlet f = 2")],
            // Rename: item tree changes.
            &[("a.fai", "module A\npublic g : Int\nlet g = 2")],
            // Add an item: item tree grows.
            &[("a.fai", "module A\npublic g : Int\nlet g = 2\nlet h = 3")],
            // A second file appears.
            &[("a.fai", "module A\nlet g = 2"), ("b.fai", "module B\npublic k : Int\nlet k = 0")],
        ],
        |db, ids| {
            ids.iter().map(|&id| item_tree(db, db.source_file(id).unwrap())).collect::<Vec<_>>()
        },
    );
}
