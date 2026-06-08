//! Tests for code actions / quick fixes, verified by applying the edits and
//! re-checking the result.

use fai_db::{Db, FaiDatabase, SourceFile};
use fai_ide::{CodeActionEdit, code_actions_at};
use indoc::indoc;

/// A database with the standard library plus one user file, and its text.
fn workspace(source: &str) -> (FaiDatabase, SourceFile, String) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    let text = file.text(&db).clone();
    (db, file, text)
}

/// The byte offset of `needle` in `text`.
fn at(text: &str, needle: &str) -> u32 {
    text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found")) as u32
}

/// Applies single-file edits to `text` (right-to-left, so offsets stay valid).
fn apply(text: &str, edits: &[CodeActionEdit]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|e| std::cmp::Reverse(e.span.byte_start));
    let mut out = text.to_owned();
    for e in edits {
        out.replace_range(e.span.byte_start as usize..e.span.byte_end as usize, &e.new_text);
    }
    out
}

/// The `FAInnnn` codes reported for `source` (with the std library loaded).
fn codes(source: &str) -> Vec<String> {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    let mut out: Vec<String> = Vec::new();
    out.extend(
        fai_resolve::resolve::accumulated::<fai_db::Diag>(&db, file)
            .into_iter()
            .map(|d| d.0.code.as_str().to_owned()),
    );
    out.extend(
        fai_types::check_file::accumulated::<fai_db::Diag>(&db, file)
            .into_iter()
            .map(|d| d.0.code.as_str().to_owned()),
    );
    out
}

#[test]
fn missing_public_signature_offers_the_inferred_signature() {
    let source = "module M\n\npublic let inc x = x + 1\n";
    let (db, file, text) = workspace(source);
    let offset = at(&text, "inc");
    let actions =
        code_actions_at(&db, &[file], file, offset, offset, &fai_db::DbSpanResolver::new(&db));
    let fix = actions.iter().find(|a| a.title == "Add the inferred signature").expect("a fix");
    let fixed = apply(&text, &fix.edits);
    assert_eq!(
        fixed, "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n",
        "fixed: {fixed:?}"
    );
    // The fix removes the diagnostic it addresses.
    assert!(codes(source).iter().any(|c| c == "FAI3003"), "the original is missing a signature");
    assert!(!codes(&fixed).iter().any(|c| c == "FAI3003"), "the fix satisfies the requirement");
}

#[test]
fn missing_signature_fix_keeps_nested_indentation() {
    let source = indoc! {r#"
        module M

        module Inner =
          public let answer = 42
    "#};
    let (db, file, text) = workspace(source);
    let offset = at(&text, "answer");
    let actions =
        code_actions_at(&db, &[file], file, offset, offset, &fai_db::DbSpanResolver::new(&db));
    let fix = actions.iter().find(|a| a.title == "Add the inferred signature").expect("a fix");
    let fixed = apply(&text, &fix.edits);
    // Both the new signature line and the binding keep the two-space indent.
    assert!(fixed.contains("  public answer : Int\n  let answer = 42"), "fixed: {fixed:?}");
}

#[test]
fn unbound_name_offers_qualification() {
    let source = "module M\n\npublic ids : List Int -> List Int\nlet ids xs = map identity xs\n";
    let (db, file, text) = workspace(source);
    let offset = at(&text, "map identity") + 1; // inside `map`
    let actions =
        code_actions_at(&db, &[file], file, offset, offset, &fai_db::DbSpanResolver::new(&db));
    // `map` is exported by several std modules; `List.map` must be offered.
    let list = actions
        .iter()
        .find(|a| a.title == "Qualify as `List.map`")
        .unwrap_or_else(|| panic!("no List.map fix among {:?}", titles(&actions)));
    assert_eq!(list.edits.len(), 1);
    assert_eq!(list.edits[0].new_text, "List.map");
    let fixed = apply(&text, &list.edits);
    assert!(fixed.contains("let ids xs = List.map identity xs"), "fixed: {fixed:?}");
    // Qualifying clears the unbound-name error for `map`.
    assert!(codes(source).iter().any(|c| c == "FAI2001"), "`map` starts unbound");
    assert!(!codes(&fixed).iter().any(|c| c == "FAI2001"), "qualifying resolves it");
}

fn titles(actions: &[fai_ide::CodeAction]) -> Vec<String> {
    actions.iter().map(|a| a.title.clone()).collect()
}

#[test]
fn no_actions_on_a_clean_range() {
    let source = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let (db, file, text) = workspace(source);
    let offset = at(&text, "inc x");
    let actions =
        code_actions_at(&db, &[file], file, offset, offset, &fai_db::DbSpanResolver::new(&db));
    assert!(actions.is_empty(), "a well-formed binding offers nothing: {:?}", titles(&actions));
}
