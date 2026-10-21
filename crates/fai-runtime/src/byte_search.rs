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

fn span(bytes: &[u8], start: i64, end: i64, words: [u64; 4]) -> i64 {
    let start = start.clamp(0, bytes.len() as i64) as usize;
    let end = end.clamp(0, bytes.len() as i64) as usize;
    if end < start {
        return start as i64;
    }
    let count = crate::byte_class::prefix(&bytes[start..end], words);
    (start + count) as i64
}

/// Return the end of a byte-class prefix, consuming all operands. Bounds are
/// clamped to the buffer; an empty/reversed range returns its clamped start.
/// Class word `byte / 64`, bit `byte % 64`, determines membership.
#[unsafe(no_mangle)]
pub extern "C" fn fai_bytes_span(bytes: Value, start: Value, end: Value, class: Value) -> Value {
    let result = fai_bytes_span_borrowed(bytes, start, end, class);
    fai_drop(bytes);
    fai_drop(class);
    result
}

/// Borrow the byte buffer and four-word class; consume the scalar bounds.
#[unsafe(no_mangle)]
pub extern "C" fn fai_bytes_span_borrowed(
    bytes: Value,
    start: Value,
    end: Value,
    class: Value,
) -> Value {
    let (first, last) = (unbox_int(start), unbox_int(end));
    fai_drop(start);
    fai_drop(end);
    // SAFETY: class is a typed four-Int tuple and both buffers remain borrowed
    // through this call. The class words are copied before scanning the bytes.
    let result = unsafe {
        let object = crate::as_obj(class);
        let offset = crate::data_offset(object);
        let words =
            std::array::from_fn(|i| unbox_int(crate::read_i64(object, offset + i * 8)) as u64);
        span(bytes_bytes(bytes), first, last, words)
    };
    fai_box_int(result)
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
    fn byte_class_scan_stops_at_the_first_rejected_byte() {
        let words = [0, 1 << (b'a' - 64), 0, 0];
        assert_eq!(span(b"xaaab", 1, 5, words), 4);
    }

    #[test]
    fn byte_class_scan_clamps_full_width_bounds() {
        assert_eq!(span(&[0, 128, 255], i64::MIN, i64::MAX, [u64::MAX; 4]), 3);
    }

    #[test]
    fn byte_class_scan_keeps_an_empty_reversed_prefix() {
        assert_eq!(span(b"abc", 2, 0, [u64::MAX; 4]), 2);
    }

    #[test]
    fn byte_class_scan_preserves_borrowed_owners() {
        let _guard = lock();
        let base = (live_count(), live_bytes());
        let bytes = make_bytes(&[0, 128, 255]);
        let fields = [make_int(-1); 4];
        // SAFETY: all four immediate values transfer into the tuple.
        let class = unsafe { crate::fai_make_data(0, 4, fields.as_ptr()) };
        let result = fai_bytes_span_borrowed(bytes, make_int(i64::MIN), make_int(i64::MAX), class);
        assert_eq!(read_int(result), 3);
        fai_drop(result);
        fai_drop(class);
        fai_drop(bytes);
        assert_eq!((live_count(), live_bytes()), base);
    }

    #[test]
    fn byte_class_scan_accepts_empty_input() {
        assert_eq!(span(&[], i64::MIN, i64::MAX, [u64::MAX; 4]), 0);
    }

    #[test]
    fn byte_class_scan_rejects_the_first_byte() {
        assert_eq!(span(b"abc", 0, 3, [0; 4]), 0);
    }

    #[test]
    fn byte_class_scan_stops_at_the_exclusive_end() {
        assert_eq!(span(b"aaa!", 0, 3, [0, 1 << (b'a' - 64), 0, 0]), 3);
    }

    #[test]
    fn byte_class_scan_checks_all_word_boundaries() {
        let boundaries = [0, 63, 64, 127, 128, 191, 192, 255];
        assert_eq!(span(&boundaries, 0, 8, [0x8000000000000001; 4]), 8);
    }

    #[test]
    fn owned_byte_class_scan_releases_boxed_class_words_and_bounds() {
        let _guard = lock();
        let base = (live_count(), live_bytes());
        let fields = [make_int(i64::MIN), make_int(i64::MAX), make_int(0), make_int(0)];
        // SAFETY: the four owned Int fields transfer into the class tuple.
        let class = unsafe { crate::fai_make_data(0, 4, fields.as_ptr()) };
        let result = fai_bytes_span(
            make_bytes(&[63, 64, 126, 127]),
            make_int(i64::MIN),
            make_int(i64::MAX),
            class,
        );
        assert_eq!(read_int(result), 3);
        fai_drop(result);
        assert_eq!((live_count(), live_bytes()), base);
    }

    #[test]
    fn byte_class_scan_can_share_a_boxed_class_between_threads() {
        let _guard = lock();
        let base = (live_count(), live_bytes());
        let fields = [make_int(i64::MIN), make_int(i64::MAX), make_int(0), make_int(0)];
        // SAFETY: every owned field is initialized and transferred into the tuple.
        let class = crate::fai_mark_shared(unsafe { crate::fai_make_data(0, 4, fields.as_ptr()) });
        let bytes = crate::fai_mark_shared(make_bytes(&[63, 64, 126, 127]));
        let other_class = crate::fai_dup(class);
        let other_bytes = crate::fai_dup(bytes);
        let worker = std::thread::spawn(move || {
            let result = fai_bytes_span(other_bytes, make_int(1), make_int(4), other_class);
            assert_eq!(read_int(result), 3);
            fai_drop(result);
        });
        let result = fai_bytes_span(bytes, make_int(0), make_int(2), class);
        assert_eq!(read_int(result), 2);
        fai_drop(result);
        worker.join().unwrap();
        assert_eq!((live_count(), live_bytes()), base);
    }

    #[test]
    fn repeated_byte_class_scans_keep_live_and_backing_storage_bounded() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes(), crate::allocation_stats::current());
        let observe = |rounds| {
            let result = std::thread::spawn(move || {
                crate::reset_allocations();
                for _ in 0..rounds {
                    let fields =
                        [make_int(i64::MAX), make_int(i64::MIN), make_int(-1), make_int(-1)];
                    // SAFETY: the owned Int words transfer into a four-field tuple.
                    let class = unsafe { crate::fai_make_data(0, 4, fields.as_ptr()) };
                    let result = fai_bytes_span(
                        make_bytes(&[42; 8192]),
                        make_int(i64::MIN),
                        make_int(i64::MAX),
                        class,
                    );
                    assert_eq!(read_int(result), 8192);
                    fai_drop(result);
                }
                assert_eq!((live_count(), live_bytes()), (baseline.0, baseline.1));
                (
                    crate::peak_live_bytes(),
                    crate::allocation_stats::peak_bytes(),
                    crate::allocation_stats::current(),
                )
            })
            .join()
            .unwrap();
            assert_eq!(crate::allocation_stats::current(), baseline.2);
            result
        };
        assert_eq!(observe(12), observe(1200));
    }

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
    use super::{find, span};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn byte_class_scan_agrees_with_set_membership(
            bytes in proptest::collection::vec(any::<u8>(), 0..256),
            allowed in proptest::collection::btree_set(any::<u8>(), 0..256),
            start in -16i64..272, end in -16i64..272,
        ) {
            let mut words = [0u64; 4];
            for &byte in &allowed {
                words[usize::from(byte / 64)] |= 1 << (byte % 64);
            }
            let first = start.clamp(0, bytes.len() as i64) as usize;
            let last = end.clamp(0, bytes.len() as i64) as usize;
            let mut expected = first;
            while expected < last && allowed.contains(&bytes[expected]) {
                expected += 1;
            }
            prop_assert_eq!(span(&bytes, start, end, words), expected as i64);
        }

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
