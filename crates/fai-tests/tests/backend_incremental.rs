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
            (*object_code(db, main, m)).clone(),
            (*object_code(db, helper, h)).clone(),
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
            (*object_code(db, lib, top)).clone(),
            pretty_def(&helper_inlined(db, lib, top)),
            pretty_def(&rc(db, lib, top)),
            pretty_def(&helper_inlined(db, lib, mk)),
        )
    });
}
