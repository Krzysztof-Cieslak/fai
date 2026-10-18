//! Checked byte-range copies preserve their borrowed source and own their result.
use super::*;
use crate::tests::lock;

#[track_caller]
fn check_result(value: Value, expected: Result<&str, ()>) {
    let payload = fai_data_field(value, 0);
    check_parts(read_int(fai_data_tag(value)), payload, expected);
    fai_drop(value);
}

#[track_caller]
fn check_parts(tag: i64, payload: Value, expected: Result<&str, ()>) {
    match expected {
        Ok(text) => {
            assert_eq!(tag, 0);
            assert_eq!(read_string(payload), text.as_bytes());
        }
        Err(()) => {
            assert_eq!(tag, 1);
            assert_eq!(read_int(fai_data_field(payload, 0)), 7);
            let message = fai_data_field(payload, 1);
            assert_eq!(read_string(message), b"invalid text span");
            assert_eq!(read_int(fai_data_field(payload, 2)), -1);
            assert_eq!(read_int(fai_data_field(payload, 3)), -1);
            fai_drop(message);
        }
    }
    fai_drop(payload);
}

fn slice_parts(source: Value, start: Value, end: Value) -> (i64, Value) {
    let mut payload = FAI_UNIT;
    // SAFETY: payload is an aligned, writable slot with no owned old contents.
    let tag = unsafe { fai_regex_slice_parts_borrowed(source, start, end, &mut payload) };
    (tag, payload)
}

#[track_caller]
fn check_slice(text: &str, start: i64, end: i64, expected: Result<&str, ()>) {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str(text);
    let result = fai_regex_slice_borrowed(source, make_int(start), make_int(end));
    let (tag, payload) = slice_parts(source, make_int(start), make_int(end));
    assert_eq!(read_string(source), text.as_bytes());
    // SAFETY: the borrowed operation leaves the source's sole owner intact.
    assert_eq!(unsafe { rc_load(as_obj(source)) }, 1);
    fai_drop(source);
    check_result(result, expected);
    check_parts(tag, payload, expected);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn borrowed_slice_copies_unicode_independently() {
    check_slice("Aé🙂B", 1, 7, Ok("é🙂"));
}

#[test]
fn borrowed_full_slice_does_not_retain_the_source() {
    check_slice("abc", 0, 3, Ok("abc"));
}

#[test]
fn borrowed_slice_accepts_empty_input() {
    check_slice("", 0, 0, Ok(""));
}

#[test]
fn borrowed_slice_accepts_an_empty_range_at_the_end() {
    check_slice("é", 2, 2, Ok(""));
}

#[test]
fn borrowed_slice_rejects_a_split_start_scalar() {
    check_slice("é", 1, 2, Err(()));
}

#[test]
fn borrowed_slice_rejects_a_split_end_scalar() {
    check_slice("é", 0, 1, Err(()));
}

#[test]
fn borrowed_slice_rejects_reversed_bounds() {
    check_slice("abc", 2, 1, Err(()));
}

#[test]
fn borrowed_slice_releases_a_full_width_negative_start() {
    check_slice("abc", i64::MIN, 1, Err(()));
}

#[test]
fn borrowed_slice_releases_a_full_width_oversized_end() {
    check_slice("abc", 0, i64::MAX, Err(()));
}

#[test]
fn borrowed_slice_consumes_boxed_indices_on_success() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str("abc");
    let start = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    let end = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    // SAFETY: each valid uniform Int box has an initialized payload and one owner.
    unsafe {
        write_i64(start, INT_VALUE_OFFSET, 0);
        write_i64(end, INT_VALUE_OFFSET, 3);
    }
    check_result(fai_regex_slice_borrowed(source, from_obj(start), from_obj(end)), Ok("abc"));
    assert_eq!(live_count(), baseline.0 + 1, "only the source remains live");
    fai_drop(source);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn borrowed_slice_reads_a_string_view() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str("prefixé🙂suffix");
    // SAFETY: these byte boundaries delimit two complete scalars in source.
    let view = unsafe { make_string_slice(source, 6, 6) };
    fai_drop(source);
    let result = fai_regex_slice_borrowed(view, make_int(2), make_int(6));
    let (tag, payload) = slice_parts(view, make_int(2), make_int(6));
    fai_drop(view);
    check_result(result, Ok("🙂"));
    check_parts(tag, payload, Ok("🙂"));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn owned_slice_releases_its_source() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    check_result(fai_regex_slice(make_str("é🙂"), make_int(2), make_int(6)), Ok("🙂"));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn native_result_success_allocates_only_the_copied_text() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str("Aé🙂B");
    reset_allocations();
    let (tag, payload) = slice_parts(source, make_int(1), make_int(7));
    assert_eq!(allocations(), 1);
    fai_drop(source);
    check_parts(tag, payload, Ok("é🙂"));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn native_result_error_allocates_no_result_wrapper() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str("é");
    reset_allocations();
    let (tag, payload) = slice_parts(source, make_int(1), make_int(2));
    assert_eq!(allocations(), 2, "only the error message and Error value");
    fai_drop(source);
    check_parts(tag, payload, Err(()));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn owned_native_result_releases_its_source() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let mut payload = FAI_UNIT;
    // SAFETY: payload is an aligned, writable slot with no owned old contents.
    let tag = unsafe {
        fai_regex_slice_parts(make_str("é🙂"), make_int(2), make_int(6), &mut payload)
    };
    check_parts(tag, payload, Ok("🙂"));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn native_result_consumes_boxed_indices() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str("abc");
    let start = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    let end = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    // SAFETY: both Int boxes have initialized payloads and transfer to the call.
    unsafe {
        write_i64(start, INT_VALUE_OFFSET, 0);
        write_i64(end, INT_VALUE_OFFSET, 3);
    }
    let (tag, payload) = slice_parts(source, from_obj(start), from_obj(end));
    check_parts(tag, payload, Ok("abc"));
    assert_eq!(live_count(), baseline.0 + 1, "only the source remains live");
    fai_drop(source);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_sources_support_parallel_borrowed_slices() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let source = make_str("Aé🙂B");
    fai_mark_shared(source);
    let other = fai_dup(source);
    let worker = std::thread::spawn(move || {
        let (tag, payload) = slice_parts(other, make_int(3), make_int(7));
        check_parts(tag, payload, Ok("🙂"));
        fai_drop(other);
    });
    check_result(fai_regex_slice_borrowed(source, make_int(1), make_int(3)), Ok("é"));
    worker.join().unwrap();
    assert_eq!(read_string(source), "Aé🙂B".as_bytes());
    fai_drop(source);
    assert_eq!((live_count(), live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn borrowed_slices_match_checked_rust_ranges(text in ".{0,32}", start in -2i64..130, end in -2i64..130) {
            let expected = usize::try_from(start).ok().zip(usize::try_from(end).ok())
                .and_then(|(start, end)| text.get(start..end)).ok_or(());
            check_slice(&text, start, end, expected);
        }
    }
}
