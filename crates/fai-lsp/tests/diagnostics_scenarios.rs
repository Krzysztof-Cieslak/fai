//! Real-world diagnostics scenarios: which errors a file reports, and how they
//! appear and clear as the buffer is edited.

mod harness;

use harness::Harness;
use indoc::indoc;

/// The diagnostic codes reported for `src` (opened as `Main.fai`).
fn codes(tag: &str, src: &str) -> Vec<String> {
    let (mut h, uri) = Harness::open_main(tag, src);
    let codes = h.diagnostic_codes(&uri);
    h.shutdown();
    codes
}

fn has(codes: &[String], code: &str) -> bool {
    codes.iter().any(|c| c == code)
}

#[test]
fn a_well_typed_file_has_no_diagnostics() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    assert!(codes("d-clean", src).is_empty());
}

#[test]
fn an_empty_module_has_no_diagnostics() {
    assert!(codes("d-empty", "module M\n").is_empty());
}

#[test]
fn a_comment_only_body_has_no_diagnostics() {
    assert!(codes("d-comment", "module M\n\n// just a note\n").is_empty());
}

#[test]
fn a_type_mismatch_is_reported() {
    let src = "module M\n\npublic bad : Int -> Bool\nlet bad x = x + 1\n";
    assert!(has(&codes("d-mismatch", src), "FAI3004"), "expected a signature mismatch");
}

#[test]
fn an_unbound_name_is_reported() {
    let src = "module M\n\npublic f : Int -> Int\nlet f x = mystery x\n";
    assert!(has(&codes("d-unbound", src), "FAI2001"));
}

#[test]
fn an_unbound_constructor_is_reported() {
    let src = "module M\n\npublic f : Int\nlet f = Mystery\n";
    assert!(has(&codes("d-unbound-ctor", src), "FAI2012"));
}

#[test]
fn a_missing_public_signature_is_reported() {
    let src = "module M\n\npublic let f x = x + 1\n";
    assert!(has(&codes("d-missing-sig", src), "FAI3003"));
}

#[test]
fn a_duplicate_definition_is_reported() {
    let src = "module M\n\nlet a = 1\nlet a = 2\n";
    assert!(has(&codes("d-dup", src), "FAI2004"));
}

#[test]
fn a_non_exhaustive_match_is_reported() {
    let src = indoc! {r#"
        module M

        public type Color =
          | Red
          | Green

        public f : Color -> Int
        let f c =
          match c with
          | Red -> 0
    "#};
    assert!(has(&codes("d-exhaust", src), "FAI4001"));
}

#[test]
fn an_unreachable_match_arm_is_reported() {
    let src = indoc! {r#"
        module M

        public type Color =
          | Red
          | Green

        public f : Color -> Int
        let f c =
          match c with
          | other -> 0
          | Red -> 1
          | Green -> 2
    "#};
    assert!(has(&codes("d-unreachable", src), "FAI4002"));
}

#[test]
fn a_private_type_in_a_public_signature_is_reported() {
    let src = "module M\n\ntype Color =\n  | Red\n\npublic c : Color\nlet c = Red\n";
    assert!(has(&codes("d-private-ty", src), "FAI2015"));
}

#[test]
fn a_syntax_error_is_reported() {
    let src = "module M\n\npublic f : Int\nlet f = (\n";
    let codes = codes("d-syntax", src);
    assert!(codes.iter().any(|c| c.starts_with("FAI1")), "{codes:?}");
}

#[test]
fn shadowing_a_prelude_name_warns() {
    let src = "module M\n\nlet identity = 1\n";
    assert!(has(&codes("d-shadow", src), "FAI2010"));
}

#[test]
fn several_errors_are_reported_together() {
    // A missing signature and an unbound name in one binding.
    let src = "module M\n\npublic let f x = mystery x\n";
    let codes = codes("d-multi", src);
    assert!(has(&codes, "FAI3003") && has(&codes, "FAI2001"), "{codes:?}");
}

#[test]
fn a_diagnostic_points_inside_the_file() {
    let src = "module M\n\npublic bad : Int -> Bool\nlet bad x = x + 1\n";
    let (mut h, uri) = Harness::open_main("d-range", src);
    let diags = h.diagnostics(&uri);
    let d = &diags[0];
    assert!(d["range"]["start"]["line"].as_i64().unwrap() >= 2, "points at the body: {d}");
    assert_eq!(d["source"], "fai");
    h.shutdown();
}

#[test]
fn fixing_a_type_error_clears_the_diagnostic() {
    let broken = "module M\n\npublic inc : Int -> Bool\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("d-fix", broken);
    assert!(!h.diagnostics(&uri).is_empty(), "starts broken");
    let fixed = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    h.did_change(&uri, fixed);
    assert!(h.diagnostics(&uri).is_empty(), "the fix clears it");
    h.shutdown();
}

#[test]
fn fixing_one_error_leaves_the_other() {
    // Two independent type errors; fix one, the other remains.
    let two = indoc! {r#"
        module M

        public a : Int -> Bool
        let a x = x + 1

        public b : Int -> Bool
        let b y = y + 2
    "#};
    let (mut h, uri) = Harness::open_main("d-partial", two);
    assert_eq!(h.diagnostics(&uri).len(), 2, "two mismatches");
    let one_fixed = indoc! {r#"
        module M

        public a : Int -> Int
        let a x = x + 1

        public b : Int -> Bool
        let b y = y + 2
    "#};
    h.did_change(&uri, one_fixed);
    assert_eq!(h.diagnostics(&uri).len(), 1, "one mismatch remains");
    h.shutdown();
}

#[test]
fn breaking_then_repairing_a_buffer_cycles_diagnostics() {
    let clean = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("d-cycle", clean);
    assert!(h.diagnostics(&uri).is_empty());
    let broken = "module M\n\npublic inc : Int -> Int\nlet inc x = x + true\n";
    h.did_change(&uri, broken);
    assert!(!h.diagnostics(&uri).is_empty(), "broken now");
    h.did_change(&uri, clean);
    assert!(h.diagnostics(&uri).is_empty(), "repaired again");
    h.shutdown();
}
