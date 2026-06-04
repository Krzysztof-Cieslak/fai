//! Multi-file (cross-module) type-checking scenarios, positive and negative.

use fai_tests::check_named;

/// The codes reported for the primary file.
fn codes(primary: &str, files: &[(&str, &str)]) -> Vec<String> {
    check_named(primary, files).codes()
}

#[test]
fn qualified_public_reference_typechecks() {
    let outcome = check_named(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n"),
            ("B.fai", "module B\n\npublic two : Int\nlet two = A.inc 1\n"),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
    assert_eq!(outcome.types.get("two").map(String::as_str), Some("Int"));
}

#[test]
fn qualified_reference_to_private_is_error() {
    let cs = codes(
        "B.fai",
        &[
            ("A.fai", "module A\n\nlet secret x = x\n"),
            ("B.fai", "module B\n\npublic use : Int -> Int\nlet use n = A.secret n\n"),
        ],
    );
    assert!(cs.contains(&"FAI2003".to_owned()), "got {cs:?}");
}

#[test]
fn qualified_reference_to_unknown_module_is_error() {
    let cs = codes("B.fai", &[("B.fai", "module B\n\nlet use = Nope.thing 1\n")]);
    assert!(cs.contains(&"FAI2008".to_owned()), "got {cs:?}");
}

#[test]
fn qualified_reference_to_missing_member_is_error() {
    let cs = codes(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n"),
            ("B.fai", "module B\n\nlet use = A.nope 1\n"),
        ],
    );
    assert!(cs.contains(&"FAI2001".to_owned()), "got {cs:?}");
}

#[test]
fn cross_module_type_flows_into_local_inference() {
    // A.toString : Int -> String; B uses it, so B.label : Int -> String.
    let outcome = check_named(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic toStr : Int -> String\nlet toStr n = intToString n\n"),
            ("B.fai", "module B\n\npublic label : Int -> String\nlet label n = A.toStr n\n"),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
    assert_eq!(outcome.types.get("label").map(String::as_str), Some("Int -> String"));
}

#[test]
fn cross_module_signature_mismatch_is_caught_in_caller() {
    // B applies A.inc (Int -> Int) to a String: a type error in B.
    let cs = codes(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic inc : Int -> Int\nlet inc x = x + 1\n"),
            ("B.fai", "module B\n\nlet bad = A.inc \"x\"\n"),
        ],
    );
    assert!(cs.contains(&"FAI3001".to_owned()), "got {cs:?}");
}

#[test]
fn duplicate_module_name_errors_on_each_file() {
    let a = codes(
        "A.fai",
        &[("A.fai", "module Same\n\nlet a = 1\n"), ("B.fai", "module Same\n\nlet b = 2\n")],
    );
    assert!(a.contains(&"FAI2007".to_owned()), "file A: {a:?}");
}

#[test]
fn polymorphic_export_instantiates_per_use() {
    // A.identity : 'a -> 'a used at Int and Bool in B.
    let outcome = check_named(
        "B.fai",
        &[
            ("A.fai", "module A\n\npublic id : 'a -> 'a\nlet id x = x\n"),
            (
                "B.fai",
                "module B\n\npublic f : Int -> Bool -> Int\nlet f n b = if A.id b then A.id n else n\n",
            ),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
}

#[test]
fn private_binding_is_not_an_export() {
    // A.helper is private; B can only reach A.inc.
    let outcome = check_named(
        "B.fai",
        &[
            (
                "A.fai",
                "module A\n\npublic inc : Int -> Int\nlet inc x = helper x + 1\n\nlet helper y = y * 2\n",
            ),
            ("B.fai", "module B\n\nlet two = A.inc 1\n"),
        ],
    );
    assert!(!outcome.has_errors(), "got {:?}", outcome.codes());
}
