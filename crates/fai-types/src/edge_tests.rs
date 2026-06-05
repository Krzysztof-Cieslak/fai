//! Edge-case and negative unit tests for inference, organized by rule.
//!
//! These complement the broad cases in `tests.rs` and the `.fai` fixture corpus
//! in `fai-tests`. They poke specific corners: nesting, shadowing, operator
//! interactions, defaulting, generalization, patterns, and each diagnostic code.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};

use crate::{check_file, def_type, render_scheme};

fn db1(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), format!("module M\n\n{src}\n"));
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn ty(src: &str, name: &str) -> String {
    let (db, file) = db1(src);
    render_scheme(&def_type(&db, file, fai_syntax::Symbol::intern(name)))
}

fn codes(src: &str) -> Vec<String> {
    let (db, file) = db1(src);
    let source = file.source(&db);
    let mut out = Vec::new();
    for d in fai_resolve::resolve::accumulated::<Diag>(&db, file) {
        if d.0.primary.source() == source {
            out.push(d.0.code.as_str().to_owned());
        }
    }
    for d in check_file::accumulated::<Diag>(&db, file) {
        if d.0.primary.source() == source {
            out.push(d.0.code.as_str().to_owned());
        }
    }
    out
}

fn clean(src: &str) {
    let cs = codes(src);
    assert!(
        !cs.iter().any(|c| c.starts_with("FAI3") || c == "FAI2001" || c == "FAI2003"),
        "expected clean, got {cs:?}"
    );
}

// ---- literals --------------------------------------------------------------

#[test]
fn literal_int() {
    assert_eq!(ty("let x = 42", "x"), "Int");
}

#[test]
fn literal_float() {
    assert_eq!(ty("let x = 3.14", "x"), "Float");
}

#[test]
fn literal_string() {
    assert_eq!(ty("let x = \"hi\"", "x"), "String");
}

#[test]
fn literal_char() {
    assert_eq!(ty("let x = 'a'", "x"), "Char");
}

#[test]
fn literal_unit() {
    assert_eq!(ty("let x = ()", "x"), "()");
}

#[test]
fn literal_bool() {
    assert_eq!(ty("let x = true", "x"), "Bool");
    assert_eq!(ty("let y = false", "y"), "Bool");
}

// ---- arithmetic & numeric defaulting ---------------------------------------

#[test]
fn nested_int_arithmetic_defaults_int() {
    assert_eq!(ty("let x = (1 + 2) * 3 - 4 / 2", "x"), "Int");
}

#[test]
fn float_chain_stays_float() {
    assert_eq!(ty("let x = 1.0 + 2.0 * 3.0", "x"), "Float");
}

#[test]
fn remainder_is_numeric() {
    assert_eq!(ty("let x = 7 % 3", "x"), "Int");
}

#[test]
fn negation_is_numeric() {
    assert_eq!(ty("let x = 0 - 5", "x"), "Int");
}

#[test]
fn mixing_known_int_and_float_is_an_error() {
    // A value pinned to Int by a signature cannot be added to a Float: no
    // implicit coercion. (A bare integer literal is numeric-polymorphic, so
    // `1 + 2.0` legitimately unifies at Float — that is not a coercion.)
    assert!(codes("public n : Int\nlet n = 1\nlet x = n + 2.0").contains(&"FAI3001".to_owned()));
}

#[test]
fn arithmetic_on_bool_is_an_error() {
    assert!(codes("let x = true + 1").contains(&"FAI3001".to_owned()));
}

#[test]
fn arithmetic_on_string_is_an_error() {
    assert!(codes("let x = \"a\" - 1").contains(&"FAI3001".to_owned()));
}

// ---- comparison & equality -------------------------------------------------

#[test]
fn comparison_yields_bool() {
    assert_eq!(ty("let x = 1 < 2", "x"), "Bool");
}

#[test]
fn comparison_on_string_ok() {
    // Ord admits String.
    clean("public f : String -> String -> Bool\nlet f a b = a < b");
}

#[test]
fn equality_yields_bool() {
    assert_eq!(ty("let x = 1 = 1", "x"), "Bool");
}

#[test]
fn equality_generalizes() {
    assert_eq!(ty("let eq a b = a = b", "eq"), "'a -> 'a -> Bool");
}

#[test]
fn equality_on_function_errors() {
    assert!(
        codes("public f : Int -> Int\nlet f x = x\nlet bad = f = f")
            .contains(&"FAI3006".to_owned())
    );
}

// ---- boolean logic ---------------------------------------------------------

#[test]
fn boolean_and_or() {
    assert_eq!(ty("let x = true && false || true", "x"), "Bool");
}

#[test]
fn boolean_operand_must_be_bool() {
    assert!(codes("let x = 1 && true").contains(&"FAI3001".to_owned()));
}

// ---- if --------------------------------------------------------------------

#[test]
fn if_branches_must_match() {
    assert!(codes("let x = if true then 1 else \"no\"").contains(&"FAI3001".to_owned()));
}

#[test]
fn if_condition_must_be_bool() {
    assert!(codes("let x = if 1 then 1 else 2").contains(&"FAI3001".to_owned()));
}

#[test]
fn nested_if_chain() {
    assert_eq!(
        ty("let classify n = if n < 0 then 0 - 1 else if n = 0 then 0 else 1", "classify"),
        "Int -> Int"
    );
}

// ---- functions, application, currying --------------------------------------

#[test]
fn curried_application() {
    assert_eq!(ty("let add x y = x + y\nlet z = add 1 2", "z"), "Int");
}

#[test]
fn partial_application() {
    assert_eq!(ty("let add x y = x + y\nlet inc = add 1", "inc"), "Int -> Int");
}

#[test]
fn higher_order_argument() {
    assert_eq!(ty("let app f x = f x", "app"), "('a -> 'b) -> 'a -> 'b");
}

#[test]
fn applying_a_non_function_errors() {
    assert!(codes("let x = 1 2").contains(&"FAI3001".to_owned()));
}

#[test]
fn lambda_inference() {
    assert_eq!(ty("let id = fun x -> x", "id"), "'a -> 'a");
}

#[test]
fn lambda_multi_param() {
    assert_eq!(ty("let f = fun x y -> x + y", "f"), "Int -> Int -> Int");
}

// ---- let blocks & locals ---------------------------------------------------

#[test]
fn local_let_chain() {
    assert_eq!(ty("let f a =\n  let b = a + 1\n  let c = b + 1\n  c", "f"), "Int -> Int");
}

#[test]
fn local_shadowing_inner_wins() {
    // Inner `x` shadows the parameter; result type follows the inner binding.
    assert_eq!(ty("let f x =\n  let x = \"s\"\n  x", "f"), "'a -> String");
}

#[test]
fn local_let_polymorphism() {
    // A locally-bound identity used at two types.
    clean(
        "public f : Int -> Bool -> Int\nlet f n b =\n  let id = fun x -> x\n  let a = id n\n  let c = id b\n  if c then a else a",
    );
}

// ---- tuples & patterns -----------------------------------------------------

#[test]
fn tuple_construction() {
    assert_eq!(ty("let p = (1, true, \"x\")", "p"), "Int * Bool * String");
}

#[test]
fn nested_tuple() {
    assert_eq!(ty("let p = (1, (true, \"x\"))", "p"), "Int * (Bool * String)");
}

#[test]
fn tuple_pattern_in_param() {
    assert_eq!(
        ty("public fst : 'a * 'b -> 'a\nlet fst p =\n  let (a, b) = p\n  a", "fst"),
        "'a * 'b -> 'a"
    );
}

#[test]
fn wildcard_pattern() {
    assert_eq!(ty("let f _ = 1", "f"), "'a -> Int");
}

#[test]
fn unit_pattern() {
    assert_eq!(ty("public f : () -> Int\nlet f () = 1", "f"), "() -> Int");
}

// ---- lists -----------------------------------------------------------------

#[test]
fn empty_list_polymorphic() {
    assert_eq!(ty("let xs = []", "xs"), "List 'a");
}

#[test]
fn list_of_ints() {
    assert_eq!(ty("let xs = [1, 2, 3]", "xs"), "List Int");
}

#[test]
fn heterogeneous_list_errors() {
    assert!(codes("let xs = [1, true]").contains(&"FAI3001".to_owned()));
}

#[test]
fn cons_builds_list() {
    assert_eq!(ty("let xs = 1 :: [2, 3]", "xs"), "List Int");
}

#[test]
fn cons_unifies_element_and_list() {
    assert!(codes("let xs = 1 :: [true]").contains(&"FAI3001".to_owned()));
}

// ---- concat (String only) --------------------------------------------------

#[test]
fn concat_is_string() {
    assert_eq!(ty("let s = \"a\" ++ \"b\"", "s"), "String");
}

#[test]
fn concat_on_int_errors() {
    assert!(codes("let s = 1 ++ 2").contains(&"FAI3001".to_owned()));
}

// ---- pipe & compose --------------------------------------------------------

#[test]
fn pipe_threads_value() {
    assert_eq!(ty("let inc x = x + 1\nlet y = 1 |> inc", "y"), "Int");
}

#[test]
fn compose_builds_function() {
    assert_eq!(ty("let inc x = x + 1\nlet f = inc >> inc", "f"), "Int -> Int");
}

// ---- required signatures & mismatches --------------------------------------

#[test]
fn private_no_signature_is_fine() {
    clean("let helper x = x + 1");
}

#[test]
fn public_without_signature_errors() {
    assert!(codes("public let f x = x").contains(&"FAI3003".to_owned()));
}

#[test]
fn signature_too_general_is_mismatch() {
    // Declared polymorphic but body forces Int.
    assert!(codes("public f : 'a -> 'a\nlet f x = x + 1").contains(&"FAI3004".to_owned()));
}

#[test]
fn signature_correct_is_clean() {
    clean("public f : Int -> Int\nlet f x = x + 1");
}

#[test]
fn signature_polymorphic_matches() {
    clean("public id : 'a -> 'a\nlet id x = x");
}

// ---- unknown type & field access -------------------------------------------

#[test]
fn unknown_type_constructor_errors() {
    assert!(codes("public f : Foo -> Int\nlet f x = 1").contains(&"FAI3008".to_owned()));
}

#[test]
fn record_field_access_infers_open_row() {
    assert_eq!(ty("let f r = r.field", "f"), "{ field : 'a | _ } -> 'a");
    assert!(codes("let f r = r.field").is_empty());
}

// ---- contracts -------------------------------------------------------------

#[test]
fn example_must_be_bool() {
    assert!(codes("public n : Int\nlet n = 1\nexample: n + 1").contains(&"FAI3007".to_owned()));
}

#[test]
fn forall_binders_are_typed_from_use() {
    clean("public f : Int -> Int\nlet f x = x\nforall n: f n = f n");
}

#[test]
fn duplicate_forall_binder_errors() {
    assert!(
        codes("public f : Int -> Int\nlet f x = x\nforall n n: f n = f n")
            .contains(&"FAI2011".to_owned())
    );
}

// ---- occurs ----------------------------------------------------------------

#[test]
fn self_application_occurs_check() {
    assert!(codes("let f x = x x").contains(&"FAI3002".to_owned()));
}

// ---- determinism -----------------------------------------------------------

#[test]
fn inference_is_deterministic() {
    let src = "let f g x = g (g x)\nlet h a b = if a then b else b";
    let first = ty(src, "f");
    for _ in 0..5 {
        assert_eq!(ty(src, "f"), first);
    }
}
