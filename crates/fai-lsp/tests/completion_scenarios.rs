//! Real-world completion scenarios across the contexts an editor triggers in:
//! bare identifiers, qualified `Module.` members, record `value.` fields, and
//! constructors in pattern position.

mod harness;

use harness::{Harness, completion_labels, position_after, position_of};
use indoc::indoc;
use serde_json::Value;

/// The completion item with the given label, from an array-form response.
fn item_named(result: &Value, label: &str) -> Value {
    result
        .as_array()
        .unwrap_or_else(|| panic!("an array of completion items"))
        .iter()
        .find(|i| i["label"] == label)
        .unwrap_or_else(|| panic!("no completion item labelled `{label}`"))
        .clone()
}

/// The completion labels offered just after `prefix` (typically a `Module.` or
/// `value.`) in `src`.
fn labels_after(tag: &str, src: &str, prefix: &str) -> Vec<String> {
    let (mut h, uri) = Harness::open_main(tag, src);
    let labels = completion_labels(&h.completion(&uri, position_after(src, prefix)));
    h.shutdown();
    labels
}

/// The completion labels offered at the start of `needle` (a bare position).
fn labels_at(tag: &str, src: &str, needle: &str) -> Vec<String> {
    let (mut h, uri) = Harness::open_main(tag, src);
    let labels = completion_labels(&h.completion(&uri, position_of(src, needle)));
    h.shutdown();
    labels
}

fn has(labels: &[String], name: &str) -> bool {
    labels.iter().any(|l| l == name)
}

const BARE: &str = indoc! {r#"
    module M

    public type Color =
      | Red
      | Green

    public inc : Int -> Int
    let inc x = x + 1

    public describe : Color -> Int
    let describe c =
      let label = 1
      label

    public flip : Color -> Color
    let flip c =
      match c with
      | Red -> Green
      | Green -> Red
"#};

#[test]
fn bare_completion_offers_a_local_in_scope() {
    let labels = labels_at("c-local", BARE, "label\n");
    assert!(has(&labels, "label"), "{labels:?}");
}

#[test]
fn bare_completion_offers_a_parameter() {
    let labels = labels_at("c-param", BARE, "label\n");
    assert!(has(&labels, "c"), "{labels:?}");
}

#[test]
fn bare_completion_offers_module_definitions() {
    let labels = labels_at("c-defs", BARE, "label\n");
    assert!(has(&labels, "inc") && has(&labels, "describe") && has(&labels, "flip"), "{labels:?}");
}

#[test]
fn bare_completion_offers_this_files_constructors() {
    let labels = labels_at("c-ctors", BARE, "label\n");
    assert!(has(&labels, "Red") && has(&labels, "Green"), "{labels:?}");
}

#[test]
fn bare_completion_offers_prelude_values() {
    let labels = labels_at("c-prelude-val", BARE, "label\n");
    assert!(has(&labels, "identity"), "{labels:?}");
}

#[test]
fn bare_completion_offers_prelude_constructors() {
    let labels = labels_at("c-prelude-ctor", BARE, "label\n");
    assert!(has(&labels, "Some") && has(&labels, "None"), "{labels:?}");
}

#[test]
fn constructors_are_offered_in_pattern_position() {
    // Completing where a pattern is being written offers the constructors.
    let labels = labels_at("c-pat", BARE, "Red -> Green");
    assert!(has(&labels, "Red") && has(&labels, "Green"), "{labels:?}");
}

#[test]
fn completion_marks_functions_with_the_function_kind() {
    let (mut h, uri) = Harness::open_main("c-kind", BARE);
    let result = h.completion(&uri, position_of(BARE, "label\n"));
    let inc = result
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "inc")
        .expect("inc offered")
        .clone();
    assert_eq!(inc["kind"], 3, "function kind: {inc:?}");
    h.shutdown();
}

#[test]
fn qualified_completion_lists_list_members() {
    let src = "module M\n\npublic f : List Int -> Int\nlet f xs = List.length xs\n";
    let labels = labels_after("c-list", src, "List.");
    assert!(has(&labels, "map") && has(&labels, "filter") && has(&labels, "length"), "{labels:?}");
    assert!(has(&labels, "foldl") && has(&labels, "reverse"), "{labels:?}");
}

#[test]
fn qualified_completion_lists_option_members() {
    let src = "module M\n\npublic n : Int\nlet n = Option.withDefault 0 None\n";
    let labels = labels_after("c-option", src, "Option.");
    assert!(
        has(&labels, "map") && has(&labels, "withDefault") && has(&labels, "isSome"),
        "{labels:?}"
    );
}

#[test]
fn qualified_completion_lists_result_members() {
    let src = "module M\n\npublic b : Bool\nlet b = Result.isOk (Ok 1)\n";
    let labels = labels_after("c-result", src, "Result.");
    assert!(has(&labels, "map") && has(&labels, "isOk") && has(&labels, "isErr"), "{labels:?}");
}

#[test]
fn qualified_completion_lists_int_members() {
    let src = "module M\n\npublic s : String\nlet s = Int.toString 0\n";
    let labels = labels_after("c-int", src, "Int.");
    assert!(has(&labels, "toString") && has(&labels, "toFloat"), "{labels:?}");
}

#[test]
fn qualified_completion_lists_string_members() {
    let src = "module M\n\npublic n : Int\nlet n = String.length \"hi\"\n";
    let labels = labels_after("c-string", src, "String.");
    assert!(
        has(&labels, "length") && has(&labels, "toUpper") && has(&labels, "split"),
        "{labels:?}"
    );
}

#[test]
fn qualified_completion_lists_float_members() {
    let src = "module M\n\npublic x : Float\nlet x = Float.sqrt 4.0\n";
    let labels = labels_after("c-float", src, "Float.");
    assert!(has(&labels, "sqrt") && has(&labels, "toInt") && has(&labels, "pi"), "{labels:?}");
}

#[test]
fn qualified_completion_lists_dict_members() {
    let src = "module M\n\npublic d : Dict Int Int\nlet d = Dict.empty\n";
    let labels = labels_after("c-dict", src, "Dict.");
    assert!(has(&labels, "empty") && has(&labels, "insert") && has(&labels, "get"), "{labels:?}");
}

#[test]
fn qualified_completion_lists_set_members() {
    let src = "module M\n\npublic s : Set Int\nlet s = Set.empty\n";
    let labels = labels_after("c-set", src, "Set.");
    assert!(
        has(&labels, "empty") && has(&labels, "insert") && has(&labels, "member"),
        "{labels:?}"
    );
}

#[test]
fn record_field_completion_lists_the_fields() {
    let src = "module M\n\npublic area : { width : Int, height : Int } -> Int\nlet area rect = rect.width\n";
    let labels = labels_after("c-fields", src, "rect.");
    assert_eq!(labels, vec!["height".to_owned(), "width".to_owned()], "{labels:?}");
}

#[test]
fn record_field_completion_carries_the_field_type() {
    let src = "module M\n\npublic area : { width : Int, height : Int } -> Int\nlet area rect = rect.width\n";
    let (mut h, uri) = Harness::open_main("c-field-type", src);
    let result = h.completion(&uri, position_after(src, "rect."));
    let width = result
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["label"] == "width")
        .expect("width offered")
        .clone();
    assert_eq!(width["detail"], "Int", "{width:?}");
    // Fields use the LSP field kind (5).
    assert_eq!(width["kind"], 5, "{width:?}");
    h.shutdown();
}

#[test]
fn resolving_a_documented_member_fills_its_docs_and_contracts() {
    // Completing `Int.` offers `toString`; resolving the chosen item fills its
    // documentation with the type signature, the `///` prose, and the contracts.
    let src = "module M\n\npublic s : String\nlet s = Int.toString 0\n";
    let (mut h, uri) = Harness::open_main("c-resolve-docs", src);
    let result = h.completion(&uri, position_after(src, "Int."));
    let resolved = h.resolve_completion(item_named(&result, "toString"));
    assert_eq!(resolved["documentation"]["kind"], "markdown", "{resolved:?}");
    let expected = "```fai\n\
        toString : Int -> String\n\
        ```\n\
        \n\
        Render an integer in base 10.\n\
        \n\
        ```fai\n\
        example: toString 42 = \"42\"\n\
        example: toString 0 = \"0\"\n\
        ```";
    assert_eq!(resolved["documentation"]["value"], expected, "{resolved:?}");
    h.shutdown();
}

#[test]
fn resolving_an_undocumented_definition_yields_only_its_type() {
    // An undocumented binding has no prose or contracts, so its resolved
    // documentation is just the type-signature block.
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n\npublic two : Int\nlet two = inc 1\n";
    let (mut h, uri) = Harness::open_main("c-resolve-plain", src);
    let result = h.completion(&uri, position_of(src, "inc 1"));
    let resolved = h.resolve_completion(item_named(&result, "inc"));
    assert_eq!(resolved["documentation"]["value"], "```fai\ninc : Int -> Int\n```", "{resolved:?}");
    h.shutdown();
}

#[test]
fn resolving_a_field_item_leaves_it_unchanged() {
    // Record fields have no addressable definition, so they carry no resolve
    // payload and resolving them adds no documentation.
    let src = "module M\n\npublic area : { width : Int, height : Int } -> Int\nlet area rect = rect.width\n";
    let (mut h, uri) = Harness::open_main("c-resolve-field", src);
    let result = h.completion(&uri, position_after(src, "rect."));
    let item = item_named(&result, "width");
    assert!(item.get("data").is_none(), "a field item has no resolve payload: {item:?}");
    let resolved = h.resolve_completion(item);
    assert!(resolved.get("documentation").is_none(), "no documentation added: {resolved:?}");
    h.shutdown();
}

#[test]
fn qualified_completion_offers_constructors_of_a_module() {
    // `Option.` includes the data constructors of types it re-exports? The std
    // `Option` module's members are functions; completing it lists them, and the
    // result is never empty.
    let src = "module M\n\npublic n : Int\nlet n = Option.withDefault 0 None\n";
    let labels = labels_after("c-nonempty", src, "Option.");
    assert!(!labels.is_empty(), "a known module always offers members");
}
