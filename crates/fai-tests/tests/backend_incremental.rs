//! Incremental-vs-clean verification for the backend queries.
//!
//! Replays a sequence of workspace edits against one long-lived (incremental)
//! database and, at each revision, against a fresh database, asserting that the
//! lowered IR, the reference-counted IR, and the emitted object code all match.
//! A stale cache would diverge from the clean build, so this guards the
//! correctness of `core`/`rc`/`object_code` invalidation (object code must be
//! deterministic for this to hold).

use fai_core::{core, helper_inlined, pretty_def};
use fai_db::Db;
use fai_driver::object_code;
use fai_rc::rc;
use fai_syntax::Symbol;
use fai_tests::{Revision, assert_incremental_matches_clean};
use indoc::indoc;

const MAIN_A: &str = indoc! {r#"
    module Main

    public main : Runtime -> Unit / { Console }
    let main r = r.console.writeLine (Int.toString (Helper.helper 41))
"#};
const MAIN_B: &str = indoc! {r#"
    module Main

    public main : Runtime -> Unit / { Console }
    let main r = r.console.writeLine (Int.toString (Helper.helper 7))
"#};
const HELPER_1: &str = indoc! {r#"
    module Helper

    public helper : Int -> Int
    let helper x = x + 1
"#};
const HELPER_2: &str = indoc! {r#"
    module Helper

    public helper : Int -> Int
    let helper x = x + 2
"#};
const HELPER_COMMENT: &str = indoc! {r#"
    module Helper

    // shift byte offsets without changing the item tree
    public helper : Int -> Int
    let helper x = x + 2
"#};

#[test]
fn backend_queries_are_incrementally_correct() {
    let r0: &[(&str, &str)] = &[("Main.fai", MAIN_A), ("Helper.fai", HELPER_1)];
    let r1: &[(&str, &str)] = &[("Main.fai", MAIN_A), ("Helper.fai", HELPER_2)];
    let r2: &[(&str, &str)] = &[("Main.fai", MAIN_B), ("Helper.fai", HELPER_2)];
    let r3: &[(&str, &str)] = &[("Main.fai", MAIN_B), ("Helper.fai", HELPER_COMMENT)];
    let r4: &[(&str, &str)] = &[("Main.fai", MAIN_A), ("Helper.fai", HELPER_1)];
    let revisions: &[Revision] = &[r0, r1, r2, r3, r4];

    assert_incremental_matches_clean(revisions, |db, ids| {
        let main = db.source_file(ids[0]).unwrap();
        let helper = db.source_file(ids[1]).unwrap();
        let (m, h) = (Symbol::intern("main"), Symbol::intern("helper"));
        (
            (*object_code(db, main, m, false)).clone(),
            (*object_code(db, helper, h, false)).clone(),
            pretty_def(&core(db, main, m)),
            pretty_def(&rc(db, helper, h)),
        )
    });
}

// An intra-module helper (`mk`) folded into its caller (`top`). Editing `mk`'s body
// must invalidate the inlined `top` correctly: from-scratch and incremental builds
// must agree on the folded Core, the reference-counted IR, and the object code.
const LIB_MK1: &str = indoc! {r#"
    module Lib

    mk : Int -> Int
    let mk x = x + x

    public top : Int -> Int
    let top x = mk x + 1
"#};
const LIB_MK2: &str = indoc! {r#"
    module Lib

    mk : Int -> Int
    let mk x = x + x + x

    public top : Int -> Int
    let top x = mk x + 1
"#};
const LIB_MK_COMMENT: &str = indoc! {r#"
    module Lib

    // shift byte offsets without changing the folded body
    mk : Int -> Int
    let mk x = x + x + x

    public top : Int -> Int
    let top x = mk x + 1
"#};

#[test]
fn inlined_helper_is_incrementally_correct() {
    let r0: &[(&str, &str)] = &[("Lib.fai", LIB_MK1)];
    let r1: &[(&str, &str)] = &[("Lib.fai", LIB_MK2)];
    let r2: &[(&str, &str)] = &[("Lib.fai", LIB_MK_COMMENT)];
    let r3: &[(&str, &str)] = &[("Lib.fai", LIB_MK1)];
    let revisions: &[Revision] = &[r0, r1, r2, r3];

    assert_incremental_matches_clean(revisions, |db, ids| {
        let lib = db.source_file(ids[0]).unwrap();
        let (top, mk) = (Symbol::intern("top"), Symbol::intern("mk"));
        (
            (*object_code(db, lib, top, false)).clone(),
            pretty_def(&helper_inlined(db, lib, top)),
            pretty_def(&rc(db, lib, top)),
            pretty_def(&helper_inlined(db, lib, mk)),
        )
    });
}

#[test]
fn data_layout_changes_match_clean_native_objects() {
    let small = "module M\npublic type T = | End | C Int\npublic head : T -> Int\nlet head value = match value with | End -> 0 | C x -> x\n";
    let wide = "module M\npublic type T = | End | C Int Float Float Float Float Float Float Float Float Float\npublic head : T -> Int\nlet head value = match value with | End -> 0 | C x _ _ _ _ _ _ _ _ _ -> x\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", small)], &[("M.fai", wide)], &[("M.fai", small)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("head");
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn unrelated_body_edits_preserve_data_layout_cutoff() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let a =
        "module A\npublic type T = | End | C Int\npublic other : Int -> Int\nlet other x = x + 1\n";
    db.add_source("A.fai".into(), a.into());
    let b = db.add_source("B.fai".into(), "module B\npublic head : A.T -> Int\nlet head value = match value with | A.End -> 0 | A.C x -> x\n".into());
    let file = db.source_file(b).unwrap();
    let name = Symbol::intern("head");
    let before = rc(&db, file, name);
    db.add_source("A.fai".into(), a.replace("x + 1", "x + 2"));
    let after = rc(&db, file, name);
    assert!(
        std::sync::Arc::ptr_eq(&before, &after),
        "a body edit leaves the consumer's typed layout query cached"
    );
}

#[test]
fn recursive_scalar_body_edits_match_clean_native_objects() {
    let source = "module M\nlet f n = if n <= 1 then n else f (n - 1) + f (n - 2)\n";
    let edited = source.replace("then n", "then n + 1");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            (*object_code(db, file, Symbol::intern("f"), false)).clone()
        },
    );
}

#[test]
fn scalar_list_scan_edits_match_clean_native_objects() {
    let source = "module M\nlet scan acc xs = match xs with | [] -> acc | x :: rest -> scan (acc + x) rest\n";
    let edited = source.replace("acc + x", "acc - x");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*object_code(db, db.source_file(files[0]).unwrap(), Symbol::intern("scan"), false))
                .clone()
        },
    );
}

#[test]
fn addition_tail_recursion_edits_match_clean_native_objects() {
    let source = "module M\nlet sum n = if n <= 0 then 0 else n + sum (n - 1)\n";
    let edited = source.replace("n + sum", "(n * 2) + sum");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*object_code(db, db.source_file(files[0]).unwrap(), Symbol::intern("sum"), false))
                .clone()
        },
    );
}

#[test]
fn resource_free_search_edits_match_clean_native_objects() {
    let source = "module M\ntype T = | End | Node Int T\nlet scan tree = match tree with | End -> true | Node _ rest -> scan rest\n";
    let resource = source.replace("Node Int T", "Node Reader T");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &resource)], &[("M.fai", source)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("scan");
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn unrelated_body_edits_preserve_search_cutoff() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let a = "module A\npublic type T = | End | Node Int T\npublic other : Int -> Int\nlet other x = x + 1\n";
    db.add_source("A.fai".into(), a.into());
    let b = db.add_source("B.fai".into(), "module B\npublic scan : A.T -> Bool\nlet scan tree = match tree with | A.End -> true | A.Node _ rest -> scan rest\n".into());
    let file = db.source_file(b).unwrap();
    let name = Symbol::intern("scan");
    let before = object_code(&db, file, name, false);
    db.add_source("A.fai".into(), a.replace("x + 1", "x + 2"));
    let after = object_code(&db, file, name, false);
    assert!(
        std::sync::Arc::ptr_eq(&before, &after),
        "unrelated bodies do not invalidate the data-search object"
    );
}

#[test]
fn polymorphic_borrow_changes_update_callers_incrementally() {
    let borrowed = "module A\npublic same : 'a -> 'a -> Bool\nlet same a b = a = b\n";
    let owned = "module A\npublic same : 'a -> 'a -> Bool\nlet same a b = (a, b) = (a, b)\n";
    let caller = "module B\npublic same : String -> String -> Bool\nlet same a b = A.same a b\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("A.fai", borrowed), ("B.fai", caller)], &[("A.fai", owned), ("B.fai", caller)]],
        |db, files| {
            let name = Symbol::intern("same");
            let file = db.source_file(files[1]).unwrap();
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn callback_invariance_edits_match_clean_native_objects() {
    let invariant = "module M\nlet loop f g n x = if n <= 0 then x else loop f g (n - 1) (f x)\n";
    let changing = invariant.replace("loop f g (n - 1)", "loop g f (n - 1)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", invariant)], &[("M.fai", &changing)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            (*object_code(db, file, Symbol::intern("loop"), false)).clone()
        },
    );
}

#[test]
fn uniform_leaf_eligibility_edits_match_clean_native_objects() {
    let source = "module M\nlet make u = fun a b -> a - b\n";
    let edited = source.replace("a - b", "(a - b) + 1");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*object_code(db, db.source_file(files[0]).unwrap(), Symbol::intern("make"), false))
                .clone()
        },
    );
}

#[test]
fn discarded_field_edits_match_clean_native_objects() {
    let source = "module M\ntype Slot 'a 'b = | Full 'a 'b\nlet probe i xs = match Array.unsafeGet i xs with | Full k v -> (k, xs)\n";
    let edited = source.replace("(k, xs)", "(v, xs)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("probe");
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn unique_constructor_tag_edits_match_clean_native_objects() {
    let source = "module M\ntype T = | Empty | Full Int\nlet kind value = match value with | Empty -> 0 | _ -> 1\n";
    let edited = source.replace("Full Int", "Full Int | Other Bool");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)], &[("M.fai", source)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("kind");
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn borrowed_field_lifetime_edits_match_clean_native_objects() {
    let source = "module M\ntype Slot 'a = | Full 'a String\nlet probe key i xs = match Array.unsafeGet i xs with | Full stored _ -> if stored = key then Array.length xs else 0\n";
    let edited = source.replace("if stored = key then Array.length xs else 0", "(stored, xs)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("probe");
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn spread_self_tail_edits_match_clean_native_objects() {
    let source = "module M\ntype State = { value : Float }\nlet loop n state = if n <= 0 then state else loop (n - 1) { value = state.value + 1.0 }\n";
    let edited = source.replace(
        "else loop (n - 1) { value = state.value + 1.0 }",
        "else\n  let prior = loop (n - 1) state\n  { value = prior.value + 1.0 }",
    );
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*object_code(db, db.source_file(files[0]).unwrap(), Symbol::intern("loop"), false))
                .clone()
        },
    );
}

#[test]
fn canonical_callback_shape_edits_match_clean_native_objects() {
    let source = "module M\nlet make _ = fun a b -> a - b\n";
    let edited = source.replace("a - b", "b - a");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*object_code(db, db.source_file(files[0]).unwrap(), Symbol::intern("make"), false))
                .clone()
        },
    );
}

#[test]
fn numeric_array_loop_eligibility_edits_match_clean_native_objects() {
    let source =
        "module M\nlet loop n xs = if n <= 0 then xs else loop (n - 1) (Array.unsafeSet 0 n xs)\n";
    let edited = source.replace("Array.unsafeSet 0 n xs", "Array.push n xs");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*object_code(db, db.source_file(files[0]).unwrap(), Symbol::intern("loop"), false))
                .clone()
        },
    );
}

#[test]
fn checked_access_order_edits_update_callee_facts() {
    let source = "module M\nlet probe depth i xs = if depth <= 0 then Array.unsafeGet i xs else probe (depth - 1) i xs\npublic read : Int -> Array Int -> Int\nlet read i xs =\n  let first = Array.unsafeGet i xs\n  first + probe 0 i xs\n";
    let edited = source.replace(
        "let first = Array.unsafeGet i xs\n  first + probe 0 i xs",
        "let first = probe 0 i xs\n  first + Array.unsafeGet i xs",
    );
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("probe");
            (fai_rc::entry_bounds(db, file, name), (*object_code(db, file, name, false)).clone())
        },
    );
}

#[test]
fn borrowed_list_scan_edits_update_the_callers_ownership() {
    let source = "module A\npublic sum : Int -> List Int -> Int\nlet sum acc xs = match xs with | [] -> acc | x :: rest -> sum (acc + x) rest\n";
    let changed = source
        .replace("sum (acc + x) rest", "if x < 0 then sum acc (0 :: rest) else sum (acc + x) rest");
    let caller = "module B\npublic run : List Int -> Int\nlet run xs = A.sum 0 xs + A.sum 0 xs\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("A.fai", source), ("B.fai", caller)], &[("A.fai", &changed), ("B.fai", caller)]],
        |db, files| {
            let file = db.source_file(files[1]).unwrap();
            let name = Symbol::intern("run");
            ((*rc(db, file, name)).clone(), (*object_code(db, file, name, false)).clone())
        },
    );
}
