//! Incremental-vs-clean verification for the backend queries.
//!
//! Replays a sequence of workspace edits against one long-lived (incremental)
//! database and, at each revision, against a fresh database, asserting that the
//! lowered IR, the reference-counted IR, and the emitted object code all match.
//! A stale cache would diverge from the clean build, so this guards the
//! correctness of `core`/`rc`/`object_code` invalidation (object code must be
//! deterministic for this to hold).

use fai_core::{core, pretty_def};
use fai_db::Db;
use fai_driver::object_code;
use fai_rc::rc;
use fai_syntax::Symbol;
use fai_tests::{Revision, assert_incremental_matches_clean};

const MAIN_A: &str = "module Main\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (Int.toString (Helper.helper 41))\n";
const MAIN_B: &str = "module Main\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (Int.toString (Helper.helper 7))\n";
const HELPER_1: &str = "module Helper\n\npublic helper : Int -> Int\nlet helper x = x + 1\n";
const HELPER_2: &str = "module Helper\n\npublic helper : Int -> Int\nlet helper x = x + 2\n";
const HELPER_COMMENT: &str = "module Helper\n\n// shift byte offsets without changing the item tree\npublic helper : Int -> Int\nlet helper x = x + 2\n";

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
