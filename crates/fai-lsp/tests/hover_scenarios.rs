//! Real-world hover scenarios: what an editor shows when the cursor rests on
//! different kinds of names, literals, and expressions.

mod harness;

use harness::{Harness, position_of, position_within};
use indoc::indoc;

const SAMPLE: &str = indoc! {r#"
    module M

    public inc : Int -> Int
    let inc x = x + 1

    public two : Int
    let two = inc 1

    public type Color =
      | Red
      | Green

    public favorite : Color
    let favorite = Red

    public area : { width : Int, height : Int } -> Int
    let area rect = rect.width

    public total : List Int -> Int
    let total items = List.length items

    public step : Int -> Int
    let step n =
      let bumped = n + 1
      bumped

    public pair : Int -> Int * Int
    let pair n = (n, n)

    public ratio : Float
    let ratio = 1.5

    public flag : Bool
    let flag = true
"#};

fn sample() -> (Harness, String) {
    Harness::open_main("hover", SAMPLE)
}

#[test]
fn hover_on_a_function_reference_shows_its_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "inc 1")).expect("hover");
    assert!(text.contains("inc : Int -> Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_parameter_shows_its_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "x + 1")).expect("hover");
    assert!(text.contains("Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_local_binding_use_shows_its_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "bumped\n")).expect("hover");
    assert!(text.contains("bumped : Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_constructor_use_shows_the_adt() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_within(SAMPLE, "= Red", 2)).expect("hover");
    assert!(text.contains("Red : Color"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_record_value_shows_the_record_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "rect.width")).expect("hover");
    assert!(text.contains("width") && text.contains("height"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_field_access_shows_the_field_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_within(SAMPLE, "rect.width", 5)).expect("hover");
    assert!(text.contains("Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_qualified_member_shows_its_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_within(SAMPLE, "List.length", 5)).expect("hover");
    assert!(text.contains("length") && text.contains("Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_float_literal_shows_float() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "1.5")).expect("hover");
    assert!(text.contains("Float"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_bool_literal_shows_bool() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "true")).expect("hover");
    assert!(text.contains("Bool"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_an_int_literal_shows_int() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_within(SAMPLE, "inc 1", 4)).expect("hover");
    assert!(text.contains("Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_on_a_tuple_shows_the_product_type() {
    let (mut h, uri) = sample();
    let text = h.hover_text(&uri, position_of(SAMPLE, "(n, n)")).expect("hover");
    assert!(text.contains("Int * Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_off_a_keyword_is_empty() {
    let (mut h, uri) = sample();
    assert!(h.hover(&uri, position_of(SAMPLE, "module M")).is_null());
    h.shutdown();
}

#[test]
fn hover_on_a_list_literal_shows_the_list_type() {
    let src = "module M\n\npublic xs : List Int\nlet xs = [1, 2, 3]\n";
    let (mut h, uri) = Harness::open_main("hover-list", src);
    let text = h.hover_text(&uri, position_of(src, "[1, 2, 3]")).expect("hover");
    assert!(text.contains("List") && text.contains("Int"), "{text}");
    h.shutdown();
}

#[test]
fn hover_includes_doc_prose() {
    let src = indoc! {r#"
        module M

        /// Doubles its argument.
        public double : Int -> Int
        let double x = x + x

        public caller : Int
        let caller = double 3
    "#};
    let (mut h, uri) = Harness::open_main("hover-doc", src);
    let text = h.hover_text(&uri, position_of(src, "double 3")).expect("hover");
    assert!(text.contains("Doubles its argument."), "{text}");
    h.shutdown();
}

#[test]
fn hover_includes_attached_contracts() {
    let src = indoc! {r#"
        module M

        public double : Int -> Int
        let double x = x + x
        example: double 2 = 4

        public caller : Int
        let caller = double 3
    "#};
    let (mut h, uri) = Harness::open_main("hover-contract", src);
    let text = h.hover_text(&uri, position_of(src, "double 3")).expect("hover");
    assert!(text.contains("example: double 2 = 4"), "{text}");
    h.shutdown();
}

#[test]
fn hover_reflects_a_signature_edit() {
    let before = "module M\n\npublic f : Int -> Int\nlet f x = x\n\npublic g : Int\nlet g = f 0\n";
    let (mut h, uri) = Harness::open_main("hover-edit", before);
    let first = h.hover_text(&uri, position_of(before, "f 0")).expect("hover");
    assert!(first.contains("f : Int -> Int"), "{first}");
    // Retype `f` and hover again.
    let after =
        "module M\n\npublic f : Int -> Bool\nlet f x = x > 0\n\npublic g : Int\nlet g = f 0\n";
    h.did_change(&uri, after);
    let second = h.hover_text(&uri, position_of(after, "f 0")).expect("hover");
    assert!(second.contains("f : Int -> Bool"), "{second}");
    h.shutdown();
}

#[test]
fn hover_on_a_qualified_call_argument_shows_its_type() {
    let (mut h, uri) = sample();
    // `items` in `List.length items` is the function's parameter, a `List Int`.
    let text = h.hover_text(&uri, position_of(SAMPLE, "items\n")).expect("hover");
    assert!(text.contains("List") && text.contains("Int"), "{text}");
    h.shutdown();
}
