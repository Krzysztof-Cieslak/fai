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
//! descriptor points at a static [`Descriptor`] whose `scan` decrements the
//! object's reference-counted children before it is freed. Reference counting is
//! plain (no reuse): [`fai_dup`]/[`fai_drop`] are tag-checked, so immediates are
//! no-ops and polymorphic code reference-counts correctly with no type
//! information.
//!
//! Functions are closures `{ header, code, arity, env_count, env… }`; every
//! application goes through [`fai_apply_n`], which matches the argument count to
//! the arity (exact call / partial-application closure / over-application).

use std::alloc::Layout;
use std::sync::Mutex;
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

/// A heap-type descriptor: a static record identifying a boxed value's kind and
/// how to release its reference-counted children. Referenced by address from
/// every object header (and, for static objects, from generated code).
#[repr(C)]
pub struct Descriptor {
    /// A human-readable kind name (used in leak/debug reporting).
    pub name: &'static str,
    /// Releases the object's reference-counted children, if any. `None` for leaf
    /// objects (`String`, boxed `Int`). Called once, when the count hits zero,
    /// just before the object is freed.
    pub scan: Option<unsafe extern "C" fn(*mut u8)>,
}

// SAFETY: a `Descriptor` holds only a `&'static str` and an optional function
// pointer, both of which are `Sync`; it carries no interior mutability.
unsafe impl Sync for Descriptor {}

/// Descriptor for `String` objects (leaf: inline bytes, no children).
#[unsafe(no_mangle)]
pub static FAI_STRING_DESC: Descriptor = Descriptor { name: "String", scan: None };

/// Descriptor for boxed (overflowed) `Int` objects (leaf).
#[unsafe(no_mangle)]
pub static FAI_INT_DESC: Descriptor = Descriptor { name: "Int", scan: None };

/// Descriptor for closures (children: the captured environment slots).
#[unsafe(no_mangle)]
pub static FAI_CLOSURE_DESC: Descriptor = Descriptor { name: "Closure", scan: Some(closure_scan) };

/// Descriptor for partial applications (children: the target plus stored args).
#[unsafe(no_mangle)]
pub static FAI_PAP_DESC: Descriptor = Descriptor { name: "Pap", scan: Some(pap_scan) };

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

/// The number of heap objects currently allocated (debug leak detection).
static LIVE: AtomicI64 = AtomicI64::new(0);

/// Returns the number of live heap objects (used by the leak check and tests).
#[must_use]
pub fn live_count() -> i64 {
    LIVE.load(Ordering::Relaxed)
}

/// Allocates a zeroed object of `size` bytes with `rc = 1` and `descriptor`,
/// returning its pointer. Increments the live counter.
fn alloc_obj(size: usize, descriptor: *const Descriptor) -> *mut u8 {
    let layout = Layout::from_size_align(size, ALIGN).expect("valid layout");
    // SAFETY: `layout` has nonzero size (>= HEADER_SIZE) and valid alignment.
    let p = unsafe { std::alloc::alloc(layout) };
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: `p` points to `size >= HEADER_SIZE` freshly allocated bytes.
    unsafe {
        write_u64(p, RC_OFFSET, 1);
        write_ptr(p, DESC_OFFSET, descriptor.cast());
        write_u64(p, SIZE_OFFSET, size as u64);
    }
    LIVE.fetch_add(1, Ordering::Relaxed);
    p
}

/// Frees an object's backing memory (no child scan) and decrements the live
/// counter.
unsafe fn free_obj(p: *mut u8) {
    // SAFETY: `p` was returned by `alloc_obj`, so the size field is valid.
    let size = unsafe { read_u64(p, SIZE_OFFSET) } as usize;
    let layout = Layout::from_size_align(size, ALIGN).expect("valid layout");
    // SAFETY: `p`/`layout` match the original allocation.
    unsafe { std::alloc::dealloc(p, layout) };
    LIVE.fetch_sub(1, Ordering::Relaxed);
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
            let desc = read_ptr(p, DESC_OFFSET).cast::<Descriptor>();
            if let Some(scan) = (*desc).scan {
                scan(p);
            }
            free_obj(p);
        }
    }
}

/// Releases a closure's captured environment slots.
unsafe extern "C" fn closure_scan(p: *mut u8) {
    // SAFETY: `p` is a live closure object.
    unsafe {
        let env_count = read_u64(p, CLOSURE_ENV_COUNT_OFFSET);
        for i in 0..env_count as usize {
            let slot = read_i64(p, CLOSURE_ENV_OFFSET + i * 8);
            fai_drop(slot);
        }
    }
}

/// Releases a partial application's target and stored arguments.
unsafe extern "C" fn pap_scan(p: *mut u8) {
    // SAFETY: `p` is a live partial-application object.
    unsafe {
        fai_drop(read_i64(p, PAP_FUNC_OFFSET));
        let nargs = read_u64(p, PAP_NARGS_OFFSET);
        for i in 0..nargs as usize {
            fai_drop(read_i64(p, PAP_ARGS_OFFSET + i * 8));
        }
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
        /// Integer arithmetic primitive (operands borrowed).
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(a: Value, b: Value) -> Value {
            let f: fn(i64, i64) -> i64 = $op;
            fai_box_int(f(unbox_int(a), unbox_int(b)))
        }
    };
}

int_binop!(fai_int_add, |a, b| a.wrapping_add(b));
int_binop!(fai_int_sub, |a, b| a.wrapping_sub(b));
int_binop!(fai_int_mul, |a, b| a.wrapping_mul(b));

/// Integer division (operands borrowed); aborts on division by zero.
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_div(a: Value, b: Value) -> Value {
    let d = unbox_int(b);
    if d == 0 {
        fai_panic("integer division by zero");
    }
    fai_box_int(unbox_int(a).wrapping_div(d))
}

/// Integer remainder (operands borrowed); aborts on division by zero.
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_rem(a: Value, b: Value) -> Value {
    let d = unbox_int(b);
    if d == 0 {
        fai_panic("integer remainder by zero");
    }
    fai_box_int(unbox_int(a).wrapping_rem(d))
}

/// Encodes a Rust `bool` as a Fai `Bool` immediate.
#[inline]
fn from_bool(b: bool) -> Value {
    imm_int(i64::from(b))
}

macro_rules! int_cmp {
    ($name:ident, $op:tt) => {
        /// Integer comparison primitive, returning a `Bool` (operands borrowed).
        #[unsafe(no_mangle)]
        pub extern "C" fn $name(a: Value, b: Value) -> Value {
            from_bool(unbox_int(a) $op unbox_int(b))
        }
    };
}

int_cmp!(fai_int_lt, <);
int_cmp!(fai_int_le, <=);
int_cmp!(fai_int_gt, >);
int_cmp!(fai_int_ge, >=);

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

/// Concatenates two `String`s into a fresh one (operands borrowed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_string_concat(a: Value, b: Value) -> Value {
    // SAFETY: `a` and `b` are boxed `String`s (guaranteed by typing).
    let (ab, bb) = unsafe { (string_bytes(a), string_bytes(b)) };
    let mut out = Vec::with_capacity(ab.len() + bb.len());
    out.extend_from_slice(ab);
    out.extend_from_slice(bb);
    make_string(&out)
}

/// Renders an `Int` as a `String` (operand borrowed).
#[unsafe(no_mangle)]
pub extern "C" fn fai_int_to_string(n: Value) -> Value {
    make_string(unbox_int(n).to_string().as_bytes())
}

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
        // Steal the stored target and arguments, free the shell without
        // scanning (ownership has moved), then apply the combined arguments.
        // SAFETY: `p` is a partial application.
        unsafe {
            let func = read_i64(p, PAP_FUNC_OFFSET);
            let stored = read_u64(p, PAP_NARGS_OFFSET);
            let total = stored + argc;
            let mut combined: Vec<i64> = Vec::with_capacity(total as usize);
            for i in 0..stored as usize {
                combined.push(read_i64(p, PAP_ARGS_OFFSET + i * 8));
            }
            for i in 0..argc as usize {
                combined.push(*args.add(i));
            }
            free_obj(p);
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
/// borrowed). Function equality is rejected by the type checker, so closures are
/// never compared here.
#[unsafe(no_mangle)]
pub extern "C" fn fai_equal(a: Value, b: Value) -> Value {
    from_bool(values_equal(a, b))
}

fn values_equal(a: Value, b: Value) -> bool {
    match (is_boxed(a), is_boxed(b)) {
        (false, false) => a == b,
        (true, true) => {
            // SAFETY: both are boxed values.
            unsafe {
                let da = read_ptr(as_obj(a), DESC_OFFSET).cast::<Descriptor>();
                let db = read_ptr(as_obj(b), DESC_OFFSET).cast::<Descriptor>();
                if std::ptr::eq(da, &FAI_STRING_DESC) && std::ptr::eq(db, &FAI_STRING_DESC) {
                    string_bytes(a) == string_bytes(b)
                } else if std::ptr::eq(da, &FAI_INT_DESC) && std::ptr::eq(db, &FAI_INT_DESC) {
                    read_i64(as_obj(a), INT_VALUE_OFFSET) == read_i64(as_obj(b), INT_VALUE_OFFSET)
                } else {
                    false
                }
            }
        }
        // A small immediate Int can never equal a boxed (overflowed) one.
        _ => false,
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

/// `Console.writeLine`: writes a `String` followed by a newline to the sink. The
/// `Runtime` capability argument is ignored in this build; the string is
/// borrowed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_console_write_line(_runtime: Value, s: Value) {
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

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Runs a program: applies the entry closure (`main : Runtime -> Unit`) to a
/// fresh `Runtime`, drops the result, and reports leaks. Returns a process exit
/// code (0 success, 70 if objects leaked). Consumes `entry`.
///
/// Both the AOT `main` (emitted by codegen) and the JIT runner call this.
#[must_use]
pub fn run_entry(entry: Value) -> i32 {
    let args = [FAI_UNIT];
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

/// C entry shim called from generated `main`: runs the entry closure.
#[unsafe(no_mangle)]
pub extern "C" fn fai_run_main(entry: Value) -> i32 {
    run_entry(entry)
}

#[cfg(test)]
mod tests;
