//! `String` runtime unit tests: the in-place amortized-append concatenation.
//!
//! These exercise [`fai_string_concat`] directly — the unique in-place append, the
//! unique-but-full grow, the shared fork, and the empty-operand fast paths — plus
//! aliasing, large (pool-bypassing) builds, and multibyte UTF-8. Every test holds
//! the global [`lock`] and asserts reference-count balance (the live count returns
//! to its start); the in-place/grow/shared tests additionally pin the allocation
//! counter and [`string_copies`] (the uniqueness-loss signal). Both counters are
//! compiled in only under `debug_assertions`, the default for `cargo test`.

use super::*;
use crate::tests::lock;

/// A string's live byte length.
fn len_of(s: Value) -> usize {
    // SAFETY: `s` is a boxed string.
    unsafe { read_u64(as_obj(s), STRING_LEN_OFFSET) as usize }
}

/// A string's inline byte capacity (live bytes plus spare).
fn cap_of(s: Value) -> usize {
    // SAFETY: `s` is a boxed string.
    unsafe { string_cap(s) }
}

/// A string's content as an owned `String` (the test holds the lock, so borrowing
/// the bytes for the copy is sound).
fn contents(s: Value) -> String {
    // SAFETY: `s` is a boxed `String` of valid UTF-8.
    unsafe { string_str(s) }.to_owned()
}

// ===========================================================================
// Unique left operand: append in place, growing (doubling) when full.
// ===========================================================================

#[test]
fn concat_appends_in_place_when_unique_with_spare() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    // A 3-byte string rounds up to 8 bytes of capacity, so it has spare room.
    let a = make_string(b"abc");
    assert!(cap_of(a) >= 5, "a tight short string has alignment slack to append into");
    let b = make_string(b"de");
    let before = allocations();
    let r = fai_string_concat(a, b);
    assert_eq!(allocations() - before, 0, "a unique in-place append allocates nothing");
    assert_eq!(string_copies(), 0, "a unique in-place append copies nothing");
    assert_eq!(r, a, "the result reuses the left buffer in place");
    assert_eq!(contents(r), "abcde");
    assert_eq!(len_of(r), 5);
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn concat_grows_with_doubled_capacity_when_unique_but_full() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    // An 8-byte string fills its 8-byte capacity exactly: the next append must grow.
    let a = make_string(b"abcdefgh");
    assert_eq!(len_of(a), 8);
    assert_eq!(cap_of(a), 8, "a multiple-of-8 string is full (no slack)");
    let b = make_string(b"xy");
    let before = allocations();
    let r = fai_string_concat(a, b);
    assert_eq!(allocations() - before, 1, "a full unique buffer grows into one new buffer");
    assert_eq!(
        string_copies(),
        0,
        "amortized growth of a unique buffer is not a uniqueness-loss copy"
    );
    assert_ne!(r, a, "growth moves to a fresh buffer");
    assert_eq!(contents(r), "abcdefghxy");
    assert_eq!(cap_of(r), 16, "capacity doubled so further appends amortize");
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

// ===========================================================================
// Shared left operand: fork a fresh tight buffer, original unchanged.
// ===========================================================================

#[test]
fn concat_forks_a_copy_when_left_is_shared() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let a = make_string(b"abc");
    fai_dup(a); // a second owner: the left operand is now shared (rc == 2).
    let b = make_string(b"de");
    let before = allocations();
    let r = fai_string_concat(a, b);
    assert_eq!(allocations() - before, 1, "a shared concat forks one buffer");
    assert_eq!(string_copies(), 1, "a shared concat is a counted uniqueness-loss copy");
    assert_ne!(r, a, "the shared operand is not mutated");
    assert_eq!(contents(r), "abcde");
    assert_eq!(contents(a), "abc", "the shared left operand is left intact");
    fai_drop(a); // release the surviving owner
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn concat_of_an_aliased_string_takes_the_shared_path() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = make_string(b"xy");
    fai_dup(s); // pass the same object as both operands (rc == 2), as codegen would.
    let r = fai_string_concat(s, s);
    assert_eq!(contents(r), "xyxy");
    assert_eq!(string_copies(), 1, "an aliased (shared) left operand forks rather than mutating");
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free (both passed references consumed)");
}

// ===========================================================================
// Empty-operand fast paths: the result is the other operand, no allocation.
// ===========================================================================

#[test]
fn concat_with_empty_right_returns_the_left_operand() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let a = make_string(b"abc");
    let empty = make_string(b"");
    let before = allocations();
    let r = fai_string_concat(a, empty);
    assert_eq!(allocations() - before, 0, "concatenating the empty string allocates nothing");
    assert_eq!(string_copies(), 0);
    assert_eq!(r, a, "the result is the left operand");
    assert_eq!(contents(r), "abc");
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free (the empty operand was released)");
}

#[test]
fn concat_with_empty_left_returns_the_right_operand() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let empty = make_string(b"");
    let a = make_string(b"abc");
    let before = allocations();
    let r = fai_string_concat(empty, a);
    assert_eq!(allocations() - before, 0, "concatenating onto the empty string allocates nothing");
    assert_eq!(r, a, "the result is the right operand");
    assert_eq!(contents(r), "abc");
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free (the empty operand was released)");
}

// ===========================================================================
// Large (pool-bypassing) unique build, and multibyte UTF-8.
// ===========================================================================

#[test]
fn unique_build_past_the_pool_size_never_loses_uniqueness() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    // Build well past MAX_POOLED_SIZE (512 B) by repeated append onto a unique
    // accumulator: every step is an in-place append or an amortized grow, so the
    // build never forks (zero uniqueness-loss copies) however large it gets.
    let mut s = make_string(b"start");
    for _ in 0..200 {
        s = fai_string_concat(s, make_string(b"0123456789"));
    }
    assert_eq!(len_of(s), 5 + 200 * 10);
    assert!(contents(s).starts_with("start0123456789"));
    assert!(contents(s).ends_with("0123456789"));
    assert_eq!(
        string_copies(),
        0,
        "a unique builder never loses uniqueness, even past the pool size"
    );
    fai_drop(s);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn concat_preserves_multibyte_utf8() {
    let _g = lock();
    let base = live_count();
    let a = make_string("héllo".as_bytes());
    let b = make_string("→世界".as_bytes());
    let r = fai_string_concat(a, b);
    assert_eq!(contents(r), "héllo→世界");
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}
