//! Test support for the Fai workspace.
//!
//! The reusable piece is the **incremental verifier**: it replays a sequence of
//! workspace revisions against one long-lived (incremental) database and, at
//! each revision, against a freshly built (clean) database, asserting the query
//! results match. Stale incremental results would diverge from the clean run,
//! so this guards the correctness of early cutoff and invalidation as later
//! phases add real queries.

pub mod algorithms;
pub mod bench_summary;
mod checker;

pub use checker::{
    CheckOutcome, check_named, check_source, local_type, local_types, run_annotated, sym, type_of,
};

use std::fmt::Debug;

use camino::Utf8PathBuf;
use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::Diagnostic;
use fai_span::SourceId;

/// Collects the resolution + type diagnostics belonging to `file` (filtering out
/// diagnostics from other files that transitive accumulation surfaces). Shared by
/// the corpus self-check and the performance guards.
#[must_use]
pub fn check_source_diagnostics(db: &dyn Db, file: SourceFile) -> Vec<Diagnostic> {
    let source = file.source(db);
    let mut out = Vec::new();
    for diag in fai_syntax::parse::accumulated::<Diag>(db, file) {
        if diag.0.primary.source() == source {
            out.push(diag.0.clone());
        }
    }
    for diag in fai_resolve::resolve::accumulated::<Diag>(db, file) {
        if diag.0.primary.source() == source {
            out.push(diag.0.clone());
        }
    }
    for diag in fai_types::check_file::accumulated::<Diag>(db, file) {
        if diag.0.primary.source() == source {
            out.push(diag.0.clone());
        }
    }
    out
}

/// One workspace revision: the full set of `(path, text)` files at that point.
pub type Revision<'a> = &'a [(&'a str, &'a str)];

/// Asserts that running `query` against an incrementally updated database
/// matches running it against a database built from scratch, at every revision.
///
/// `query` receives the database and the [`SourceId`]s of the current revision's
/// files (in declaration order).
///
/// # Panics
///
/// Panics if any revision's incremental result differs from the clean result.
pub fn assert_incremental_matches_clean<T, Q>(revisions: &[Revision], query: Q)
where
    T: PartialEq + Debug,
    Q: Fn(&FaiDatabase, &[SourceId]) -> T,
{
    let mut incremental = FaiDatabase::new();
    for (index, revision) in revisions.iter().enumerate() {
        let incremental_ids = load(&mut incremental, revision);
        let incremental_result = query(&incremental, &incremental_ids);

        let mut clean = FaiDatabase::new();
        let clean_ids = load(&mut clean, revision);
        let clean_result = query(&clean, &clean_ids);

        assert_eq!(
            incremental_result, clean_result,
            "incremental result diverged from a clean build at revision {index}"
        );
    }
}

/// Loads a revision's files into `db`, returning their ids in order.
fn load(db: &mut FaiDatabase, revision: Revision) -> Vec<SourceId> {
    revision
        .iter()
        .map(|(path, text)| db.add_source(Utf8PathBuf::from(*path), (*text).to_owned()))
        .collect()
}
