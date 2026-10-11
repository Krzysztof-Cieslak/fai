//! Compiler-independent workloads shared by native peers and correctness checks.
//!
//! This crate deliberately has no dependency on the Fai compiler or runtime.
//! Native comparison executables select a function directly, so unrelated
//! workload functions and the registry are not roots in the delivered image.

pub mod algorithms;
pub mod benchmark_aot;
pub mod benchmark_process;
pub mod contracts;
pub mod sorting;
pub mod tail_components;
pub mod tree_lookup;

/// The Linux process high-water resident set size, in KiB.
#[must_use]
pub fn peak_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let value = status.lines().find_map(|line| line.strip_prefix("VmHWM:"))?;
    value.split_whitespace().next()?.parse().ok()
}
