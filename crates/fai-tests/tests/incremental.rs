//! Exercises the incremental verifier on line counts and source membership.

use fai_db::{Db, line_count};
use fai_tests::assert_incremental_matches_clean;

#[test]
fn line_count_incremental_matches_clean() {
    assert_incremental_matches_clean(
        &[
            &[("a.fai", "x\ny")],
            // Add a line: count changes.
            &[("a.fai", "x\ny\nz")],
            // Same line count, different content: early cutoff territory.
            &[("a.fai", "p\nq\nr")],
            // Fewer lines.
            &[("a.fai", "one")],
            // A second file appears alongside the first.
            &[("a.fai", "one"), ("b.fai", "1\n2\n3\n4")],
        ],
        |db, ids| {
            ids.iter().map(|&id| line_count(db, db.source_file(id).unwrap())).collect::<Vec<_>>()
        },
    );
}

#[test]
fn active_sources_match_clean_after_removal_and_readdition() {
    assert_incremental_matches_clean(
        &[
            &[("a.fai", "a"), ("b.fai", "b")],
            &[("b.fai", "b")],
            &[],
            &[("b.fai", "new b"), ("a.fai", "new a")],
        ],
        |db, _| {
            let mut sources: Vec<_> = db
                .all_source_files()
                .iter()
                .map(|file| (file.path(db).clone(), file.text(db).clone()))
                .collect();
            sources.sort();
            sources
        },
    );
}
