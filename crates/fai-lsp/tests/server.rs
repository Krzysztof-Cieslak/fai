//! End-to-end tests for the language server over an in-memory connection.
//!
//! Each test stands up a real workspace on disk, runs [`fai_lsp::serve`] on a
//! background thread against one half of an in-memory [`Connection`], and drives
//! the other half as a client: it performs the initialize handshake, then opens
//! documents and issues requests, asserting on the JSON the server returns.

mod harness;
use harness::{Harness, apply_text_edits, changes_for};
use indoc::indoc;
use serde_json::{Value, json};

const MAIN: &str = indoc! {r#"
    module Main

    public inc : Int -> Int
    let inc x = x + 1

    public two : Int
    let two = inc 1
"#};

#[test]
fn publishes_empty_diagnostics_for_a_clean_file() {
    let mut harness = Harness::start("clean", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let diagnostics = harness.diagnostics(&uri);
    assert!(diagnostics.is_empty(), "a well-typed file has no diagnostics: {diagnostics:?}");
    harness.shutdown();
}

#[test]
fn publishes_type_errors_as_diagnostics() {
    let bad = indoc! {r#"
        module Main

        public bad : Int -> Bool
        let bad x = x + 1
    "#};
    let mut harness = Harness::start("errors", &[("Main.fai", bad)]);
    let uri = harness.did_open("Main.fai", bad);
    let diagnostics = harness.diagnostics(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let d = &diagnostics[0];
    assert!(d["code"].as_str().unwrap().starts_with("FAI"), "{d:?}");
    assert_eq!(d["severity"], 1, "a type error is an LSP error (severity 1)");
    assert_eq!(d["source"], "fai");
    // The diagnostic points inside the file (line 3, the body `x + 1`).
    assert_eq!(d["range"]["start"]["line"], 3);
    harness.shutdown();
}

#[test]
fn hover_reports_the_referenced_type() {
    let mut harness = Harness::start("hover", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    // Hover over `inc` in `let two = inc 1` (line 6, column 10).
    let result = harness.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } }),
    );
    let value = result["contents"]["value"].as_str().unwrap();
    assert!(value.contains("inc : Int -> Int"), "hover text: {value:?}");
    // The hover range underlines the reference `inc`.
    assert_eq!(result["range"]["start"], json!({ "line": 6, "character": 10 }));
    assert_eq!(result["range"]["end"], json!({ "line": 6, "character": 13 }));
    harness.shutdown();
}

#[test]
fn definition_jumps_to_the_binding() {
    let mut harness = Harness::start("def", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let result = harness.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } }),
    );
    let locations = result.as_array().expect("an array of locations");
    assert_eq!(locations.len(), 1, "{locations:?}");
    // It jumps to the `inc` binding (line 3).
    assert!(locations[0]["uri"].as_str().unwrap().ends_with("Main.fai"));
    assert_eq!(locations[0]["range"]["start"]["line"], 3);
    harness.shutdown();
}

#[test]
fn formatting_returns_canonical_text() {
    let messy = "module Main\n\npublic two : Int\nlet two=inc 1\n\npublic inc : Int -> Int\nlet inc x=x+1\n";
    let mut harness = Harness::start("fmt", &[("Main.fai", messy)]);
    let uri = harness.did_open("Main.fai", messy);
    let result = harness.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let edits = result.as_array().expect("an array of edits");
    let new_text = edits[0]["newText"].as_str().unwrap();
    // The canonical form spaces the operators and the `=`.
    assert!(new_text.contains("let two = inc 1"), "{new_text:?}");
    assert!(new_text.contains("let inc x = x + 1"), "{new_text:?}");
    assert!(!new_text.contains("=inc") && !new_text.contains("x=x"), "{new_text:?}");
    harness.shutdown();
}

// --- navigation & structure --------------------------------------------------

#[test]
fn document_symbol_lists_top_level_bindings() {
    let mut harness = Harness::start("docsym", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    let result =
        harness.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": uri } }));
    let symbols = result.as_array().expect("an array of document symbols");
    // Sorted by name: `inc` then `two`.
    let names: Vec<&str> = symbols.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["inc", "two"], "{symbols:?}");
    // `inc` is a function (LSP kind 12), `two` a value (kind 13).
    assert_eq!(symbols[0]["kind"], 12, "{symbols:?}");
    assert_eq!(symbols[1]["kind"], 13, "{symbols:?}");
    assert_eq!(symbols[0]["detail"], "Int -> Int");
    harness.shutdown();
}

#[test]
fn workspace_symbol_finds_by_query() {
    let mut harness = Harness::start("wssym", &[("Main.fai", MAIN)]);
    let _ = harness.did_open("Main.fai", MAIN);
    let result = harness.request("workspace/symbol", json!({ "query": "inc" }));
    let symbols = result.as_array().expect("an array of symbols");
    assert_eq!(symbols.len(), 1, "only `inc` matches: {symbols:?}");
    assert_eq!(symbols[0]["name"], "inc");
    assert!(symbols[0]["location"]["uri"].as_str().unwrap().ends_with("Main.fai"), "{symbols:?}");
    assert_eq!(symbols[0]["containerName"], "Main");
    harness.shutdown();
}

#[test]
fn references_list_uses_and_declaration() {
    let mut harness = Harness::start("refs", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    // Invoke on the use `inc` in `let two = inc 1` (line 6, col 10).
    let with_decl = harness.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 6, "character": 10 },
            "context": { "includeDeclaration": true }
        }),
    );
    let locs = with_decl.as_array().expect("locations");
    assert_eq!(locs.len(), 3, "the signature and binding names plus one use: {locs:?}");
    let lines: Vec<i64> =
        locs.iter().map(|l| l["range"]["start"]["line"].as_i64().unwrap()).collect();
    assert!(
        lines.contains(&2) && lines.contains(&3) && lines.contains(&6),
        "signature on line 2, binding on line 3, use on line 6: {lines:?}"
    );
    harness.shutdown();
}

#[test]
fn references_span_multiple_modules() {
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n\nlet four = A.inc (A.inc two)\n";
    let mut harness = Harness::start("refs-multi", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let _ = harness.did_open("B.fai", b);
    // Invoke on the `inc` binding's name in A (line 3, col 4).
    let result = harness.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri_a },
            "position": { "line": 3, "character": 4 },
            "context": { "includeDeclaration": true }
        }),
    );
    let locs = result.as_array().expect("locations");
    // Two declaration names in A (signature + binding), three uses in B.
    assert_eq!(locs.len(), 5, "{locs:?}");
    let in_b = locs.iter().filter(|l| l["uri"].as_str().unwrap().ends_with("B.fai")).count();
    assert_eq!(in_b, 3, "{locs:?}");
    harness.shutdown();
}

#[test]
fn prepare_rename_returns_the_name_range() {
    let mut harness = Harness::start("prep-rename", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    // On the `inc` use in `let two = inc 1` (line 6, col 10).
    let result = harness.request(
        "textDocument/prepareRename",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } }),
    );
    assert_eq!(result["placeholder"], "inc", "{result:?}");
    assert_eq!(result["range"]["start"], json!({ "line": 6, "character": 10 }));
    assert_eq!(result["range"]["end"], json!({ "line": 6, "character": 13 }));
    harness.shutdown();
}

#[test]
fn rename_rewrites_across_modules() {
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n\nlet four = A.inc (A.inc two)\n";
    let mut harness = Harness::start("rename-multi", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let _ = harness.did_open("B.fai", b);
    // Rename from the `inc` declaration in A (line 3, col 4).
    let result = harness.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri_a },
            "position": { "line": 3, "character": 4 },
            "newName": "increment"
        }),
    );
    // The workspace-edit keys are canonicalized URIs, so match them by file suffix.
    let changes = result["changes"].as_object().expect("a changes map");
    let edits_for = |suffix: &str| -> Vec<Value> {
        changes
            .iter()
            .find(|(k, _)| k.ends_with(suffix))
            .and_then(|(_, v)| v.as_array().cloned())
            .unwrap_or_default()
    };
    let a_edits = edits_for("A.fai");
    let b_edits = edits_for("B.fai");
    assert_eq!(a_edits.len(), 2, "the signature and binding names in A: {a_edits:?}");
    assert_eq!(b_edits.len(), 3, "three uses in B: {b_edits:?}");
    assert!(a_edits.iter().all(|e| e["newText"] == "increment"), "{a_edits:?}");
    assert!(b_edits.iter().all(|e| e["newText"] == "increment"), "{b_edits:?}");
    harness.shutdown();
}

#[test]
fn rename_rejects_an_invalid_name() {
    let mut harness = Harness::start("rename-bad", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    // A value cannot be renamed to an upper-case (constructor) name.
    let result = harness.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 6, "character": 10 },
            "newName": "Inc"
        }),
    );
    assert!(result.is_null(), "an invalid rename yields no edit: {result:?}");
    harness.shutdown();
}

#[test]
fn completion_offers_qualified_members() {
    let src = "module M\n\npublic total : List Int -> Int\nlet total xs = List.length xs\n";
    let mut harness = Harness::start("complete-qual", &[("M.fai", src)]);
    let uri = harness.did_open("M.fai", src);
    // Right after `List.` (line 3, col 20).
    let result = harness.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 20 } }),
    );
    let items = result.as_array().expect("an array of completion items");
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"length") && labels.contains(&"map"), "{labels:?}");
    // `length` is a function (LSP completion kind 3).
    let length = items.iter().find(|i| i["label"] == "length").unwrap();
    assert_eq!(length["kind"], 3, "{length:?}");
    harness.shutdown();
}

#[test]
fn completion_offers_record_fields() {
    let src =
        "module M\n\npublic area : { width : Int, height : Int } -> Int\nlet area r = r.width\n";
    let mut harness = Harness::start("complete-field", &[("M.fai", src)]);
    let uri = harness.did_open("M.fai", src);
    // Right after `r.` (line 3, col 15).
    let result = harness.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 15 } }),
    );
    let items = result.as_array().expect("an array of completion items");
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert_eq!(labels, vec!["height", "width"], "{labels:?}");
    // Fields are LSP completion kind 5.
    assert!(items.iter().all(|i| i["kind"] == 5), "{items:?}");
    harness.shutdown();
}

#[test]
fn completion_offers_in_scope_names() {
    let src =
        "module M\n\npublic describe : Int -> Int\nlet describe c =\n  let label = 1\n  label\n";
    let mut harness = Harness::start("complete-bare", &[("M.fai", src)]);
    let uri = harness.did_open("M.fai", src);
    // In the trailing `label` (line 5).
    let result = harness.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 5, "character": 4 } }),
    );
    let items = result.as_array().expect("an array of completion items");
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"c"), "the parameter: {labels:?}");
    assert!(labels.contains(&"label"), "the local: {labels:?}");
    assert!(labels.contains(&"describe"), "the module definition: {labels:?}");
    harness.shutdown();
}

#[test]
fn hover_includes_doc_prose_and_contracts() {
    let src = indoc! {r#"
        module Main

        /// Increment by one.
        public inc : Int -> Int
        let inc x = x + 1
        example: inc 1 = 2

        public two : Int
        let two = inc 7
    "#};
    let mut harness = Harness::start("hover-doc", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    // Hover the `inc` use in `let two = inc 7` (line 8, col 10).
    let result = harness.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 8, "character": 10 } }),
    );
    let value = result["contents"]["value"].as_str().unwrap();
    assert!(value.contains("inc : Int -> Int"), "type line: {value:?}");
    assert!(value.contains("Increment by one."), "doc prose: {value:?}");
    assert!(value.contains("example: inc 1 = 2"), "attached contract: {value:?}");
    harness.shutdown();
}

#[test]
fn signature_help_reports_parameters_and_active() {
    let src = indoc! {r#"
        module Main

        public add : Int -> Int -> Int
        let add x y = x + y

        public apply : Int
        let apply = add 1 2
    "#};
    let mut harness = Harness::start("sighelp", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    // On the first argument `1` (line 6, col 16).
    let first = harness.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 16 } }),
    );
    assert_eq!(first["signatures"][0]["label"], "add : Int -> Int -> Int", "{first:?}");
    assert_eq!(first["activeParameter"], 0, "{first:?}");
    let params = first["signatures"][0]["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2, "{params:?}");
    // On the second argument `2` (col 18): parameter 1.
    let second = harness.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 18 } }),
    );
    assert_eq!(second["activeParameter"], 1, "{second:?}");
    harness.shutdown();
}

#[test]
fn code_action_adds_a_missing_signature() {
    let src = "module Main\n\npublic let inc x = x + 1\n";
    let mut harness = Harness::start("ca-sig", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    let result = harness.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 2, "character": 11 },
                "end": { "line": 2, "character": 14 }
            },
            "context": { "diagnostics": [] }
        }),
    );
    let actions = result.as_array().expect("an array of code actions");
    let fix = actions
        .iter()
        .find(|a| a["title"] == "Add the inferred signature")
        .unwrap_or_else(|| panic!("no signature fix: {actions:?}"));
    assert_eq!(fix["kind"], "quickfix");
    let changes = fix["edit"]["changes"].as_object().unwrap();
    let edits = changes.values().next().unwrap().as_array().unwrap();
    assert!(
        edits[0]["newText"].as_str().unwrap().contains("public inc : Int -> Int"),
        "edit: {edits:?}"
    );
    harness.shutdown();
}

#[test]
fn code_action_qualifies_an_unbound_name() {
    let src = "module Main\n\npublic ids : List Int -> List Int\nlet ids xs = map identity xs\n";
    let mut harness = Harness::start("ca-qual", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    // Range over the bare `map` (line 3, cols 13..16).
    let result = harness.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 13 },
                "end": { "line": 3, "character": 16 }
            },
            "context": { "diagnostics": [] }
        }),
    );
    let actions = result.as_array().expect("an array of code actions");
    let titles: Vec<&str> = actions.iter().map(|a| a["title"].as_str().unwrap()).collect();
    let fix = actions
        .iter()
        .find(|a| a["title"] == "Qualify as `List.map`")
        .unwrap_or_else(|| panic!("no List.map fix among {titles:?}"));
    let changes = fix["edit"]["changes"].as_object().unwrap();
    let edits = changes.values().next().unwrap().as_array().unwrap();
    assert_eq!(edits[0]["newText"], "List.map", "{edits:?}");
    harness.shutdown();
}

#[test]
fn inlay_hints_show_inferred_binder_types() {
    let src = "module Main\n\npublic area : Int -> Int\nlet area w = w + 1\n";
    let mut harness = Harness::start("inlay", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    let result = harness.request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 9, "character": 0 }
            }
        }),
    );
    let hints = result.as_array().expect("an array of inlay hints");
    // The parameter `w` is hinted as `Int` (LSP inlay kind 1 = Type).
    let w = hints
        .iter()
        .find(|h| h["label"] == ": Int")
        .unwrap_or_else(|| panic!("no `: Int` hint: {hints:?}"));
    assert_eq!(w["position"], json!({ "line": 3, "character": 10 }), "after `w`: {w:?}");
    assert_eq!(w["kind"], 1, "{w:?}");
    harness.shutdown();
}

#[test]
fn semantic_tokens_encode_the_document() {
    let src = "module Main\n\npublic two : Int\nlet two = 2\n";
    let mut harness = Harness::start("semtok", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    let result = harness
        .request("textDocument/semanticTokens/full", json!({ "textDocument": { "uri": uri } }));
    let data = result["data"].as_array().expect("delta-encoded token data");
    assert!(!data.is_empty() && data.len().is_multiple_of(5), "5-tuples: {data:?}");
    // The first token is the `module` keyword: line 0, col 0, length 6, type 0
    // (keyword is the first legend entry), no modifiers.
    let head: Vec<i64> = data[0..5].iter().map(|v| v.as_i64().unwrap()).collect();
    assert_eq!(head, vec![0, 0, 6, 0, 0], "leading `module` keyword: {head:?}");
    harness.shutdown();
}

// --- editing fidelity & dependent diagnostics --------------------------------

#[test]
fn advertises_completion_item_resolve() {
    let (h, init) = Harness::start_with_caps("cap-resolve", &[("Main.fai", MAIN)], json!({}));
    assert_eq!(
        init["capabilities"]["completionProvider"]["resolveProvider"], true,
        "the server offers lazy completion resolution: {init:?}"
    );
    h.shutdown();
}

#[test]
fn advertises_on_type_formatting() {
    let (h, init) = Harness::start_with_caps("cap-ontype", &[("Main.fai", MAIN)], json!({}));
    assert_eq!(
        init["capabilities"]["documentOnTypeFormattingProvider"]["firstTriggerCharacter"], "\n",
        "the server reformats on a newline trigger: {init:?}"
    );
    h.shutdown();
}

#[test]
fn negotiates_position_encoding() {
    // With no client preference, the server advertises the LSP default UTF-16.
    let (def, init) = Harness::start_with_caps("enc-default", &[("Main.fai", MAIN)], json!({}));
    assert_eq!(init["capabilities"]["positionEncoding"], "utf-16", "{init:?}");
    def.shutdown();
    // When the client offers UTF-8, the server picks it.
    let (utf8, init) = Harness::start_with_caps(
        "enc-utf8",
        &[("Main.fai", MAIN)],
        json!({ "general": { "positionEncodings": ["utf-8", "utf-16"] } }),
    );
    assert_eq!(init["capabilities"]["positionEncoding"], "utf-8", "{init:?}");
    utf8.shutdown();
}

#[test]
fn incremental_range_edit_updates_the_buffer() {
    let clean = "module Main\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let mut harness = Harness::start("incremental", &[("Main.fai", clean)]);
    let uri = harness.did_open("Main.fai", clean);
    assert!(harness.diagnostics(&uri).is_empty(), "clean to start");
    // Replace the body `x + 1` (line 3, cols 12..17) with `true` — a type error.
    harness.did_change_range(
        &uri,
        json!({ "start": { "line": 3, "character": 12 }, "end": { "line": 3, "character": 17 } }),
        "true",
    );
    let diagnostics = harness.diagnostics(&uri);
    assert!(!diagnostics.is_empty(), "the range edit introduced a type error: {diagnostics:?}");
    assert!(
        diagnostics.iter().all(|d| d["code"].as_str().unwrap().starts_with("FAI")),
        "{diagnostics:?}"
    );
    harness.shutdown();
}

#[test]
fn cross_module_change_refreshes_open_dependents() {
    let a = "module A\n\npublic n : Int\nlet n = 0\n";
    let b = "module B\n\npublic m : Int\nlet m = A.n + 1\n";
    let mut harness = Harness::start("dependent", &[("A.fai", a), ("B.fai", b)]);
    let _uri_a = harness.did_open("A.fai", a);
    let uri_b = harness.did_open("B.fai", b);
    assert!(harness.diagnostics(&uri_b).is_empty(), "B is valid against A's `n : Int`");
    // Retype `n` to `Bool` in A only — this breaks B's `A.n + 1`.
    let broken_a = "module A\n\npublic n : Bool\nlet n = true\n";
    harness.did_change(&_uri_a, broken_a);
    // B was not touched, yet its diagnostics refresh because A changed.
    assert!(
        !harness.diagnostics(&uri_b).is_empty(),
        "B's diagnostics refresh after the cross-module edit"
    );
    harness.shutdown();
}

#[test]
fn range_formatting_touches_only_the_range() {
    let messy = "module Main\n\npublic a : Int\nlet a=1\n\npublic b : Int\nlet b=2\n";
    let mut harness = Harness::start("range-fmt", &[("Main.fai", messy)]);
    let uri = harness.did_open("Main.fai", messy);
    // Format only the first binding's line (line 3, `let a=1`).
    let result = harness.request(
        "textDocument/rangeFormatting",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 3, "character": 7 }
            },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let edits = result.as_array().expect("an array of edits");
    assert!(!edits.is_empty(), "the messy `let a=1` is reformatted: {edits:?}");
    // Every edit stays on line 3 — `let b=2` on line 6 is left untouched.
    for edit in edits {
        assert_eq!(edit["range"]["start"]["line"], 3, "edit outside the range: {edit:?}");
    }
    let joined: String = edits.iter().map(|e| e["newText"].as_str().unwrap()).collect();
    assert!(joined.contains("let a = 1"), "{joined:?}");
    harness.shutdown();
}

// --- real-world editing sessions ---------------------------------------------
//
// These drive multi-step flows the way an editor does: request an action, apply
// the server's edits back into the buffer, re-sync, and verify the result — so a
// fix that is incomplete (e.g. a rename that misses the signature) is caught.

#[test]
fn quick_fix_round_trip_adds_a_signature() {
    let src = "module Main\n\npublic let inc x = x + 1\n";
    let mut harness = Harness::start("rt-sig", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    assert!(
        harness.diagnostics(&uri).iter().any(|d| d["code"] == "FAI3003"),
        "starts missing a public signature"
    );
    // Ask for the quick fix over the binding and apply its edit.
    let actions = harness.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 2, "character": 11 },
                "end": { "line": 2, "character": 14 }
            },
            "context": { "diagnostics": [] }
        }),
    );
    let fix = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Add the inferred signature")
        .expect("a signature fix")
        .clone();
    let fixed = apply_text_edits(src, &changes_for(&fix["edit"], "Main.fai"));
    assert!(fixed.contains("public inc : Int -> Int\nlet inc x = x + 1"), "{fixed:?}");
    // Apply it the way an editor would, then the diagnostic is gone.
    harness.did_change(&uri, &fixed);
    assert!(harness.diagnostics(&uri).is_empty(), "the fix resolves the diagnostic");
    harness.shutdown();
}

#[test]
fn quick_fix_round_trip_qualifies_a_name() {
    let src = "module Main\n\npublic ids : List Int -> List Int\nlet ids xs = map identity xs\n";
    let mut harness = Harness::start("rt-qual", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    assert!(
        harness.diagnostics(&uri).iter().any(|d| d["code"] == "FAI2001"),
        "`map` starts unbound"
    );
    let actions = harness.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 13 },
                "end": { "line": 3, "character": 16 }
            },
            "context": { "diagnostics": [] }
        }),
    );
    let fix = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Qualify as `List.map`")
        .expect("a qualify fix")
        .clone();
    let fixed = apply_text_edits(src, &changes_for(&fix["edit"], "Main.fai"));
    assert!(fixed.contains("let ids xs = List.map identity xs"), "{fixed:?}");
    harness.did_change(&uri, &fixed);
    assert!(harness.diagnostics(&uri).is_empty(), "qualifying resolves the program");
    harness.shutdown();
}

#[test]
fn rename_round_trip_keeps_the_program_valid() {
    // A signatured definition used across a module boundary: renaming it must
    // rewrite the signature, the binding, and the cross-module uses together, or
    // applying the edit would leave a stale signature (orphan + private use).
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n";
    let mut harness = Harness::start("rt-rename", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let uri_b = harness.did_open("B.fai", b);
    assert!(harness.diagnostics(&uri_a).is_empty() && harness.diagnostics(&uri_b).is_empty());

    let edit = harness.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri_a },
            "position": { "line": 3, "character": 4 },
            "newName": "bump"
        }),
    );
    let new_a = apply_text_edits(a, &changes_for(&edit, "A.fai"));
    let new_b = apply_text_edits(b, &changes_for(&edit, "B.fai"));
    harness.did_change(&uri_a, &new_a);
    harness.did_change(&uri_b, &new_b);

    // The signature, binding, and the cross-module use are all renamed …
    assert!(new_a.contains("public bump : Int -> Int\nlet bump x = x + 1"), "A: {new_a:?}");
    assert!(new_b.contains("A.bump 1"), "B: {new_b:?}");
    // … and both files still typecheck.
    assert!(harness.diagnostics(&uri_a).is_empty(), "A valid: {:?}", harness.diagnostics(&uri_a));
    assert!(harness.diagnostics(&uri_b).is_empty(), "B valid: {:?}", harness.diagnostics(&uri_b));
    harness.shutdown();
}

#[test]
fn typing_arguments_one_at_a_time_clears_the_error() {
    // `result : Int` is first bound to the bare function `add` (a type mismatch),
    // then arguments are typed in until the call has the right type.
    let src = "module Main\n\npublic add : Int -> Int -> Int\nlet add x y = x + y\n\npublic result : Int\nlet result = add\n";
    let mut harness = Harness::start("typing", &[("Main.fai", src)]);
    let uri = harness.did_open("Main.fai", src);
    assert!(!harness.diagnostics(&uri).is_empty(), "`result = add` is a type mismatch");

    // Type ` 1` after `add` (line 6, col 16).
    harness.did_change_range(
        &uri,
        json!({ "start": { "line": 6, "character": 16 }, "end": { "line": 6, "character": 16 } }),
        " 1",
    );
    assert!(!harness.diagnostics(&uri).is_empty(), "`add 1` is still `Int -> Int`");

    // Type ` 2` after `add 1` (now col 18).
    harness.did_change_range(
        &uri,
        json!({ "start": { "line": 6, "character": 18 }, "end": { "line": 6, "character": 18 } }),
        " 2",
    );
    assert!(harness.diagnostics(&uri).is_empty(), "`add 1 2 : Int` typechecks against `result`");
    harness.shutdown();
}

#[test]
fn changing_a_dependency_then_fixing_the_caller() {
    // Edit a module's public type, watch the dependent break, then fix the caller.
    let a = "module A\n\npublic n : Int\nlet n = 0\n";
    let b = "module B\n\npublic m : Int\nlet m = A.n + 1\n";
    let mut harness = Harness::start("api-change", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let uri_b = harness.did_open("B.fai", b);
    assert!(harness.diagnostics(&uri_b).is_empty(), "B is valid to begin with");

    // A.n becomes a `Bool`; B's `A.n + 1` no longer typechecks.
    harness.did_change(&uri_a, "module A\n\npublic n : Bool\nlet n = true\n");
    assert!(!harness.diagnostics(&uri_b).is_empty(), "B breaks against A's new type");

    // Fix the caller B to use `A.n` at its new (Bool) type.
    harness.did_change(&uri_b, "module B\n\npublic m : Int\nlet m = if A.n then 1 else 0\n");
    assert!(
        harness.diagnostics(&uri_b).is_empty(),
        "B is valid again: {:?}",
        harness.diagnostics(&uri_b)
    );
    harness.shutdown();
}

#[test]
fn cross_file_hover_tracks_a_dependency_edit() {
    // Hovering `A.inc` in B reports A's declared type; editing A's signature is
    // reflected on the next hover, even though B was not touched.
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n";
    let mut harness = Harness::start("xfile-hover", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let uri_b = harness.did_open("B.fai", b);
    // Hover the `inc` of `A.inc` in B (line 3, col 12).
    let position =
        json!({ "textDocument": { "uri": uri_b }, "position": { "line": 3, "character": 12 } });
    let before = harness.request("textDocument/hover", position.clone());
    assert!(
        before["contents"]["value"].as_str().unwrap().contains("inc : Int -> Int"),
        "before: {before:?}"
    );
    // Retype A's `inc` to `Int -> Bool`; B is untouched.
    harness.did_change(&uri_a, "module A\n\npublic inc : Int -> Bool\nlet inc x = x > 0\n");
    let after = harness.request("textDocument/hover", position);
    assert!(
        after["contents"]["value"].as_str().unwrap().contains("inc : Int -> Bool"),
        "hover reflects the dependency edit: {after:?}"
    );
    harness.shutdown();
}

#[test]
fn navigate_to_a_definition_then_find_its_references() {
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n";
    let mut harness = Harness::start("navigate", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = harness.did_open("A.fai", a);
    let uri_b = harness.did_open("B.fai", b);

    // Jump from the use `A.inc` in B (line 3, col 12) to its definition in A.
    let def = harness.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri_b }, "position": { "line": 3, "character": 12 } }),
    );
    let locations = def.as_array().expect("definition locations");
    let target = &locations[0];
    assert!(target["uri"].as_str().unwrap().ends_with("A.fai"), "lands in A: {target:?}");
    let def_line = target["range"]["start"]["line"].clone();
    assert_eq!(def_line, 3, "the `inc` binding line");

    // From the definition site (in the document already open as `uri_a`), find its
    // uses with declarations excluded — the single call in B.
    let refs = harness.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri_a },
            "position": { "line": def_line, "character": 4 },
            "context": { "includeDeclaration": false }
        }),
    );
    let refs = refs.as_array().expect("references");
    assert_eq!(refs.len(), 1, "the single use in B: {refs:?}");
    assert!(refs[0]["uri"].as_str().unwrap().ends_with("B.fai"), "{refs:?}");
    harness.shutdown();
}

// --- dirty (unsaved) buffers -------------------------------------------------
//
// These exercise the case the earlier tests do not: the open buffer differs from
// the file on disk. The server overlays the buffer into the warm database, so
// analysis must track the unsaved edits, and a close must hand ownership back to
// the filesystem.

#[test]
fn did_change_analyzes_unsaved_edits_not_disk() {
    // Disk stays clean for the whole test; only the buffer changes. A reported
    // error can therefore only come from the overlaid (unsaved) text.
    let mut harness = Harness::start("dirty-change", &[("Main.fai", MAIN)]);
    let uri = harness.did_open("Main.fai", MAIN);
    assert!(harness.diagnostics(&uri).is_empty(), "clean buffer, no diagnostics");

    // Edit the buffer to a type error (the signature no longer matches the body).
    let broken = "module Main\n\npublic inc : Int -> Bool\nlet inc x = x + 1\n\npublic two : Int\nlet two = inc 1\n";
    harness.did_change(&uri, broken);
    let diagnostics = harness.diagnostics(&uri);
    assert!(!diagnostics.is_empty(), "the unsaved edit is type-checked: {diagnostics:?}");
    assert!(
        diagnostics.iter().all(|d| d["code"].as_str().unwrap().starts_with("FAI")),
        "{diagnostics:?}"
    );

    // Edit back to a clean buffer: the diagnostics clear again.
    harness.did_change(&uri, MAIN);
    assert!(harness.diagnostics(&uri).is_empty(), "fixing the buffer clears diagnostics");
    harness.shutdown();
}

#[test]
fn hover_and_definition_track_the_unsaved_buffer() {
    // On disk, `Main` has only `two`; the buffer adds `inc` and a reference to it.
    let disk = indoc! {r#"
        module Main

        public two : Int
        let two = 0
    "#};
    let mut harness = Harness::start("dirty-hover", &[("Main.fai", disk)]);
    let uri = harness.did_open("Main.fai", disk);
    let buffer = MAIN; // adds `inc` (line 3) and `let two = inc 1` (line 6)
    harness.did_change(&uri, buffer);

    // `inc` and line 6 exist only in the buffer; answers here prove the overlay
    // (the offset would be out of range against the 4-line disk file).
    let position =
        json!({ "textDocument": { "uri": uri }, "position": { "line": 6, "character": 10 } });
    let hover = harness.request("textDocument/hover", position.clone());
    assert!(
        hover["contents"]["value"].as_str().unwrap().contains("inc : Int -> Int"),
        "hover: {hover:?}"
    );
    let definition = harness.request("textDocument/definition", position);
    let locations = definition.as_array().expect("locations");
    assert_eq!(locations[0]["range"]["start"]["line"], 3, "jumps to the buffer-only binding");
    harness.shutdown();
}

#[test]
fn did_close_reverts_the_overlay_to_disk() {
    // `A` defines `n : Int`; `B` uses it as an `Int` (valid against disk).
    let disk_a = "module A\n\npublic n : Int\nlet n = 0\n";
    let disk_b = "module B\n\npublic m : Int\nlet m = A.n + 1\n";
    let mut harness = Harness::start("dirty-close", &[("A.fai", disk_a), ("B.fai", disk_b)]);

    // Open `A` with an unsaved edit that retypes `n` to `Bool` (valid in `A`),
    // which breaks `B`'s `A.n + 1`.
    let dirty_a = "module A\n\npublic n : Bool\nlet n = true\n";
    let uri_a = harness.did_open("A.fai", dirty_a);
    let uri_b = harness.did_open("B.fai", disk_b);
    assert_eq!(harness.diagnostics(&uri_b).len(), 1, "B sees A's unsaved edit across modules");

    // Close `A` without saving: the overlay must revert to the on-disk `n : Int`.
    harness.did_close(&uri_a);
    // Re-check `B` (an identical-content change re-runs analysis): now clean.
    harness.did_change(&uri_b, disk_b);
    assert!(
        harness.diagnostics(&uri_b).is_empty(),
        "closing A restored its on-disk type, so B is valid again"
    );
    harness.shutdown();
}

#[test]
fn formatting_uses_the_unsaved_buffer() {
    // On disk the file is already canonical and has no `inc`.
    let disk = indoc! {r#"
        module Main

        public two : Int
        let two = 0
    "#};
    let mut harness = Harness::start("dirty-fmt", &[("Main.fai", disk)]);
    let uri = harness.did_open("Main.fai", disk);
    // The unsaved buffer adds a (badly spaced) `inc` that is not on disk.
    let messy =
        "module Main\n\npublic two : Int\nlet two=0\n\npublic inc : Int -> Int\nlet inc x=x+1\n";
    harness.did_change(&uri, messy);
    let result = harness.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let new_text = result.as_array().expect("edits")[0]["newText"].as_str().unwrap();
    // The buffer-only `inc` is formatted, proving the unsaved text was used.
    assert!(new_text.contains("let inc x = x + 1"), "{new_text:?}");
    assert!(new_text.contains("let two = 0") && !new_text.contains("two=0"), "{new_text:?}");
    harness.shutdown();
}
