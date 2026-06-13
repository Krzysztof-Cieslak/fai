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
use std::collections::BTreeMap;
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

/// Byte offset of a `String`'s byte length. A borrowing slice
/// ([`KIND_STRING_SLICE`]) stores its byte length at the same offset, so reading a
/// string's length is uniform across the inline and slice representations.
pub const STRING_LEN_OFFSET: usize = HEADER_SIZE;
/// Byte offset of an *inline* `String`'s first content byte.
pub const STRING_BYTES_OFFSET: usize = HEADER_SIZE + 8;

/// Byte offset of a string slice's base (the inline `String` it views; a slice
/// always points at an inline base, never another slice).
pub const SLICE_BASE_OFFSET: usize = HEADER_SIZE + 8;
/// Byte offset of a string slice's start, in bytes into the base's content.
pub const SLICE_OFFSET_OFFSET: usize = HEADER_SIZE + 16;

/// Byte offset of an `Array`'s element count (the live length).
pub const ARRAY_LEN_OFFSET: usize = HEADER_SIZE;
/// Byte offset of an `Array`'s first element slot. Capacity is derived from the
/// object's allocation size (`(size - ARRAY_ELEMS_OFFSET) / 8`), so only slots
/// `0..length` are ever live; the rest is spare capacity for in-place growth.
pub const ARRAY_ELEMS_OFFSET: usize = HEADER_SIZE + 8;

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
/// generated code). A value's kind is recovered from the descriptor's [`kind`]
/// tag (not its address), so generated code may emit *per-shape* data descriptors
/// — each carrying a [`scalar_bitmap`] of which data slots hold an unboxed `f64`
/// rather than a uniform word — without breaking kind dispatch. Releasing an
/// object's reference-counted children is driven by kind in [`scan_push`] (see
/// [`fai_drop`]).
///
/// [`kind`]: Descriptor::kind
/// [`scalar_bitmap`]: Descriptor::scalar_bitmap
#[repr(C)]
pub struct Descriptor {
    /// The object kind (`KIND_*`), the discriminant used to recover a value's
    /// representation. Compared by value, so distinct descriptors of the same kind
    /// (e.g. per-shape data descriptors) dispatch identically.
    pub kind: u64,
    /// For a `KIND_DATA` descriptor, the set of field slots stored as a raw,
    /// unboxed `f64` (bit `i` set ⇒ slot `i` is a scalar float, carrying no
    /// reference count): generic walkers skip these in reference counting and
    /// compare them as floats. Zero for every other kind and for data cells with
    /// no scalar fields (the shared [`FAI_DATA_DESC`]).
    pub scalar_bitmap: u64,
    /// A human-readable kind name (used in leak/debug reporting). A raw pointer +
    /// length rather than a `&'static str` so a generated descriptor may leave it
    /// null (it is never dereferenced by runtime logic).
    pub name_ptr: *const u8,
    /// The length of [`name_ptr`](Descriptor::name_ptr).
    pub name_len: usize,
}

// SAFETY: a `Descriptor` is immutable (all fields written once at definition) and
// its `name_ptr` points either to a `'static` string or is null; it carries no
// interior mutability, so sharing it across threads is sound.
unsafe impl Sync for Descriptor {}

/// `String` objects (leaf: inline bytes, no children).
pub const KIND_STRING: u64 = 0;
/// Boxed (overflowed) `Int` objects (leaf).
pub const KIND_INT: u64 = 1;
/// Boxed `Float` objects (leaf).
pub const KIND_FLOAT: u64 = 2;
/// Closures (children: the captured environment slots).
pub const KIND_CLOSURE: u64 = 3;
/// Partial applications (children: the target plus stored args).
pub const KIND_PAP: u64 = 4;
/// Data values — constructors, records, and tuples (children: the non-scalar
/// fields, per the descriptor's [`Descriptor::scalar_bitmap`]).
pub const KIND_DATA: u64 = 5;
/// Contiguous arrays (children: the boxed element slots).
pub const KIND_ARRAY: u64 = 6;
/// A borrowing substring view (child: the inline base `String` it slices).
pub const KIND_STRING_SLICE: u64 = 7;
/// The niche `Option`'s `None` sentinel (a single immortal object; a leaf with no
/// children). A Scheme-B niche `Option` represents `None` as the shared sentinel
/// and `Some x` as `x`, so the sentinel's distinct kind keeps `None` unequal to —
/// and ordered before — any `Some` payload in the generic equality/ordering walks.
pub const KIND_NONE: u64 = 8;

/// Builds a runtime-static descriptor with no scalar fields.
const fn descriptor(kind: u64, name: &'static str) -> Descriptor {
    Descriptor { kind, scalar_bitmap: 0, name_ptr: name.as_ptr(), name_len: name.len() }
}

/// Descriptor for the niche `None` sentinel (a childless leaf).
static FAI_NONE_DESC: Descriptor = descriptor(KIND_NONE, "None");

/// A boxed heap object's fixed header, for the leaked sentinel object.
#[repr(C)]
struct ObjHeader {
    rc: u64,
    descriptor: *const Descriptor,
    size: u64,
}

/// The address of the single niche `None` sentinel object, allocated once on
/// first use (stored as a `usize`, which is `Sync`).
static NONE_SENTINEL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// The Scheme-B niche `None` value: a shared, immortal, childless heap object's
/// address (a boxed pointer). Code generation calls this to build a `None` and to
/// test for it.
///
/// The object is a leaked `Box` (writable heap memory, so the generic
/// reference-count operations may touch its count without faulting — unlike a
/// read-only static) that is **not** routed through the object allocator, so it
/// neither counts toward the live-object leak check nor is ever recycled. Its
/// immortal count means it is never reclaimed, so the same object backs every
/// `None` and all `None`s are pointer-equal.
#[unsafe(no_mangle)]
pub extern "C" fn fai_none_value() -> Value {
    let addr = *NONE_SENTINEL.get_or_init(|| {
        let header = Box::new(ObjHeader {
            rc: IMMORTAL_RC,
            descriptor: &raw const FAI_NONE_DESC,
            size: HEADER_SIZE as u64,
        });
        Box::leak(header) as *mut ObjHeader as usize
    });
    addr as Value
}

/// Descriptor for `String` objects (leaf: inline bytes, no children).
#[unsafe(no_mangle)]
pub static FAI_STRING_DESC: Descriptor = descriptor(KIND_STRING, "String");

/// Descriptor for string slices — a borrowing substring view (one child: the
/// inline base `String` it views).
#[unsafe(no_mangle)]
pub static FAI_STRING_SLICE_DESC: Descriptor = descriptor(KIND_STRING_SLICE, "StringSlice");

/// Descriptor for boxed (overflowed) `Int` objects (leaf).
#[unsafe(no_mangle)]
pub static FAI_INT_DESC: Descriptor = descriptor(KIND_INT, "Int");

/// Descriptor for closures (children: the captured environment slots).
#[unsafe(no_mangle)]
pub static FAI_CLOSURE_DESC: Descriptor = descriptor(KIND_CLOSURE, "Closure");

/// Descriptor for partial applications (children: the target plus stored args).
#[unsafe(no_mangle)]
pub static FAI_PAP_DESC: Descriptor = descriptor(KIND_PAP, "Pap");

/// Descriptor for boxed `Float` objects (leaf).
#[unsafe(no_mangle)]
pub static FAI_FLOAT_DESC: Descriptor = descriptor(KIND_FLOAT, "Float");

/// Descriptor for data values with no scalar fields — the shared descriptor for
/// every constructor, record, and tuple whose slots are all uniform words. Cells
/// with one or more unboxed `f64` fields instead point at a per-shape descriptor
/// (generated, or runtime-interned) carrying the scalar bitmap; the field count is
/// always derived from the object's size.
#[unsafe(no_mangle)]
pub static FAI_DATA_DESC: Descriptor = descriptor(KIND_DATA, "Data");

/// The kind tag of the descriptor at `desc`.
///
/// # Safety
/// `desc` must point to a valid [`Descriptor`].
#[inline]
unsafe fn desc_kind(desc: *const Descriptor) -> u64 {
    // SAFETY: the caller guarantees `desc` is a valid descriptor pointer.
    unsafe { (*desc).kind }
}

/// The scalar-slot bitmap of the descriptor at `desc` (zero for any kind without
/// unboxed `f64` fields). Bit `i` set ⇒ data slot `i` holds a raw `f64`.
///
/// # Safety
/// `desc` must point to a valid [`Descriptor`].
#[inline]
unsafe fn desc_scalar_bitmap(desc: *const Descriptor) -> u64 {
    // SAFETY: the caller guarantees `desc` is a valid descriptor pointer.
    unsafe { (*desc).scalar_bitmap }
}

/// The descriptor of a live boxed object.
///
/// # Safety
/// `p` must point to a valid live heap object.
#[inline]
unsafe fn obj_descriptor(p: *const u8) -> *const Descriptor {
    // SAFETY: a live object stores its descriptor pointer at `DESC_OFFSET`.
    unsafe { read_ptr(p, DESC_OFFSET).cast::<Descriptor>() }
}

/// Descriptor for `Array` objects — a contiguous, growable sequence (children:
/// the live element slots `0..length`). Capacity beyond `length` is spare and
/// uninitialized, so the child scan and structural ops touch only `0..length`.
#[unsafe(no_mangle)]
pub static FAI_ARRAY_DESC: Descriptor = descriptor(KIND_ARRAY, "Array");

/// Whether data slot `index` of the cell at `p` holds an unboxed `f64` (per its
/// descriptor's scalar bitmap). Slots at index ≥ 64 are always uniform (the bitmap
/// caps at 64; wider cells fall back to all-uniform at construction).
///
/// # Safety
/// `p` must point to a valid live data object.
#[inline]
unsafe fn slot_is_scalar(p: *const u8, index: usize) -> bool {
    if index >= 64 {
        return false;
    }
    // SAFETY: `p` is a live data object with a valid descriptor.
    let bitmap = unsafe { desc_scalar_bitmap(obj_descriptor(p)) };
    bitmap & (1u64 << index) != 0
}

/// The interned per-shape data descriptors, keyed by scalar bitmap. A
/// row-polymorphic record update whose result changes a field's float-ness needs a
/// descriptor for the new bitmap that no generated static provides; it is created
/// here once per bitmap and leaked (descriptors live for the process). Kind
/// dispatch is by `kind`, not address, so an interned descriptor and a generated
/// one with the same bitmap are interchangeable.
static INTERNED_DATA_DESCRIPTORS: Mutex<BTreeMap<u64, &'static Descriptor>> =
    Mutex::new(BTreeMap::new());

/// Returns a `KIND_DATA` descriptor with the given scalar bitmap: the shared
/// [`FAI_DATA_DESC`] when empty, else an interned per-shape descriptor.
fn intern_data_descriptor(scalar_bitmap: u64) -> *const Descriptor {
    if scalar_bitmap == 0 {
        return &raw const FAI_DATA_DESC;
    }
    let mut map = INTERNED_DATA_DESCRIPTORS.lock().expect("descriptor table");
    let desc = map.entry(scalar_bitmap).or_insert_with(|| {
        Box::leak(Box::new(Descriptor {
            kind: KIND_DATA,
            scalar_bitmap,
            name_ptr: std::ptr::null(),
            name_len: 0,
        }))
    });
    std::ptr::from_ref::<Descriptor>(desc)
}

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

/// The cumulative number of times an array operation duplicated the whole buffer
/// because the array was *shared* (`rc != 1`) at the mutation point — the
/// uniqueness-loss copies in [`fai_array_set`]/[`fai_array_push`]. Distinct from
/// [`ALLOCATIONS`]: each such copy is a single `alloc_array` call (one allocation)
/// but does O(length) element work, so a per-element copy regression is invisible
/// to the allocation *count* yet shows up here as copies that scale with the
/// length. The expected, amortized capacity growth of a *unique* array is not a
/// copy and is not counted. Never decreases until reset.
#[cfg(debug_assertions)]
static ARRAY_COPIES: AtomicI64 = AtomicI64::new(0);

/// The cumulative number of times [`fai_string_concat`] duplicated the whole
/// left-operand buffer because it was *shared* (`rc != 1`) at the concatenation
/// point — the uniqueness-loss copies. Like [`ARRAY_COPIES`], distinct from
/// [`ALLOCATIONS`]: each such copy is a single allocation but does O(length)
/// byte work, so a per-step re-copy regression (the O(n²) string build) is
/// invisible to the allocation *count* yet shows up here as copies that scale
/// with the build. The amortized capacity growth of a *unique* builder (the
/// grow-and-double path) is not a uniqueness-loss copy and is not counted.
/// Never decreases until reset.
#[cfg(debug_assertions)]
static STRING_COPIES: AtomicI64 = AtomicI64::new(0);

/// The cumulative number of borrowing substring **views** created (the zero-copy
/// slice path in `substring`/`take`/`drop`/`split`), as opposed to the small
/// pieces those operations copy. Allocation *count* cannot distinguish a view from
/// a copy — both are one allocation — but a view is a small fixed-size header that
/// shares the base's bytes, where a copy allocates and writes the piece's bytes;
/// this counter is the observable signal that the borrowing path was taken. Never
/// decreases until reset.
#[cfg(debug_assertions)]
static STRING_VIEWS: AtomicI64 = AtomicI64::new(0);

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

/// Records one shared-array buffer copy (a uniqueness-loss duplication). Compiled
/// to nothing in a release build (the counter is absent there).
#[inline(always)]
fn note_array_copy() {
    #[cfg(debug_assertions)]
    ARRAY_COPIES.fetch_add(1, Ordering::Relaxed);
}

/// Records one shared-string buffer copy (a uniqueness-loss duplication in
/// [`fai_string_concat`]). Compiled to nothing in a release build (the counter is
/// absent there).
#[inline(always)]
fn note_string_copy() {
    #[cfg(debug_assertions)]
    STRING_COPIES.fetch_add(1, Ordering::Relaxed);
}

/// Records one borrowing substring view created (the zero-copy slice path).
/// Compiled to nothing in a release build (the counter is absent there).
#[inline(always)]
fn note_string_view() {
    #[cfg(debug_assertions)]
    STRING_VIEWS.fetch_add(1, Ordering::Relaxed);
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

/// Returns the cumulative count of shared-array buffer copies (uniqueness-loss
/// duplications in `set`/`push`). Always zero in a release build, where the
/// counter is compiled out.
#[must_use]
pub fn array_copies() -> i64 {
    #[cfg(debug_assertions)]
    {
        ARRAY_COPIES.load(Ordering::Relaxed)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

/// Returns the cumulative count of shared-string buffer copies (uniqueness-loss
/// duplications in `fai_string_concat`). Always zero in a release build, where the
/// counter is compiled out.
#[must_use]
pub fn string_copies() -> i64 {
    #[cfg(debug_assertions)]
    {
        STRING_COPIES.load(Ordering::Relaxed)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

/// Returns the cumulative count of borrowing substring views created (the
/// zero-copy slice path). Always zero in a release build, where the counter is
/// compiled out.
#[must_use]
pub fn string_views() -> i64 {
    #[cfg(debug_assertions)]
    {
        STRING_VIEWS.load(Ordering::Relaxed)
    }
    #[cfg(not(debug_assertions))]
    {
        0
    }
}

/// Resets the cumulative allocation, array-copy, string-copy, and string-view
/// counters (tests/benchmarks). A no-op in a release build, where the counters are
/// compiled out.
pub fn reset_allocations() {
    #[cfg(debug_assertions)]
    {
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ARRAY_COPIES.store(0, Ordering::Relaxed);
        STRING_COPIES.store(0, Ordering::Relaxed);
        STRING_VIEWS.store(0, Ordering::Relaxed);
    }
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
/// recovered from the object's kind, identified by its descriptor's kind tag.
///
/// # Safety
/// `p` is a live object pointer.
unsafe fn scan_push(p: *mut u8, work: &mut DropWork) {
    // SAFETY: `p` is a live object; its descriptor and fields are in bounds.
    unsafe {
        let desc = obj_descriptor(p);
        if desc_kind(desc) == KIND_DATA {
            let size = read_u64(p, SIZE_OFFSET) as usize;
            let nfields = (size - DATA_FIELDS_OFFSET) / 8;
            // Scalar (`f64`) slots carry no reference count, so they are skipped.
            let scalar = desc_scalar_bitmap(desc);
            for i in 0..nfields {
                if i < 64 && scalar & (1u64 << i) != 0 {
                    continue;
                }
                let field = read_i64(p, DATA_FIELDS_OFFSET + i * 8);
                if is_boxed(field) {
                    work.push(field);
                }
            }
        } else if desc_kind(desc) == KIND_CLOSURE {
            let env_count = read_u64(p, CLOSURE_ENV_COUNT_OFFSET) as usize;
            for i in 0..env_count {
                let slot = read_i64(p, CLOSURE_ENV_OFFSET + i * 8);
                if is_boxed(slot) {
                    work.push(slot);
                }
            }
        } else if desc_kind(desc) == KIND_PAP {
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
        } else if desc_kind(desc) == KIND_ARRAY {
            // An array's children are its live element slots (`0..length`); the
            // spare capacity beyond `length` is uninitialized and not scanned.
            let len = read_u64(p, ARRAY_LEN_OFFSET) as usize;
            for i in 0..len {
                let elem = read_i64(p, ARRAY_ELEMS_OFFSET + i * 8);
                if is_boxed(elem) {
                    work.push(elem);
                }
            }
        } else if desc_kind(desc) == KIND_STRING_SLICE {
            // A slice's one child is the inline base `String` it views.
            let base = read_i64(p, SLICE_BASE_OFFSET);
            if is_boxed(base) {
                work.push(base);
            }
        }
        // Leaf kinds (inline `String`, boxed `Int`/`Float`) have no children.
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

/// Allocates a data value `{ tag, fields… }` carrying the per-shape descriptor
/// `desc` (whose scalar bitmap marks which slots are raw `f64`), copying `nfields`
/// words from `fields`. A scalar slot's word is raw float bits (no reference
/// count); a uniform slot's is an owned value (ownership transfers in). The plain
/// [`fai_make_data`] handles the all-uniform case.
///
/// # Safety
/// `desc` is a valid `KIND_DATA` descriptor whose bitmap matches the field layout;
/// `fields` points to `nfields` words, each owned unless its slot is scalar.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_make_data_scalar(
    desc: *const Descriptor,
    tag: i64,
    nfields: i64,
    fields: *const i64,
) -> Value {
    let n = nfields as usize;
    let size = DATA_FIELDS_OFFSET + n * 8;
    let p = alloc_obj(size, desc);
    // SAFETY: `p` has room for the tag and `n` fields; `fields` points to `n`.
    unsafe {
        write_u64(p, DATA_TAG_OFFSET, tag as u64);
        for i in 0..n {
            write_i64(p, DATA_FIELDS_OFFSET + i * 8, *fields.add(i));
        }
    }
    from_obj(p)
}

/// Converts a niche Scheme-A `Option` (`None` is the immediate `1`, `Some p` is
/// the payload pointer `p`) to the standard boxed representation, consuming `v`. A
/// `None` is already `1` in both forms; a `Some` wraps the payload in a fresh
/// `{ Some, p }` cell (the payload's ownership transfers in). Used at a boundary
/// where a niche value flows into a uniform slot.
#[unsafe(no_mangle)]
pub extern "C" fn fai_niche_a_to_std(v: Value) -> Value {
    if !is_boxed(v) {
        return v;
    }
    let p = alloc_obj(DATA_FIELDS_OFFSET + 8, &FAI_DATA_DESC);
    // SAFETY: `p` has room for the tag and one field; the payload moves in.
    unsafe {
        write_u64(p, DATA_TAG_OFFSET, 1); // `Some` is tag 1 of `Option`.
        write_i64(p, DATA_FIELDS_OFFSET, v);
    }
    from_obj(p)
}

/// Converts a standard boxed `Option` to the niche Scheme-A representation,
/// consuming `o`. A standard `None` (immediate `1`) is already the niche `None`; a
/// standard `Some` cell yields its payload — freeing the wrapper shell directly
/// when unique (handing its payload ref to the caller), else duplicating the
/// payload and decrementing the still-shared wrapper.
#[unsafe(no_mangle)]
pub extern "C" fn fai_std_to_niche_a(o: Value) -> Value {
    if !is_boxed(o) {
        return o;
    }
    let p = as_obj(o);
    // SAFETY: a boxed standard `Option` is a `Some` cell with one field (the
    // payload); reading the count and field is in bounds.
    unsafe {
        let payload = read_i64(p, DATA_FIELDS_OFFSET);
        let rc = read_u64(p, RC_OFFSET);
        if rc == 1 {
            free_obj(p);
        } else {
            fai_dup(payload);
            write_u64(p, RC_OFFSET, rc - 1);
        }
        payload
    }
}

/// Whether `v` is the niche `None` sentinel (a boxed `KIND_NONE` object).
fn is_none_sentinel(v: Value) -> bool {
    // SAFETY: a boxed value has a valid descriptor pointer.
    is_boxed(v) && unsafe { desc_kind(obj_descriptor(as_obj(v))) == KIND_NONE }
}

/// Converts a niche Scheme-B `Option` (`None` is the sentinel, `Some x` is `x` in
/// its uniform representation) to the standard boxed representation, consuming `v`.
/// `None` becomes the immediate `1`; a `Some` wraps the payload in a `{ Some, x }`
/// cell. Used where a niche value flows into a uniform slot.
#[unsafe(no_mangle)]
pub extern "C" fn fai_niche_b_to_std(v: Value) -> Value {
    if is_none_sentinel(v) {
        // Standard `None` is the nullary tag-0 immediate `(0 << 1) | 1`. The owned
        // `None`'s reference to the sentinel is consumed, so release it (the count
        // stays balanced; the immortal sentinel is never actually reclaimed).
        fai_drop(v);
        return 1;
    }
    let p = alloc_obj(DATA_FIELDS_OFFSET + 8, &FAI_DATA_DESC);
    // SAFETY: `p` has room for the tag and one field; the payload moves in.
    unsafe {
        write_u64(p, DATA_TAG_OFFSET, 1); // `Some` is tag 1 of `Option`.
        write_i64(p, DATA_FIELDS_OFFSET, v);
    }
    from_obj(p)
}

/// Converts a standard boxed `Option` to the niche Scheme-B representation,
/// consuming `o`. A standard `None` (immediate `1`) becomes the sentinel; a
/// standard `Some` cell yields its payload (freeing/duplicating the wrapper as in
/// [`fai_std_to_niche_a`]).
#[unsafe(no_mangle)]
pub extern "C" fn fai_std_to_niche_b(o: Value) -> Value {
    if !is_boxed(o) {
        // Standard `None` (immediate, no count) → an owned niche `None`: take a
        // reference to the sentinel (balanced by the matching drop).
        return fai_dup(fai_none_value());
    }
    let p = as_obj(o);
    // SAFETY: a boxed standard `Option` is a `Some` cell with one field.
    unsafe {
        let payload = read_i64(p, DATA_FIELDS_OFFSET);
        let rc = read_u64(p, RC_OFFSET);
        if rc == 1 {
            free_obj(p);
        } else {
            fai_dup(payload);
            write_u64(p, RC_OFFSET, rc - 1);
        }
        payload
    }
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
///
/// A scalar `f64` slot holds raw bits, not a uniform value, so this generic
/// projection (reached only when the caller's static type does not know the field
/// is `Float`) boxes the bits into a fresh `Float` — the uniform value the caller
/// expects. A concrete `Float` projection reads the bits directly in generated
/// code and never calls here.
#[unsafe(no_mangle)]
pub extern "C" fn fai_data_field(v: Value, index: i64) -> Value {
    let i = index as usize;
    // SAFETY: `v` is a boxed data value with at least `index + 1` fields.
    unsafe {
        let bits = read_i64(as_obj(v), DATA_FIELDS_OFFSET + i * 8);
        if slot_is_scalar(as_obj(v), i) {
            // A fresh box (rc = 1) owned by the caller; the cell keeps its bits.
            fai_box_float(bits)
        } else {
            fai_dup(bits);
            bits
        }
    }
}

/// Row-polymorphic record update with the field at `index` (an immediate `Int`
/// slot) replaced by `value`. When `record` is the unique owner, the field is
/// overwritten **in place** (no allocation, no copying); otherwise a fresh copy is
/// built. Consumes `record` and `value`; the replaced field is released.
///
/// Descriptor-aware: `value` arrives uniform (boxed). If it is a `Float`, the
/// updated slot must be scalar (an unboxed `f64`) to keep the cell's invariant
/// that a `Float` field is raw, so its bits are stored and its box consumed; a
/// non-`Float` value is stored as the uniform word. The replaced field is released
/// only when its slot was uniform (a scalar slot carries no reference count). When
/// the update changes the slot's float-ness (a type-changing `{ r with x = v }`),
/// the result carries a descriptor for the new bitmap (the shared
/// [`FAI_DATA_DESC`] when none remain scalar, else an interned per-shape one).
#[unsafe(no_mangle)]
pub extern "C" fn fai_record_update(record: Value, index: Value, value: Value) -> Value {
    let slot = unbox_int(index) as usize;
    // SAFETY: `record` is a boxed data value; `slot` is a valid field index.
    unsafe {
        let p = as_obj(record);
        let old_bitmap = desc_scalar_bitmap(obj_descriptor(p));
        let old_slot_scalar = slot < 64 && old_bitmap & (1u64 << slot) != 0;
        // The new field is scalar iff the replacement is a `Float` (and the slot is
        // representable in the bitmap).
        let new_slot_scalar =
            slot < 64 && is_boxed(value) && desc_kind(obj_descriptor(as_obj(value))) == KIND_FLOAT;
        let new_bitmap = if new_slot_scalar {
            old_bitmap | (1u64 << slot)
        } else if slot < 64 {
            old_bitmap & !(1u64 << slot)
        } else {
            old_bitmap
        };
        // The word to store: a scalar slot takes the float's raw bits (consuming the
        // box); a uniform slot takes the value word itself. Computed once.
        let stored = if new_slot_scalar {
            let bits = read_i64(as_obj(value), FLOAT_VALUE_OFFSET);
            fai_drop(value);
            bits
        } else {
            value
        };

        // Unique owner: overwrite the field in place, releasing the old one.
        if read_u64(p, RC_OFFSET) == 1 {
            let old = read_i64(p, DATA_FIELDS_OFFSET + slot * 8);
            write_i64(p, DATA_FIELDS_OFFSET + slot * 8, stored);
            // A scalar (raw) old field carries no reference count.
            if !old_slot_scalar {
                fai_drop(old);
            }
            if new_bitmap != old_bitmap {
                write_ptr(p, DESC_OFFSET, intern_data_descriptor(new_bitmap).cast());
            }
            return record;
        }
        // Shared: copy the record with the field replaced, under the new bitmap.
        let tag = read_u64(p, DATA_TAG_OFFSET) as i64;
        let n = data_field_count(record);
        let size = DATA_FIELDS_OFFSET + n * 8;
        let q = alloc_obj(size, intern_data_descriptor(new_bitmap));
        write_u64(q, DATA_TAG_OFFSET, tag as u64);
        for i in 0..n {
            if i == slot {
                write_i64(q, DATA_FIELDS_OFFSET + i * 8, stored);
            } else {
                let field = read_i64(p, DATA_FIELDS_OFFSET + i * 8);
                // A scalar (raw) field is copied as bits; a uniform one is dup'd.
                if !(i < 64 && old_bitmap & (1u64 << i) != 0) {
                    fai_dup(field);
                }
                write_i64(q, DATA_FIELDS_OFFSET + i * 8, field);
            }
        }
        // Release this reference; dropping it releases the copied-out uniform fields
        // once (balancing the dups), skips scalar slots, and releases the replaced
        // field once when it was uniform.
        fai_drop(record);
        from_obj(q)
    }
}

// ---------------------------------------------------------------------------
// Arrays: a contiguous, growable sequence.
// ---------------------------------------------------------------------------
//
// Layout: the object header, then `length` (the live element count) at
// `ARRAY_LEN_OFFSET`, then the element slots at `ARRAY_ELEMS_OFFSET`. Capacity —
// the number of slots the allocation holds — is *derived* from the object's size
// (`(size - ARRAY_ELEMS_OFFSET) / 8`), not stored, so a unique array grows in
// place into its spare capacity. Only slots `0..length` are live (initialized,
// reference-counted, compared); the slots beyond are uninitialized spare.
//
// Mutation follows the same uniqueness rule as `fai_record_update`: the unique
// owner mutates in place, a shared array is copied. An out-of-bounds index aborts
// (a value error, like division by zero); the standard library's safe `get`/`set`
// bounds-check first and never reach that path.

/// An array's live element count.
///
/// # Safety
/// `v` is a boxed `Array`.
unsafe fn array_len(v: Value) -> usize {
    // SAFETY: a boxed array stores its length at `ARRAY_LEN_OFFSET`.
    unsafe { read_u64(as_obj(v), ARRAY_LEN_OFFSET) as usize }
}

/// An array's capacity (element slots the allocation holds), derived from its size.
///
/// # Safety
/// `v` is a boxed `Array`.
unsafe fn array_cap(v: Value) -> usize {
    // SAFETY: a boxed array stores its allocation size in the header.
    let size = unsafe { read_u64(as_obj(v), SIZE_OFFSET) } as usize;
    (size - ARRAY_ELEMS_OFFSET) / 8
}

/// Allocates an array object of `length` live slots within `cap` total capacity
/// (slots past `length` are left uninitialized for in-place growth). `cap` must be
/// at least `length`.
fn alloc_array(length: usize, cap: usize) -> *mut u8 {
    debug_assert!(cap >= length);
    let p = alloc_obj(ARRAY_ELEMS_OFFSET + cap * 8, &FAI_ARRAY_DESC);
    // SAFETY: `p` has room for the length field and `cap` slots.
    unsafe { write_u64(p, ARRAY_LEN_OFFSET, length as u64) };
    p
}

/// The capacity to grow to so an array of capacity `cap` can hold `needed`
/// elements: double from a small base until it fits.
fn grow_cap(cap: usize, needed: usize) -> usize {
    let mut c = if cap == 0 { 4 } else { cap };
    while c < needed {
        c *= 2;
    }
    c
}

/// Builds an empty array with room for `cap` elements (`Array.withCapacity`).
/// Capacity is a hint: pushes within it append in place, beyond it grow. Consumes
/// the immediate `cap`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_with_capacity(cap: Value) -> Value {
    let cap = unbox_int(cap).max(0) as usize;
    from_obj(alloc_array(0, cap))
}

/// An array's length as an immediate `Int` (operand consumed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_length(arr: Value) -> Value {
    // SAFETY: `arr` is a boxed array (guaranteed by typing).
    let len = unsafe { array_len(arr) };
    fai_drop(arr);
    imm_int(len as i64)
}

/// An array's length, *borrowing* the operand (the caller releases it).
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_length_borrowed(arr: Value) -> Value {
    // SAFETY: `arr` is a boxed array.
    imm_int(unsafe { array_len(arr) } as i64)
}

/// Aborts on an out-of-bounds array index — a value error the well-typed safe API
/// guards against (the unchecked fast path and standard-library internals reach
/// this only on a genuine bug, never from user code).
fn array_bounds_check(index: usize, len: usize) {
    if index >= len {
        fai_panic("array index out of bounds");
    }
}

/// Element `index`, returning an owned reference and consuming `arr` (the element
/// is duplicated so it outlives `arr`'s drop). Out-of-bounds aborts.
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_get(arr: Value, index: Value) -> Value {
    let index = unbox_int(index) as usize;
    // SAFETY: `arr` is a boxed array; the slot is in bounds after the check.
    let elem = unsafe {
        array_bounds_check(index, array_len(arr));
        read_i64(as_obj(arr), ARRAY_ELEMS_OFFSET + index * 8)
    };
    fai_dup(elem);
    fai_drop(arr);
    elem
}

/// Element `index`, *borrowing* `arr` (the caller releases it); the element is
/// duplicated so the returned reference is owned independently. Out-of-bounds
/// aborts.
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_get_borrowed(arr: Value, index: Value) -> Value {
    let index = unbox_int(index) as usize;
    // SAFETY: `arr` is a boxed array; the slot is in bounds after the check.
    let elem = unsafe {
        array_bounds_check(index, array_len(arr));
        read_i64(as_obj(arr), ARRAY_ELEMS_OFFSET + index * 8)
    };
    fai_dup(elem)
}

/// Replaces element `index` with `value`, consuming `arr` and `value`. The unique
/// owner overwrites in place (releasing the old element); a shared array is copied
/// with the element replaced. Out-of-bounds aborts.
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_set(arr: Value, index: Value, value: Value) -> Value {
    let index = unbox_int(index) as usize;
    // SAFETY: `arr` is a boxed array; the slot is in bounds after the check.
    unsafe {
        let len = array_len(arr);
        array_bounds_check(index, len);
        let p = as_obj(arr);
        // Unique owner: overwrite the slot in place, releasing the old element.
        if read_u64(p, RC_OFFSET) == 1 {
            let old = read_i64(p, ARRAY_ELEMS_OFFSET + index * 8);
            write_i64(p, ARRAY_ELEMS_OFFSET + index * 8, value);
            fai_drop(old);
            return arr;
        }
        // Shared: copy the live elements with the one at `index` replaced. This is
        // a uniqueness-loss buffer duplication (O(length) work), counted so a
        // mutation driven through code that fails to keep the array unique is
        // observable even though it is a single allocation.
        note_array_copy();
        let q = alloc_array(len, len);
        for i in 0..len {
            if i == index {
                write_i64(q, ARRAY_ELEMS_OFFSET + i * 8, value);
            } else {
                let e = read_i64(p, ARRAY_ELEMS_OFFSET + i * 8);
                fai_dup(e);
                write_i64(q, ARRAY_ELEMS_OFFSET + i * 8, e);
            }
        }
        // Release this reference: drops the copied-out elements once (balancing the
        // dups) and the replaced old element once.
        fai_drop(arr);
        from_obj(q)
    }
}

/// Appends `value`, consuming `arr` and `value`. The unique owner appends in place
/// when it has spare capacity and grows (reallocates) when full; a shared array is
/// copied with room for the new element.
#[unsafe(no_mangle)]
pub extern "C" fn fai_array_push(arr: Value, value: Value) -> Value {
    // SAFETY: `arr` is a boxed array.
    unsafe {
        let len = array_len(arr);
        let cap = array_cap(arr);
        let p = as_obj(arr);
        let unique = read_u64(p, RC_OFFSET) == 1;
        if unique && len < cap {
            // In place: write the spare slot and bump the length.
            write_i64(p, ARRAY_ELEMS_OFFSET + len * 8, value);
            write_u64(p, ARRAY_LEN_OFFSET, (len + 1) as u64);
            return arr;
        }
        let q = alloc_array(len + 1, grow_cap(cap, len + 1));
        if unique {
            // Unique but full: move the elements into the larger buffer (no
            // dup/drop — ownership transfers) and reclaim the old memory directly.
            std::ptr::copy_nonoverlapping(
                p.add(ARRAY_ELEMS_OFFSET),
                q.add(ARRAY_ELEMS_OFFSET),
                len * 8,
            );
            write_i64(q, ARRAY_ELEMS_OFFSET + len * 8, value);
            free_obj(p);
        } else {
            // Shared: copy the elements (dup each — now shared with the original),
            // then release this reference (balancing the dups). A uniqueness-loss
            // buffer duplication (counted), as opposed to the unique-but-full grow
            // above, which is expected amortized growth.
            note_array_copy();
            for i in 0..len {
                let e = read_i64(p, ARRAY_ELEMS_OFFSET + i * 8);
                fai_dup(e);
                write_i64(q, ARRAY_ELEMS_OFFSET + i * 8, e);
            }
            write_i64(q, ARRAY_ELEMS_OFFSET + len * 8, value);
            fai_drop(arr);
        }
        from_obj(q)
    }
}

/// Structural equality of two boxed arrays (equal length, elementwise equal).
///
/// # Safety
/// `a` and `b` are boxed arrays.
unsafe fn array_equal(a: Value, b: Value) -> bool {
    // SAFETY: both are boxed arrays; only `0..length` is live.
    unsafe {
        let n = array_len(a);
        if n != array_len(b) {
            return false;
        }
        for i in 0..n {
            let ea = read_i64(as_obj(a), ARRAY_ELEMS_OFFSET + i * 8);
            let eb = read_i64(as_obj(b), ARRAY_ELEMS_OFFSET + i * 8);
            if !values_equal(ea, eb) {
                return false;
            }
        }
        true
    }
}

/// Lexicographic ordering of two boxed arrays — element by element, and on a
/// shared prefix the shorter array is less (matching `List`/`String`).
///
/// # Safety
/// `a` and `b` are boxed arrays.
unsafe fn array_compare(a: Value, b: Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // SAFETY: both are boxed arrays; only `0..length` is live.
    unsafe {
        let (na, nb) = (array_len(a), array_len(b));
        for i in 0..na.min(nb) {
            let ea = read_i64(as_obj(a), ARRAY_ELEMS_OFFSET + i * 8);
            let eb = read_i64(as_obj(b), ARRAY_ELEMS_OFFSET + i * 8);
            match values_compare(ea, eb) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        na.cmp(&nb)
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

/// Builds a data value carrying the per-shape descriptor `desc` (scalar bitmap),
/// reusing `token`'s memory in place when it is a non-null token of exactly the
/// right size, else allocating fresh. The scalar peer of [`fai_reuse`]: a reused
/// cell's descriptor is overwritten with `desc`, so a token from a differently
/// shaped (same-size) cell rebuilds correctly. Scalar slots in `fields` are raw
/// `f64` bits; uniform slots are owned values.
///
/// # Safety
/// As [`fai_reuse`]; `desc` is a valid `KIND_DATA` descriptor matching the layout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fai_reuse_scalar(
    desc: *const Descriptor,
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
            // SAFETY: `p` has room for the header, tag, and `n` fields; its children
            // were already released by `fai_drop_reuse` (per the old descriptor).
            unsafe {
                write_u64(p, RC_OFFSET, 1);
                write_ptr(p, DESC_OFFSET, desc.cast());
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
    // SAFETY: `fields` points to `n` words (owned unless scalar).
    unsafe { fai_make_data_scalar(desc, tag, nfields, fields) }
}

/// Frees a reuse token produced by [`fai_drop_reuse`] that no [`fai_reuse`] will
/// consume — reclaiming the held cell's memory — or a no-op on the null token
/// (the cell was shared, so nothing was reset). A non-null token is a reset cell's
/// memory: reference count zero with its children already released by
/// [`fai_drop_reuse`], which is exactly [`free_obj`]'s precondition.
#[unsafe(no_mangle)]
pub extern "C" fn fai_free_reuse(token: Value) {
    if token != NO_REUSE {
        // SAFETY: a non-null token is a reset object's memory (rc 0, childless), so
        // its size header is valid and `free_obj` reclaims it correctly.
        unsafe { free_obj(token as usize as *mut u8) };
    }
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

/// Allocates a `String` object of `len` live bytes within `cap_bytes` of inline
/// capacity (rc = 1). The spare past `len` is left uninitialized, available for
/// in-place append (like an `Array`'s spare slots). `cap_bytes` must be at least
/// `len`. The allocation is rounded up to the heap alignment, so even a tight
/// string carries a few bytes of slack that its length never reflects.
fn alloc_string(len: usize, cap_bytes: usize) -> *mut u8 {
    debug_assert!(cap_bytes >= len);
    let size = (STRING_BYTES_OFFSET + cap_bytes + ALIGN - 1) & !(ALIGN - 1);
    let p = alloc_obj(size.max(STRING_BYTES_OFFSET), &FAI_STRING_DESC);
    // SAFETY: `p` has room for the length field and `cap_bytes` content bytes.
    unsafe { write_u64(p, STRING_LEN_OFFSET, len as u64) };
    p
}

/// Allocates a tight `String` object from `bytes` (rc = 1).
fn make_string(bytes: &[u8]) -> Value {
    let len = bytes.len();
    let p = alloc_string(len, len);
    // SAFETY: `p` has room for `len` content bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(STRING_BYTES_OFFSET), len) };
    from_obj(p)
}

/// Whether `kind` is one of the two `String` representations — an inline string or
/// a borrowing slice — which compare and order by content regardless of layout.
fn is_string_kind(kind: u64) -> bool {
    kind == KIND_STRING || kind == KIND_STRING_SLICE
}

/// Whether `v` is a borrowing substring view rather than an inline `String`.
///
/// # Safety
/// `v` is a boxed string-like value (an inline `String` or a slice).
unsafe fn is_string_slice(v: Value) -> bool {
    // SAFETY: `v` is a live boxed value with a valid descriptor.
    unsafe { desc_kind(obj_descriptor(as_obj(v))) == KIND_STRING_SLICE }
}

/// Borrows a boxed string value as a byte slice: an inline `String`'s own bytes,
/// or, for a borrowing slice, the viewed window of its (inline) base's bytes. The
/// byte length is at `STRING_LEN_OFFSET` for both representations.
unsafe fn string_bytes<'a>(v: Value) -> &'a [u8] {
    let p = as_obj(v);
    // SAFETY: `v` is a boxed string-like value; its length is inline at
    // `STRING_LEN_OFFSET`, and a slice's base is an inline string.
    unsafe {
        let len = read_u64(p, STRING_LEN_OFFSET) as usize;
        if desc_kind(obj_descriptor(p)) == KIND_STRING_SLICE {
            let base = as_obj(read_i64(p, SLICE_BASE_OFFSET));
            let off = read_u64(p, SLICE_OFFSET_OFFSET) as usize;
            std::slice::from_raw_parts(base.add(STRING_BYTES_OFFSET + off), len)
        } else {
            std::slice::from_raw_parts(p.add(STRING_BYTES_OFFSET), len)
        }
    }
}

/// A boxed `String`'s inline byte capacity (the content bytes the allocation can
/// hold), derived from its size header. Bytes `0..length` are live; the rest is
/// spare for in-place append.
///
/// # Safety
/// `v` is a boxed `String`.
unsafe fn string_cap(v: Value) -> usize {
    // SAFETY: a boxed string stores its allocation size in the header.
    let size = unsafe { read_u64(as_obj(v), SIZE_OFFSET) } as usize;
    size - STRING_BYTES_OFFSET
}

/// Concatenates two `String`s (both operands consumed). When the left operand is
/// uniquely owned this appends in place — into its spare capacity, or, when full,
/// into a freshly grown (doubled) buffer whose old memory is then reclaimed — so
/// building a string by repeated concatenation onto a unique accumulator is
/// amortized O(total length) rather than re-copying the whole accumulator at each
/// step. A *shared* left operand is forked into a fresh tight buffer (a
/// uniqueness-loss copy). Concatenation with the empty string returns the other
/// operand without copying.
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_concat(a: Value, b: Value) -> Value {
    // SAFETY: `a` and `b` are boxed `String`s (guaranteed by typing). A uniquely
    // owned left operand is a distinct allocation from `b` (aliasing would make it
    // shared, rc != 1), so the in-place append never overlaps `b`.
    unsafe {
        let (pa, pb) = (as_obj(a), as_obj(b));
        let la = read_u64(pa, STRING_LEN_OFFSET) as usize;
        let lb = read_u64(pb, STRING_LEN_OFFSET) as usize;

        // Empty operand: the result is the other operand (release the empty one).
        if lb == 0 {
            fai_drop(b);
            return a;
        }
        if la == 0 {
            fai_drop(a);
            return b;
        }

        // Both operands' bytes, resolved uniformly (either may be a slice view).
        let (a_ptr, b_ptr) = (string_bytes(a).as_ptr(), string_bytes(b).as_ptr());
        let need = la + lb;
        // In place only when the left operand is an inline, uniquely-owned buffer
        // with spare capacity: a slice views a shared base and owns no extensible
        // buffer, so it can never be appended into.
        let inline_unique = !is_string_slice(a) && read_u64(pa, RC_OFFSET) == 1;
        if inline_unique && need <= string_cap(a) {
            // Append `b`'s bytes after `a`'s and bump the length — no allocation,
            // `a`'s bytes not re-copied.
            std::ptr::copy_nonoverlapping(b_ptr, pa.add(STRING_BYTES_OFFSET + la), lb);
            write_u64(pa, STRING_LEN_OFFSET, need as u64);
            fai_drop(b);
            return a;
        }

        let q = if inline_unique {
            // Unique inline but full: grow into a doubled buffer so further appends
            // amortize. Expected growth, not a uniqueness-loss copy — uncounted.
            alloc_string(need, grow_cap(string_cap(a), need))
        } else {
            // Shared, or a slice (which never owns extensible capacity): fork a
            // fresh tight buffer. A uniqueness-loss copy (counted).
            note_string_copy();
            alloc_string(need, need)
        };
        std::ptr::copy_nonoverlapping(a_ptr, q.add(STRING_BYTES_OFFSET), la);
        std::ptr::copy_nonoverlapping(b_ptr, q.add(STRING_BYTES_OFFSET + la), lb);
        if inline_unique {
            // Ownership of `a`'s bytes moved into `q`; reclaim its old memory
            // directly (no child scan — an inline string is a leaf).
            free_obj(pa);
        } else {
            // Drop the old left: a shared inline operand decrements; a slice's drop
            // also releases its base child.
            fai_drop(a);
        }
        fai_drop(b);
        from_obj(q)
    }
}

/// The smallest piece (in bytes) worth a borrowing view: below this a copy is
/// cheaper than a view header plus the base it pins.
const SLICE_MIN_VIEW_BYTES: usize = 32;
/// A piece is viewed only when it is at least `1 / SLICE_VIEW_RATIO` of its base,
/// so a viewed piece retains at most `SLICE_VIEW_RATIO`× its own bytes; a small
/// piece of a large base is copied (never pinning it).
const SLICE_VIEW_RATIO: usize = 4;

/// Whether a `byte_len`-byte piece of a `base_byte_len`-byte base should be a
/// borrowing view (large enough, both absolutely and as a fraction of the base)
/// rather than an owned copy.
fn should_view(byte_len: usize, base_byte_len: usize) -> bool {
    byte_len >= SLICE_MIN_VIEW_BYTES && byte_len.saturating_mul(SLICE_VIEW_RATIO) >= base_byte_len
}

/// The byte offset of the `char_idx`-th character of `s`, clamped to `[0, len]`
/// (an index at or past the character count yields the byte length; a negative
/// index yields 0). Used to turn the char-indexed slice API into a byte range.
fn char_byte_offset(s: &str, char_idx: i64) -> usize {
    if char_idx <= 0 {
        return 0;
    }
    // `nth(k)` yields the k-th char's byte index, or `None` once exhausted.
    s.char_indices().nth(char_idx as usize).map_or(s.len(), |(i, _)| i)
}

/// Builds a borrowing slice viewing `byte_len` bytes of `base` from byte offset
/// `byte_off`. `base` is **borrowed**: the slice takes its own reference to the
/// inline base it ends up viewing. Slicing a slice flattens to the underlying
/// inline base (adding the offsets), so a slice's base is always an inline
/// `String` and [`string_bytes`] never recurses. Counts one view.
///
/// # Safety
/// `base` is a boxed string-like value; `byte_off + byte_len` is within its bytes.
unsafe fn make_string_slice(base: Value, byte_off: usize, byte_len: usize) -> Value {
    note_string_view();
    // SAFETY: `base` is string-like; a slice's base field is an inline string.
    unsafe {
        let (inline_base, total_off) = if is_string_slice(base) {
            let pb = as_obj(base);
            (read_i64(pb, SLICE_BASE_OFFSET), read_u64(pb, SLICE_OFFSET_OFFSET) as usize + byte_off)
        } else {
            (base, byte_off)
        };
        fai_dup(inline_base); // the slice holds its own reference to the inline base
        let p = alloc_obj(SLICE_OFFSET_OFFSET + 8, &FAI_STRING_SLICE_DESC);
        write_u64(p, STRING_LEN_OFFSET, byte_len as u64);
        write_i64(p, SLICE_BASE_OFFSET, inline_base);
        write_u64(p, SLICE_OFFSET_OFFSET, total_off as u64);
        from_obj(p)
    }
}

/// Builds the `byte_off..byte_off+byte_len` piece of `base` (borrowed): a borrowing
/// view when the piece is large relative to the ultimate inline base, else an owned
/// copy. The retention ratio is measured against the inline base that a view would
/// pin, so slicing an existing slice does not view a tiny window of a huge buffer.
///
/// # Safety
/// `base` is a boxed string-like value; the byte range is within its bytes.
unsafe fn make_piece(base: Value, byte_off: usize, byte_len: usize) -> Value {
    // SAFETY: `base` is string-like; the range is valid.
    unsafe {
        let inline_base_len = if is_string_slice(base) {
            read_u64(as_obj(read_i64(as_obj(base), SLICE_BASE_OFFSET)), STRING_LEN_OFFSET) as usize
        } else {
            read_u64(as_obj(base), STRING_LEN_OFFSET) as usize
        };
        if should_view(byte_len, inline_base_len) {
            make_string_slice(base, byte_off, byte_len)
        } else {
            make_string(&string_bytes(base)[byte_off..byte_off + byte_len])
        }
    }
}

/// `String.substring`: the `len`-character substring of `s` starting at character
/// `start` (both clamped). A large piece is a borrowing view; a small one is
/// copied. Operand `s` consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_substring(start: Value, len: Value, s: Value) -> Value {
    let (cstart, clen) = (unbox_int(start), unbox_int(len));
    // SAFETY: `s` is a boxed string-like value of valid UTF-8.
    unsafe {
        let str = string_str(s);
        let total = str.len();
        let b0 = char_byte_offset(str, cstart);
        let b1 = if clen <= 0 { b0 } else { char_byte_offset(str, cstart.saturating_add(clen)) };
        if b0 == 0 && b1 == total {
            return s; // the whole string: hand the operand back
        }
        let piece = make_piece(s, b0, b1 - b0);
        fai_drop(s);
        piece
    }
}

/// `String.take`: the first `n` characters of `s` (a large prefix is a view, a
/// small one is copied). Operand consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_take(n: Value, s: Value) -> Value {
    let n = unbox_int(n);
    // SAFETY: `s` is a boxed string-like value of valid UTF-8.
    unsafe {
        let end = char_byte_offset(string_str(s), n);
        if end >= string_bytes(s).len() {
            return s; // n >= length: the whole string
        }
        let piece = make_piece(s, 0, end);
        fai_drop(s);
        piece
    }
}

/// `String.drop`: all but the first `n` characters of `s` (the kept suffix is a
/// view when large). Operand consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_drop(n: Value, s: Value) -> Value {
    let n = unbox_int(n);
    // SAFETY: `s` is a boxed string-like value of valid UTF-8.
    unsafe {
        let total = string_bytes(s).len();
        let start = char_byte_offset(string_str(s), n);
        if start == 0 {
            return s; // n <= 0: drop nothing
        }
        if start >= total {
            fai_drop(s);
            return make_string(b""); // n >= length: dropped everything
        }
        let piece = make_piece(s, start, total - start);
        fai_drop(s);
        piece
    }
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
/// caller releases them at their last use). Each piece is built with [`make_piece`]
/// — a borrowing view sharing `s`'s buffer when it is large enough, an owned copy
/// otherwise — so splitting into a few large pieces avoids copying their bytes
/// (many small pieces, e.g. words, fall below the threshold and are copied).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_split_borrowed(sep: Value, s: Value) -> Value {
    // SAFETY: both are boxed string-like values; the computed byte ranges of the
    // pieces lie within `s`'s bytes.
    let pieces: Vec<Value> = unsafe {
        let (sep_s, src) = (string_str(sep), string_str(s));
        if sep_s.is_empty() {
            // Each character is its own piece (always tiny, hence copied).
            src.char_indices().map(|(i, c)| make_piece(s, i, c.len_utf8())).collect()
        } else {
            // The spans between (and around) the separators, by byte offset.
            let mut pieces = Vec::new();
            let mut start = 0;
            for (idx, _) in src.match_indices(sep_s) {
                pieces.push(make_piece(s, start, idx - start));
                start = idx + sep_s.len();
            }
            pieces.push(make_piece(s, start, src.len() - start));
            pieces
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
    let desc = unsafe { obj_descriptor(p) };

    if unsafe { desc_kind(desc) } == KIND_PAP {
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

    if unsafe { desc_kind(desc) } != KIND_CLOSURE {
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
    let kind = unsafe { desc_kind(obj_descriptor(as_obj(v))) };
    kind == KIND_CLOSURE || kind == KIND_PAP
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
                let ka = desc_kind(obj_descriptor(as_obj(a)));
                let kb = desc_kind(obj_descriptor(as_obj(b)));
                // An inline `String` and a borrowing slice are both `String` values:
                // compare them by content regardless of representation (so a sliced
                // `"abc"` equals an inline `"abc"`, e.g. as `Dict` keys).
                if is_string_kind(ka) && is_string_kind(kb) {
                    return string_bytes(a) == string_bytes(b);
                }
                // Different kinds are never equal (comparison is between same-typed
                // values, so two data cells share a kind regardless of per-shape
                // descriptor identity).
                if ka != kb {
                    return false;
                }
                match ka {
                    KIND_INT => {
                        read_i64(as_obj(a), INT_VALUE_OFFSET)
                            == read_i64(as_obj(b), INT_VALUE_OFFSET)
                    }
                    KIND_FLOAT => {
                        read_u64(as_obj(a), FLOAT_VALUE_OFFSET)
                            == read_u64(as_obj(b), FLOAT_VALUE_OFFSET)
                    }
                    KIND_DATA => data_equal(a, b),
                    KIND_ARRAY => array_equal(a, b),
                    // Two niche `None` sentinels (the single shared object): equal.
                    // A `None` and a `Some` differ in kind, handled by `ka != kb`.
                    KIND_NONE => true,
                    _ => false,
                }
            }
        }
        // A small immediate Int can never equal a boxed (overflowed) one, and a
        // nullary constructor (immediate) never equals a non-nullary one (boxed).
        _ => false,
    }
}

/// Structural equality of two boxed data values (same tag and equal fields). A
/// scalar `f64` slot is compared by its bits (the same equality a boxed `Float`
/// uses); a uniform slot recurses through [`values_equal`].
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
        // Both values share a type, hence a scalar bitmap; read it from `a`.
        let scalar = desc_scalar_bitmap(obj_descriptor(as_obj(a)));
        for i in 0..n {
            let fa = read_i64(as_obj(a), DATA_FIELDS_OFFSET + i * 8);
            let fb = read_i64(as_obj(b), DATA_FIELDS_OFFSET + i * 8);
            if i < 64 && scalar & (1u64 << i) != 0 {
                // Scalar float slot: compare the raw bits (a boxed `Float` compares
                // the same way).
                if fa != fb {
                    return false;
                }
            } else if !values_equal(fa, fb) {
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
    // A niche Scheme-B `None` (the sentinel) sorts before any `Some` (matching the
    // constructor-tag order None=0 < Some=1); two `None`s are equal.
    let a_none = is_none_sentinel(a);
    let b_none = is_none_sentinel(b);
    if a_none || b_none {
        return match (a_none, b_none) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, _) => Ordering::Greater,
        };
    }
    // Otherwise identify the kind from a boxed operand (both share a type).
    let boxed = if is_boxed(a) { a } else { b };
    // SAFETY: `boxed` is a boxed value.
    let kind = unsafe { desc_kind(obj_descriptor(as_obj(boxed))) };
    if kind == KIND_FLOAT {
        return unbox_float(a).total_cmp(&unbox_float(b));
    }
    if is_string_kind(kind) {
        // SAFETY: both are boxed string-like values; ordering is by content,
        // regardless of inline-vs-slice representation.
        return unsafe { string_bytes(a).cmp(string_bytes(b)) };
    }
    if kind == KIND_ARRAY {
        // SAFETY: both are boxed arrays (same type).
        return unsafe { array_compare(a, b) };
    }
    if kind == KIND_DATA {
        let ta = data_tag(a) >> 1;
        let tb = data_tag(b) >> 1;
        match ta.cmp(&tb) {
            Ordering::Equal => {
                // Same constructor (both boxed): compare fields lexicographically.
                if is_boxed(a) && is_boxed(b) {
                    // SAFETY: both are boxed data values with equal field counts.
                    unsafe {
                        let n = data_field_count(a);
                        // Both share a type, hence a scalar bitmap; read it from `a`.
                        let scalar = desc_scalar_bitmap(obj_descriptor(as_obj(a)));
                        for i in 0..n {
                            let fa = read_i64(as_obj(a), DATA_FIELDS_OFFSET + i * 8);
                            let fb = read_i64(as_obj(b), DATA_FIELDS_OFFSET + i * 8);
                            let ord = if i < 64 && scalar & (1u64 << i) != 0 {
                                // Scalar float slot: compare as `f64` (a boxed
                                // `Float` uses the same `total_cmp`).
                                f64::from_bits(fa as u64).total_cmp(&f64::from_bits(fb as u64))
                            } else {
                                values_compare(fa, fb)
                            };
                            match ord {
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

/// The process's peak resident set size in KiB, or `None` where it cannot be
/// determined. Read from `/proc/self/status` (`VmHWM`, the high-water mark of
/// resident memory), so it reflects the largest physical-memory footprint reached
/// at any point in the run, not the footprint at the instant of the call.
/// Linux-only; every other platform yields `None`.
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
/// of `/proc/self/status`. Split out from [`peak_rss_kib`] so the line parsing is
/// testable on any platform, not only where `/proc` exists.
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
    // Opt-in peak-memory self-report for the Fai-vs-Rust memory comparison: the
    // benchmark harness sets `FAI_REPORT_RSS` in the spawned binary's environment
    // and parses this line from stderr. Off by default, so a normal run (and the
    // in-process JIT path, which never sets it) is unaffected.
    if std::env::var_os("FAI_REPORT_RSS").is_some()
        && let Some(kib) = peak_rss_kib()
    {
        eprintln!("fai-peak-rss-kib: {kib}");
    }
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
mod array_tests;
#[cfg(test)]
mod proptests;
#[cfg(test)]
mod reuse_tests;
#[cfg(test)]
mod scalar_tests;
#[cfg(test)]
mod string_tests;
#[cfg(test)]
mod tests;
