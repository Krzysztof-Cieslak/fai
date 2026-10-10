//! Array destruction bounds auxiliary space and preserves live aliases and order.

use super::*;
use crate::tests::lock;

fn array(values: impl ExactSizeIterator<Item = Value>) -> Value {
    let mut result = fai_array_with_capacity(imm_int(values.len() as i64));
    for value in values {
        result = fai_array_push(result, value);
    }
    result
}

fn boxed_array(width: usize) -> Value {
    array((0..width).map(|i| fai_box_int(i64::MIN + i as i64)))
}

fn drop_queued(value: Value) -> usize {
    let mut work = DropWork::new();
    work.push(value);
    drain(&mut work);
    assert_eq!(work.len, 0);
    assert!(work.spill.is_empty());
    work.spill.capacity()
}

#[test]
fn wide_boxed_arrays_need_no_spill_storage() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let short = drop_queued(boxed_array(2 * DROP_INLINE + 1));
    let long = drop_queued(boxed_array(8 * DROP_INLINE + 1));
    assert_eq!((short, long), (0, 0));
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn nested_array_spill_storage_depends_on_depth_not_width() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let short = array((0..DROP_INLINE + 1).map(|_| boxed_array(DROP_INLINE + 1)));
    let short_capacity = drop_queued(short);
    let long = array((0..2 * DROP_INLINE + 1).map(|_| boxed_array(2 * DROP_INLINE + 1)));
    let long_capacity = drop_queued(long);
    assert_eq!(short_capacity, long_capacity);
    assert!(long_capacity <= DROP_INLINE);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn array_batches_preserve_reverse_element_release_order() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let cell = || {
        let fields = [imm_int(1), imm_int(2)];
        // SAFETY: both initialized fields transfer owned immediate values.
        unsafe { fai_make_data(0, 2, fields.as_ptr()) }
    };
    let original: Vec<_> = (0..2 * DROP_INLINE + 1).map(|_| cell()).collect();
    drop_queued(array(original.iter().copied()));
    let rebuilt: Vec<_> = (0..original.len()).map(|_| cell()).collect();
    assert_eq!(rebuilt, original, "LIFO pools expose the reverse destruction order");
    for value in rebuilt {
        fai_drop(value);
    }
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn shared_array_is_untouched_until_its_final_release() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let values = boxed_array(2 * DROP_INLINE + 1);
    let retained = fai_dup(values);
    drop_queued(values);
    assert_eq!(read_int(fai_array_length_borrowed(retained)), (2 * DROP_INLINE + 1) as i64);
    let last = fai_array_get_borrowed(retained, imm_int((2 * DROP_INLINE) as i64));
    assert_eq!(read_int(last), i64::MIN + (2 * DROP_INLINE) as i64);
    fai_drop(last);
    assert_eq!(drop_queued(retained), 0);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn formerly_task_shared_arrays_release_all_batches() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let values = fai_mark_shared(boxed_array(2 * DROP_INLINE + 1));
    let retained = fai_dup(values);
    drop_queued(values);
    assert_eq!(read_int(fai_array_length_borrowed(retained)), (2 * DROP_INLINE + 1) as i64);
    assert_eq!(drop_queued(retained), 0);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn raw_float_arrays_remain_child_free() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let values = array((0..2 * DROP_INLINE + 1).map(|_| fai_box_float((-0.0f64).to_bits() as i64)));
    assert_eq!(drop_queued(values), 0);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn empty_array_does_not_scan_uninitialized_capacity() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    assert_eq!(drop_queued(fai_array_with_capacity(imm_int(256))), 0);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn sparse_boxed_values_survive_immediate_only_batches() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let values = array((0..5 * DROP_INLINE + 1).map(|i| {
        if i == 0 || i == 2 * DROP_INLINE || i == 5 * DROP_INLINE {
            fai_box_int(i64::MIN + i as i64)
        } else {
            imm_int(i as i64)
        }
    }));
    assert_eq!(drop_queued(values), 0);
    assert_eq!((live_count(), live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]
        #[test]
        fn integer_arrays_release_with_bounded_worklist(values in prop::collection::vec(any::<i64>(), 0..1024), shared in any::<bool>()) {
            let _guard = lock();
            let baseline = (live_count(), live_bytes());
            let array = array(values.into_iter().map(|value| fai_box_int(value)));
            if shared { fai_mark_shared(array); }
            prop_assert_eq!(drop_queued(array), 0);
            prop_assert_eq!((live_count(), live_bytes()), baseline);
        }
    }
}
