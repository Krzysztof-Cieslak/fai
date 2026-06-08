//! Real-world scenarios for document/workspace symbols and signature help.

mod harness;

use harness::{Harness, position_of, position_within};
use indoc::indoc;

const SAMPLE: &str = indoc! {r#"
    module M

    public inc : Int -> Int
    let inc x = x + 1

    public two : Int
    let two = inc 1

    module Inner =
      public deep : Int
      let deep = 0
"#};

fn symbol_names(result: &serde_json::Value) -> Vec<String> {
    result
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

// --- document symbols --------------------------------------------------------

#[test]
fn document_symbols_list_top_level_bindings() {
    let (mut h, uri) = Harness::open_main("ds-top", SAMPLE);
    let names = symbol_names(&h.document_symbols(&uri));
    assert!(names.contains(&"inc".to_owned()) && names.contains(&"two".to_owned()), "{names:?}");
    h.shutdown();
}

#[test]
fn document_symbols_carry_kinds() {
    let (mut h, uri) = Harness::open_main("ds-kind", SAMPLE);
    let result = h.document_symbols(&uri);
    let array = result.as_array().unwrap();
    let inc = array.iter().find(|s| s["name"] == "inc").unwrap();
    let two = array.iter().find(|s| s["name"] == "two").unwrap();
    assert_eq!(inc["kind"], 12, "function");
    assert_eq!(two["kind"], 13, "value");
    h.shutdown();
}

#[test]
fn document_symbols_carry_signatures_as_detail() {
    let (mut h, uri) = Harness::open_main("ds-detail", SAMPLE);
    let result = h.document_symbols(&uri);
    let inc = result.as_array().unwrap().iter().find(|s| s["name"] == "inc").unwrap().clone();
    assert_eq!(inc["detail"], "Int -> Int", "{inc}");
    h.shutdown();
}

#[test]
fn document_symbols_nest_modules() {
    let (mut h, uri) = Harness::open_main("ds-nested", SAMPLE);
    let result = h.document_symbols(&uri);
    let inner = result.as_array().unwrap().iter().find(|s| s["name"] == "Inner").unwrap().clone();
    assert_eq!(inner["kind"], 2, "a module");
    let children = inner["children"].as_array().expect("nested children");
    assert!(children.iter().any(|c| c["name"] == "deep"), "{children:?}");
    h.shutdown();
}

#[test]
fn document_symbols_of_an_empty_module_are_empty() {
    let (mut h, uri) = Harness::open_main("ds-empty", "module M\n");
    assert!(h.document_symbols(&uri).as_array().unwrap().is_empty());
    h.shutdown();
}

// --- workspace symbols -------------------------------------------------------

fn multi() -> Harness {
    let a =
        "module A\n\npublic alpha : Int\nlet alpha = 1\n\npublic shared : Int\nlet shared = 2\n";
    let b =
        "module B\n\npublic beta : Int\nlet beta = 3\n\npublic shared2 : Int\nlet shared2 = 4\n";
    Harness::start("ws", &[("A.fai", a), ("B.fai", b)])
}

#[test]
fn workspace_symbols_find_by_name() {
    let mut h = multi();
    let names = symbol_names(&h.workspace_symbols("alpha"));
    assert_eq!(names, vec!["alpha".to_owned()], "{names:?}");
    h.shutdown();
}

#[test]
fn workspace_symbols_match_a_substring() {
    let mut h = multi();
    let names = symbol_names(&h.workspace_symbols("shared"));
    assert!(
        names.contains(&"shared".to_owned()) && names.contains(&"shared2".to_owned()),
        "{names:?}"
    );
    h.shutdown();
}

#[test]
fn workspace_symbols_span_files() {
    let mut h = multi();
    let names = symbol_names(&h.workspace_symbols(""));
    assert!(names.contains(&"alpha".to_owned()) && names.contains(&"beta".to_owned()), "{names:?}");
    h.shutdown();
}

#[test]
fn workspace_symbols_are_case_insensitive() {
    let mut h = multi();
    let names = symbol_names(&h.workspace_symbols("ALPHA"));
    assert_eq!(names, vec!["alpha".to_owned()], "{names:?}");
    h.shutdown();
}

#[test]
fn workspace_symbols_report_the_container_module() {
    let mut h = multi();
    let result = h.workspace_symbols("beta");
    let beta = result.as_array().unwrap()[0].clone();
    assert_eq!(beta["containerName"], "B", "{beta}");
    assert!(beta["location"]["uri"].as_str().unwrap().ends_with("B.fai"), "{beta}");
    h.shutdown();
}

// --- signature help ----------------------------------------------------------

const CALL: &str = indoc! {r#"
    module M

    public add : Int -> Int -> Int
    let add x y = x + y

    public r : Int
    let r = add 7 8
"#};

#[test]
fn signature_help_shows_the_callee_signature() {
    let (mut h, uri) = Harness::open_main("sig-label", CALL);
    let result = h.signature_help(&uri, position_of(CALL, "7 8"));
    assert_eq!(result["signatures"][0]["label"], "add : Int -> Int -> Int", "{result}");
    h.shutdown();
}

#[test]
fn signature_help_highlights_the_first_argument() {
    let (mut h, uri) = Harness::open_main("sig-p0", CALL);
    let result = h.signature_help(&uri, position_of(CALL, "7 8"));
    assert_eq!(result["activeParameter"], 0, "{result}");
    h.shutdown();
}

#[test]
fn signature_help_highlights_the_second_argument() {
    let (mut h, uri) = Harness::open_main("sig-p1", CALL);
    let result = h.signature_help(&uri, position_within(CALL, "7 8", 2));
    assert_eq!(result["activeParameter"], 1, "{result}");
    h.shutdown();
}

#[test]
fn signature_help_lists_two_parameters() {
    let (mut h, uri) = Harness::open_main("sig-params", CALL);
    let result = h.signature_help(&uri, position_of(CALL, "7 8"));
    let params = result["signatures"][0]["parameters"].as_array().expect("parameters");
    assert_eq!(params.len(), 2, "{params:?}");
    h.shutdown();
}

#[test]
fn signature_help_works_for_a_qualified_call() {
    let src =
        "module M\n\npublic f : List Int -> List Int -> List Int\nlet f a b = List.append a b\n";
    let (mut h, uri) = Harness::open_main("sig-qual", src);
    let result = h.signature_help(&uri, position_within(src, "append a b", 7));
    let label = result["signatures"][0]["label"].as_str().expect("label");
    assert!(label.contains("append"), "{label}");
    assert_eq!(result["activeParameter"], 0, "{result}");
    h.shutdown();
}

#[test]
fn signature_help_off_a_call_is_empty() {
    let (mut h, uri) = Harness::open_main("sig-none", CALL);
    assert!(h.signature_help(&uri, position_of(CALL, "module M")).is_null());
    h.shutdown();
}

#[test]
fn signature_help_parameter_labels_index_the_signature() {
    let (mut h, uri) = Harness::open_main("sig-offsets", CALL);
    let result = h.signature_help(&uri, position_of(CALL, "7 8"));
    let label = result["signatures"][0]["label"].as_str().unwrap().to_owned();
    let param0 = &result["signatures"][0]["parameters"][0]["label"];
    let (start, end) = (param0[0].as_u64().unwrap() as usize, param0[1].as_u64().unwrap() as usize);
    assert_eq!(&label[start..end], "Int", "first parameter slice: {label}");
    h.shutdown();
}
