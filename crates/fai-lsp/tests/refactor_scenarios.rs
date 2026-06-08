//! Real-world refactoring scenarios: rename and code actions, mostly as
//! round-trips — request the edit, apply it back into the buffer, and re-check.

mod harness;

use harness::{
    Harness, action_titles, apply_text_edits, changes_for, position_after, position_of,
    position_within, range,
};
use indoc::indoc;

// --- rename ------------------------------------------------------------------

#[test]
fn rename_a_local_round_trip() {
    let src = "module M\n\npublic f : Int -> Int\nlet f n =\n  let temp = n + 1\n  temp\n";
    let (mut h, uri) = Harness::open_main("rn-local", src);
    let edit = h.rename(&uri, position_of(src, "temp\n"), "result");
    let fixed = apply_text_edits(src, &changes_for(&edit, "Main.fai"));
    assert!(fixed.contains("let result = n + 1") && fixed.contains("  result"), "{fixed}");
    h.did_change(&uri, &fixed);
    assert!(h.diagnostics(&uri).is_empty(), "rename keeps it valid");
    h.shutdown();
}

#[test]
fn rename_a_definition_round_trip() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n\npublic two : Int\nlet two = inc 1\n";
    let (mut h, uri) = Harness::open_main("rn-def", src);
    let edit = h.rename(&uri, position_of(src, "inc 1"), "bump");
    let fixed = apply_text_edits(src, &changes_for(&edit, "Main.fai"));
    assert!(fixed.contains("public bump : Int -> Int"), "{fixed}");
    assert!(fixed.contains("let bump x = x + 1") && fixed.contains("bump 1"), "{fixed}");
    h.did_change(&uri, &fixed);
    assert!(h.diagnostics(&uri).is_empty(), "rename keeps it valid");
    h.shutdown();
}

#[test]
fn rename_a_constructor_round_trip() {
    let src = indoc! {r#"
        module M

        public type Color =
          | Red
          | Green

        public favorite : Color
        let favorite = Red

        public describe : Color -> Int
        let describe c =
          match c with
          | Red -> 0
          | Green -> 1
    "#};
    let (mut h, uri) = Harness::open_main("rn-ctor", src);
    // The cursor is on `Red` in `= Red`.
    let edit = h.rename(&uri, position_within(src, "= Red", 2), "Crimson");
    let fixed = apply_text_edits(src, &changes_for(&edit, "Main.fai"));
    assert!(fixed.contains("| Crimson") && fixed.contains("= Crimson"), "{fixed}");
    assert!(fixed.contains("| Crimson -> 0"), "{fixed}");
    h.did_change(&uri, &fixed);
    assert!(h.diagnostics(&uri).is_empty(), "rename keeps it valid");
    h.shutdown();
}

#[test]
fn rename_a_parameter_round_trip() {
    let src = "module M\n\npublic sq : Int -> Int\nlet sq x = x * x\n";
    let (mut h, uri) = Harness::open_main("rn-param", src);
    let edit = h.rename(&uri, position_of(src, "x * x"), "n");
    let fixed = apply_text_edits(src, &changes_for(&edit, "Main.fai"));
    assert!(fixed.contains("let sq n = n * n"), "{fixed}");
    h.did_change(&uri, &fixed);
    assert!(h.diagnostics(&uri).is_empty());
    h.shutdown();
}

#[test]
fn rename_across_modules_round_trip() {
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n";
    let mut h = Harness::start("rn-cross", &[("A.fai", a), ("B.fai", b)]);
    let uri_a = h.did_open("A.fai", a);
    let uri_b = h.did_open("B.fai", b);
    let edit = h.rename(&uri_a, position_within(a, "inc x", 0), "bump");
    let new_a = apply_text_edits(a, &changes_for(&edit, "A.fai"));
    let new_b = apply_text_edits(b, &changes_for(&edit, "B.fai"));
    h.did_change(&uri_a, &new_a);
    h.did_change(&uri_b, &new_b);
    assert!(new_b.contains("A.bump 1"), "{new_b}");
    assert!(h.diagnostics(&uri_a).is_empty() && h.diagnostics(&uri_b).is_empty());
    h.shutdown();
}

#[test]
fn rename_a_qualified_use_edits_only_the_member() {
    let a = "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let b = "module B\n\npublic two : Int\nlet two = A.inc 1\n";
    let mut h = Harness::start("rn-qual", &[("A.fai", a), ("B.fai", b)]);
    let _uri_a = h.did_open("A.fai", a);
    let uri_b = h.did_open("B.fai", b);
    let edit = h.rename(&uri_b, position_within(b, "A.inc", 2), "bump");
    let edits_b = changes_for(&edit, "B.fai");
    // The edit replaces only `inc`, leaving the `A.` qualifier intact.
    assert!(edits_b.iter().all(|e| e["newText"] == "bump"), "{edits_b:?}");
    let new_b = apply_text_edits(b, &edits_b);
    assert!(new_b.contains("A.bump 1"), "{new_b}");
    h.shutdown();
}

#[test]
fn rename_rejects_a_cross_namespace_name() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("rn-ns", src);
    // A value cannot become an upper-case (constructor) name.
    assert!(h.rename(&uri, position_within(src, "inc x", 0), "Inc").is_null());
    h.shutdown();
}

#[test]
fn rename_rejects_a_malformed_name() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("rn-bad", src);
    assert!(h.rename(&uri, position_within(src, "inc x", 0), "in c").is_null());
    assert!(h.rename(&uri, position_within(src, "inc x", 0), "").is_null());
    h.shutdown();
}

#[test]
fn prepare_rename_reports_the_range_and_placeholder() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n\npublic two : Int\nlet two = inc 1\n";
    let (mut h, uri) = Harness::open_main("rn-prep", src);
    let result = h.prepare_rename(&uri, position_of(src, "inc 1"));
    assert_eq!(result["placeholder"], "inc", "{result}");
    assert!(result["range"].is_object(), "a precise range: {result}");
    h.shutdown();
}

#[test]
fn prepare_rename_rejects_a_standard_library_symbol() {
    let src = "module M\n\npublic f : List Int -> Int\nlet f xs = List.length xs\n";
    let (mut h, uri) = Harness::open_main("rn-std", src);
    assert!(h.prepare_rename(&uri, position_within(src, "List.length", 5)).is_null());
    h.shutdown();
}

#[test]
fn prepare_rename_rejects_a_builtin_operator() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("rn-op", src);
    assert!(h.prepare_rename(&uri, position_of(src, "+ 1")).is_null());
    h.shutdown();
}

// --- code actions ------------------------------------------------------------

#[test]
fn add_missing_signature_round_trip() {
    let src = "module Main\n\npublic let inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("ca-sig", src);
    assert!(h.diagnostic_codes(&uri).iter().any(|c| c == "FAI3003"));
    let actions = h.code_actions(&uri, range(position_of(src, "inc"), position_after(src, "inc")));
    let fix = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Add the inferred signature")
        .expect("a fix")
        .clone();
    let fixed = apply_text_edits(src, &changes_for(&fix["edit"], "Main.fai"));
    assert!(fixed.contains("public inc : Int -> Int"), "{fixed}");
    h.did_change(&uri, &fixed);
    assert!(h.diagnostics(&uri).is_empty(), "the fix resolves the diagnostic");
    h.shutdown();
}

#[test]
fn add_missing_signature_for_a_nested_binding_keeps_indentation() {
    let src = indoc! {r#"
        module M

        module Inner =
          public let answer = 42
    "#};
    let (mut h, uri) = Harness::open_main("ca-nested", src);
    let actions =
        h.code_actions(&uri, range(position_of(src, "answer"), position_after(src, "answer")));
    let fix = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Add the inferred signature")
        .expect("a fix")
        .clone();
    let fixed = apply_text_edits(src, &changes_for(&fix["edit"], "Main.fai"));
    assert!(fixed.contains("  public answer : Int\n  let answer = 42"), "{fixed}");
    h.shutdown();
}

#[test]
fn qualify_unbound_name_round_trip() {
    let src = "module Main\n\npublic ids : List Int -> List Int\nlet ids xs = map identity xs\n";
    let (mut h, uri) = Harness::open_main("ca-qual", src);
    assert!(h.diagnostic_codes(&uri).iter().any(|c| c == "FAI2001"));
    let actions = h.code_actions(&uri, range(position_of(src, "map"), position_after(src, "map")));
    let fix = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Qualify as `List.map`")
        .expect("a fix")
        .clone();
    let fixed = apply_text_edits(src, &changes_for(&fix["edit"], "Main.fai"));
    assert!(fixed.contains("List.map identity xs"), "{fixed}");
    h.did_change(&uri, &fixed);
    assert!(h.diagnostics(&uri).is_empty(), "qualifying resolves it");
    h.shutdown();
}

#[test]
fn qualify_offers_every_exporting_module() {
    // `map` is exported by several standard modules.
    let src = "module Main\n\npublic ids : List Int -> List Int\nlet ids xs = map identity xs\n";
    let (mut h, uri) = Harness::open_main("ca-multi", src);
    let actions = h.code_actions(&uri, range(position_of(src, "map"), position_after(src, "map")));
    let titles = action_titles(&actions);
    assert!(titles.iter().any(|t| t == "Qualify as `List.map`"), "{titles:?}");
    assert!(titles.iter().any(|t| t == "Qualify as `Option.map`"), "{titles:?}");
    assert!(titles.iter().any(|t| t == "Qualify as `Result.map`"), "{titles:?}");
    h.shutdown();
}

#[test]
fn no_code_actions_on_a_clean_range() {
    let src = "module Main\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("ca-clean", src);
    let actions =
        h.code_actions(&uri, range(position_of(src, "x + 1"), position_after(src, "x + 1")));
    assert!(actions.as_array().unwrap().is_empty(), "{actions}");
    h.shutdown();
}
