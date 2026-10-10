//! Linear compact chains release iteratively and preserve shared descendants.

use super::*;
use crate::tests::lock;

fn cell(values: &[Value]) -> Value {
    // SAFETY: every field is initialized and transfers one owned reference.
    unsafe { fai_make_data(3, values.len() as i64, values.as_ptr()) }
}

#[test]
fn long_linear_chains_release_every_cell_without_new_allocations() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let mut root = imm_int(0);
    for _ in 0..10_000 {
        root = cell(&[imm_int(7), root]);
    }
    let allocated = allocations();
    fai_drop(root);
    assert_eq!(allocations(), allocated);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn a_linear_chain_stops_at_its_shared_tail() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let tail = cell(&[imm_int(11)]);
    let mut root = fai_dup(tail);
    for _ in 0..64 {
        root = cell(&[root, imm_int(7)]);
    }
    fai_drop(root);
    assert_eq!(live_count(), baseline.0 + 1);
    let value = fai_data_field(tail, 0);
    assert_eq!(read_int(value), 11);
    fai_drop(value);
    fai_drop(tail);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn linear_chains_continue_through_branching_and_extended_children() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let first = make_str("first");
    let second = make_str("second");
    let mut root = cell(&[first, second]);
    for _ in 0..64 {
        root = cell(&[imm_int(7), root]);
    }
    fai_drop(root);
    assert_eq!((live_count(), live_bytes()), baseline);
}

#[test]
fn raw_float_slots_in_linear_cells_are_not_children() {
    let _guard = lock();
    let baseline = (live_count(), live_bytes());
    let fields = [(-0.0f64).to_bits() as i64, cell(&[imm_int(7)])];
    let descriptor = intern_data_descriptor(1);
    // SAFETY: the first field is raw Float bits, the second an owned child.
    let root = unsafe { fai_make_data_scalar(descriptor, 3, 2, fields.as_ptr()) };
    fai_drop(root);
    assert_eq!((live_count(), live_bytes()), baseline);
}
