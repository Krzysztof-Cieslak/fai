//! Closed dependency refreshes preserve authoritative unsaved editor buffers.

mod harness;
use harness::{Harness, position_of};
use serde_json::json;

const MAIN: &str = "module Main\nlet copied = Dep.value\n";
const DEP: &str = "module Dep\npublic value : Int\nlet value = 1\n";

fn write(harness: &Harness, name: &str, text: &str) {
    let path = lsp_types::Url::parse(&harness.uri(name)).unwrap().to_file_path().unwrap();
    std::fs::write(path, text).unwrap();
}

fn remove(harness: &Harness, name: &str) {
    let path = lsp_types::Url::parse(&harness.uri(name)).unwrap().to_file_path().unwrap();
    std::fs::remove_file(path).unwrap();
}

fn changed(harness: &Harness, name: &str, kind: u32) {
    harness.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": harness.uri(name), "type": kind}]}),
    );
}

#[test]
fn registers_for_source_file_events_when_the_client_supports_it() {
    let (h, _) = Harness::start_with_caps(
        "watch-registration",
        &[],
        json!({
            "workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}}
        }),
    );
    let registration = h.registration();
    assert_eq!(registration["registrations"][0]["method"], "workspace/didChangeWatchedFiles");
    assert_eq!(
        registration["registrations"][0]["registerOptions"]["watchers"],
        json!([{"globPattern": "**/*.fai", "kind": 7}])
    );
    h.shutdown();
}

#[test]
fn changed_closed_dependency_updates_hover() {
    let mut h = Harness::start("watch-hover", &[("Main.fai", MAIN), ("Dep.fai", DEP)]);
    let uri = h.did_open("Main.fai", MAIN);
    assert!(h.hover_text(&uri, position_of(MAIN, "copied")).unwrap().contains("Int"));
    write(&h, "Dep.fai", "module Dep\npublic value : Bool\nlet value = true\n");
    changed(&h, "Dep.fai", 2);
    assert!(h.hover_text(&uri, position_of(MAIN, "copied")).unwrap().contains("Bool"));
    h.shutdown();
}

#[test]
fn changed_closed_dependency_republishes_open_diagnostics() {
    let main = "module Main\npublic copied : Int\nlet copied = Dep.value\n";
    let mut h = Harness::start("watch-diagnostics", &[("Main.fai", main), ("Dep.fai", DEP)]);
    let uri = h.did_open("Main.fai", main);
    assert!(h.diagnostics(&uri).is_empty());
    write(&h, "Dep.fai", "module Dep\npublic value : Bool\nlet value = true\n");
    changed(&h, "Dep.fai", 2);
    assert!(h.diagnostic_codes(&uri).contains(&"FAI3004".to_owned()));
    h.shutdown();
}

#[test]
fn deleted_dependency_and_recreation_update_membership() {
    let mut h = Harness::start("watch-membership", &[("Main.fai", MAIN), ("Dep.fai", DEP)]);
    let uri = h.did_open("Main.fai", MAIN);
    assert!(h.diagnostics(&uri).is_empty());
    remove(&h, "Dep.fai");
    changed(&h, "Dep.fai", 3);
    assert!(h.diagnostic_codes(&uri).contains(&"FAI2008".to_owned()));
    write(&h, "Dep.fai", DEP);
    changed(&h, "Dep.fai", 1);
    assert!(h.diagnostics(&uri).is_empty());
    h.shutdown();
}

#[test]
fn newly_created_dependency_resolves_an_open_file() {
    let mut h = Harness::start("watch-create", &[("Main.fai", MAIN)]);
    let uri = h.did_open("Main.fai", MAIN);
    assert!(h.diagnostic_codes(&uri).contains(&"FAI2008".to_owned()));
    write(&h, "Dep.fai", DEP);
    changed(&h, "Dep.fai", 1);
    assert!(h.diagnostics(&uri).is_empty());
    h.shutdown();
}

#[test]
fn external_edits_do_not_replace_open_unsaved_dependencies() {
    let mut h = Harness::start("watch-overlay", &[("Main.fai", MAIN), ("Dep.fai", DEP)]);
    let uri = h.did_open("Main.fai", MAIN);
    let dep_uri = h.did_open("Dep.fai", DEP);
    h.did_change(&dep_uri, "module Dep\npublic value : Bool\nlet value = true\n");
    write(&h, "Dep.fai", "module Dep\npublic value : String\nlet value = \"disk\"\n");
    changed(&h, "Dep.fai", 2);
    assert!(h.hover_text(&uri, position_of(MAIN, "copied")).unwrap().contains("Bool"));
    h.did_close(&dep_uri);
    assert!(h.hover_text(&uri, position_of(MAIN, "copied")).unwrap().contains("String"));
    h.shutdown();
}

#[test]
fn external_deletion_keeps_an_open_overlay_until_close() {
    let mut h = Harness::start("watch-deleted-overlay", &[("Main.fai", MAIN), ("Dep.fai", DEP)]);
    let uri = h.did_open("Main.fai", MAIN);
    let dep_uri = h.did_open("Dep.fai", DEP);
    remove(&h, "Dep.fai");
    changed(&h, "Dep.fai", 3);
    assert!(h.hover_text(&uri, position_of(MAIN, "copied")).unwrap().contains("Int"));
    h.did_close(&dep_uri);
    assert!(h.diagnostic_codes(&uri).contains(&"FAI2008".to_owned()));
    h.shutdown();
}
