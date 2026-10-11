//! Opt-in accounting for allocator-owned storage, including dead recycling cells.
//!
//! Slabs remain counted until their last cell/cursor owner releases the mapping.
//! Large mappings include their alignment prefix; system allocations count the
//! requested layout. OS page rounding and the host allocator's own arenas are
//! outside these counters, so process RSS remains a separate measurement.

#[cfg(any(debug_assertions, feature = "allocation-stats"))]
use std::sync::atomic::{AtomicI64, Ordering};

/// Storage still owned by the runtime, whether its objects are live or recycled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Storage {
    /// Complete small-object slabs, including unused space and recycling lists.
    pub slab_bytes: i64,
    /// Requested large-object mappings, including the alignment prefix.
    pub mapped_bytes: i64,
    /// Requested unpooled system allocations below the mapping threshold.
    pub system_bytes: i64,
}

impl Storage {
    /// Total allocator-owned bytes, excluding OS/host-allocator rounding.
    #[must_use]
    pub fn total(self) -> i64 {
        self.slab_bytes + self.mapped_bytes + self.system_bytes
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Slab,
    Mapped,
    System,
}

#[cfg(any(debug_assertions, feature = "allocation-stats"))]
static BYTES: [AtomicI64; 3] = [const { AtomicI64::new(0) }; 3];
#[cfg(any(debug_assertions, feature = "allocation-stats"))]
static TOTAL: AtomicI64 = AtomicI64::new(0);
#[cfg(any(debug_assertions, feature = "allocation-stats"))]
static PEAK: AtomicI64 = AtomicI64::new(0);

/// Current storage accounting. Fields are zero without debug assertions or allocation-stats.
/// Read when allocation workers are quiescent for a consistent category snapshot.
#[must_use]
pub fn current() -> Storage {
    #[cfg(any(debug_assertions, feature = "allocation-stats"))]
    {
        Storage {
            slab_bytes: BYTES[Kind::Slab as usize].load(Ordering::Relaxed),
            mapped_bytes: BYTES[Kind::Mapped as usize].load(Ordering::Relaxed),
            system_bytes: BYTES[Kind::System as usize].load(Ordering::Relaxed),
        }
    }
    #[cfg(not(any(debug_assertions, feature = "allocation-stats")))]
    Storage::default()
}

/// Maximum total owned storage since `reset_allocations`, including free slabs.
/// Zero without accounting. The high-water mark also covers worker threads.
#[must_use]
pub fn peak_bytes() -> i64 {
    #[cfg(any(debug_assertions, feature = "allocation-stats"))]
    {
        PEAK.load(Ordering::Relaxed)
    }
    #[cfg(not(any(debug_assertions, feature = "allocation-stats")))]
    {
        0
    }
}

pub(crate) fn reset_peak() {
    #[cfg(any(debug_assertions, feature = "allocation-stats"))]
    PEAK.store(TOTAL.load(Ordering::Relaxed), Ordering::Relaxed);
}

#[inline]
pub(crate) fn acquire(kind: Kind, size: usize) {
    #[cfg(any(debug_assertions, feature = "allocation-stats"))]
    {
        BYTES[kind as usize].fetch_add(size as i64, Ordering::Relaxed);
        let total = TOTAL.fetch_add(size as i64, Ordering::Relaxed) + size as i64;
        PEAK.fetch_max(total, Ordering::Relaxed);
    }
    #[cfg(not(any(debug_assertions, feature = "allocation-stats")))]
    let _ = (kind, size);
}

#[inline]
pub(crate) fn release(kind: Kind, size: usize) {
    #[cfg(any(debug_assertions, feature = "allocation-stats"))]
    {
        BYTES[kind as usize].fetch_sub(size as i64, Ordering::Relaxed);
        TOTAL.fetch_sub(size as i64, Ordering::Relaxed);
    }
    #[cfg(not(any(debug_assertions, feature = "allocation-stats")))]
    let _ = (kind, size);
}
