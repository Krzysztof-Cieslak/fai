//! Unit tests for the size-class recycling allocator ([`alloc_obj`]/[`free_obj`]
//! and the thread-local pool).
//!
//! These drive the raw allocator directly (a leaf descriptor, no children), so
//! `free_obj` simply reclaims a block. Every test holds the global [`lock`] (the
//! live counter and the pool's recycling are observed across calls) and asserts
//! reference-count balance; the recycling identities (a just-freed cell is the
//! next same-class allocation) hold regardless of the pool's prior state because
//! the free list is LIFO — the freed cell is the head the next pop returns.
//!
//! The live counter is debug-only, so the balance assertions are meaningful only
//! in a debug build (the default for `cargo test`).

use super::*;
use crate::tests::lock;

/// The descriptor stored in a block's header.
fn descriptor_of(p: *mut u8) -> *const Descriptor {
    // SAFETY: `p` is a live block with a descriptor at `DESC_OFFSET`.
    unsafe { read_ptr(p, DESC_OFFSET).cast::<Descriptor>() }
}

#[test]
fn size_class_indexes_by_eight_bytes() {
    // A box (32 B), a cons cell (48 B), and the pool's top class all map to their
    // exact 8-byte class; capacity (class * 8) equals the request.
    assert_eq!(size_class(HEADER_SIZE + 8), Some((HEADER_SIZE + 8) / SIZE_STEP));
    assert_eq!(size_class(32), Some(4));
    assert_eq!(size_class(48), Some(6));
    assert_eq!(size_class(MAX_POOLED_SIZE), Some(NUM_CLASSES - 1));
}

#[test]
fn size_class_excludes_oversized_allocations() {
    // Just past the cap, and well past it, are not pooled.
    assert_eq!(size_class(MAX_POOLED_SIZE + SIZE_STEP), None);
    assert_eq!(size_class(4096), None);
}

#[test]
fn free_then_alloc_same_class_recycles_the_cell() {
    let _g = lock();
    let base = live_count();
    let p = alloc_obj(48, &FAI_INT_DESC);
    // SAFETY: `p` is a fresh block; free then re-request the same class.
    unsafe { free_obj(p) };
    let q = alloc_obj(48, &FAI_INT_DESC);
    assert_eq!(q, p, "a same-class allocation reuses the just-freed cell");
    // SAFETY: `q` is live (it is `p` recycled); free it once.
    unsafe { free_obj(q) };
    assert_eq!(live_count(), base);
}

#[test]
fn free_then_alloc_different_class_does_not_recycle() {
    let _g = lock();
    let base = live_count();
    let p = alloc_obj(48, &FAI_INT_DESC); // class 6
    // SAFETY: fresh block, freed once into class 6.
    unsafe { free_obj(p) };
    let q = alloc_obj(56, &FAI_INT_DESC); // class 7 — cannot take a class-6 cell
    assert_ne!(q, p, "a different size class does not reuse the freed cell");
    // SAFETY: both `p` (still pooled in class 6) and `q` are accounted for; free
    // `q`, and drain `p` by re-popping its class.
    unsafe { free_obj(q) };
    let drained = alloc_obj(48, &FAI_INT_DESC);
    assert_eq!(drained, p, "the class-6 cell is still pooled");
    // SAFETY: `drained` is `p` recycled; free it once.
    unsafe { free_obj(drained) };
    assert_eq!(live_count(), base);
}

#[test]
fn recycled_cell_holds_its_full_requested_size() {
    let _g = lock();
    let base = live_count();
    let p = alloc_obj(48, &FAI_INT_DESC);
    // SAFETY: free then recycle the same class.
    unsafe { free_obj(p) };
    let q = alloc_obj(48, &FAI_INT_DESC);
    // The recycled cell has capacity >= 48: writing and reading its whole payload
    // (header..48) must round-trip.
    // SAFETY: `q` has at least 48 writable bytes.
    unsafe {
        for off in HEADER_SIZE..48 {
            q.add(off).write(0xAB);
        }
        for off in HEADER_SIZE..48 {
            assert_eq!(q.add(off).read(), 0xAB, "payload byte {off} did not round-trip");
        }
        free_obj(q);
    }
    assert_eq!(live_count(), base);
}

#[test]
fn recycling_overwrites_the_header() {
    let _g = lock();
    let base = live_count();
    // A boxed Float and a boxed Int are both 32 bytes (class 4): recycling a dead
    // Float cell into an Int allocation must rewrite the descriptor.
    let f = alloc_obj(HEADER_SIZE + 8, &FAI_FLOAT_DESC);
    assert!(std::ptr::eq(descriptor_of(f), &FAI_FLOAT_DESC));
    // SAFETY: free then recycle the same class.
    unsafe { free_obj(f) };
    let i = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    assert_eq!(i, f, "the dead Float cell is recycled");
    assert!(std::ptr::eq(descriptor_of(i), &FAI_INT_DESC), "the descriptor was rewritten");
    // SAFETY: `i` is live; free it once.
    unsafe { free_obj(i) };
    assert_eq!(live_count(), base);
}

#[test]
fn large_allocation_round_trips_through_the_system_allocator() {
    let _g = lock();
    let base = live_count();
    // Past the pool cap: served and reclaimed by the system allocator directly.
    let size = MAX_POOLED_SIZE + 8 * SIZE_STEP;
    assert_eq!(size_class(size), None, "this size is not pooled");
    let p = alloc_obj(size, &FAI_INT_DESC);
    // SAFETY: `p` has at least `size` writable bytes; write and verify the payload.
    unsafe {
        for off in HEADER_SIZE..size {
            p.add(off).write(0xCD);
        }
        for off in HEADER_SIZE..size {
            assert_eq!(p.add(off).read(), 0xCD, "large payload byte {off} did not round-trip");
        }
        assert_eq!(read_u64(p, SIZE_OFFSET) as usize, size, "size header preserved");
        free_obj(p);
    }
    assert_eq!(live_count(), base);
}

#[test]
fn run_ops_is_balanced_on_a_fixed_sequence() {
    let _g = lock();
    let base = live_count();
    // A hand-picked mix of allocations (high bit clear) and frees (high bit set)
    // spanning pooled and large sizes; the harness asserts its own invariants.
    run_ops(&[0x01, 0x10, 0x7f, 0x20, 0x85, 0x40, 0x03, 0xC0, 0x00, 0xFF, 0x55, 0x2a]);
    assert_eq!(live_count(), base, "run_ops leaves nothing live");
}

/// A long, deterministic alloc/free sequence from a seeded LCG, fed to [`run_ops`]
/// so the same stress runs identically every time (a regression anchor alongside
/// the proptest's random search). Each seed is its own `#[test]` so a failure
/// names the seed.
#[track_caller]
fn stress_with_seed(seed: u64) {
    let _g = lock();
    let base = live_count();
    let mut state = seed;
    let mut data = Vec::with_capacity(2000);
    for _ in 0..2000 {
        // SplitMix64-style step; take the top byte for good bit mixing.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        data.push((z >> 56) as u8);
    }
    run_ops(&data);
    assert_eq!(live_count(), base, "stress sequence freed everything");
}

#[test]
fn stress_seed_1() {
    stress_with_seed(1);
}

#[test]
fn stress_seed_0xdeadbeef() {
    stress_with_seed(0xDEAD_BEEF);
}

#[test]
fn stress_seed_max() {
    stress_with_seed(u64::MAX);
}
