//! Tests for code completion over small workspaces (with the standard library
//! loaded, so prelude names and qualified `List.`/`Option.` members resolve).

use fai_db::{Db, FaiDatabase, SourceFile};
use fai_ide::{CompletionKind, completions_at};
use indoc::indoc;

/// A database with the embedded standard library plus one user file.
fn workspace(source: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("C.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

/// The byte offset just after `needle` in `text`.
fn after(text: &str, needle: &str) -> u32 {
    let start = text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found"));
    (start + needle.len()) as u32
}

fn labels(items: &[fai_ide::CompletionItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

#[test]
fn record_field_access_offers_fields_with_types() {
    let source = indoc! {r#"
        module C

        public area : { width : Int, height : Int } -> Int
        let area r = r.width
    "#};
    let (db, file) = workspace(source);
    // Complete right after the `.` in `r.width`.
    let offset = after(source, "r.");
    let result = completions_at(&db, file, offset);
    let names = labels(&result.items);
    assert_eq!(names, vec!["height", "width"], "only the record's fields: {names:?}");
    assert!(result.items.iter().all(|i| i.kind == CompletionKind::Field), "{:?}", result.items);
    let width = result.items.iter().find(|i| i.label == "width").unwrap();
    assert_eq!(width.detail.as_deref(), Some("Int"));
}

#[test]
fn qualified_module_offers_public_members() {
    let source = indoc! {r#"
        module C

        public total : List Int -> Int
        let total xs = List.length xs
    "#};
    let (db, file) = workspace(source);
    let offset = after(source, "List.");
    let result = completions_at(&db, file, offset);
    let names = labels(&result.items);
    // The std `List` module exposes these (among others), all qualified members.
    assert!(names.contains(&"length"), "{names:?}");
    assert!(names.contains(&"map"), "{names:?}");
    // `length : List 'a -> Int` is a function.
    let length = result.items.iter().find(|i| i.label == "length").unwrap();
    assert_eq!(length.kind, CompletionKind::Function);
    assert!(length.detail.is_some(), "members carry a rendered type");
}

#[test]
fn bare_context_offers_locals_defs_constructors_and_prelude() {
    let source = indoc! {r#"
        module C

        type Color =
          | Red
          | Green

        public describe : Color -> Int
        let describe c =
          let label = 1
          label
    "#};
    let (db, file) = workspace(source);
    // Complete at the trailing `label` (the block tail).
    let offset = text_tail_label(source);
    let result = completions_at(&db, file, offset);
    let names = labels(&result.items);
    // The parameter and the local in scope.
    assert!(names.contains(&"c"), "{names:?}");
    assert!(names.contains(&"label"), "{names:?}");
    // This module's own definition.
    assert!(names.contains(&"describe"), "{names:?}");
    // This file's constructors.
    assert!(names.contains(&"Red") && names.contains(&"Green"), "{names:?}");
    // Prelude values and constructors are auto-imported.
    assert!(names.contains(&"identity"), "prelude value: {names:?}");
    assert!(names.contains(&"Some"), "prelude constructor: {names:?}");
}

/// The offset of the block-tail `label` (the second occurrence of `label`).
fn text_tail_label(text: &str) -> u32 {
    let first = text.find("label").unwrap();
    let second = text[first + 1..].find("label").unwrap() + first + 1;
    (second + "lab".len()) as u32
}

#[test]
fn locals_are_scoped_to_their_branch() {
    let source = indoc! {r#"
        module C

        public f : Result Int Int -> Int
        let f r =
          match r with
          | Ok x -> x
          | Err y -> y
    "#};
    let (db, file) = workspace(source);
    // Complete at the `y` in the second arm body.
    let offset = after(source, "Err y -> ");
    let result = completions_at(&db, file, offset);
    let names = labels(&result.items);
    assert!(names.contains(&"y"), "the arm's own binder is in scope: {names:?}");
    assert!(names.contains(&"r"), "the parameter is in scope: {names:?}");
    assert!(!names.contains(&"x"), "the other arm's binder is out of scope: {names:?}");
}

#[test]
fn no_member_context_off_a_dot_is_bare() {
    // A plain identifier position (no preceding dot) is the bare context, which
    // includes constructors usable in a pattern.
    let source = indoc! {r#"
        module C

        type Color =
          | Red
          | Green

        public flip : Color -> Color
        let flip c =
          match c with
          | Red -> Green
          | Green -> Red
    "#};
    let (db, file) = workspace(source);
    // Complete at the `Green` result of the first arm.
    let offset = after(source, "Red -> Gre");
    let result = completions_at(&db, file, offset);
    let names = labels(&result.items);
    assert!(names.contains(&"Green") && names.contains(&"Red"), "constructors offered: {names:?}");
}
