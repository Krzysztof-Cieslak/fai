//! Demand-filled small-object slabs, retained across thread-local pool lifetimes.

use std::alloc::Layout;
use std::sync::atomic::{AtomicUsize, Ordering, fence};

/// Slab alignment also makes its ownership header recoverable from any cell.
pub(super) const BYTES: usize = 64 * 1024;
/// Offset of the first eight-byte-aligned cell.
pub(super) const START: usize = std::mem::size_of::<Slab>();

/// One reference for the allocating pool's unfinished cursor and one for each
/// issued cell, whether live or on a thread-local free list. This is independent
/// of the Fai reference count stored inside each live object.
#[repr(C)]
pub(super) struct Slab {
    remaining: AtomicUsize,
}

/// Maps a slab but touches only its ownership header. Cells are provisioned by
/// the pool in small batches, leaving unused pages uncommitted until needed.
pub(super) fn allocate() -> *mut Slab {
    let memory = map();
    debug_assert_eq!(memory.addr() % BYTES, 0);
    let slab = memory.cast::<Slab>();
    // SAFETY: map returns an aligned, writable BYTES-byte mapping.
    unsafe { slab.write(Slab { remaining: AtomicUsize::new(1) }) };
    slab
}

/// Accounts for newly issued cells while the allocating pool owns the cursor.
///
/// # Safety
/// `slab` is live and the caller retains its cursor reference.
pub(super) unsafe fn issue(slab: *mut Slab, count: usize) {
    // SAFETY: the cursor reference keeps the ownership header alive.
    unsafe { (*slab).remaining.fetch_add(count, Ordering::Relaxed) };
}

/// Releases a cursor or cell reference; returns whether this unmapped the slab.
///
/// # Safety
/// The caller owns one outstanding slab reference, consumed by this operation.
pub(super) unsafe fn release(slab: *mut Slab) -> bool {
    // SAFETY: the caller's outstanding reference keeps the header alive until
    // this decrement. Other owners likewise hold references while accessing it.
    if unsafe { (*slab).remaining.fetch_sub(1, Ordering::Release) } != 1 {
        return false;
    }
    fence(Ordering::Acquire);
    // SAFETY: all cells and the cursor have been released; no owner can touch
    // the mapping after this final decrement.
    unsafe { unmap(slab.cast()) };
    true
}

/// Releases the slab reference of a cell removed from a dying thread's pool.
///
/// # Safety
/// `cell` is an issued slab cell, no longer live or linked from any free list.
pub(super) unsafe fn release_cell(cell: *mut u8) -> bool {
    let header = cell.map_addr(|address| address & !(BYTES - 1)).cast::<Slab>();
    // SAFETY: slab alignment recovers the header within the same allocation;
    // the issued cell owns the reference being released.
    unsafe { release(header) }
}

fn allocation_failed() -> ! {
    std::alloc::handle_alloc_error(Layout::from_size_align(BYTES, BYTES).expect("slab layout"))
}

#[cfg(all(unix, not(miri)))]
fn map() -> *mut u8 {
    // Supported Unix page sizes divide BYTES. Reserve twice the slab size and
    // trim the page-aligned prefix/suffix to obtain one aligned mapping.
    // SAFETY: anonymous private mappings need no file descriptor or input memory.
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            BYTES * 2,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        allocation_failed();
    }
    let raw = raw.cast::<u8>();
    let address = (raw.addr() + BYTES - 1) & !(BYTES - 1);
    let start = raw.with_addr(address);
    let prefix = address - raw.addr();
    let suffix = BYTES - prefix;
    // SAFETY: both trimmed ranges lie in the fresh mapping and are page-aligned;
    // the retained BYTES-byte slab remains mapped and writable.
    unsafe {
        if prefix != 0 {
            let result = libc::munmap(raw.cast(), prefix);
            debug_assert_eq!(result, 0);
        }
        if suffix != 0 {
            let result = libc::munmap(start.add(BYTES).cast(), suffix);
            debug_assert_eq!(result, 0);
        }
    }
    start
}

#[cfg(all(unix, not(miri)))]
unsafe fn unmap(memory: *mut u8) {
    // SAFETY: memory is exactly the retained aligned mapping returned by map.
    let result = unsafe { libc::munmap(memory.cast(), BYTES) };
    debug_assert_eq!(result, 0);
}

#[cfg(all(windows, not(miri)))]
fn map() -> *mut u8 {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    // SAFETY: a null address requests a fresh allocation; Windows allocation
    // granularity aligns the reservation to 64 KiB.
    let memory =
        unsafe { VirtualAlloc(std::ptr::null(), BYTES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    if memory.is_null() {
        allocation_failed();
    }
    memory.cast()
}

#[cfg(all(windows, not(miri)))]
unsafe fn unmap(memory: *mut u8) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: memory is the original reservation address; size zero releases it.
    let result = unsafe { VirtualFree(memory.cast(), 0, MEM_RELEASE) };
    debug_assert_ne!(result, 0);
}

#[cfg(any(miri, not(any(unix, windows))))]
fn map() -> *mut u8 {
    let layout = Layout::from_size_align(BYTES, BYTES).expect("slab layout");
    // SAFETY: layout is nonzero and valid; this path is also supported by Miri.
    let memory = unsafe { std::alloc::alloc(layout) };
    if memory.is_null() {
        allocation_failed();
    }
    memory
}

#[cfg(any(miri, not(any(unix, windows))))]
unsafe fn unmap(memory: *mut u8) {
    let layout = Layout::from_size_align(BYTES, BYTES).expect("slab layout");
    // SAFETY: memory was allocated by map with this exact layout.
    unsafe { std::alloc::dealloc(memory, layout) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pool, SIZE_STEP};

    struct OwnedCell(*mut u8);

    // SAFETY: this wrapper moves one exclusively owned issued cell between
    // threads; the slab lifetime is guarded by its atomic ownership count.
    unsafe impl Send for OwnedCell {}

    impl OwnedCell {
        fn release(self) -> bool {
            // SAFETY: this wrapper owns the cell's outstanding slab reference.
            unsafe { release_cell(self.0) }
        }
    }

    #[test]
    fn one_cell_does_not_provision_a_full_slab() {
        let pool = Pool::new();
        let class = 48 / SIZE_STEP;
        let cell = pool.take(class);
        assert_eq!(pool.offsets[class].get(), START + 64 * 48);
        assert!(pool.offsets[class].get() < BYTES);
        drop(pool);
        // SAFETY: the issued cell retains the last slab reference after the pool.
        assert!(unsafe { release_cell(cell) });
    }

    #[test]
    fn refill_crosses_slab_boundaries_without_overlapping_cells() {
        let pool = Pool::new();
        let class = 48 / SIZE_STEP;
        let count = (BYTES / 48) * 2 + 3;
        let cells: Vec<_> = (0..count).map(|_| pool.take(class)).collect();
        let distinct: std::collections::HashSet<_> = cells.iter().map(|p| p.addr()).collect();
        assert_eq!(distinct.len(), count);
        for (i, &cell) in cells.iter().enumerate() {
            // SAFETY: each issued cell owns 48 disjoint writable bytes.
            unsafe {
                cell.cast::<u64>().write(i as u64);
                cell.add(40).cast::<u64>().write(!i as u64);
            }
        }
        drop(pool);
        for (i, cell) in cells.into_iter().enumerate() {
            // SAFETY: the outstanding cell keeps its slab alive after pool drop.
            unsafe {
                assert_eq!(cell.cast::<u64>().read(), i as u64);
                assert_eq!(cell.add(40).cast::<u64>().read(), !i as u64);
                release_cell(cell);
            }
        }
    }

    #[test]
    fn a_cell_outlives_its_allocating_thread() {
        let cell = std::thread::spawn(|| {
            let pool = Pool::new();
            let cell = pool.take(32 / SIZE_STEP);
            // SAFETY: this thread owns the fresh cell.
            unsafe { cell.cast::<u64>().write(42) };
            OwnedCell(cell)
        })
        .join()
        .unwrap();
        // SAFETY: the issued cell retained its slab after the thread's pool died.
        unsafe {
            assert_eq!(cell.0.cast::<u64>().read(), 42);
        }
        assert!(cell.release());
    }

    #[test]
    fn concurrent_last_owners_reclaim_exactly_once() {
        let cells = std::thread::spawn(|| {
            let pool = Pool::new();
            [OwnedCell(pool.take(32 / SIZE_STEP)), OwnedCell(pool.take(32 / SIZE_STEP))]
        })
        .join()
        .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let reclaimed = std::thread::scope(|scope| {
            let jobs: Vec<_> = cells
                .into_iter()
                .map(|cell| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        cell.release()
                    })
                })
                .collect();
            jobs.into_iter().map(|job| job.join().unwrap()).filter(|reclaimed| *reclaimed).count()
        });
        assert_eq!(reclaimed, 1);
    }
}
