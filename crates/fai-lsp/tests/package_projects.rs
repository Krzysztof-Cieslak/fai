//! Package-aware editor navigation and saved manifest refreshes.

mod harness;
use harness::{Harness, position_of, position_within};
use serde_json::json;

const ROOT: &str = "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n[dependencies]\nlib = {path = 'lib'}\n";
const LIB: &str = "manifest-version = 1\n[package]\nname = 'lib'\nversion = '1.0.0'\n";
const VALUE: &str = "module Values\npublic value : Int\nlet value = 42\n";
const MAIN: &str = "module App\nimport lib.Values\nlet answer = Values.value\n";

fn project() -> Harness {
    Harness::start(
        "packages",
        &[
            ("fai.toml", ROOT),
            ("src/App.fai", MAIN),
            ("lib/fai.toml", LIB),
            ("lib/src/Values.fai", VALUE),
        ],
    )
}

#[test]
fn navigating_and_opening_a_dependency_retains_its_package_identity() {
    let mut h = project();
    let uri = h.did_open("src/App.fai", MAIN);
    let locations = h.definition(&uri, position_within(MAIN, "Values.value", 8));
    assert_eq!(locations[0]["uri"], h.uri("lib/src/Values.fai"));
    let dependency = h.did_open("lib/src/Values.fai", VALUE);
    assert!(h.hover_text(&dependency, position_of(VALUE, "value")).unwrap().contains("Int"));
    h.did_change(&dependency, "module Values\npublic value : Bool\nlet value = true\n");
    assert!(h.hover_text(&uri, position_of(MAIN, "answer")).unwrap().contains("Bool"));
    h.shutdown();
}

#[test]
fn a_saved_dependency_manifest_change_updates_open_imports() {
    let mut h = project();
    let uri = h.did_open("src/App.fai", MAIN);
    assert!(h.diagnostics(&uri).is_empty());
    let path = lsp_types::Url::parse(&h.uri("fai.toml")).unwrap().to_file_path().unwrap();
    std::fs::write(path, ROOT.replace("lib = {", "other = {")).unwrap();
    h.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": h.uri("fai.toml"), "type": 2}]}),
    );
    assert!(h.diagnostic_codes(&uri).contains(&"FAI2021".to_owned()));
    h.shutdown();
}

#[test]
fn dependency_diagnostics_do_not_use_the_importers_coordinates() {
    let mut h = project();
    let uri = h.did_open("src/App.fai", MAIN);
    let dependency = h.did_open("lib/src/Values.fai", VALUE);
    assert!(h.diagnostics(&uri).is_empty());
    h.did_change(&dependency, "module Values\npublic value : Int\nlet value = true\n");
    assert!(h.diagnostics(&uri).is_empty());
    h.did_change(&dependency, "module Values\npublic value : Int\nlet value = false\n");
    assert!(h.diagnostic_codes(&dependency).contains(&"FAI3004".to_owned()));
    h.shutdown();
}
