//! Bounded byte-pattern search, borrowing buffers and consuming scalar bounds.

use crate::{Value, bytes_bytes, fai_box_int, fai_drop, string_bytes, unbox_int};

fn find(haystack: &[u8], needle: &[u8], start: i64, end: i64) -> i64 {
    let length = haystack.len() as i64;
    let start = start.clamp(0, length) as usize;
    let end = end.clamp(0, length) as usize;
    haystack
        .get(start..end)
        .and_then(|range| memchr::memmem::find(range, needle))
        .map_or(-1, |index| (start + index) as i64)
}

fn search(haystack: &[u8], needle: &[u8], start: Value, end: Value) -> Value {
    let (first, last) = (unbox_int(start), unbox_int(end));
    fai_drop(start);
    fai_drop(end);
    fai_box_int(find(haystack, needle, first, last))
}

/// Find the first complete pattern in a clamped byte range, returning an absolute
/// byte offset or -1. Both buffers and both scalar bounds are consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_bytes_find(bytes: Value, needle: Value, start: Value, end: Value) -> Value {
    let result = fai_bytes_find_borrowed(bytes, needle, start, end);
    fai_drop(bytes);
    fai_drop(needle);
    result
}

/// Search while borrowing both buffers. Scalar bounds are consumed, including
/// full-width integer boxes. An empty pattern matches the start of a valid range;
/// a reversed range has no match.
#[unsafe(no_mangle)]
pub extern "C" fn fai_bytes_find_borrowed(
    bytes: Value,
    needle: Value,
    start: Value,
    end: Value,
) -> Value {
    // SAFETY: both arguments are live Bytes buffers retained by the caller.
    unsafe { search(bytes_bytes(bytes), bytes_bytes(needle), start, end) }
}

/// Search for the UTF-8 bytes of a String without constructing a pattern buffer.
/// Both buffers and both scalar bounds are consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_bytes_find_utf8(
    bytes: Value,
    needle: Value,
    start: Value,
    end: Value,
) -> Value {
    let result = fai_bytes_find_utf8_borrowed(bytes, needle, start, end);
    fai_drop(bytes);
    fai_drop(needle);
    result
}

/// Borrow a byte buffer and a String pattern, consuming the two scalar bounds.
/// The pattern may be an inline String or a substring view.
#[unsafe(no_mangle)]
pub extern "C" fn fai_bytes_find_utf8_borrowed(
    bytes: Value,
    needle: Value,
    start: Value,
    end: Value,
) -> Value {
    // SAFETY: bytes is Bytes and needle is String; the caller retains both.
    unsafe { search(bytes_bytes(bytes), string_bytes(needle), start, end) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::lock;
    use crate::{live_bytes, live_count, make_bytes, make_int, make_str, read_int};

    #[test]
    fn binary_patterns_include_nul_and_high_bytes() {
        assert_eq!(find(&[8, 0, 255, 0, 255], &[0, 255], 0, 5), 1);
    }

    #[test]
    fn overlapping_matches_choose_the_first() {
        assert_eq!(find(b"ababa", b"aba", 1, 5), 2);
    }

    #[test]
    fn range_results_are_absolute_offsets() {
        assert_eq!(find(b"xxabcabc", b"abc", 3, 8), 5);
    }

    #[test]
    fn match_must_fit_before_exclusive_end() {
        assert_eq!(find(b"xxabc", b"abc", 0, 4), -1);
    }

    #[test]
    fn match_may_end_at_exclusive_end() {
        assert_eq!(find(b"xxabc", b"abc", 0, 5), 2);
    }

    #[test]
    fn empty_pattern_matches_empty_buffer() {
        assert_eq!(find(b"", b"", 0, 0), 0);
    }

    #[test]
    fn empty_pattern_matches_clamped_start() {
        assert_eq!(find(b"abc", b"", i64::MAX, i64::MAX), 3);
    }

    #[test]
    fn bounds_are_clamped_before_testing_range_order() {
        assert_eq!(find(b"abc", b"", 9, 8), 3);
    }

    #[test]
    fn full_width_bounds_are_clamped() {
        assert_eq!(find(b"abc", b"b", i64::MIN, i64::MAX), 1);
    }

    #[test]
    fn reversed_range_has_no_empty_match() {
        assert_eq!(find(b"abc", b"", 2, 1), -1);
    }

    #[test]
    fn empty_range_has_no_nonempty_match() {
        assert_eq!(find(b"abc", b"a", i64::MIN, -1), -1);
    }

    #[test]
    fn byte_search_can_start_inside_a_unicode_scalar() {
        assert_eq!(find("é🙂".as_bytes(), &[0xa9], 1, 6), 1);
    }

    #[test]
    fn long_repetitive_patterns_preserve_first_match() {
        let mut bytes = vec![b'a'; 20000];
        bytes.push(b'b');
        let mut needle = vec![b'a'; 1000];
        needle.push(b'b');
        assert_eq!(find(&bytes, &needle, 0, i64::MAX), 19000);
    }

    #[test]
    fn owned_search_releases_both_inputs_and_bounds() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        assert_eq!(
            read_int(fai_bytes_find(
                make_bytes(b"abc"),
                make_bytes(b"b"),
                make_int(i64::MIN),
                make_int(i64::MAX)
            )),
            1
        );
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn borrowed_search_preserves_owners_without_allocating() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let bytes = make_bytes(b"abc");
        let needle = make_bytes(b"b");
        let start = make_int(i64::MIN);
        let end = make_int(i64::MAX);
        crate::reset_allocations();
        assert_eq!(read_int(fai_bytes_find_borrowed(bytes, needle, start, end)), 1);
        assert_eq!(crate::allocations(), 0);
        // SAFETY: borrowed search leaves both input owners intact.
        unsafe {
            assert_eq!(bytes_bytes(bytes), b"abc");
            assert_eq!(bytes_bytes(needle), b"b");
        }
        fai_drop(bytes);
        fai_drop(needle);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn same_buffer_can_supply_both_borrowed_arguments() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let bytes = make_bytes(b"abc");
        assert_eq!(read_int(fai_bytes_find_borrowed(bytes, bytes, make_int(0), make_int(3))), 0);
        fai_drop(bytes);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn string_pattern_views_are_borrowed() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let needle =
            crate::fai_string_substring(make_int(1), make_int(40), make_str(&"é".repeat(64)));
        let bytes = make_bytes(format!("x{}", "é".repeat(40)).as_bytes());
        assert_eq!(
            read_int(fai_bytes_find_utf8_borrowed(bytes, needle, make_int(0), make_int(i64::MAX))),
            1
        );
        fai_drop(bytes);
        fai_drop(needle);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn owned_utf8_search_releases_its_pattern() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        assert_eq!(
            read_int(fai_bytes_find_utf8(
                make_bytes("aé".as_bytes()),
                make_str("é"),
                make_int(0),
                make_int(3)
            )),
            1
        );
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn shared_buffers_support_parallel_searches() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let bytes = crate::fai_mark_shared(make_bytes(b"abcabc"));
        let other = crate::fai_dup(bytes);
        let thread = std::thread::spawn(move || {
            assert_eq!(
                read_int(fai_bytes_find_utf8(other, make_str("bc"), make_int(2), make_int(6))),
                4
            );
        });
        assert_eq!(
            read_int(fai_bytes_find_utf8(bytes, make_str("bc"), make_int(0), make_int(6))),
            1
        );
        thread.join().unwrap();
        assert_eq!((live_count(), live_bytes()), baseline);
    }
}

#[cfg(test)]
mod proptests {
    use super::find;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn native_search_agrees_with_window_scan(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            needle in proptest::collection::vec(any::<u8>(), 0..64),
            start in any::<i64>(), end in any::<i64>(),
        ) {
            let lo = start.clamp(0, bytes.len() as i64) as usize;
            let hi = end.clamp(0, bytes.len() as i64) as usize;
            let expected = bytes.get(lo..hi).and_then(|slice| {
                if needle.is_empty() { Some(lo) } else { slice.windows(needle.len()).position(|part| part == needle).map(|i| lo + i) }
            }).map_or(-1, |i| i as i64);
            prop_assert_eq!(find(&bytes, &needle, start, end), expected);
        }
    }
}
