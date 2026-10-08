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

#[cfg(unix)]
#[test]
fn a_user_file_with_an_embedded_display_path_keeps_its_own_location() {
    let source = "module Main\nlet values = List.map identity [1]\nlet answer = Fake.value\n";
    let mut h = Harness::start("std-path-collision", &[("Main.fai", source)]);
    let path = "<std>/collections/List.fai";
    let user_uri = h.uri(path);
    let disk = Url::parse(&user_uri).unwrap().to_file_path().unwrap();
    std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
    let fake = "module Fake\npublic value : Int\nlet value = 42\n";
    std::fs::write(&disk, fake).unwrap();
    h.did_open(path, fake);
    let main_uri = h.did_open("Main.fai", source);
    let user = h.definition(&main_uri, position_within(source, "Fake.value", 5));
    assert_eq!(user[0]["uri"], user_uri);
    let standard = h.definition(&main_uri, position_within(source, "List.map", 5));
    assert_ne!(standard[0]["uri"], user_uri);
    let standard_uri = Url::parse(standard[0]["uri"].as_str().unwrap()).unwrap();
    assert!(standard_uri.path().ends_with("/collections/List.fai"));
    let rename = h.rename(&main_uri, position_within(source, "Fake.value", 5), "renamed");
    let changes = rename["changes"].as_object().unwrap();
    assert!(changes.contains_key(&user_uri));
    assert!(!changes.contains_key(standard_uri.as_str()));
    assert_eq!(
        std::fs::read_to_string(standard_uri.to_file_path().unwrap()).unwrap(),
        include_str!("../../../std/collections/List.fai")
    );
    h.shutdown();
}
