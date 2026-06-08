//! Advanced local-inference assertions over the real-world fixture programs.
//!
//! The inline `//~ LOCAL` annotations in the fixtures check individual locals;
//! these tests load the same files and assert *relationships* between locals
//! (shared variable numbering) and whole-function local maps — the cases a
//! single-line annotation cannot express.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fai_tests::{local_type, local_types};

fn fixture(name: &str) -> String {
    let path: PathBuf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typed").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

// ── Geometry: record components share the Vec2/Vec3 structure consistently ───

#[test]
fn geometry_add2_locals_are_all_float() {
    let src = fixture("Geometry.fai");
    let locals = local_types(&src, "add2");
    // Every destructured field is a Float; the params are Vec2 records.
    assert_eq!(
        locals,
        map(&[
            ("a", "{ x : Float, y : Float }"),
            ("ax", "Float"),
            ("ay", "Float"),
            ("b", "{ x : Float, y : Float }"),
            ("bx", "Float"),
            ("by", "Float"),
        ])
    );
}

#[test]
fn geometry_cross3_locals() {
    let src = fixture("Geometry.fai");
    let locals = local_types(&src, "cross3");
    // Both Vec3 params, their destructured components, and the cross-product
    // results are all Float.
    assert_eq!(
        locals,
        map(&[
            ("a", "{ x : Float, y : Float, z : Float }"),
            ("ax", "Float"),
            ("ay", "Float"),
            ("az", "Float"),
            ("b", "{ x : Float, y : Float, z : Float }"),
            ("bx", "Float"),
            ("by", "Float"),
            ("bz", "Float"),
            ("cx", "Float"),
            ("cy", "Float"),
            ("cz", "Float"),
        ])
    );
}

#[test]
fn geometry_step_threads_vec2_through_locals() {
    let src = fixture("Geometry.fai");
    let locals = local_types(&src, "step");
    // The physics step keeps every vector as a Vec2 record; `state` is the Body
    // and `dt` the Float timestep.
    assert_eq!(
        locals,
        map(&[
            ("dt", "Float"),
            ("gravity", "{ x : Float, y : Float }"),
            ("newPos", "{ x : Float, y : Float }"),
            ("newVel", "{ x : Float, y : Float }"),
            ("pos", "{ x : Float, y : Float }"),
            ("state", "{ pos : { x : Float, y : Float }, vel : { x : Float, y : Float } }"),
            ("vel", "{ x : Float, y : Float }"),
        ])
    );
}

// ── Rational: every intermediate is Int, params destructure consistently ─────

#[test]
fn rational_add_r_all_ints() {
    let src = fixture("Rational.fai");
    let locals = local_types(&src, "addR");
    assert_eq!(
        locals,
        map(&[
            ("a", "{ den : Int, num : Int }"),
            ("ad", "Int"),
            ("an", "Int"),
            ("b", "{ den : Int, num : Int }"),
            ("bd", "Int"),
            ("bn", "Int"),
            ("den", "Int"),
            ("num", "Int"),
        ])
    );
}

#[test]
fn rational_gcd_params_are_int() {
    let src = fixture("Rational.fai");
    assert_eq!(local_type(&src, "gcd", "a"), "Int");
    assert_eq!(local_type(&src, "gcd", "b"), "Int");
}

#[test]
fn rational_reduce_locals() {
    let src = fixture("Rational.fai");
    let locals = local_types(&src, "reduce");
    assert_eq!(locals.get("num").map(String::as_str), Some("Int"));
    assert_eq!(locals.get("den").map(String::as_str), Some("Int"));
    assert_eq!(locals.get("g").map(String::as_str), Some("Int"));
    assert_eq!(locals.get("s").map(String::as_str), Some("Int"));
}

// ── Matrix2: 4-tuple destructuring yields all Floats ─────────────────────────

#[test]
fn matrix_mul_m_locals() {
    let src = fixture("Matrix2.fai");
    let locals = local_types(&src, "mulM");
    // Both Mat2 params plus every destructured element and product term is Float.
    assert_eq!(
        locals,
        map(&[
            ("a1", "Float"),
            ("a2", "Float"),
            ("b1", "Float"),
            ("b2", "Float"),
            ("c1", "Float"),
            ("c2", "Float"),
            ("d1", "Float"),
            ("d2", "Float"),
            ("m", "{ a : Float, b : Float, c : Float, d : Float }"),
            ("n", "{ a : Float, b : Float, c : Float, d : Float }"),
            ("r11", "Float"),
            ("r12", "Float"),
            ("r21", "Float"),
            ("r22", "Float"),
        ])
    );
}

#[test]
fn matrix_inverse_locals() {
    let src = fixture("Matrix2.fai");
    let locals = local_types(&src, "inverse");
    assert_eq!(locals.get("det").map(String::as_str), Some("Float"));
    assert_eq!(locals.get("invDet").map(String::as_str), Some("Float"));
    assert_eq!(locals.get("a").map(String::as_str), Some("Float"));
}

#[test]
fn matrix_apply_mixes_matrix_and_vector_locals() {
    let src = fixture("Matrix2.fai");
    let locals = local_types(&src, "apply");
    // The Mat2 and Vec2 params, plus their destructured Float components.
    assert_eq!(
        locals,
        map(&[
            ("a", "Float"),
            ("b", "Float"),
            ("c", "Float"),
            ("d", "Float"),
            ("m", "{ a : Float, b : Float, c : Float, d : Float }"),
            ("v", "{ x : Float, y : Float }"),
            ("x", "Float"),
            ("y", "Float"),
        ])
    );
}

// ── Combinators: polymorphic local inference ─────────────────────────────────

#[test]
fn combinators_generic_local_is_polymorphic() {
    let src = fixture("Combinators.fai");
    // `id` is a generalized local: polymorphic identity.
    assert_eq!(local_type(&src, "useGenericLocal", "id"), "'a -> 'a");
    // Its two uses resolved to concrete types.
    let locals = local_types(&src, "useGenericLocal");
    assert_eq!(locals.get("i").map(String::as_str), Some("Int"));
    assert_eq!(locals.get("b").map(String::as_str), Some("Bool"));
}

#[test]
fn combinators_dup_local_is_polymorphic() {
    let src = fixture("Combinators.fai");
    // `dup = fun x -> (x, x)` generalizes to 'a -> 'a * 'a.
    assert_eq!(local_type(&src, "buildPairs", "dup"), "'a -> 'a * 'a");
}

#[test]
fn combinators_local_function_types() {
    let src = fixture("Combinators.fai");
    let locals = local_types(&src, "pipeline");
    assert_eq!(locals.get("inc").map(String::as_str), Some("Int -> Int"));
    assert_eq!(locals.get("double").map(String::as_str), Some("Int -> Int"));
    assert_eq!(locals.get("step").map(String::as_str), Some("Int -> Int"));
}

#[test]
fn combinators_partial_application_local() {
    let src = fixture("Combinators.fai");
    let locals = local_types(&src, "addThree");
    assert_eq!(locals.get("add").map(String::as_str), Some("Int -> Int -> Int"));
    assert_eq!(locals.get("addThreeFn").map(String::as_str), Some("Int -> Int"));
}

#[test]
fn combinators_onpair_components_share_variable() {
    let src = fixture("Combinators.fai");
    // `a` and `b` come from the same `'a * 'a` pair, so they share a variable.
    let locals = local_types(&src, "onPair");
    assert_eq!(locals.get("a"), locals.get("b"));
    assert_eq!(locals.get("p").map(String::as_str), Some("'a * 'a"));
}
