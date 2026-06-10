// The runtime is the one place (with fai-codegen) permitted hand-written unsafe:
// it manipulates raw tagged values, the heap, and reference counts. Every unsafe
// block carries a `// SAFETY:` note. The crate is std-only and dependency-free so
// the AOT archive can be produced by a single `$RUSTC` invocation.
#![allow(unsafe_code)]

//! The Fai native runtime.
//!
//! Every Fai value is a single 64-bit word ([`Value`], an `i64`) using **LSB
//! pointer tagging**: an *immediate* has its low bit set (`payload << 1 | 1`);
//! a *boxed* value is an 8-aligned pointer (low bit clear). `Int` is immediate
//! when it fits 63 bits and boxed otherwise (preserving the full 64-bit range);
//! `Bool`/`Unit`/`Runtime` are immediates; `String` and closures are always
//! boxed.
//!
//! Heap objects begin with a [`Header`] (`{ rc, descriptor, size }`); the
//! descriptor identifies the object's kind (by address), from which [`fai_drop`]
//! recovers and releases the object's reference-counted children. A dead object's
//! descendants are released with an explicit worklist rather than native
//! recursion, so freeing an arbitrarily deep structure never overflows the stack.
//! [`fai_dup`]/[`fai_drop`] are tag-checked, so immediates are no-ops and
//! polymorphic code reference-counts correctly with no type information.
//!
//! Heap memory comes from a size-class recycling allocator: a freed cell is kept
//! on a per-size, thread-local free list and handed back to the next same-size
//! allocation (turning the common alloc/free into a few pointer moves), with sizes
//! above a cap falling back to the system allocator.
//!
//! Generated code inlines the common reference-count work — a tag-check, then an
//! in-place increment (dup) or a decrement and zero-test (drop) — and only calls
//! out to the runtime to actually reclaim memory: [`fai_free`] for a childless
//! leaf, or [`fai_drop_dead`] for a variable-shape cell whose children the
//! descriptor identifies. [`fai_dup`]/[`fai_drop`] remain for the first-class
//! application path and as the fallback for values of unknown (polymorphic) type.
//!
//! Functions are closures `{ header, code, arity, env_count, env… }`; every
//! application goes through [`fai_apply_n`], which matches the argument count to
//! the arity (exact call / partial-application closure / over-application).

use std::alloc::Layout;
use std::cell::Cell;
use std::sync::Mutex;
// The leak/allocation counters are the only users of these atomics, and they are
// compiled in only under `debug_assertions`, so the import is gated to match (a
// release build references no atomics here and must not carry an unused import).
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicI64, Ordering};

/// A Fai value: a tagged 64-bit word (see the crate docs).
pub type Value = i64;

// ---------------------------------------------------------------------------
// Object layout (shared with fai-codegen, which emits static strings/closures
// and reads these offsets).
// ---------------------------------------------------------------------------

/// Byte offset of the reference count in a heap object.
pub const RC_OFFSET: usize = 0;
/// Byte offset of the descriptor pointer in a heap object.
pub const DESC_OFFSET: usize = 8;
/// Byte offset of the allocation size in a heap object.
pub const SIZE_OFFSET: usize = 16;
/// Size of the object header in bytes.
pub const HEADER_SIZE: usize = 24;

/// Byte offset of a boxed `Int`'s value.
pub const INT_VALUE_OFFSET: usize = HEADER_SIZE;

/// Byte offset of a boxed `Float`'s IEEE-754 bits.
pub const FLOAT_VALUE_OFFSET: usize = HEADER_SIZE;

/// Byte offset of a data value's constructor tag.
pub const DATA_TAG_OFFSET: usize = HEADER_SIZE;
/// Byte offset of a data value's first field.
pub const DATA_FIELDS_OFFSET: usize = HEADER_SIZE + 8;

/// Byte offset of a `String`'s byte length.
pub const STRING_LEN_OFFSET: usize = HEADER_SIZE;
/// Byte offset of a `String`'s first content byte.
pub const STRING_BYTES_OFFSET: usize = HEADER_SIZE + 8;

/// Byte offset of a closure's code pointer.
pub const CLOSURE_CODE_OFFSET: usize = HEADER_SIZE;
/// Byte offset of a closure's arity.
pub const CLOSURE_ARITY_OFFSET: usize = HEADER_SIZE + 8;
/// Byte offset of a closure's captured-slot count.
pub const CLOSURE_ENV_COUNT_OFFSET: usize = HEADER_SIZE + 16;
/// Byte offset of a closure's first captured slot.
pub const CLOSURE_ENV_OFFSET: usize = HEADER_SIZE + 24;

/// Byte offset of a partial application's target function.
const PAP_FUNC_OFFSET: usize = HEADER_SIZE;
/// Byte offset of a partial application's stored-argument count.
const PAP_NARGS_OFFSET: usize = HEADER_SIZE + 8;
/// Byte offset of a partial application's first stored argument.
const PAP_ARGS_OFFSET: usize = HEADER_SIZE + 16;

/// The reference count given to statically-emitted (immortal) objects — string
/// literals and top-level function closures. So large that balanced dup/drop
/// never reaches zero, so they are never freed (they are not heap-allocated).
pub const IMMORTAL_RC: u64 = 1 << 60;

/// The canonical immediate used for `Unit` and the `Runtime` capability value
/// (payload 0, tagged). Distinct types are segregated by the type checker, so the
/// shared encoding is harmless.
pub const FAI_UNIT: Value = 1;

/// The alignment of every heap object (all fields are 64-bit).
const ALIGN: usize = 8;

// ---------------------------------------------------------------------------
// Heap header & descriptors.
// ---------------------------------------------------------------------------

/// A heap-type descriptor: a static record identifying a boxed value's kind.
/// Referenced by address from every object header (and, for static objects, from
/// generated code), so a value's kind is recovered by comparing its descriptor
/// pointer against these statics. Releasing an object's reference-counted children
/// is driven by kind in [`scan_push`] (see [`fai_drop`]).
#[repr(C)]
pub struct Descriptor {
    /// A human-readable kind name (used in leak/debug reporting).
    pub name: &'static str,
}

// SAFETY: a `Descriptor` holds only a `&'static str`, which is `Sync`; it carries
// no interior mutability.
unsafe impl Sync for Descriptor {}

/// Descriptor for `String` objects (leaf: inline bytes, no children).
#[unsafe(no_mangle)]
pub static FAI_STRING_DESC: Descriptor = Descriptor { name: "String" };

/// Descriptor for boxed (overflowed) `Int` objects (leaf).
#[unsafe(no_mangle)]
pub static FAI_INT_DESC: Descriptor = Descriptor { name: "Int" };

/// Descriptor for closures (children: the captured environment slots).
#[unsafe(no_mangle)]
pub static FAI_CLOSURE_DESC: Descriptor = Descriptor { name: "Closure" };

/// Descriptor for partial applications (children: the target plus stored args).
#[unsafe(no_mangle)]
pub static FAI_PAP_DESC: Descriptor = Descriptor { name: "Pap" };

/// Descriptor for boxed `Float` objects (leaf).
#[unsafe(no_mangle)]
pub static FAI_FLOAT_DESC: Descriptor = Descriptor { name: "Float" };

/// Descriptor for data values — constructors, records, and tuples (children: all
/// fields). A single descriptor serves every shape; the field count is derived
/// from the object's size.
#[unsafe(no_mangle)]
pub static FAI_DATA_DESC: Descriptor = Descriptor { name: "Data" };

// ---------------------------------------------------------------------------
// Tagging helpers.
// ---------------------------------------------------------------------------

/// Whether `v` is a boxed (heap) value rather than an immediate.
#[inline]
fn is_boxed(v: Value) -> bool {
    v & 1 == 0
}

/// Reinterprets a boxed value as a raw object pointer.
#[inline]
fn as_obj(v: Value) -> *mut u8 {
    debug_assert!(is_boxed(v));
    v as usize as *mut u8
}

/// Tags an object pointer as a boxed value.
#[inline]
fn from_obj(p: *mut u8) -> Value {
    p as usize as Value
}

/// Encodes a 63-bit-or-smaller integer as an immediate (`n << 1 | 1`).
#[inline]
fn imm_int(n: i64) -> Value {
    (n << 1) | 1
}

/// Whether `n` fits the immediate (63-bit signed) range.
#[inline]
fn fits_immediate(n: i64) -> bool {
    ((n << 1) >> 1) == n
}

// ---------------------------------------------------------------------------
// Raw field access.
// ---------------------------------------------------------------------------

#[inline]
unsafe fn read_u64(obj: *const u8, off: usize) -> u64 {
    // SAFETY: callers pass a valid object pointer and an in-bounds offset.
    unsafe { obj.add(off).cast::<u64>().read() }
}

#[inline]
unsafe fn read_i64(obj: *const u8, off: usize) -> i64 {
    // SAFETY: as `read_u64`.
    unsafe { obj.add(off).cast::<i64>().read() }
}

#[inline]
unsafe fn read_ptr(obj: *const u8, off: usize) -> *const u8 {
    // SAFETY: as `read_u64`.
    unsafe { obj.add(off).cast::<*const u8>().read() }
}

#[inline]
unsafe fn write_u64(obj: *mut u8, off: usize, val: u64) {
    // SAFETY: callers pass a valid, writable object pointer and in-bounds offset.
    unsafe { obj.add(off).cast::<u64>().write(val) }
}

#[inline]
unsafe fn write_i64(obj: *mut u8, off: usize, val: i64) {
    // SAFETY: as `write_u64`.
    unsafe { obj.add(off).cast::<i64>().write(val) }
}

#[inline]
unsafe fn write_ptr(obj: *mut u8, off: usize, val: *const u8) {
    // SAFETY: as `write_u64`.
    unsafe { obj.add(off).cast::<*const u8>().write(val) }
}

// ---------------------------------------------------------------------------
// Allocation & the live-object counter.
// ---------------------------------------------------------------------------
//
// The live-object and cumulative-allocation counters exist only to detect leaks
// (the end-of-run check in `run_entry`) and to make reuse observable in tests, so
// they are compiled in only under `debug_assertions` — a release build pays none
// of the per-alloc/free atomics. With the counters absent, `live_count` and
// `allocations` report zero and the leak check is a no-op. (`debug_assertions` is
// on for `cargo test`/dev and off for release/bench; an optimized build can opt
// the counters back in with `[profile.release] debug-assertions = true`.)

/// The number of heap objects currently allocated (debug leak detection).
#[cfg(debug_assertions)]
static LIVE: AtomicI64 = AtomicI64::new(0);

/// The cumulative number of heap allocations since the last reset. Unlike [`LIVE`]
/// it never decreases, so reuse (which writes in place rather than allocating) is
/// observable as allocations that did *not* happen.
#[cfg(debug_assertions)]
static ALLOCATIONS: AtomicI64 = AtomicI64::new(0);

/// Records one heap allocation in the debug counters. Compiled to nothing in a
/// release build (the counters are absent there).
#[inline(always)]
fn note_alloc() {
    #[cfg(debug_assertions)]
    {
        LIVE.fetch_add(1, Ordering::Relaxed);
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Records one heap free in the debug counters. Compiled to nothing in a release
/// build (the counters are absent there).
#[inline(always)]
fn note_free() {
    #[cfg(debug_assertions)]
    LIVE.fetch_sub(1, Ordering::Relaxed);
}

/// Returns the number of live heap objects (used by the leak check and tests).
/// Always zero in a release build, where the counter is compiled out.
#[must_use]
pub fn live_count() -> i64 {
    #[cfg(debug_assertions)]
    {
        LIVE.load(Ordering::Relaxed)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

/// Returns the cumulative allocation count (used by reuse benchmarks and tests).
/// Always zero in a release build, where the counter is compiled out.
#[must_use]
pub fn allocations() -> i64 {
    #[cfg(debug_assertions)]
    {
        ALLOCATIONS.load(Ordering::Relaxed)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

/// Resets the cumulative allocation counter (tests/benchmarks). A no-op in a
/// release build, where the counter is compiled out.
pub fn reset_allocations() {
    #[cfg(debug_assertions)]
    ALLOCATIONS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Size-class recycling allocator.
// ---------------------------------------------------------------------------
//
// Most Fai objects are small fixed sizes (a cons cell 48 B, an `Int`/`Float` box
// 32 B), allocated and freed in bursts, so going to the system allocator for each
// one dominates allocation cost. Instead, a freed cell is kept on a per-size
// free list and handed back to the next same-size allocation, turning the common
// alloc/free into a few non-atomic pointer moves. Sizes above `MAX_POOLED_SIZE`
// (rare — large strings, wide records) bypass the pool and use the system
// allocator directly.
//
// The free lists are **thread-local** and need no synchronization: Fai execution
// is single-threaded, and a cell is always allocated and freed on the same thread,
// so a list is only ever touched by its owning thread. The list is **intrusive** —
// a dead cell's own first word holds the next-free pointer — so pooling allocates
// nothing itself. Sizes are exact 8-byte classes (capacity equals the request for
// the 8-multiple sizes the runtime emits), so recycling carries no internal
// fragmentation. Blocks are recycled until the thread exits, when [`Pool`]'s drop
// returns them to the system allocator.

/// The byte granularity of a size class. Every heap object is 8-aligned and a
/// multiple of 8 bytes, so each distinct size is its own class.
const SIZE_STEP: usize = ALIGN;

/// The largest object served from the recycling pool; larger allocations go
/// straight to the system allocator. Covers the small objects that dominate
/// allocation traffic (boxes, cons cells, typical records/closures/PAPs).
const MAX_POOLED_SIZE: usize = 512;

/// The number of size-class free lists. Class `c` (`= size.div_ceil(8)`) holds
/// cells of capacity `c * SIZE_STEP`; the low classes below the minimum object
/// size are simply never used.
const NUM_CLASSES: usize = MAX_POOLED_SIZE / SIZE_STEP + 1;

/// The size class for an allocation of `size` bytes, or `None` if it exceeds
/// `MAX_POOLED_SIZE` (and so is not pooled). Class `c` serves any request with
/// `(c - 1) * 8 < size <= c * 8`; its cells have capacity `c * 8 >= size`, so a
/// recycled cell always fits. A cell's class is therefore stable across reuse,
/// which keeps its deallocation layout recoverable.
#[inline]
fn size_class(size: usize) -> Option<usize> {
    if size == 0 || size > MAX_POOLED_SIZE { None } else { Some(size.div_ceil(SIZE_STEP)) }
}

/// Per-thread size-class free lists. `heads[c]` is the most-recently-freed cell
/// of class `c` (an intrusive singly-linked stack threaded through each dead
/// cell's first word), or null when the class is empty.
struct Pool {
    heads: [Cell<*mut u8>; NUM_CLASSES],
}

impl Pool {
    const fn new() -> Self {
        Pool { heads: [const { Cell::new(std::ptr::null_mut()) }; NUM_CLASSES] }
    }
}

impl Drop for Pool {
    /// Returns every pooled block to the system allocator when the owning thread
    /// exits (so a thread's recycled memory is not stranded for the process
    /// lifetime). A class-`c` block was allocated with capacity `c * SIZE_STEP`.
    fn drop(&mut self) {
        for (class, head) in self.heads.iter().enumerate() {
            let cap = class * SIZE_STEP;
            if cap == 0 {
                continue;
            }
            let Ok(layout) = Layout::from_size_align(cap, ALIGN) else { continue };
            let mut p = head.get();
            while !p.is_null() {
                // SAFETY: `p` is a pooled class-`class` block; its first word holds
                // the next-free pointer, and it was allocated with `layout`.
                unsafe {
                    let next = p.cast::<*mut u8>().read();
                    std::alloc::dealloc(p, layout);
                    p = next;
                }
            }
        }
    }
}

thread_local! {
    static POOL: Pool = const { Pool::new() };
}

/// Pushes a dead cell `p` of class `c` onto its free list, storing the previous
/// head in `p`'s first word (the now-unused reference-count slot).
///
/// # Safety
/// `p` is a dead object's memory of class `c` (capacity `c * SIZE_STEP`, at least
/// `SIZE_STEP` bytes); it must not be used as an object again until popped.
unsafe fn pool_push(c: usize, p: *mut u8) {
    POOL.with(|pool| {
        let head = pool.heads[c].get();
        // SAFETY: `p` has room for one pointer at offset 0.
        unsafe { p.cast::<*mut u8>().write(head) };
        pool.heads[c].set(p);
    });
}

/// Pops a recycled cell of class `c`, or null if the free list is empty.
fn pool_pop(c: usize) -> *mut u8 {
    POOL.with(|pool| {
        let head = pool.heads[c].get();
        if head.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `head` is a pooled block; its first word is the next-free pointer.
        let next = unsafe { head.cast::<*mut u8>().read() };
        pool.heads[c].set(next);
        head
    })
}

/// Allocates `size` bytes (8-aligned) from the system allocator, aborting on OOM.
fn system_alloc(size: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, ALIGN).expect("valid layout");
    // SAFETY: `layout` has nonzero size (a class capacity or a large request) and
    // valid alignment.
    let p = unsafe { std::alloc::alloc(layout) };
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    p
}

/// Returns a (large, unpooled) block of `size` bytes to the system allocator.
///
/// # Safety
/// `p` was returned by `system_alloc(size)` with the same size and alignment.
unsafe fn system_dealloc(p: *mut u8, size: usize) {
    let layout = Layout::from_size_align(size, ALIGN).expect("valid layout");
    // SAFETY: `p`/`layout` match the original system allocation.
    unsafe { std::alloc::dealloc(p, layout) };
}

/// Allocates an object of `size` bytes with `rc = 1` and `descriptor`, returning
/// its pointer. Recycles a same-size cell from the thread-local pool when one is
/// available, otherwise takes a fresh block from the system allocator (at the
/// class capacity, so every cell of a class is interchangeable). The contents
/// past the header are left uninitialized — every caller writes all of an object's
/// fields. Increments the live counter.
fn alloc_obj(size: usize, descriptor: *const Descriptor) -> *mut u8 {
    debug_assert!(size >= HEADER_SIZE, "object smaller than its header");
    let p = match size_class(size) {
        Some(c) => {
            let recycled = pool_pop(c);
            if recycled.is_null() { system_alloc(c * SIZE_STEP) } else { recycled }
        }
        None => system_alloc(size),
    };
    // SAFETY: `p` points to at least `size` writable bytes — the class capacity
    // (>= size) for a pooled cell, or exactly `size` for the large path. The
    // header overwrite repurposes any recycled cell (descriptor, size, and the
    // intrusive next-pointer that occupied the rc slot are all replaced).
    unsafe {
        write_u64(p, RC_OFFSET, 1);
        write_ptr(p, DESC_OFFSET, descriptor.cast());
        write_u64(p, SIZE_OFFSET, size as u64);
    }
    note_alloc();
    p
}

/// Frees an object's backing memory (no child scan) and decrements the live
/// counter. A pooled-size cell is returned to its thread-local free list for
/// reuse; a larger block is returned to the system allocator.
///
/// # Safety
/// `p` was returned by [`alloc_obj`] and is dead (its reference count is zero and
/// its children, if any, have been released); it must not be used afterward.
unsafe fn free_obj(p: *mut u8) {
    // SAFETY: `p` was returned by `alloc_obj`, so the size field is valid.
    let size = unsafe { read_u64(p, SIZE_OFFSET) } as usize;
    match size_class(size) {
        // SAFETY: `p` is a dead class-`c` cell; pooling repurposes its memory.
        Some(c) => unsafe { pool_push(c, p) },
        // SAFETY: a large block was system-allocated with this exact size.
        None => unsafe { system_dealloc(p, size) },
    }
    note_free();
}

/// Aborts the process with a runtime error message (only reached on conditions a
/// well-typed program cannot produce, e.g. applying a non-function).
fn fai_panic(msg: &str) -> ! {
    eprintln!("fai runtime error: {msg}");
    std::process::abort()
}

// ---------------------------------------------------------------------------
// Reference counting.
// ---------------------------------------------------------------------------

/// Increments a value's reference count, returning it. No-op for immediates.
#[unsafe(no_mangle)]
pub extern "C" fn fai_dup(v: Value) -> Value {
    if is_boxed(v) {
        let p = as_obj(v);
        // SAFETY: `p` is a live object pointer.
        unsafe {
            let rc = read_u64(p, RC_OFFSET);
            write_u64(p, RC_OFFSET, rc + 1);
        }
    }
    v
}

/// Decrements a value's reference count, releasing it (and its children) at
/// zero. No-op for immediates.
///
/// Releasing a dead object walks its descendants with an explicit [`DropWork`]
/// worklist rather than native recursion, so dropping an arbitrarily deep
/// structure (e.g. a long list) never overflows the native stack. The common
/// case — decrementing a still-shared value — touches no worklist.
#[unsafe(no_mangle)]
pub extern "C" fn fai_drop(v: Value) {
    if !is_boxed(v) {
        return;
    }
    let p = as_obj(v);
    // SAFETY: `p` is a live object pointer.
    unsafe {
        let rc = read_u64(p, RC_OFFSET) - 1;
        write_u64(p, RC_OFFSET, rc);
        if rc == 0 {
            // Dead: release its reference-counted children and reclaim its memory.
            release_dead(p);
        }
    }
}

/// Releases the reference-counted children of a dead object `p` and reclaims its
/// memory. Shared by [`fai_drop`]'s dead branch and [`fai_drop_dead`]. The
/// children (and their descendants) are drained iteratively with an explicit
/// worklist, so freeing an arbitrarily deep structure never overflows the native
/// stack. The child pointers are gathered (into the worklist) before `p` is
/// freed; the heap is acyclic, so a child release can never reach `p`.
///
/// # Safety
/// `p` is a live object pointer whose reference count has reached zero.
unsafe fn release_dead(p: *mut u8) {
    // SAFETY: `p` is a dead live object; its descriptor and fields are in bounds,
    // and `free_obj` matches the original allocation.
    unsafe {
        let mut work = DropWork::new();
        scan_push(p, &mut work);
        free_obj(p);
        drain(&mut work);
    }
}

/// Releases a dead object's reference-counted children and reclaims its memory —
/// the out-of-line dead path generated code calls once its inlined decrement has
/// driven the reference count to zero. The variable-shape counterpart of
/// [`fai_free`] (which assumes the object has no reference-counted children): the
/// children to release are recovered from the object's descriptor, and every
/// descendant that reaches zero is freed too, iteratively.
///
/// # Safety
/// `v` must be a boxed object whose reference count has already reached zero (as
/// the inlined drop guarantees); it must not be used afterward.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_drop_dead(v: Value) {
    // SAFETY: the caller guarantees `v` is a boxed, dead object, so its descriptor
    // and size header are valid and its memory matches the original allocation.
    unsafe { release_dead(as_obj(v)) };
}

/// A drop worklist: boxed values whose reference count is still to be decremented.
/// A fixed inline buffer keeps shallow drops allocation-free; deeper structures
/// spill onto the heap. Either way the walk is iterative, so no structure
/// overflows the native stack when it is freed.
struct DropWork {
    inline: [Value; DROP_INLINE],
    len: usize,
    spill: Vec<Value>,
}

/// The inline worklist capacity before spilling to the heap.
const DROP_INLINE: usize = 32;

impl DropWork {
    fn new() -> Self {
        Self { inline: [0; DROP_INLINE], len: 0, spill: Vec::new() }
    }

    fn push(&mut self, v: Value) {
        if self.len < DROP_INLINE {
            self.inline[self.len] = v;
            self.len += 1;
        } else {
            self.spill.push(v);
        }
    }

    fn pop(&mut self) -> Option<Value> {
        if let Some(v) = self.spill.pop() {
            return Some(v);
        }
        if self.len > 0 {
            self.len -= 1;
            return Some(self.inline[self.len]);
        }
        None
    }
}

/// Pushes the boxed, reference-counted children of the live object `p` onto
/// `work` (immediates carry no count and are skipped). The child layout is
/// recovered from the object's kind, identified by its descriptor address.
///
/// # Safety
/// `p` is a live object pointer.
unsafe fn scan_push(p: *mut u8, work: &mut DropWork) {
    // SAFETY: `p` is a live object; its descriptor and fields are in bounds.
    unsafe {
        let desc = read_ptr(p, DESC_OFFSET).cast::<Descriptor>();
        if std::ptr::eq(desc, &FAI_DATA_DESC) {
            let size = read_u64(p, SIZE_OFFSET) as usize;
            let nfields = (size - DATA_FIELDS_OFFSET) / 8;
            for i in 0..nfields {
                let field = read_i64(p, DATA_FIELDS_OFFSET + i * 8);
                if is_boxed(field) {
                    work.push(field);
                }
            }
        } else if std::ptr::eq(desc, &FAI_CLOSURE_DESC) {
            let env_count = read_u64(p, CLOSURE_ENV_COUNT_OFFSET) as usize;
            for i in 0..env_count {
                let slot = read_i64(p, CLOSURE_ENV_OFFSET + i * 8);
                if is_boxed(slot) {
                    work.push(slot);
                }
            }
        } else if std::ptr::eq(desc, &FAI_PAP_DESC) {
            let func = read_i64(p, PAP_FUNC_OFFSET);
            if is_boxed(func) {
                work.push(func);
            }
            let nargs = read_u64(p, PAP_NARGS_OFFSET) as usize;
            for i in 0..nargs {
                let arg = read_i64(p, PAP_ARGS_OFFSET + i * 8);
                if is_boxed(arg) {
                    work.push(arg);
                }
            }
        }
        // Leaf kinds (`String`, boxed `Int`/`Float`) have no children.
    }
}

/// Drains a drop worklist: decrement each queued value, freeing (and enqueuing
/// the children of) any that reaches zero. Purely iterative.
fn drain(work: &mut DropWork) {
    while let Some(w) = work.pop() {
        // Only boxed values are ever pushed.
        let q = as_obj(w);
        // SAFETY: `q` is a live object pointer.
        unsafe {
            let rc = read_u64(q, RC_OFFSET) - 1;
            write_u64(q, RC_OFFSET, rc);
            if rc == 0 {
                scan_push(q, work);
                free_obj(q);
            }
        }
    }
}

/// Reclaims a dead cell's memory directly — the deallocator the inlined drop
/// (generated code's specialized release of a known monomorphic data cell) calls
/// once it has decremented the cell to zero and released its reference-counted
/// children. Reclaims the backing memory and decrements the live-object counter.
///
/// # Safety
/// `v` must be a boxed object whose reference count has reached zero and whose
/// reference-counted children have already been released (as the inlined drop
/// guarantees); it must not be used afterward.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_free(v: Value) {
    // SAFETY: the caller guarantees `v` is a boxed, dead, child-released object,
    // so its size header is valid and its memory matches the original allocation.
    unsafe { free_obj(as_obj(v)) };
}

// ---------------------------------------------------------------------------
// Data values (constructors, records, tuples).
// ---------------------------------------------------------------------------

/// Allocates a data value `{ tag, fields… }` (rc = 1), copying `nfields` owned
/// values from `fields` (ownership transfers in). Nullary constructors never
/// reach here — codegen represents them as tagged immediates.
///
/// # Safety
/// `fields` must point to `nfields` owned values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_make_data(tag: i64, nfields: i64, fields: *const i64) -> Value {
    let n = nfields as usize;
    let size = DATA_FIELDS_OFFSET + n * 8;
    let p = alloc_obj(size, &FAI_DATA_DESC);
    // SAFETY: `p` has room for the tag and `n` fields; `fields` points to `n`.
    unsafe {
        write_u64(p, DATA_TAG_OFFSET, tag as u64);
        for i in 0..n {
            write_i64(p, DATA_FIELDS_OFFSET + i * 8, *fields.add(i));
        }
    }
    from_obj(p)
}

/// The number of fields in a boxed data value.
unsafe fn data_field_count(v: Value) -> usize {
    // SAFETY: `v` is a boxed data value.
    let size = unsafe { read_u64(as_obj(v), SIZE_OFFSET) } as usize;
    (size - DATA_FIELDS_OFFSET) / 8
}

/// Reads a data value's constructor tag (**borrowing** `v`), as an immediate
/// `Int`. A nullary constructor is an immediate whose payload is its tag. The
/// base is not released here; its owner drops it once at its last use.
#[unsafe(no_mangle)]
pub extern "C" fn fai_data_tag(v: Value) -> Value {
    let tag = if is_boxed(v) {
        // SAFETY: a boxed data value stores its tag at `DATA_TAG_OFFSET`.
        unsafe { read_u64(as_obj(v), DATA_TAG_OFFSET) as i64 }
    } else {
        v >> 1
    };
    imm_int(tag)
}

/// Projects field `index` of a data value, returning an owned reference to it and
/// **borrowing** `v` (the base is not released here — its owner drops it once at
/// its last use; the projected field is duplicated so it outlives that drop).
#[unsafe(no_mangle)]
pub extern "C" fn fai_data_field(v: Value, index: i64) -> Value {
    // SAFETY: `v` is a boxed data value with at least `index + 1` fields.
    let field = unsafe { read_i64(as_obj(v), DATA_FIELDS_OFFSET + index as usize * 8) };
    fai_dup(field);
    field
}

/// Row-polymorphic record update with the field at `index` (an immediate `Int`
/// slot) replaced by `value`. When `record` is the unique owner, the field is
/// overwritten **in place** (no allocation, no copying); otherwise a fresh copy is
/// built. Consumes `record` and `value`; the replaced field is released.
#[unsafe(no_mangle)]
pub extern "C" fn fai_record_update(record: Value, index: Value, value: Value) -> Value {
    let slot = unbox_int(index) as usize;
    // SAFETY: `record` is a boxed data value; `slot` is a valid field index.
    unsafe {
        let p = as_obj(record);
        // Unique owner: overwrite the field in place, releasing the old one.
        if read_u64(p, RC_OFFSET) == 1 {
            let old = read_i64(p, DATA_FIELDS_OFFSET + slot * 8);
            write_i64(p, DATA_FIELDS_OFFSET + slot * 8, value);
            fai_drop(old);
            return record;
        }
        // Shared: copy the record with the field replaced.
        let tag = read_u64(p, DATA_TAG_OFFSET) as i64;
        let n = data_field_count(record);
        let size = DATA_FIELDS_OFFSET + n * 8;
        let q = alloc_obj(size, &FAI_DATA_DESC);
        write_u64(q, DATA_TAG_OFFSET, tag as u64);
        for i in 0..n {
            if i == slot {
                write_i64(q, DATA_FIELDS_OFFSET + i * 8, value);
            } else {
                let field = read_i64(p, DATA_FIELDS_OFFSET + i * 8);
                fai_dup(field);
                write_i64(q, DATA_FIELDS_OFFSET + i * 8, field);
            }
        }
        // Release this reference; dropping it releases the copied-out fields once
        // (balancing the dups) and the replaced field once.
        fai_drop(record);
        from_obj(q)
    }
}

// ---------------------------------------------------------------------------
// Reuse: reset a dead cell to a token, then build into it in place.
// ---------------------------------------------------------------------------

/// The null reuse token (a boxed-tagged zero; real object pointers are never
/// null), meaning "no cell to reuse — allocate fresh."
const NO_REUSE: Value = 0;

/// Releases `v` for reuse. If `v` is the unique owner of a boxed object, releases
/// its reference-counted children (iteratively, like [`fai_drop`]) and returns the
/// object's raw memory as a reuse token **without freeing or untracking it**;
/// otherwise (shared, or an immediate) decrements as a normal drop would and
/// returns the null token. Consumes one reference of `v`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_drop_reuse(v: Value) -> Value {
    if !is_boxed(v) {
        return NO_REUSE;
    }
    let p = as_obj(v);
    // SAFETY: `p` is a live object pointer.
    unsafe {
        let rc = read_u64(p, RC_OFFSET) - 1;
        write_u64(p, RC_OFFSET, rc);
        if rc == 0 {
            // Release the children but keep `p`'s memory live (no `free_obj`);
            // `fai_reuse` rebuilds into it.
            let mut work = DropWork::new();
            scan_push(p, &mut work);
            drain(&mut work);
            return from_obj(p);
        }
    }
    NO_REUSE
}

/// Builds a data value `{ tag, fields… }`, reusing `token`'s memory in place when
/// it is a non-null token of exactly the right size, otherwise allocating a fresh
/// object (and freeing a wrong-sized token). Ownership of the `fields` transfers
/// in.
///
/// # Safety
/// `fields` must point to `nfields` owned values; `token` is `0` or a token
/// returned by [`fai_drop_reuse`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_reuse(
    token: Value,
    tag: i64,
    nfields: i64,
    fields: *const i64,
) -> Value {
    let n = nfields as usize;
    let size = DATA_FIELDS_OFFSET + n * 8;
    if token != NO_REUSE {
        let p = token as usize as *mut u8;
        // SAFETY: `token` is a reset object's memory; its size header is valid.
        let cell_size = unsafe { read_u64(p, SIZE_OFFSET) } as usize;
        if cell_size == size {
            // SAFETY: `p` has exactly room for the header, tag, and `n` fields;
            // its children were already released by `fai_drop_reuse`.
            unsafe {
                write_u64(p, RC_OFFSET, 1);
                write_ptr(p, DESC_OFFSET, std::ptr::addr_of!(FAI_DATA_DESC).cast());
                write_u64(p, DATA_TAG_OFFSET, tag as u64);
                for i in 0..n {
                    write_i64(p, DATA_FIELDS_OFFSET + i * 8, *fields.add(i));
                }
            }
            return from_obj(p);
        }
        // Wrong size: the token's children are gone, so just reclaim its memory.
        // SAFETY: `p` is a reset object's memory.
        unsafe { free_obj(p) };
    }
    // SAFETY: `fields` points to `n` owned values.
    unsafe { fai_make_data(tag, nfields, fields) }
}

// ---------------------------------------------------------------------------
// Integers.
// ---------------------------------------------------------------------------

/// Boxes an `i64` as a Fai `Int` value: immediate when it fits 63 bits, a heap
/// object otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn fai_box_int(n: i64) -> Value {
    if fits_immediate(n) {
        return imm_int(n);
    }
    let p = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    // SAFETY: `p` has room for the value field.
    unsafe { write_i64(p, INT_VALUE_OFFSET, n) };
    from_obj(p)
}

/// Reads an `Int` value (immediate or boxed) as an `i64`.
fn unbox_int(v: Value) -> i64 {
    if is_boxed(v) {
        // SAFETY: a boxed `Int` has its value at `INT_VALUE_OFFSET`.
        unsafe { read_i64(as_obj(v), INT_VALUE_OFFSET) }
    } else {
        v >> 1
    }
}

macro_rules! int_binop {
    ($name:ident, $op:expr) => {
        /// Integer arithmetic primitive (operands consumed).
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(a: Value, b: Value) -> Value {
            let f: fn(i64, i64) -> i64 = $op;
            let r = f(unbox_int(a), unbox_int(b));
            fai_drop(a);
            fai_drop(b);
            fai_box_int(r)
        }
    };
}

int_binop!(fai_int_add, |a, b| a.wrapping_add(b));
int_binop!(fai_int_sub, |a, b| a.wrapping_sub(b));
int_binop!(fai_int_mul, |a, b| a.wrapping_mul(b));

/// Integer division (operands consumed); aborts on division by zero.
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_div(a: Value, b: Value) -> Value {
    let d = unbox_int(b);
    if d == 0 {
        fai_panic("integer division by zero");
    }
    let r = unbox_int(a).wrapping_div(d);
    fai_drop(a);
    fai_drop(b);
    fai_box_int(r)
}

/// Integer remainder (operands consumed); aborts on division by zero.
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_rem(a: Value, b: Value) -> Value {
    let d = unbox_int(b);
    if d == 0 {
        fai_panic("integer remainder by zero");
    }
    let r = unbox_int(a).wrapping_rem(d);
    fai_drop(a);
    fai_drop(b);
    fai_box_int(r)
}

// Bitwise integer primitives (operands consumed). Shifts mask the amount to
// `0..63` so they are always well-defined; `fai_int_shr` is arithmetic
// (sign-extending) and `fai_int_shr_logical` is logical (zero-filling).
int_binop!(fai_int_and, |a, b| a & b);
int_binop!(fai_int_or, |a, b| a | b);
int_binop!(fai_int_xor, |a, b| a ^ b);
int_binop!(fai_int_shl, |a, b| ((a as u64) << ((b & 63) as u32)) as i64);
int_binop!(fai_int_shr, |a, b| a >> ((b & 63) as u32));
int_binop!(fai_int_shr_logical, |a, b| ((a as u64) >> ((b & 63) as u32)) as i64);

/// Bitwise complement (operand consumed): `complement 0 = -1`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_complement(a: Value) -> Value {
    let r = !unbox_int(a);
    fai_drop(a);
    fai_box_int(r)
}

/// Encodes a Rust `bool` as a Fai `Bool` immediate.
#[inline]
fn from_bool(b: bool) -> Value {
    imm_int(i64::from(b))
}

/// Boolean negation (operand consumed). `not true = false`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_not(b: Value) -> Value {
    let r = b == imm_int(0);
    fai_drop(b);
    from_bool(r)
}

macro_rules! int_cmp {
    ($name:ident, $op:tt) => {
        /// Integer comparison primitive, returning a `Bool` (operands consumed).
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(a: Value, b: Value) -> Value {
            let r = unbox_int(a) $op unbox_int(b);
            fai_drop(a);
            fai_drop(b);
            from_bool(r)
        }
    };
}

int_cmp!(fai_int_lt, <);
int_cmp!(fai_int_le, <=);
int_cmp!(fai_int_gt, >);
int_cmp!(fai_int_ge, >=);

// ---------------------------------------------------------------------------
// Floats (always boxed, since immediates are reserved for `Int`/`Bool`/`Unit`).
// ---------------------------------------------------------------------------

/// Boxes an `f64` (given by its IEEE-754 bit pattern) as a Fai `Float` value.
#[unsafe(no_mangle)]
pub extern "C" fn fai_box_float(bits: i64) -> Value {
    let p = alloc_obj(HEADER_SIZE + 8, &FAI_FLOAT_DESC);
    // SAFETY: `p` has room for the value field.
    unsafe { write_i64(p, FLOAT_VALUE_OFFSET, bits) };
    from_obj(p)
}

/// Reads a boxed `Float`'s value.
fn unbox_float(v: Value) -> f64 {
    // SAFETY: `v` is a boxed `Float`.
    unsafe { f64::from_bits(read_u64(as_obj(v), FLOAT_VALUE_OFFSET)) }
}

macro_rules! float_binop {
    ($name:ident, $op:expr) => {
        /// Float arithmetic primitive (operands consumed).
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(a: Value, b: Value) -> Value {
            let f: fn(f64, f64) -> f64 = $op;
            let r = f(unbox_float(a), unbox_float(b));
            fai_drop(a);
            fai_drop(b);
            fai_box_float(r.to_bits() as i64)
        }
    };
}

float_binop!(fai_float_add, |a, b| a + b);
float_binop!(fai_float_sub, |a, b| a - b);
float_binop!(fai_float_mul, |a, b| a * b);
float_binop!(fai_float_div, |a, b| a / b);

macro_rules! float_cmp {
    ($name:ident, $op:tt) => {
        /// Float comparison primitive, returning a `Bool` (operands consumed).
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(a: Value, b: Value) -> Value {
            let r = unbox_float(a) $op unbox_float(b);
            fai_drop(a);
            fai_drop(b);
            from_bool(r)
        }
    };
}

float_cmp!(fai_float_lt, <);
float_cmp!(fai_float_le, <=);
float_cmp!(fai_float_gt, >);
float_cmp!(fai_float_ge, >=);

/// Square root (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqrt(v: Value) -> Value {
    let r = unbox_float(v).sqrt();
    fai_drop(v);
    fai_box_float(r.to_bits() as i64)
}

/// Reinterprets an `Int`'s 64-bit pattern as a `Float` (operand consumed). The
/// inverse of [`fai_float_to_bits`]; lets any bit pattern (incl. NaN/inf) be
/// produced, which value generators rely on.
#[unsafe(no_mangle)]
pub extern "C" fn fai_float_from_bits(bits: Value) -> Value {
    let r = unbox_int(bits);
    fai_drop(bits);
    fai_box_float(r)
}

/// Reinterprets a `Float`'s IEEE-754 bits as an `Int` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_float_to_bits(v: Value) -> Value {
    let r = unbox_float(v).to_bits() as i64;
    fai_drop(v);
    fai_box_int(r)
}

/// Converts an `Int` to a `Float` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_to_float(n: Value) -> Value {
    #[allow(clippy::cast_precision_loss)]
    let r = unbox_int(n) as f64;
    fai_drop(n);
    fai_box_float(r.to_bits() as i64)
}

/// Converts a `Float` to an `Int` by truncation (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_float_to_int(f: Value) -> Value {
    #[allow(clippy::cast_possible_truncation)]
    let r = unbox_float(f) as i64;
    fai_drop(f);
    fai_box_int(r)
}

/// Renders a `Float` as a `String` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_float_to_string(f: Value) -> Value {
    let result = make_string(format!("{:?}", unbox_float(f)).as_bytes());
    fai_drop(f);
    result
}

/// Compares two unboxed `Float`s given by their IEEE-754 bit patterns, returning
/// an immediate `Int` `-1`/`0`/`1` by the IEEE-754 total order (matching the
/// structural [`fai_compare`] on boxed floats). Allocates nothing; used by the
/// inlined structural ordering of scalar unboxed floats.
#[unsafe(no_mangle)]
pub extern "C" fn fai_float_compare_bits(a: i64, b: i64) -> Value {
    let ord = f64::from_bits(a as u64).total_cmp(&f64::from_bits(b as u64));
    imm_int(match ord {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

// ---------------------------------------------------------------------------
// Chars. A Char is an immediate Unicode scalar value, encoded exactly like an
// Int (`code << 1 | 1`), so the Char/Int conversions are typed bitcasts.
// ---------------------------------------------------------------------------

/// Renders a `Char` as a one-character `String` (operand consumed; a Char is an
/// immediate, so there is nothing to release).
#[unsafe(no_mangle)]
pub extern "C" fn fai_char_to_string(c: Value) -> Value {
    let ch = char::from_u32((c >> 1) as u32).unwrap_or('\u{FFFD}');
    make_string(ch.encode_utf8(&mut [0u8; 4]).as_bytes())
}

/// A `Char`'s Unicode scalar value as an `Int`. Char and Int share the immediate
/// encoding, so this is the identity (the bits are already an `Int` immediate).
#[unsafe(no_mangle)]
pub extern "C" fn fai_char_to_code(c: Value) -> Value {
    debug_assert!(!is_boxed(c), "a Char is always an immediate");
    c
}

/// An `Int` code point as a `Char`. The caller guarantees a valid scalar value
/// (via `isValidCharCode`), which always fits the immediate, so this is the
/// identity (the bits are already a `Char` immediate).
#[unsafe(no_mangle)]
pub extern "C" fn fai_char_from_code(n: Value) -> Value {
    debug_assert!(!is_boxed(n), "a valid code point is always an immediate");
    n
}

/// Whether an `Int` is a Unicode scalar value (in range and not a surrogate),
/// returning a `Bool` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_is_valid_char_code(n: Value) -> Value {
    let code = unbox_int(n);
    let valid = u32::try_from(code).ok().and_then(char::from_u32).is_some();
    fai_drop(n);
    from_bool(valid)
}

// ---------------------------------------------------------------------------
// Strings.
// ---------------------------------------------------------------------------

/// Allocates a `String` object from `bytes` (rc = 1).
fn make_string(bytes: &[u8]) -> Value {
    let len = bytes.len();
    let size = (STRING_BYTES_OFFSET + len + ALIGN - 1) & !(ALIGN - 1);
    let p = alloc_obj(size.max(STRING_BYTES_OFFSET), &FAI_STRING_DESC);
    // SAFETY: `p` has room for the length field and `len` content bytes.
    unsafe {
        write_u64(p, STRING_LEN_OFFSET, len as u64);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(STRING_BYTES_OFFSET), len);
    }
    from_obj(p)
}

/// Borrows a boxed `String` value as a byte slice.
unsafe fn string_bytes<'a>(v: Value) -> &'a [u8] {
    let p = as_obj(v);
    // SAFETY: `v` is a boxed `String`; its length and bytes are inline.
    unsafe {
        let len = read_u64(p, STRING_LEN_OFFSET) as usize;
        std::slice::from_raw_parts(p.add(STRING_BYTES_OFFSET), len)
    }
}

/// Concatenates two `String`s into a fresh one (operands consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_concat(a: Value, b: Value) -> Value {
    // SAFETY: `a` and `b` are boxed `String`s (guaranteed by typing).
    let out = unsafe {
        let (ab, bb) = (string_bytes(a), string_bytes(b));
        let mut out = Vec::with_capacity(ab.len() + bb.len());
        out.extend_from_slice(ab);
        out.extend_from_slice(bb);
        out
    };
    let result = make_string(&out);
    fai_drop(a);
    fai_drop(b);
    result
}

/// Concatenates two `String`s, *borrowing* both operands (the caller releases
/// them at their last use).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_concat_borrowed(a: Value, b: Value) -> Value {
    // SAFETY: `a` and `b` are boxed `String`s (guaranteed by typing).
    let out = unsafe {
        let (ab, bb) = (string_bytes(a), string_bytes(b));
        let mut out = Vec::with_capacity(ab.len() + bb.len());
        out.extend_from_slice(ab);
        out.extend_from_slice(bb);
        out
    };
    make_string(&out)
}

/// Renders an `Int` as a `String` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_to_string(n: Value) -> Value {
    let result = make_string(unbox_int(n).to_string().as_bytes());
    fai_drop(n);
    result
}

/// Borrows a boxed `String` as a `&str` (the bytes are valid UTF-8 by typing).
unsafe fn string_str<'a>(v: Value) -> &'a str {
    // SAFETY: `v` is a boxed `String` of valid UTF-8.
    unsafe { std::str::from_utf8_unchecked(string_bytes(v)) }
}

/// The number of Unicode scalar values in a `String` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_length(s: Value) -> Value {
    // SAFETY: `s` is a boxed `String`.
    let n = unsafe { string_str(s) }.chars().count();
    fai_drop(s);
    imm_int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// The number of Unicode scalar values in a `String`, *borrowing* the operand
/// (the caller releases it at its last use).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_length_borrowed(s: Value) -> Value {
    // SAFETY: `s` is a boxed `String`.
    let n = unsafe { string_str(s) }.chars().count();
    imm_int(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Uppercases a `String` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_to_upper(s: Value) -> Value {
    let result = fai_to_upper_borrowed(s);
    fai_drop(s);
    result
}

/// Uppercases a `String`, *borrowing* the operand (the caller releases it).
#[unsafe(no_mangle)]
pub extern "C" fn fai_to_upper_borrowed(s: Value) -> Value {
    // SAFETY: `s` is a boxed `String`.
    let out = unsafe { string_str(s) }.to_uppercase();
    make_string(out.as_bytes())
}

/// Lowercases a `String` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_to_lower(s: Value) -> Value {
    let result = fai_to_lower_borrowed(s);
    fai_drop(s);
    result
}

/// Lowercases a `String`, *borrowing* the operand (the caller releases it).
#[unsafe(no_mangle)]
pub extern "C" fn fai_to_lower_borrowed(s: Value) -> Value {
    // SAFETY: `s` is a boxed `String`.
    let out = unsafe { string_str(s) }.to_lowercase();
    make_string(out.as_bytes())
}

/// Trims leading and trailing ASCII whitespace from a `String` (consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_trim(s: Value) -> Value {
    let result = fai_trim_borrowed(s);
    fai_drop(s);
    result
}

/// Trims a `String`, *borrowing* the operand (the caller releases it).
#[unsafe(no_mangle)]
pub extern "C" fn fai_trim_borrowed(s: Value) -> Value {
    // SAFETY: `s` is a boxed `String`.
    let out = unsafe { string_str(s) }.trim().to_owned();
    make_string(out.as_bytes())
}

/// Whether `s` contains `needle` as a substring (both consumed) — a `Bool`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_contains(s: Value, needle: Value) -> Value {
    // SAFETY: both are boxed `String`s.
    let found = unsafe { string_str(s).contains(string_str(needle)) };
    fai_drop(s);
    fai_drop(needle);
    from_bool(found)
}

/// Whether `s` contains `needle`, *borrowing* both operands (the caller releases
/// them at their last use).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_contains_borrowed(s: Value, needle: Value) -> Value {
    // SAFETY: both are boxed `String`s.
    let found = unsafe { string_str(s).contains(string_str(needle)) };
    from_bool(found)
}

/// Builds a Fai `List` value from owned string pieces (Nil is the immediate tag).
fn list_of_strings(pieces: &[Value]) -> Value {
    let mut list = imm_int(NIL_TAG);
    for &piece in pieces.iter().rev() {
        let p = alloc_obj(DATA_FIELDS_OFFSET + 16, &FAI_DATA_DESC);
        // SAFETY: `p` has room for the tag and two fields.
        unsafe {
            write_u64(p, DATA_TAG_OFFSET, CONS_TAG as u64);
            write_i64(p, DATA_FIELDS_OFFSET, piece);
            write_i64(p, DATA_FIELDS_OFFSET + 8, list);
        }
        list = from_obj(p);
    }
    list
}

/// Splits `s` on the separator `sep` into a `List String` (both consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_split(sep: Value, s: Value) -> Value {
    let list = fai_string_split_borrowed(sep, s);
    fai_drop(sep);
    fai_drop(s);
    list
}

/// Splits `s` on `sep` into a `List String`, *borrowing* both operands (the
/// caller releases them at their last use).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_split_borrowed(sep: Value, s: Value) -> Value {
    // SAFETY: both are boxed `String`s.
    let pieces: Vec<Value> = unsafe {
        let (sep_s, src) = (string_str(sep), string_str(s));
        if sep_s.is_empty() {
            src.chars().map(|c| make_string(c.to_string().as_bytes())).collect()
        } else {
            src.split(sep_s).map(|piece| make_string(piece.as_bytes())).collect()
        }
    };
    list_of_strings(&pieces)
}

/// Joins a `List String` with the separator `sep` (both consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_join(sep: Value, list: Value) -> Value {
    let result = fai_string_join_borrowed(sep, list);
    fai_drop(sep);
    fai_drop(list);
    result
}

/// Joins a `List String` with `sep`, *borrowing* both operands (the caller
/// releases them at their last use).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_join_borrowed(sep: Value, list: Value) -> Value {
    // SAFETY: `sep` is a `String`; `list` is a `List String`.
    let out = unsafe {
        let sep_s = string_str(sep);
        let mut out = String::new();
        let mut cur = list;
        let mut first = true;
        while is_boxed(cur) {
            let p = as_obj(cur);
            let head = read_i64(p, DATA_FIELDS_OFFSET);
            if !first {
                out.push_str(sep_s);
            }
            first = false;
            out.push_str(string_str(head));
            cur = read_i64(p, DATA_FIELDS_OFFSET + 8);
        }
        out
    };
    make_string(out.as_bytes())
}

/// The constructor tags of the built-in `List` (shared with codegen lowering).
const NIL_TAG: i64 = 0;
const CONS_TAG: i64 = 1;

// ---------------------------------------------------------------------------
// Closures & application.
// ---------------------------------------------------------------------------

/// The ABI of every compiled Fai function: it borrows its closure's environment
/// and consumes its `args` (an array of exactly `arity` owned values).
type CodeFn = unsafe extern "C" fn(env: *const i64, args: *const i64) -> Value;

/// Allocates a closure capturing `env_count` slots (rc = 1).
///
/// # Safety
/// `env` must point to `env_count` owned values, whose ownership transfers into
/// the closure. `code` must be a valid [`CodeFn`] of the given `arity`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_make_closure(
    code: *const u8,
    arity: u64,
    env_count: u64,
    env: *const i64,
) -> Value {
    let size = CLOSURE_ENV_OFFSET + env_count as usize * 8;
    let p = alloc_obj(size, &FAI_CLOSURE_DESC);
    // SAFETY: `p` has room for the closure fields and `env_count` slots; `env`
    // points to `env_count` values.
    unsafe {
        write_ptr(p, CLOSURE_CODE_OFFSET, code);
        write_u64(p, CLOSURE_ARITY_OFFSET, arity);
        write_u64(p, CLOSURE_ENV_COUNT_OFFSET, env_count);
        for i in 0..env_count as usize {
            write_i64(p, CLOSURE_ENV_OFFSET + i * 8, *env.add(i));
        }
    }
    from_obj(p)
}

/// Allocates a partial application capturing `func` and `nargs` arguments
/// (rc = 1). Ownership of `func` and the args transfers in.
unsafe fn make_pap(func: Value, args: *const i64, nargs: u64) -> Value {
    let size = PAP_ARGS_OFFSET + nargs as usize * 8;
    let p = alloc_obj(size, &FAI_PAP_DESC);
    // SAFETY: `p` has room for the fields and `nargs` slots; `args` points to
    // `nargs` values.
    unsafe {
        write_i64(p, PAP_FUNC_OFFSET, func);
        write_u64(p, PAP_NARGS_OFFSET, nargs);
        for i in 0..nargs as usize {
            write_i64(p, PAP_ARGS_OFFSET + i * 8, *args.add(i));
        }
    }
    from_obj(p)
}

/// Applies `callee` to `argc` arguments, handling exact, partial, and
/// over-application. Consumes `callee` and every argument.
///
/// # Safety
/// `callee` must be a closure or partial-application value; `args` must point to
/// `argc` owned values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_apply_n(callee: Value, argc: u64, args: *const i64) -> Value {
    if !is_boxed(callee) {
        fai_panic("application of a non-function value");
    }
    let p = as_obj(callee);
    // SAFETY: `callee` is boxed.
    let desc = unsafe { read_ptr(p, DESC_OFFSET).cast::<Descriptor>() };

    if std::ptr::eq(desc, &FAI_PAP_DESC) {
        // Take owned references to the stored target and arguments, then release
        // this reference to the shell. Dropping (rather than unconditionally
        // freeing) is correct when the partial application is shared: a dup'd PAP
        // applied here must not free storage another reference still holds.
        // SAFETY: `p` is a partial application.
        unsafe {
            let func = fai_dup(read_i64(p, PAP_FUNC_OFFSET));
            let stored = read_u64(p, PAP_NARGS_OFFSET);
            let total = stored + argc;
            let mut combined: Vec<i64> = Vec::with_capacity(total as usize);
            for i in 0..stored as usize {
                combined.push(fai_dup(read_i64(p, PAP_ARGS_OFFSET + i * 8)));
            }
            for i in 0..argc as usize {
                combined.push(*args.add(i));
            }
            fai_drop(callee);
            return fai_apply_n(func, total, combined.as_ptr());
        }
    }

    if !std::ptr::eq(desc, &FAI_CLOSURE_DESC) {
        fai_panic("application of a non-function value (bad descriptor)");
    }

    // SAFETY: `p` is a closure.
    let (arity, code) =
        unsafe { (read_u64(p, CLOSURE_ARITY_OFFSET), read_ptr(p, CLOSURE_CODE_OFFSET)) };
    let env = {
        // SAFETY: the env slots follow the closure header.
        unsafe { p.add(CLOSURE_ENV_OFFSET).cast::<i64>() }
    };
    // SAFETY: `code` is a valid `CodeFn` for this closure.
    let f: CodeFn = unsafe { std::mem::transmute::<*const u8, CodeFn>(code) };

    if argc == arity {
        // SAFETY: `f` reads exactly `arity` args from `args` and borrows `env`.
        let r = unsafe { f(env, args) };
        fai_drop(callee);
        r
    } else if argc < arity {
        // SAFETY: `args` holds `argc` owned values; ownership moves into the PAP.
        unsafe { make_pap(callee, args, argc) }
    } else {
        // SAFETY: `f` consumes the first `arity` args; the rest are applied next.
        let r = unsafe { f(env, args) };
        fai_drop(callee);
        // SAFETY: `args.add(arity)` points to the remaining `argc - arity` args.
        unsafe { fai_apply_n(r, argc - arity, args.add(arity as usize)) }
    }
}

// ---------------------------------------------------------------------------
// Structural equality.
// ---------------------------------------------------------------------------

/// Structural equality over non-function values, returning a `Bool` (operands
/// consumed). Function equality is rejected by the type checker, so closures are
/// never compared here.
#[unsafe(no_mangle)]
pub extern "C" fn fai_equal(a: Value, b: Value) -> Value {
    let r = values_equal(a, b);
    fai_drop(a);
    fai_drop(b);
    from_bool(r)
}

/// Structural equality that *borrows* its operands (the caller retains ownership
/// and releases them at their last use). Used for boxed operands that reference
/// counting lent rather than transferred.
#[unsafe(no_mangle)]
pub extern "C" fn fai_equal_borrowed(a: Value, b: Value) -> Value {
    from_bool(values_equal(a, b))
}

/// Whether `v` is a function value (a closure or partial application).
fn is_function_value(v: Value) -> bool {
    if !is_boxed(v) {
        return false;
    }
    // SAFETY: `v` is boxed.
    let desc = unsafe { read_ptr(as_obj(v), DESC_OFFSET).cast::<Descriptor>() };
    std::ptr::eq(desc, &FAI_CLOSURE_DESC) || std::ptr::eq(desc, &FAI_PAP_DESC)
}

/// Aborts if either operand is a function value: equality/ordering is undefined
/// on functions. The type checker rejects this for concrete types; this guards
/// the residual case (a polymorphic comparison instantiated at a function type).
fn guard_comparable(a: Value, b: Value) {
    if is_function_value(a) || is_function_value(b) {
        eprintln!("fai: equality/ordering is not defined on functions");
        std::process::exit(71);
    }
}

fn values_equal(a: Value, b: Value) -> bool {
    guard_comparable(a, b);
    match (is_boxed(a), is_boxed(b)) {
        (false, false) => a == b,
        (true, true) => {
            // SAFETY: both are boxed values.
            unsafe {
                let da = read_ptr(as_obj(a), DESC_OFFSET).cast::<Descriptor>();
                let db = read_ptr(as_obj(b), DESC_OFFSET).cast::<Descriptor>();
                if !std::ptr::eq(da, db) {
                    return false;
                }
                if std::ptr::eq(da, &FAI_STRING_DESC) {
                    string_bytes(a) == string_bytes(b)
                } else if std::ptr::eq(da, &FAI_INT_DESC) {
                    read_i64(as_obj(a), INT_VALUE_OFFSET) == read_i64(as_obj(b), INT_VALUE_OFFSET)
                } else if std::ptr::eq(da, &FAI_FLOAT_DESC) {
                    read_u64(as_obj(a), FLOAT_VALUE_OFFSET)
                        == read_u64(as_obj(b), FLOAT_VALUE_OFFSET)
                } else if std::ptr::eq(da, &FAI_DATA_DESC) {
                    data_equal(a, b)
                } else {
                    false
                }
            }
        }
        // A small immediate Int can never equal a boxed (overflowed) one, and a
        // nullary constructor (immediate) never equals a non-nullary one (boxed).
        _ => false,
    }
}

/// Structural equality of two boxed data values (same tag and equal fields).
unsafe fn data_equal(a: Value, b: Value) -> bool {
    // SAFETY: `a` and `b` are boxed data values.
    unsafe {
        if read_u64(as_obj(a), DATA_TAG_OFFSET) != read_u64(as_obj(b), DATA_TAG_OFFSET) {
            return false;
        }
        let n = data_field_count(a);
        if n != data_field_count(b) {
            return false;
        }
        for i in 0..n {
            let fa = read_i64(as_obj(a), DATA_FIELDS_OFFSET + i * 8);
            let fb = read_i64(as_obj(b), DATA_FIELDS_OFFSET + i * 8);
            if !values_equal(fa, fb) {
                return false;
            }
        }
        true
    }
}

/// Structural ordering of two values, returning `-1`/`0`/`1` as an immediate
/// `Int` (operands consumed). Undefined on functions (rejected by the type
/// checker). Backs `< <= > >=` on non-numeric types.
#[unsafe(no_mangle)]
pub extern "C" fn fai_compare(a: Value, b: Value) -> Value {
    let ord = values_compare(a, b);
    fai_drop(a);
    fai_drop(b);
    imm_int(match ord {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

/// Structural ordering that *borrows* its operands (see [`fai_equal_borrowed`]).
#[unsafe(no_mangle)]
pub extern "C" fn fai_compare_borrowed(a: Value, b: Value) -> Value {
    imm_int(match values_compare(a, b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

fn values_compare(a: Value, b: Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    guard_comparable(a, b);
    // Both immediates: compare payloads (Int values, Bool, or nullary tags).
    if !is_boxed(a) && !is_boxed(b) {
        return (a >> 1).cmp(&(b >> 1));
    }
    // Otherwise identify the kind from a boxed operand (both share a type).
    let boxed = if is_boxed(a) { a } else { b };
    // SAFETY: `boxed` is a boxed value.
    let desc = unsafe { read_ptr(as_obj(boxed), DESC_OFFSET).cast::<Descriptor>() };
    if std::ptr::eq(desc, &FAI_FLOAT_DESC) {
        return unbox_float(a).total_cmp(&unbox_float(b));
    }
    if std::ptr::eq(desc, &FAI_STRING_DESC) {
        // SAFETY: both are boxed `String`s.
        return unsafe { string_bytes(a).cmp(string_bytes(b)) };
    }
    if std::ptr::eq(desc, &FAI_DATA_DESC) {
        let ta = data_tag(a) >> 1;
        let tb = data_tag(b) >> 1;
        match ta.cmp(&tb) {
            Ordering::Equal => {
                // Same constructor (both boxed): compare fields lexicographically.
                if is_boxed(a) && is_boxed(b) {
                    // SAFETY: both are boxed data values with equal field counts.
                    unsafe {
                        let n = data_field_count(a);
                        for i in 0..n {
                            let fa = read_i64(as_obj(a), DATA_FIELDS_OFFSET + i * 8);
                            let fb = read_i64(as_obj(b), DATA_FIELDS_OFFSET + i * 8);
                            match values_compare(fa, fb) {
                                Ordering::Equal => {}
                                other => return other,
                            }
                        }
                    }
                }
                Ordering::Equal
            }
            other => other,
        }
    } else {
        // Boxed `Int` (possibly versus an immediate `Int`).
        unbox_int(a).cmp(&unbox_int(b))
    }
}

/// Reads a data value's tag as an immediate `Int`, without consuming it.
fn data_tag(v: Value) -> Value {
    if is_boxed(v) {
        // SAFETY: a boxed data value stores its tag at `DATA_TAG_OFFSET`.
        imm_int(unsafe { read_u64(as_obj(v), DATA_TAG_OFFSET) as i64 })
    } else {
        v
    }
}

// ---------------------------------------------------------------------------
// The Console capability and its redirectable sink.
// ---------------------------------------------------------------------------

/// Where console output goes. Defaults to stdout; tests redirect to a buffer.
enum Sink {
    Stdout,
    Capture(Vec<u8>),
}

static SINK: Mutex<Sink> = Mutex::new(Sink::Stdout);

/// Redirects console output to an in-memory buffer (for in-process tests).
pub fn capture_start() {
    *SINK.lock().expect("sink") = Sink::Capture(Vec::new());
}

/// Returns the captured output and restores stdout output.
#[must_use]
pub fn capture_take() -> String {
    let mut guard = SINK.lock().expect("sink");
    let text = match &mut *guard {
        Sink::Capture(buf) => String::from_utf8_lossy(&std::mem::take(buf)).into_owned(),
        Sink::Stdout => String::new(),
    };
    *guard = Sink::Stdout;
    text
}

/// `Console.writeLine`: writes a `String` followed by a newline to the sink.
/// Consumes `s` and returns `Unit`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_console_write_line(s: Value) -> Value {
    {
        // SAFETY: `s` is a boxed `String`.
        let bytes = unsafe { string_bytes(s) };
        let mut guard = SINK.lock().expect("sink");
        match &mut *guard {
            Sink::Stdout => {
                use std::io::Write as _;
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                let _ = lock.write_all(bytes);
                let _ = lock.write_all(b"\n");
            }
            Sink::Capture(buf) => {
                buf.extend_from_slice(bytes);
                buf.push(b'\n');
            }
        }
    }
    fai_drop(s);
    FAI_UNIT
}

/// `Clock.now`: milliseconds since the Unix epoch as an immediate `Int`. Consumes
/// its `Unit` argument.
#[unsafe(no_mangle)]
pub extern "C" fn fai_clock_now(unit: Value) -> Value {
    fai_drop(unit);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    fai_box_int(millis)
}

/// `Random.nextInt`: a pseudo-random `Int` in `[0, n)` (`0` for `n <= 0`),
/// advancing a process-global xorshift state. Consumes `n`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_random_next_int(n: Value) -> Value {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x2545_f491_4f6c_dd1d);
    let bound = unbox_int(n);
    fai_drop(n);
    if bound <= 0 {
        return fai_box_int(0);
    }
    // xorshift64*
    let mut x = STATE.load(Ordering::Relaxed);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, Ordering::Relaxed);
    let r = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
    fai_box_int((r % bound as u64) as i64)
}

/// Builds the `(Bool * String)` tuple the FileSystem/Env hosts return (a flag and
/// a payload string), which the standard library unwraps into `Result`/`Option`.
/// Consumes `payload`.
fn ok_string_pair(ok: bool, payload: Value) -> Value {
    let fields = [from_bool(ok), payload];
    // SAFETY: `fields` holds two owned values, moved into the new tuple.
    unsafe { fai_make_data(0, 2, fields.as_ptr()) }
}

/// `FileSystem.readFile`: reads `path`, returning `(true, contents)` or
/// `(false, error message)`. Consumes `path`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_read(path: Value) -> Value {
    // SAFETY: `path` is a boxed `String`.
    let p = unsafe { string_str(path) }.to_owned();
    let result = match std::fs::read_to_string(&p) {
        Ok(contents) => ok_string_pair(true, make_string(contents.as_bytes())),
        Err(e) => ok_string_pair(false, make_string(e.to_string().as_bytes())),
    };
    fai_drop(path);
    result
}

/// `FileSystem.writeFile`: writes `contents` to `path`, returning `(true, "")` or
/// `(false, error message)`. Consumes both arguments.
#[unsafe(no_mangle)]
pub extern "C" fn fai_file_write(path: Value, contents: Value) -> Value {
    // SAFETY: both are boxed `String`s.
    let (p, c) = unsafe { (string_str(path).to_owned(), string_str(contents).to_owned()) };
    let result = match std::fs::write(&p, c.as_bytes()) {
        Ok(()) => ok_string_pair(true, make_string(b"")),
        Err(e) => ok_string_pair(false, make_string(e.to_string().as_bytes())),
    };
    fai_drop(path);
    fai_drop(contents);
    result
}

/// `Env.get`: looks up environment variable `name`, returning `(true, value)` or
/// `(false, "")`. Consumes `name`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_env_get(name: Value) -> Value {
    // SAFETY: `name` is a boxed `String`.
    let n = unsafe { string_str(name) }.to_owned();
    let result = match std::env::var(&n) {
        Ok(v) => ok_string_pair(true, make_string(v.as_bytes())),
        Err(_) => ok_string_pair(false, make_string(b"")),
    };
    fai_drop(name);
    result
}

/// `Env.args`: the process arguments after the program name, as a `List String`.
/// Consumes its `Unit` argument.
#[unsafe(no_mangle)]
pub extern "C" fn fai_env_args(unit: Value) -> Value {
    fai_drop(unit);
    let args: Vec<Value> = std::env::args().skip(1).map(|a| make_string(a.as_bytes())).collect();
    list_of_strings(&args)
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Runs a program: forces the `Runtime` value binding, applies the entry closure
/// (`main : Runtime -> Unit`) to it, drops the result, and reports leaks. Returns
/// a process exit code (0 success, 70 if objects leaked). Consumes both closures.
///
/// `runtime` is the standard library's zero-arity `Runtime` value binding (a
/// static closure forced by applying it to no arguments); `entry` is `main`.
/// Both the AOT `main` (emitted by codegen) and the JIT runner call this.
#[must_use]
pub fn run_entry(entry: Value, runtime: Value) -> i32 {
    // Force the zero-arity `Runtime` value binding (apply to no arguments).
    // SAFETY: `runtime` is a closure of arity 0.
    let runtime_value = unsafe { fai_apply_n(runtime, 0, std::ptr::null()) };
    let args = [runtime_value];
    // SAFETY: `entry` is a closure of arity 1; `args` holds one owned value.
    let result = unsafe { fai_apply_n(entry, 1, args.as_ptr()) };
    fai_drop(result);
    let live = live_count();
    if live != 0 {
        eprintln!("fai: memory leak detected: {live} live object(s) at exit");
        return 70;
    }
    0
}

/// C entry shim called from generated `main`: runs the entry closure against the
/// standard library's `Runtime` value binding.
#[unsafe(no_mangle)]
pub extern "C" fn fai_run_main(entry: Value, runtime: Value) -> i32 {
    run_entry(entry, runtime)
}

// ---------------------------------------------------------------------------
// Host helpers for the in-process contract runner: build `Int` arguments and
// decode result values without exposing the internal tagging.
// ---------------------------------------------------------------------------

/// Builds a Fai `Int` value from a host `i64` (immediate when it fits, else boxed).
#[must_use]
pub fn make_int(n: i64) -> Value {
    fai_box_int(n)
}

/// Reads a Fai `Int` value as a host `i64` (borrowing it; immediate or boxed).
#[must_use]
pub fn read_int(v: Value) -> i64 {
    unbox_int(v)
}

/// Reads a Fai `Float` value as a host `f64` (borrowing it).
#[must_use]
pub fn read_float(v: Value) -> f64 {
    unbox_float(v)
}

/// Applies a closure value to owned arguments (a safe wrapper over
/// [`fai_apply_n`]); the call consumes `closure` and each argument.
#[must_use]
pub fn apply(closure: Value, args: &[Value]) -> Value {
    let argc = args.len() as u64;
    // SAFETY: `closure` is a closure value and `args` holds `argc` owned values;
    // `fai_apply_n` reads exactly `argc` of them.
    unsafe { fai_apply_n(closure, argc, args.as_ptr()) }
}

/// The constructor tag of a data value (borrowing it): a nullary constructor's
/// payload, or a boxed data value's stored tag.
#[must_use]
pub fn data_tag_of(v: Value) -> i64 {
    fai_data_tag(v) >> 1
}

/// Copies a Fai `String`'s bytes into an owned `Vec` (borrowing the value).
#[must_use]
pub fn read_string(v: Value) -> Vec<u8> {
    // SAFETY: `v` is a boxed `String` (valid by the caller's typing).
    unsafe { string_bytes(v).to_vec() }
}

// ---------------------------------------------------------------------------
// Allocator fuzz/property harness.
// ---------------------------------------------------------------------------

/// Drives the size-class allocator through a sequence of alloc/free operations
/// decoded from `data`, checking its invariants after each step. One harness backs
/// three drivers: the in-crate property test (proptest-generated `data`), the
/// deterministic stress test (fixed `data`), and the cargo-fuzz target (fuzzer
/// `data`) — so all three exercise identical logic.
///
/// The decoded sizes span the pooled classes **and** the large (unpooled)
/// fallback. Invariants, all independent of the debug leak counters so they hold
/// under any build: no two live blocks share an address; every block is 8-aligned
/// and holds at least its requested size; and a unique byte pattern written across
/// each block's payload survives every later operation (verified when the block is
/// freed — every block is eventually freed — so a cell wrongly handed out twice is
/// caught). When the counters are present (a debug build), the live-object count
/// also returns to its starting value once everything is freed.
///
/// Must be called single-threaded (or under the runtime test lock): it touches the
/// process-global live counter and exercises the thread-local pool.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub fn run_ops(data: &[u8]) {
    use std::collections::HashSet;

    // Bound the work so a huge fuzzer input cannot make one run pathological.
    const MAX_OPS: usize = 4096;

    let base = live_count();
    // Live blocks: (pointer, requested size, payload sentinel byte).
    let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();
    // Distinct live addresses, to catch a cell handed out while already live.
    let mut addrs: HashSet<usize> = HashSet::new();

    for &b in data.iter().take(MAX_OPS) {
        if !live.is_empty() && b & 0x80 != 0 {
            // Free a live block, verifying its payload first.
            let idx = (b as usize) % live.len();
            let (p, size, sentinel) = live.swap_remove(idx);
            verify_payload(p, size, sentinel);
            addrs.remove(&(p as usize));
            // SAFETY: `p` is a live block from `alloc_obj`, freed exactly once.
            unsafe { free_obj(p) };
        } else {
            // Allocate a block; sizes 32..=664 straddle MAX_POOLED_SIZE (512).
            let size = HEADER_SIZE + ((b & 0x7f) as usize % 80) * SIZE_STEP;
            let p = alloc_obj(size, &FAI_INT_DESC);
            assert_eq!(p as usize % ALIGN, 0, "allocation is not {ALIGN}-aligned");
            assert!(addrs.insert(p as usize), "alloc returned an address already live");
            write_payload(p, size, b);
            live.push((p, size, b));
        }
    }

    // Free everything that remains, verifying each block's payload.
    for (p, size, sentinel) in live.drain(..) {
        verify_payload(p, size, sentinel);
        // SAFETY: `p` is a live block from `alloc_obj`, freed exactly once.
        unsafe { free_obj(p) };
    }
    assert_eq!(live_count(), base, "every allocation was freed");
}

/// Fills a block's payload (`[HEADER_SIZE, size)`) with `byte`.
#[cfg(any(test, feature = "fuzzing"))]
fn write_payload(p: *mut u8, size: usize, byte: u8) {
    // SAFETY: `p` points to at least `size` writable bytes.
    unsafe {
        for off in HEADER_SIZE..size {
            p.add(off).write(byte);
        }
    }
}

/// Asserts a block's payload still holds `byte` everywhere (no corruption from a
/// later overlapping allocation).
#[cfg(any(test, feature = "fuzzing"))]
fn verify_payload(p: *const u8, size: usize, byte: u8) {
    // SAFETY: `p` points to at least `size` readable bytes.
    unsafe {
        for off in HEADER_SIZE..size {
            assert_eq!(p.add(off).read(), byte, "payload corrupted at offset {off}");
        }
    }
}

#[cfg(test)]
mod alloc_tests;
#[cfg(test)]
mod proptests;
#[cfg(test)]
mod reuse_tests;
#[cfg(test)]
mod tests;
