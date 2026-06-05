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

// ── Geometry: tuple components share the Vec2/Vec3 structure consistently ────

#[test]
fn geometry_add2_locals_are_all_float() {
    let src = fixture("Geometry.fai");
    let locals = local_types(&src, "add2");
    // Every destructured component is a Float; the params are Float * Float.
    assert_eq!(
        locals,
        map(&[
            ("a", "Float * Float"),
            ("ax", "Float"),
            ("ay", "Float"),
            ("b", "Float * Float"),
            ("bx", "Float"),
            ("by", "Float"),
        ])
    );
}

#[test]
fn geometry_cross3_all_components_float() {
    let src = fixture("Geometry.fai");
    let locals = local_types(&src, "cross3");
    for name in ["ax", "ay", "az", "bx", "by", "bz", "cx", "cy", "cz"] {
        assert_eq!(locals.get(name).map(String::as_str), Some("Float"), "{name}");
    }
    assert_eq!(locals.get("a").map(String::as_str), Some("Float * Float * Float"));
}

#[test]
fn geometry_step_threads_vec2_through_locals() {
    let src = fixture("Geometry.fai");
    let locals = local_types(&src, "step");
    // The physics step keeps everything as Vec2 (Float * Float).
    for name in ["pos", "vel", "gravity", "newVel", "newPos"] {
        assert_eq!(locals.get(name).map(String::as_str), Some("Float * Float"), "{name}");
    }
}

// ── Rational: every intermediate is Int, params destructure consistently ─────

#[test]
fn rational_add_r_all_ints() {
    let src = fixture("Rational.fai");
    let locals = local_types(&src, "addR");
    assert_eq!(
        locals,
        map(&[
            ("a", "Int * Int"),
            ("ad", "Int"),
            ("an", "Int"),
            ("b", "Int * Int"),
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
fn matrix_mul_m_results_are_float() {
    let src = fixture("Matrix2.fai");
    let locals = local_types(&src, "mulM");
    for name in ["a1", "b1", "c1", "d1", "a2", "b2", "c2", "d2", "r11", "r12", "r21", "r22"] {
        assert_eq!(locals.get(name).map(String::as_str), Some("Float"), "{name}");
    }
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
    // Matrix components and vector components are all Float.
    for name in ["a", "b", "c", "d", "x", "y"] {
        assert_eq!(locals.get(name).map(String::as_str), Some("Float"), "{name}");
    }
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
