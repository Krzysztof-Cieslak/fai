//! One-allocation binary collection, including shared and empty chunks.
use super::*;
use crate::tests::lock;

#[test]
fn empty_collection_returns_empty_bytes() {
    let _guard = lock();
    let base = (live_count(), live_bytes());
    let result = fai_bytes_concat_many(imm_int(NIL_TAG));
    // SAFETY: result is an owned byte buffer.
    assert!(unsafe { bytes_bytes(result) }.is_empty());
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), base);
}

#[test]
fn empty_chunks_do_not_prevent_single_buffer_transfer() {
    let _guard = lock();
    let base = (live_count(), live_bytes());
    let chunk = make_bytes(&[0, 128, 255]);
    let chunks = cons_list(&[make_bytes(&[]), chunk, make_bytes(&[])]);
    reset_allocations();
    let result = fai_bytes_concat_many(chunks);
    assert_eq!(result, chunk);
    assert_eq!(allocations(), 0);
    fai_drop(result);
    assert_eq!((live_count(), live_bytes()), base);
}

#[test]
fn shared_chunks_keep_their_original_content() {
    let _guard = lock();
    let base = (live_count(), live_bytes());
    let chunk = make_bytes(&[0, 128, 255]);
    let chunks = cons_list(&[fai_dup(chunk), fai_dup(chunk)]);
    let retained = fai_dup(chunks);
    reset_allocations();
    let result = fai_bytes_concat_many(chunks);
    assert_eq!(allocations(), 1);
    // SAFETY: result and chunk both have live owners.
    unsafe {
        assert_eq!(bytes_bytes(result), [0, 128, 255, 0, 128, 255]);
        assert_eq!(bytes_bytes(chunk), [0, 128, 255]);
    }
    fai_drop(result);
    fai_drop(retained);
    fai_drop(chunk);
    assert_eq!((live_count(), live_bytes()), base);
}

#[test]
fn a_large_collection_allocates_only_its_final_buffer() {
    let _guard = lock();
    let base = (live_count(), live_bytes());
    let chunk = make_bytes(&[255; 1024]);
    let values: Vec<_> = (0..1024).map(|_| fai_dup(chunk)).collect();
    let chunks = cons_list(&values);
    reset_allocations();
    let result = fai_bytes_concat_many(chunks);
    assert_eq!(allocations(), 1);
    assert!(allocated_bytes() <= 1024 * 1024 + 64);
    // SAFETY: the result owns the complete copied payload.
    let bytes = unsafe { bytes_bytes(result) };
    assert_eq!(bytes.len(), 1024 * 1024);
    assert!(bytes.iter().all(|&b| b == 255));
    fai_drop(result);
    fai_drop(chunk);
    assert_eq!((live_count(), live_bytes()), base);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_chunks_match_flattening(chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..128), 0..64)) {
            let _guard = lock();
            let base = (live_count(), live_bytes());
            let values: Vec<_> = chunks.iter().map(|chunk| make_bytes(chunk)).collect();
            let result = fai_bytes_concat_many(cons_list(&values));
            // SAFETY: result remains owned until after its payload is copied.
            let actual = unsafe { bytes_bytes(result).to_vec() };
            fai_drop(result);
            prop_assert_eq!(actual, chunks.concat());
            prop_assert_eq!((live_count(), live_bytes()), base);
        }
    }
}
