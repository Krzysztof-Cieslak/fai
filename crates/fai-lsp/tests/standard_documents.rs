//! Embedded definitions open as readable versioned source documents.

mod harness;
use harness::{Harness, position_of, position_within};
use lsp_types::Url;
use serde_json::{Value, json};

const MAIN: &str =
    "module Main\nlet mapped = List.map identity [1, 2]\nlet size = Array.length [| 1 |]\n";

fn definition(h: &mut Harness, uri: &str, needle: &str, within: usize) -> Value {
    h.definition(uri, position_within(MAIN, needle, within))[0].clone()
}

#[track_caller]
fn check_source(needle: &str, within: usize, expected: &str) {
    let (mut h, uri) = Harness::open_main("std-source", MAIN);
    let location = definition(&mut h, &uri, needle, within);
    let target = Url::parse(location["uri"].as_str().unwrap()).unwrap();
    let path = target.to_file_path().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    assert!(std::fs::metadata(path).unwrap().permissions().readonly());
    let line = location["range"]["start"]["line"].as_u64().unwrap() as usize;
    assert!(!expected.lines().nth(line).unwrap().is_empty());
    h.shutdown();
}

#[test]
fn list_definition_has_the_embedded_list_source() {
    check_source("List.map", 5, include_str!("../../../std/collections/List.fai"));
}

#[test]
fn array_definition_has_the_embedded_array_source() {
    check_source("Array.length", 6, include_str!("../../../std/collections/Array.fai"));
}

#[test]
fn prelude_definition_has_the_embedded_prelude_source() {
    check_source("identity", 0, include_str!("../../../std/core/Prelude.fai"));
}

#[test]
fn opened_standard_documents_support_navigation_and_ignore_edits() {
    let (mut h, uri) = Harness::open_main("std-readonly", MAIN);
    let location = definition(&mut h, &uri, "List.map", 5);
    let target = location["uri"].as_str().unwrap();
    let source = include_str!("../../../std/collections/List.fai");
    h.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": target, "languageId": "fai", "version": 1, "text": source
        }}),
    );
    let position = position_of(source, "map f xs");
    let before = h.hover(target, position.clone());
    assert!(before["contents"]["value"].as_str().unwrap().contains("map"));
    h.did_change(target, "module List\nlet map = 0\n");
    h.did_save(target);
    assert_eq!(h.hover(target, position.clone()), before);
    assert!(h.prepare_rename(target, position).is_null());
    assert!(h.formatting(target).is_null());
    assert_eq!(definition(&mut h, &uri, "List.map", 5), location);
    h.shutdown();
}

#[test]
fn workspaces_share_the_same_versioned_standard_document() {
    let (mut first, first_uri) = Harness::open_main("std-first-root", MAIN);
    let (mut second, second_uri) = Harness::open_main("std-second-root", MAIN);
    let a = definition(&mut first, &first_uri, "List.map", 5);
    let b = definition(&mut second, &second_uri, "List.map", 5);
    assert_eq!(a, b);
    first.shutdown();
    second.shutdown();
}
