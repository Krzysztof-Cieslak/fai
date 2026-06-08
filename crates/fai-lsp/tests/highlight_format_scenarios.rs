//! Real-world scenarios for inlay hints, semantic tokens, and formatting.

mod harness;

use harness::{Harness, position_of, range, whole_document};
use indoc::indoc;
use serde_json::{Value, json};

// --- inlay hints -------------------------------------------------------------

const BINDERS: &str = indoc! {r#"
    module M

    public area : Int -> Int
    let area w =
      let n = w + 1
      n

    public describe : Bool -> Int
    let describe flag =
      if flag then 1 else 0

    public unwrap : Option Int -> Int
    let unwrap o =
      match o with
      | Some v -> v
      | None -> 0
"#};

fn hint_labels(result: &Value) -> Vec<String> {
    result
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["label"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

#[test]
fn inlay_hints_annotate_a_parameter() {
    let (mut h, uri) = Harness::open_main("ih-param", BINDERS);
    let labels = hint_labels(&h.inlay_hints(&uri, whole_document()));
    assert!(labels.iter().any(|l| l == ": Bool"), "the `flag` parameter: {labels:?}");
    h.shutdown();
}

#[test]
fn inlay_hints_annotate_a_local() {
    let (mut h, uri) = Harness::open_main("ih-local", BINDERS);
    let labels = hint_labels(&h.inlay_hints(&uri, whole_document()));
    // `w`, `n`, and `o` are all `Int`-ish; at least one `: Int` hint is present.
    assert!(labels.iter().any(|l| l == ": Int"), "{labels:?}");
    h.shutdown();
}

#[test]
fn inlay_hints_annotate_a_match_binder() {
    let (mut h, uri) = Harness::open_main("ih-match", BINDERS);
    let result = h.inlay_hints(&uri, whole_document());
    // The `Some v` binder `v` sits on the `| Some v ->` line.
    let v_line = position_of(BINDERS, "Some v")["line"].as_u64().unwrap();
    let on_v =
        result.as_array().unwrap().iter().any(|hint| {
            hint["position"]["line"].as_u64() == Some(v_line) && hint["label"] == ": Int"
        });
    assert!(on_v, "a hint for `v`: {result}");
    h.shutdown();
}

#[test]
fn inlay_hints_use_the_type_kind() {
    let (mut h, uri) = Harness::open_main("ih-kind", BINDERS);
    let result = h.inlay_hints(&uri, whole_document());
    assert!(result.as_array().unwrap().iter().all(|hint| hint["kind"] == 1), "{result}");
    h.shutdown();
}

#[test]
fn no_inlay_hints_for_a_nullary_binding() {
    let (mut h, uri) = Harness::open_main("ih-none", "module M\n\npublic two : Int\nlet two = 2\n");
    assert!(h.inlay_hints(&uri, whole_document()).as_array().unwrap().is_empty());
    h.shutdown();
}

#[test]
fn inlay_hints_respect_the_range() {
    let (mut h, uri) = Harness::open_main("ih-range", BINDERS);
    // A range covering only the `describe` line includes just `flag`.
    let line = position_of(BINDERS, "describe flag")["line"].as_u64().unwrap() as u32;
    let hints = h.inlay_hints(
        &uri,
        range(json!({ "line": line, "character": 0 }), json!({ "line": line, "character": 40 })),
    );
    let labels = hint_labels(&hints);
    assert_eq!(labels, vec![": Bool".to_owned()], "{labels:?}");
    h.shutdown();
}

// --- semantic tokens ---------------------------------------------------------

const HIGHLIGHT: &str = indoc! {r#"
    module M

    public greet : String
    let greet = "hello"

    public count : Int
    let count = 42

    public type Color =
      | Red

    public pick : Color
    let pick = Red

    public inc : Int -> Int
    let inc n = n + 1

    public two : Int
    let two = inc 1
    // trailing note
"#};

/// Decodes the delta-encoded semantic tokens into absolute
/// `(line, char, length, type_index)` tuples.
fn decode(result: &Value) -> Vec<(u32, u32, u32, u32)> {
    let data: Vec<u32> =
        result["data"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
    let (mut line, mut ch) = (0u32, 0u32);
    let mut out = Vec::new();
    for chunk in data.chunks(5) {
        line += chunk[0];
        ch = if chunk[0] == 0 { ch + chunk[1] } else { chunk[1] };
        out.push((line, ch, chunk[2], chunk[3]));
    }
    out
}

/// The type index of the token starting at the position of `needle`.
fn type_at(tokens: &[(u32, u32, u32, u32)], text: &str, needle: &str) -> Option<u32> {
    let p = position_of(text, needle);
    let (line, ch) = (p["line"].as_u64().unwrap() as u32, p["character"].as_u64().unwrap() as u32);
    tokens.iter().find(|t| t.0 == line && t.1 == ch).map(|t| t.3)
}

// Legend indices (see fai_ide::SEMANTIC_TOKEN_TYPES).
const KEYWORD: u32 = 0;
const FUNCTION: u32 = 1;
const TYPE: u32 = 3;
const ENUM_MEMBER: u32 = 5;
const NUMBER: u32 = 7;
const STRING: u32 = 8;
const COMMENT: u32 = 10;

#[test]
fn semantic_tokens_are_well_formed() {
    let (mut h, uri) = Harness::open_main("st-form", HIGHLIGHT);
    let data = h.semantic_tokens(&uri)["data"].as_array().unwrap().len();
    assert!(data > 0 && data % 5 == 0, "5-tuples: {data}");
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_keyword() {
    let (mut h, uri) = Harness::open_main("st-kw", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    assert_eq!(type_at(&tokens, HIGHLIGHT, "module M"), Some(KEYWORD));
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_number() {
    let (mut h, uri) = Harness::open_main("st-num", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    assert_eq!(type_at(&tokens, HIGHLIGHT, "42"), Some(NUMBER));
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_string() {
    let (mut h, uri) = Harness::open_main("st-str", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    assert_eq!(type_at(&tokens, HIGHLIGHT, "\"hello\""), Some(STRING));
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_comment() {
    let (mut h, uri) = Harness::open_main("st-cmt", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    assert_eq!(type_at(&tokens, HIGHLIGHT, "// trailing note"), Some(COMMENT));
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_constructor_use() {
    let (mut h, uri) = Harness::open_main("st-ctor", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    // `Red` in `let pick = Red` (the use, followed by `public inc`).
    assert_eq!(type_at(&tokens, HIGHLIGHT, "Red\n\npublic inc"), Some(ENUM_MEMBER));
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_function_use() {
    let (mut h, uri) = Harness::open_main("st-fn", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    assert_eq!(type_at(&tokens, HIGHLIGHT, "inc 1"), Some(FUNCTION));
    h.shutdown();
}

#[test]
fn semantic_tokens_mark_a_type_name() {
    let (mut h, uri) = Harness::open_main("st-type", HIGHLIGHT);
    let tokens = decode(&h.semantic_tokens(&uri));
    assert_eq!(type_at(&tokens, HIGHLIGHT, "Color\nlet pick"), Some(TYPE));
    h.shutdown();
}

// --- formatting --------------------------------------------------------------

#[test]
fn full_formatting_spaces_operators() {
    let messy = "module M\n\npublic a : Int\nlet a=1+2\n";
    let (mut h, uri) = Harness::open_main("fmt-full", messy);
    let edits = h.formatting(&uri);
    let new_text = edits.as_array().unwrap()[0]["newText"].as_str().unwrap();
    assert!(new_text.contains("let a = 1 + 2"), "{new_text}");
    h.shutdown();
}

#[test]
fn formatting_an_already_canonical_file_is_a_no_op() {
    let clean = "module M\n\npublic a : Int\nlet a = 1\n";
    let (mut h, uri) = Harness::open_main("fmt-noop", clean);
    let edits = h.formatting(&uri);
    // Either no edits, or an edit that reproduces the same text.
    let unchanged =
        edits.as_array().unwrap().is_empty() || edits.as_array().unwrap()[0]["newText"] == clean;
    assert!(unchanged, "{edits}");
    h.shutdown();
}

#[test]
fn range_formatting_touches_only_the_requested_lines() {
    let messy = "module M\n\npublic a : Int\nlet a=1\n\npublic b : Int\nlet b=2\n";
    let (mut h, uri) = Harness::open_main("fmt-range", messy);
    let edits = h.range_formatting(
        &uri,
        range(json!({ "line": 3, "character": 0 }), json!({ "line": 3, "character": 7 })),
    );
    let edits = edits.as_array().unwrap();
    assert!(!edits.is_empty(), "the messy line is reformatted");
    assert!(edits.iter().all(|e| e["range"]["start"]["line"] == 3), "only line 3: {edits:?}");
    h.shutdown();
}
