//! Explicit single-program allocation windows for optimized native diagnostics.
//!
//! Fai object storage and allocator backing are separate accounting domains.
//! These values exclude Rust host temporaries and OS/host-allocator rounding;
//! they must not be compared directly with a tracing collector's reserved heap.

use std::cell::Cell;

#[derive(Clone, Copy)]
struct Baseline {
    objects: i64,
    bytes: i64,
    backing: i64,
}

thread_local! {
    static WINDOW: Cell<Option<Baseline>> = const { Cell::new(None) };
}

/// Starts a quiescent, single-program allocation window when explicitly requested.
/// The raw integer ABI is callable from the benchmark's private foreign binding.
#[unsafe(no_mangle)]
pub extern "C" fn fai_benchmark_begin(_: i64) -> i64 {
    if std::env::var_os("FAI_REPORT_ALLOCATIONS").is_none() {
        return 0;
    }
    if !super::ALLOCATION_STATS_ENABLED {
        super::fai_panic("allocation diagnostics require an allocation-stats runtime");
    }
    let baseline = Baseline {
        objects: super::live_count(),
        bytes: super::live_bytes(),
        backing: super::allocation_stats::current().total(),
    };
    if WINDOW.with(|window| window.replace(Some(baseline))).is_some() {
        super::fai_panic("overlapping allocation diagnostic windows");
    }
    super::reset_allocations();
    0
}

/// Emits raw object and backing-store counters before formatting the workload result.
#[unsafe(no_mangle)]
pub extern "C" fn fai_benchmark_end(_: i64) -> i64 {
    let Some(baseline) = WINDOW.with(Cell::take) else { return 0 };
    let storage = super::allocation_stats::current();
    eprintln!(
        "fai-allocation-stats: {{\"domain\":\"fai-object-storage-v1\",\"allocations\":{},\"allocatedBytes\":{},\"baselineLiveObjects\":{},\"baselineLiveBytes\":{},\"peakLiveObjects\":{},\"peakLiveBytes\":{},\"finalLiveObjects\":{},\"finalLiveBytes\":{},\"arrayCopies\":{},\"stringCopies\":{},\"stringViews\":{},\"closureAllocations\":{},\"partialAllocations\":{},\"baselineBackingBytes\":{},\"peakBackingBytes\":{},\"retainedSlabBytes\":{},\"retainedMappedBytes\":{},\"retainedSystemBytes\":{}}}",
        super::allocations(),
        super::allocated_bytes(),
        baseline.objects,
        baseline.bytes,
        super::peak_live_count(),
        super::peak_live_bytes(),
        super::live_count(),
        super::live_bytes(),
        super::array_copies(),
        super::string_copies(),
        super::string_views(),
        super::closure_allocations(),
        super::pap_allocations(),
        baseline.backing,
        super::allocation_stats::peak_bytes(),
        storage.slab_bytes,
        storage.mapped_bytes,
        storage.system_bytes,
    );
    0
}
