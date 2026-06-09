//! Tests for inlay hints and semantic tokens.

use fai_db::{Db, FaiDatabase, SourceFile};
use fai_ide::{SemKind, inlay_hints, semantic_tokens};
use indoc::indoc;

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

/// The class of the token that starts exactly at `offset`.
fn kind_of(tokens: &[fai_ide::SemToken], offset: u32) -> SemKind {
    tokens
        .iter()
        .find(|t| t.offset == offset)
        .unwrap_or_else(|| panic!("no token at {offset}"))
        .kind
}

// --- inlay hints ------------------------------------------------------------

#[test]
fn inlay_hints_annotate_parameters_and_locals() {
    let source = indoc! {r#"
        module M

        public area : Int -> Int
        let area w =
          let n = w + 1
          n
    "#};
    let (db, file, text) = workspace(source);
    let hints = inlay_hints(&db, file, 0, text.len() as u32);
    // The parameter `w` and the local `n`, each inferred `Int`.
    assert_eq!(hints.len(), 2, "{hints:?}");
    assert!(hints.iter().all(|h| h.label == ": Int"), "{hints:?}");
    let w_end = at(&text, "area w") + "area w".len() as u32;
    let n_end = at(&text, "let n") + "let n".len() as u32;
    let offsets: Vec<u32> = hints.iter().map(|h| h.offset).collect();
    assert!(offsets.contains(&w_end), "hint after `w`: {offsets:?}");
    assert!(offsets.contains(&n_end), "hint after `n`: {offsets:?}");
}

#[test]
fn inlay_hints_respect_the_requested_range() {
    let source = indoc! {r#"
        module M

        public area : Int -> Int
        let area w =
          let n = w + 1
          n
    "#};
    let (db, file, text) = workspace(source);
    // A range covering only the `let n = …` line excludes the parameter hint.
    let line_start = text.find("  let n").unwrap() as u32;
    let line_end = text[line_start as usize..].find('\n').unwrap() as u32 + line_start;
    let hints = inlay_hints(&db, file, line_start, line_end);
    assert_eq!(hints.len(), 1, "only the local `n`: {hints:?}");
}

#[test]
fn no_inlay_hints_without_binders() {
    let source = "module M\n\npublic two : Int\nlet two = 2\n";
    let (db, file, text) = workspace(source);
    let hints = inlay_hints(&db, file, 0, text.len() as u32);
    assert!(hints.is_empty(), "a nullary binding has no binders: {hints:?}");
}

// --- semantic tokens --------------------------------------------------------

#[test]
fn semantic_tokens_classify_by_role() {
    let source = indoc! {r#"
        module M

        type Color =
          | Red
          | Green

        public pick : Color
        let pick = Red

        public area : Int -> Int
        let area w = w + 1
    "#};
    let (db, file, text) = workspace(source);
    let tokens = semantic_tokens(&db, file);
    // Keyword, the constructor *use*, a local use, a literal, and a type name.
    assert_eq!(kind_of(&tokens, at(&text, "module")), SemKind::Keyword);
    assert_eq!(kind_of(&tokens, at(&text, "= Red") + 2), SemKind::EnumMember);
    assert_eq!(kind_of(&tokens, at(&text, "w + 1")), SemKind::Variable);
    assert_eq!(kind_of(&tokens, at(&text, "w + 1") + 4), SemKind::Number);
    assert_eq!(kind_of(&tokens, at(&text, "Int ->")), SemKind::Type);
    // The operator `+`.
    assert_eq!(kind_of(&tokens, at(&text, "+ 1")), SemKind::Operator);
}

#[test]
fn semantic_tokens_classify_a_char_literal() {
    let source = "module M\n\npublic c : Char\nlet c = 'a'\n";
    let (db, file, text) = workspace(source);
    let tokens = semantic_tokens(&db, file);
    // A char literal is classified like a string literal.
    assert_eq!(kind_of(&tokens, at(&text, "'a'")), SemKind::String);
    // The `Char` in the signature is a type.
    assert_eq!(kind_of(&tokens, at(&text, "Char")), SemKind::Type);
}

#[test]
fn semantic_tokens_distinguish_module_from_member() {
    let source = "module M\n\npublic total : List Int -> Int\nlet total xs = List.length xs\n";
    let (db, file, text) = workspace(source);
    let tokens = semantic_tokens(&db, file);
    // In `List.length`, `List` is a namespace and `length` a function.
    let list = at(&text, "List.length");
    assert_eq!(kind_of(&tokens, list), SemKind::Namespace);
    assert_eq!(kind_of(&tokens, list + "List.".len() as u32), SemKind::Function);
    // The `List` in the signature is a type constructor.
    assert_eq!(kind_of(&tokens, at(&text, "List Int")), SemKind::Type);
}

#[test]
fn semantic_tokens_mark_comments_and_doc_comments() {
    let source = "module M\n\n/// A doc.\npublic two : Int\nlet two = 2 // trailing\n";
    let (db, file, text) = workspace(source);
    let tokens = semantic_tokens(&db, file);
    assert_eq!(kind_of(&tokens, at(&text, "/// A doc.")), SemKind::Comment);
    assert_eq!(kind_of(&tokens, at(&text, "// trailing")), SemKind::Comment);
}
