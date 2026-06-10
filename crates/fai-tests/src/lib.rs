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

/// The process's peak resident set size in KiB, or `None` where it cannot be
/// determined. Read from `/proc/self/status` (`VmHWM`, the high-water mark of
/// resident memory). Linux-only; every other platform yields `None`.
///
/// This duplicates the runtime's `fai_runtime::peak_rss_kib` deliberately: the
/// `algo-baseline` binary needs it but reaches the runtime only as a
/// dev-dependency (unavailable to a `[[bin]]`), so the Rust side of the memory
/// comparison self-reports through this copy while the Fai side uses the
/// runtime's.
#[must_use]
pub fn peak_rss_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        parse_vmhwm_kib(&std::fs::read_to_string("/proc/self/status").ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Extracts the `VmHWM:` value (peak resident set size, in KiB) from the contents
/// of `/proc/self/status`. Split out so the line parsing is testable on any
/// platform, not only where `/proc` exists.
#[cfg(any(target_os = "linux", test))]
fn parse_vmhwm_kib(status: &str) -> Option<u64> {
    for line in status.lines() {
        // The line reads `VmHWM:\t   <n> kB`; take the leading numeric field.
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// On Linux the peak-RSS probe (used by `algo-baseline`'s self-report) reads a
    /// positive high-water mark; off Linux it yields `None` rather than a wrong
    /// number, so the harness marks the measurement unavailable.
    #[test]
    fn peak_rss_matches_platform_availability() {
        let measured = peak_rss_kib();
        if cfg!(target_os = "linux") {
            assert!(measured.is_some_and(|kib| kib > 0), "Linux reports a positive peak RSS");
        } else {
            assert_eq!(measured, None, "peak RSS is unavailable off Linux");
        }
    }

    /// The `/proc/self/status` `VmHWM:` line is parsed to its KiB value; an absent
    /// or malformed field yields `None`.
    #[test]
    fn parses_vmhwm_from_status_text() {
        let status = "Name:\tbaseline\nVmHWM:\t   65536 kB\nVmRSS:\t 60000 kB\n";
        assert_eq!(parse_vmhwm_kib(status), Some(65536));
        assert_eq!(parse_vmhwm_kib("VmRSS:\t 100 kB\n"), None);
        assert_eq!(parse_vmhwm_kib("VmHWM:\t kB\n"), None);
        assert_eq!(parse_vmhwm_kib(""), None);
    }
}
