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

// ===========================================================================
// Borrowing slice views: take/drop/substring/split share the base buffer for a
// large piece (a view) and copy a small one. `string_views()` counts the views.
// ===========================================================================

/// Whether `v` is a borrowing slice view rather than an inline string.
fn is_view(v: Value) -> bool {
    // SAFETY: `v` is a boxed string-like value.
    unsafe { is_string_slice(v) }
}

/// A heap string of `n` ASCII `'a'` bytes.
fn ascii(n: usize) -> Value {
    make_string(&vec![b'a'; n])
}

#[test]
fn take_large_prefix_is_a_view() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = make_string(b"0123456789abcdefghijklmnopqrstuvwxyz0123456789"); // 46 bytes
    let r = fai_string_take(imm_int(40), s); // 40 >= 32 and 40*4 >= 46 -> view
    assert!(is_view(r), "a large prefix is a borrowing view");
    assert_eq!(string_views(), 1);
    assert_eq!(contents(r), "0123456789abcdefghijklmnopqrstuvwxyz0123");
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free (the view released its base)");
}

#[test]
fn take_small_prefix_is_a_copy() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = ascii(200);
    let r = fai_string_take(imm_int(10), s); // 10 < 32 -> copy
    assert!(!is_view(r), "a small prefix is copied, not viewed");
    assert_eq!(string_views(), 0);
    assert_eq!(contents(r).len(), 10);
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn drop_keeps_a_large_suffix_as_a_view() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = ascii(200);
    let r = fai_string_drop(imm_int(10), s); // keeps a 190-byte suffix -> view
    assert!(is_view(r));
    assert_eq!(string_views(), 1);
    assert_eq!(contents(r).len(), 190);
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn substring_large_is_a_view_small_is_a_copy() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = ascii(200);
    let big = fai_string_substring(imm_int(50), imm_int(100), fai_dup(s)); // 100 bytes -> view
    assert!(is_view(big));
    let small = fai_string_substring(imm_int(50), imm_int(8), s); // 8 bytes -> copy
    assert!(!is_view(small));
    assert_eq!(string_views(), 1);
    assert_eq!(contents(big).len(), 100);
    assert_eq!(contents(small).len(), 8);
    fai_drop(big);
    fai_drop(small);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn whole_string_slices_return_the_operand_uncopied() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = ascii(100);
    let r = fai_string_take(imm_int(100), s); // n >= length -> the whole string
    assert_eq!(r, s, "taking the whole string hands the operand back");
    assert_eq!(string_views(), 0, "no view or copy for the whole-string case");
    let r2 = fai_string_drop(imm_int(0), r); // drop nothing -> the whole string
    assert_eq!(r2, s);
    fai_drop(r2);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn a_slice_view_equals_an_inline_string() {
    let _g = lock();
    let base = live_count();
    let s = make_string(b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJ"); // 36 bytes
    let view = fai_string_take(imm_int(34), s); // 34 >= 32 and 34*4 >= 36 -> view
    assert!(is_view(view));
    let same = make_string(b"abcdefghijklmnopqrstuvwxyzABCDEFGH"); // the view's 34 bytes
    assert_eq!(
        fai_equal(fai_dup(view), fai_dup(same)),
        from_bool(true),
        "view == inline by content"
    );
    assert_eq!(fai_compare_borrowed(view, same), imm_int(0), "view and inline order equal");
    let other = make_string(b"abcdefghijklmnopqrstuvwxyzABCDEFGX"); // differs in the last byte
    assert_eq!(fai_equal(fai_dup(view), other), from_bool(false));
    fai_drop(view);
    fai_drop(same);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn slice_of_a_slice_flattens_to_the_inline_base() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let s = ascii(200);
    let v1 = fai_string_drop(imm_int(10), s); // view [10..200)
    assert!(is_view(v1));
    let v2 = fai_string_take(imm_int(100), v1); // a view of a view -> still a view
    assert!(is_view(v2));
    assert_eq!(string_views(), 2);
    assert_eq!(contents(v2), "a".repeat(100), "the slice-of-a-slice reads the right window");
    fai_drop(v2);
    assert_eq!(live_count(), base, "leak-free (v1's base ref transferred through v2)");
}

#[test]
fn concat_with_a_slice_left_operand_forks_to_inline() {
    let _g = lock();
    let base = live_count();
    let s = ascii(100);
    let view = fai_string_take(imm_int(50), s); // consumes s; view holds the base
    assert!(is_view(view));
    let r = fai_string_concat(view, make_string(b"XYZ")); // a slice owns no buffer -> fork
    assert!(!is_view(r), "a concatenation always yields a fresh inline string");
    assert_eq!(contents(r).len(), 53);
    assert!(contents(r).ends_with("XYZ"));
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn concat_reads_a_slice_right_operand() {
    let _g = lock();
    let base = live_count();
    let s = ascii(100);
    let view = fai_string_take(imm_int(50), s);
    let r = fai_string_concat(make_string(b"XYZ"), view); // inline left, slice right
    assert_eq!(contents(r).len(), 53);
    assert!(contents(r).starts_with("XYZ"));
    fai_drop(r);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn split_into_few_large_pieces_yields_views() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    // Two 70-byte sections: each piece is >= 32 bytes and >= 1/4 of the 141-byte
    // base, so both are views.
    let section = "a".repeat(70);
    let src = make_string(format!("{section}|{section}").as_bytes());
    let list = fai_string_split(make_string(b"|"), src);
    assert_eq!(string_views(), 2, "both large pieces are views sharing the base");
    fai_drop(list);
    assert_eq!(live_count(), base, "leak-free (each view released the shared base)");
}

#[test]
fn split_into_many_small_pieces_copies() {
    let _g = lock();
    let base = live_count();
    reset_allocations();
    let src = make_string(b"a,b,c,d,e,f,g"); // seven 1-byte pieces
    let list = fai_string_split(make_string(b","), src);
    assert_eq!(string_views(), 0, "tiny pieces fall below the threshold and are copied");
    fai_drop(list);
    assert_eq!(live_count(), base, "leak-free");
}

#[test]
fn take_and_drop_land_on_char_boundaries() {
    let _g = lock();
    let base = live_count();
    let s = make_string("héllo→世界".as_bytes());
    let t = fai_string_take(imm_int(3), fai_dup(s)); // "hél" (1+2+1 bytes)
    assert_eq!(contents(t), "hél");
    let d = fai_string_drop(imm_int(5), s); // drop "héllo", keep "→世界"
    assert_eq!(contents(d), "→世界");
    fai_drop(t);
    fai_drop(d);
    assert_eq!(live_count(), base, "leak-free");
}
