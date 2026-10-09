//! Literal spelling edits preserve incremental pattern diagnostics.

use fai_db::{Db, Diag};

#[test]
fn equivalent_literal_edits_match_clean_diagnostics() {
    let before = "module M\nlet f x =\n  match x with\n  | 255 -> 1\n  | 256 -> 2\n  | _ -> 3\n";
    let after = before.replace("256", "0xff");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", before)], &[("M.fai", &after)], &[("M.fai", before)]],
        |db, ids| {
            fai_types::check_file::accumulated::<Diag>(db, db.source_file(ids[0]).unwrap())
                .iter()
                .map(|d| (d.0.code.as_str().to_owned(), d.0.primary.range(), d.0.message.clone()))
                .collect::<Vec<_>>()
        },
    );
}
