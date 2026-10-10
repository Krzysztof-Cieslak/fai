//! Large objects use independent mappings so final release returns their pages.

use std::alloc::Layout;

/// Minimum object size allocated independently of the host allocator's arenas.
pub(super) const THRESHOLD: usize = 16 * 1024;

// Common buffer payloads start 32 bytes after the object pointer. Bias the
// page-aligned allocation so their first slot/byte starts on a cache line.
const BIAS: usize = 32;

/// Allocates an aligned object region with room for the common buffer prefix.
pub(super) fn allocate(size: usize) -> *mut u8 {
    let mapped = reservation_size(size).unwrap_or_else(|| super::fai_allocation_size_panic());
    let base = map(mapped);
    super::allocation_stats::acquire(super::allocation_stats::Kind::Mapped, mapped);
    // SAFETY: map allocated the requested object plus the fixed prefix.
    unsafe { base.add(BIAS) }
}

fn reservation_size(size: usize) -> Option<usize> {
    size.checked_add(BIAS).filter(|n| *n <= (super::MAX_ALLOCATION_SIZE & !63))
}

/// Releases an exclusively owned object region.
///
/// # Safety
/// `memory` and `size` are the unchanged pointer/length from [`allocate`], and no
/// live value or outstanding borrow can access the allocation again.
pub(super) unsafe fn release(memory: *mut u8, size: usize) {
    // SAFETY: memory and size are the original object pointer/length returned
    // above; subtracting its prefix recovers the still-live reservation.
    unsafe { unmap(memory.sub(BIAS), size + BIAS) };
    super::allocation_stats::release(super::allocation_stats::Kind::Mapped, size + BIAS);
}

fn failed(size: usize) -> ! {
    std::alloc::handle_alloc_error(
        Layout::from_size_align(size, super::ALIGN).expect("checked object layout"),
    )
}

#[cfg(all(unix, not(miri)))]
fn map(size: usize) -> *mut u8 {
    // SAFETY: a fresh anonymous mapping is writable and page aligned. The kernel
    // rounds the checked nonzero length to pages; no fixed address is requested.
    let memory = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if memory == libc::MAP_FAILED {
        failed(size);
    }
    memory.cast()
}

#[cfg(all(unix, not(miri)))]
unsafe fn unmap(memory: *mut u8, size: usize) {
    // SAFETY: the caller supplies the original mapping and requested length,
    // after every owned value has been released. The kernel rounds the length.
    let result = unsafe { libc::munmap(memory.cast(), size) };
    debug_assert_eq!(result, 0);
}

#[cfg(all(windows, not(miri)))]
fn map(size: usize) -> *mut u8 {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    // SAFETY: the null address requests a fresh page-aligned writable region.
    let memory =
        unsafe { VirtualAlloc(std::ptr::null(), size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
    if memory.is_null() {
        failed(size);
    }
    memory.cast()
}

#[cfg(all(windows, not(miri)))]
unsafe fn unmap(memory: *mut u8, _size: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: the original reservation is exclusively owned and no longer live.
    let result = unsafe { VirtualFree(memory.cast(), 0, MEM_RELEASE) };
    debug_assert_ne!(result, 0);
}

#[cfg(any(miri, not(any(unix, windows))))]
fn map(size: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, 64).expect("checked object layout");
    // SAFETY: the object layout was checked before this allocation request.
    let memory = unsafe { std::alloc::alloc(layout) };
    if memory.is_null() {
        failed(size);
    }
    memory
}

#[cfg(any(miri, not(any(unix, windows))))]
unsafe fn unmap(memory: *mut u8, size: usize) {
    let layout = Layout::from_size_align(size, 64).expect("checked object layout");
    // SAFETY: this matches the layout and pointer used by allocate on this host.
    unsafe { std::alloc::dealloc(memory, layout) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(size: usize) {
        let memory = super::super::system_alloc(size);
        // SAFETY: system_alloc gives exactly size writable bytes. All reads stay
        // within that region and it is released only after the last access.
        unsafe {
            for i in 0..size {
                memory.add(i).write((i % 251) as u8);
            }
            for i in 0..size {
                assert_eq!(memory.add(i).read(), (i % 251) as u8);
            }
            if size >= THRESHOLD {
                assert_eq!(memory.add(super::super::ARRAY_ELEMS_OFFSET).addr() % 64, 0);
            }
            super::super::system_dealloc(memory, size);
        }
    }

    #[test]
    fn below_the_mapping_threshold_uses_valid_storage() {
        round_trip(THRESHOLD - 1);
    }

    #[test]
    fn the_mapping_threshold_has_a_complete_aligned_payload() {
        round_trip(THRESHOLD);
    }

    #[test]
    fn a_partial_final_page_preserves_all_requested_bytes() {
        round_trip(THRESHOLD + 137);
    }

    #[test]
    fn reservation_checks_the_prefix_and_alignment_limit() {
        let limit = super::super::MAX_ALLOCATION_SIZE & !63;
        assert_eq!(reservation_size(limit - BIAS), Some(limit));
        assert_eq!(reservation_size(limit - BIAS + 1), None);
        assert_eq!(reservation_size(usize::MAX), None);
    }

    struct Owned {
        pointer: *mut u8,
        size: usize,
    }
    // SAFETY: this wrapper transfers exclusive ownership; it exposes no concurrent
    // access and its receiver releases the allocation after reading it.
    unsafe impl Send for Owned {}

    #[test]
    fn a_mapping_can_be_released_by_another_thread() {
        let block = std::thread::spawn(|| {
            let size = THRESHOLD * 2 + 7;
            let pointer = allocate(size);
            // SAFETY: the fresh mapping is exclusively owned by this thread.
            unsafe {
                pointer.write(17);
                pointer.add(size - 1).write(29);
            }
            Owned { pointer, size }
        })
        .join()
        .unwrap();
        // SAFETY: join transferred the sole allocation owner to this thread.
        unsafe {
            assert_eq!(block.pointer.read(), 17);
            assert_eq!(block.pointer.add(block.size - 1).read(), 29);
            release(block.pointer, block.size);
        }
    }
}
