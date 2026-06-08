//! Real-world navigation scenarios: go-to-definition and find-references over
//! locals, parameters, top-level definitions, constructors, and across modules.

mod harness;

use harness::{Harness, position_of, position_within};
use indoc::indoc;

const SAMPLE: &str = indoc! {r#"
    module M

    public type Color =
      | Red
      | Green

    public paint : Color -> Int
    let paint c =
      let shade = tag c
      shade + tag Red

    public tag : Color -> Int
    let tag c =
      match c with
      | Red -> 0
      | Green -> 1
"#};

fn sample() -> (Harness, String) {
    Harness::open_main("nav", SAMPLE)
}

const A: &str = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
const B: &str = "module B\n\npublic two : Int\nlet two = A.inc 1\n\nlet four = A.inc (A.inc two)\n";

fn ab() -> (Harness, String, String) {
    let h = Harness::start("nav-ab", &[("A.fai", A), ("B.fai", B)]);
    let uri_a = h.did_open("A.fai", A);
    let uri_b = h.did_open("B.fai", B);
    (h, uri_a, uri_b)
}

fn def_line(result: &serde_json::Value) -> i64 {
    let locations = result.as_array().expect("definition locations");
    assert!(!locations.is_empty(), "no definition: {result}");
    locations[0]["range"]["start"]["line"].as_i64().unwrap()
}

// --- go-to-definition --------------------------------------------------------

#[test]
fn definition_of_a_local_jumps_to_its_binding() {
    let (mut h, uri) = sample();
    let result = h.definition(&uri, position_of(SAMPLE, "shade + tag"));
    assert_eq!(def_line(&result), 8, "`let shade` is on line 8");
    h.shutdown();
}

#[test]
fn definition_of_a_parameter_jumps_to_the_parameter() {
    let (mut h, uri) = sample();
    let result = h.definition(&uri, position_within(SAMPLE, "tag c", 4));
    assert_eq!(def_line(&result), 7, "`paint`'s parameter `c` is on line 7");
    h.shutdown();
}

#[test]
fn definition_of_a_top_level_function_jumps_to_its_binding() {
    let (mut h, uri) = sample();
    let result = h.definition(&uri, position_of(SAMPLE, "tag Red"));
    assert_eq!(def_line(&result), 12, "`let tag` is on line 12");
    h.shutdown();
}

#[test]
fn definition_of_a_constructor_use_jumps_to_the_variant() {
    let (mut h, uri) = sample();
    let result = h.definition(&uri, position_within(SAMPLE, "tag Red", 4));
    assert_eq!(def_line(&result), 3, "the `Red` variant is on line 3");
    h.shutdown();
}

#[test]
fn definition_of_a_constructor_pattern_jumps_to_the_variant() {
    let (mut h, uri) = sample();
    let result = h.definition(&uri, position_of(SAMPLE, "Red -> 0"));
    assert_eq!(def_line(&result), 3, "the `Red` variant is on line 3");
    h.shutdown();
}

#[test]
fn definition_off_a_keyword_is_empty() {
    let (mut h, uri) = sample();
    assert!(h.definition(&uri, position_of(SAMPLE, "module M")).is_null());
    h.shutdown();
}

#[test]
fn definition_crosses_a_module_boundary() {
    let (mut h, _a, uri_b) = ab();
    let result = h.definition(&uri_b, position_within(B, "A.inc", 2));
    let locations = result.as_array().expect("locations");
    assert!(locations[0]["uri"].as_str().unwrap().ends_with("A.fai"), "{locations:?}");
    assert_eq!(locations[0]["range"]["start"]["line"], 3, "`let inc` is on line 3 of A");
    h.shutdown();
}

// --- find references ---------------------------------------------------------

#[test]
fn references_of_a_local_are_its_uses() {
    let (mut h, uri) = sample();
    let refs = h.references(&uri, position_of(SAMPLE, "shade + tag"), false);
    assert_eq!(refs.as_array().unwrap().len(), 1, "`shade` is used once: {refs}");
    h.shutdown();
}

#[test]
fn references_of_a_local_with_declaration_include_the_binding() {
    let (mut h, uri) = sample();
    let refs = h.references(&uri, position_of(SAMPLE, "shade + tag"), true);
    assert_eq!(refs.as_array().unwrap().len(), 2, "the binding plus one use: {refs}");
    h.shutdown();
}

#[test]
fn references_of_a_parameter_stay_in_the_body() {
    let (mut h, uri) = sample();
    let refs = h.references(&uri, position_within(SAMPLE, "tag c", 4), false);
    assert_eq!(refs.as_array().unwrap().len(), 1, "`c` is used once in `paint`: {refs}");
    h.shutdown();
}

#[test]
fn references_of_a_function_find_every_call() {
    let (mut h, uri) = sample();
    let refs = h.references(&uri, position_of(SAMPLE, "tag Red"), false);
    // `tag` is called in `tag c` and `tag Red`.
    assert_eq!(refs.as_array().unwrap().len(), 2, "two calls to `tag`: {refs}");
    h.shutdown();
}

#[test]
fn references_of_a_constructor_cover_expressions_and_patterns() {
    let (mut h, uri) = sample();
    let refs = h.references(&uri, position_within(SAMPLE, "tag Red", 4), true);
    // Variant declaration + the `tag Red` use + the `| Red ->` pattern.
    assert_eq!(refs.as_array().unwrap().len(), 3, "{refs}");
    h.shutdown();
}

#[test]
fn references_off_a_symbol_are_empty() {
    let (mut h, uri) = sample();
    let refs = h.references(&uri, position_of(SAMPLE, "module M"), true);
    assert!(refs.as_array().unwrap().is_empty(), "{refs}");
    h.shutdown();
}

#[test]
fn references_span_every_module() {
    let (mut h, uri_a, _b) = ab();
    // From `inc`'s binding in A, excluding the declaration: the three uses in B.
    let refs = h.references(&uri_a, position_within(A, "inc x", 0), false);
    let refs = refs.as_array().unwrap();
    assert_eq!(refs.len(), 3, "{refs:?}");
    assert!(refs.iter().all(|l| l["uri"].as_str().unwrap().ends_with("B.fai")), "{refs:?}");
    h.shutdown();
}

#[test]
fn references_with_declaration_include_signature_and_binding() {
    let (mut h, uri_a, _b) = ab();
    let refs = h.references(&uri_a, position_within(A, "inc x", 0), true);
    let refs = refs.as_array().unwrap();
    // Signature name + binding name in A, plus three uses in B.
    assert_eq!(refs.len(), 5, "{refs:?}");
    let in_a = refs.iter().filter(|l| l["uri"].as_str().unwrap().ends_with("A.fai")).count();
    assert_eq!(in_a, 2, "{refs:?}");
    h.shutdown();
}

#[test]
fn find_references_from_a_definition_in_another_open_file() {
    // Open both; jump from B's use to A, then list references from A's binding.
    let (mut h, uri_a, uri_b) = ab();
    let def = h.definition(&uri_b, position_within(B, "A.inc", 2));
    assert!(def.as_array().unwrap()[0]["uri"].as_str().unwrap().ends_with("A.fai"));
    let refs = h.references(&uri_a, position_within(A, "inc x", 0), false);
    assert_eq!(refs.as_array().unwrap().len(), 3, "B's three calls: {refs}");
    h.shutdown();
}
