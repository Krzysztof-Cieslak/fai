//! Exercises the incremental verifier on the database's line-count query.

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
