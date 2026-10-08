//! Visibility edits must agree between incremental and clean public-surface checks.

use fai_db::{Db, Diag};

#[test]
fn interface_visibility_edits_match_clean_checks() {
    let private = "module Main\ninterface Hidden = run : Unit -> Unit / { Hidden }\npublic expose : Hidden -> Int\nlet expose value = 1\n";
    let public = private.replace("interface Hidden", "public interface Hidden");
    fai_tests::assert_incremental_matches_clean(
        &[&[("Main.fai", private)], &[("Main.fai", &public)], &[("Main.fai", private)]],
        |db, ids| {
            let mut codes: Vec<_> =
                fai_resolve::resolve::accumulated::<Diag>(db, db.source_file(ids[0]).unwrap())
                    .into_iter()
                    .map(|d| d.0.code.as_str().to_owned())
                    .collect();
            codes.sort();
            codes
        },
    );
}
