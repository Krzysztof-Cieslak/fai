//! Bulk repetition initializes complete buffers and preserves element ownership.

use super::*;
use crate::tests::lock;

#[test]
fn immediate_repeat_initializes_first_and_last_slots() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let array = fai_array_repeat(imm_int(129), imm_int(7));
    assert_eq!(read_int(fai_array_length_borrowed(array)), 129);
    assert_eq!(read_int(fai_array_get_borrowed(array, imm_int(0))), 7);
    assert_eq!(read_int(fai_array_get_borrowed(array, imm_int(128))), 7);
    fai_drop(array);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn float_repeat_uses_raw_payload_slots() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let bits = 0xfff8_0000_0000_0042u64;
    let array = fai_array_repeat(imm_int(8), fai_box_float(bits as i64));
    // SAFETY: the live array has eight initialized slots.
    unsafe {
        assert!(array_obj_is_float(as_obj(array)));
        assert_eq!(read_u64(as_obj(array), ARRAY_ELEMS_OFFSET + 7 * 8), bits);
    }
    fai_drop(array);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn boxed_repeat_preserves_an_external_shared_owner() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let value = fai_box_int(i64::MAX);
    let array = fai_array_repeat(imm_int(8), fai_dup(value));
    // SAFETY: the original owner and every initialized slot own this live box.
    assert_eq!(unsafe { rc_load(as_obj(value)) }, 9);
    fai_drop(array);
    assert_eq!(read_int(value), i64::MAX);
    // SAFETY: the retained original owner still keeps the box live.
    assert_eq!(unsafe { rc_load(as_obj(value)) }, 1);
    fai_drop(value);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn nonpositive_repeat_drops_its_supplied_value() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let array = fai_array_repeat(fai_box_int(i64::MIN), make_str("unused"));
    assert_eq!(read_int(fai_array_length_borrowed(array)), 0);
    fai_drop(array);
    assert_eq!((live_count(), live_bytes()), baseline);
}
