//! Borrowed byte readers retain their owner and consume only scalar arguments.
use super::*;
use crate::tests::lock;

#[test]
fn borrowed_length_retains_its_buffer() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes(&[0, 128, 255]);
    assert_eq!(read_int(fai_bytes_length_borrowed(bytes)), 3);
    // SAFETY: borrowed reading leaves the original reference live.
    assert_eq!(unsafe { bytes_bytes(bytes) }, [0, 128, 255]);
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn borrowed_read_does_not_prevent_unique_append() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes(&[0, 255]);
    assert_eq!(read_int(fai_bytes_get_borrowed(bytes, make_int(1))), 255);
    let result = fai_bytes_concat(bytes, make_bytes(&[128]));
    assert_eq!(result, bytes, "a read must not retain a hidden reference");
    // SAFETY: the result owns the appended buffer.
    assert_eq!(unsafe { bytes_bytes(result) }, [0, 255, 128]);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn borrowed_read_consumes_a_boxed_index() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes(&[255]);
    let index = alloc_obj(HEADER_SIZE + 8, &FAI_INT_DESC);
    // SAFETY: this valid uniform Int box has an initialized payload and one owner.
    unsafe { write_i64(index, INT_VALUE_OFFSET, 0) };
    assert_eq!(read_int(fai_bytes_get_borrowed(bytes, from_obj(index))), 255);
    assert_eq!(live_count(), baseline.0 + 1, "only the buffer remains live");
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn borrowed_utf8_accepts_empty_and_multibyte_buffers() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let empty = make_bytes(&[]);
    let text = make_bytes("\0é😀".as_bytes());
    assert_eq!(fai_bytes_is_utf8_borrowed(empty), from_bool(true));
    assert_eq!(fai_bytes_is_utf8_borrowed(text), from_bool(true));
    fai_drop(empty);
    fai_drop(text);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn borrowed_utf8_rejects_invalid_bytes_without_consuming_them() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes(&[0xf0, 0x9f]);
    assert_eq!(fai_bytes_is_utf8_borrowed(bytes), from_bool(false));
    assert_eq!(read_int(fai_bytes_get_borrowed(bytes, make_int(1))), 0x9f);
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn owned_readers_release_their_buffers() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    assert_eq!(read_int(fai_bytes_length(make_bytes(&[0, 255]))), 2);
    assert_eq!(read_int(fai_bytes_get(make_bytes(&[255]), make_int(0))), 255);
    assert_eq!(fai_bytes_is_utf8(make_bytes(&[255])), from_bool(false));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_buffers_support_parallel_borrowed_readers() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bytes = make_bytes(&[0, 127, 128, 255]);
    fai_mark_shared(bytes);
    let first = fai_dup(bytes);
    let second = fai_dup(bytes);
    let a = std::thread::spawn(move || {
        assert_eq!(read_int(fai_bytes_get_borrowed(first, make_int(2))), 128);
        assert_eq!(read_int(fai_bytes_length_borrowed(first)), 4);
        fai_drop(first);
    });
    let b = std::thread::spawn(move || {
        assert_eq!(fai_bytes_is_utf8_borrowed(second), from_bool(false));
        assert_eq!(read_int(fai_bytes_get_borrowed(second, make_int(3))), 255);
        fai_drop(second);
    });
    a.join().unwrap();
    b.join().unwrap();
    fai_drop(bytes);
    assert_eq!((live_count(), live_bytes()), baseline);
}
