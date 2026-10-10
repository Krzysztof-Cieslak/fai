//! Canonical intrinsic functions retain the ordinary owned closure ABI.

use super::*;
use crate::tests::lock;

fn subtraction() -> Value {
    (&raw const FAI_INT_SUB_CLOSURE) as usize as Value
}

#[test]
fn subtraction_closure_uses_left_to_right_operands() {
    let _guard = lock();
    let baseline = live_count();
    let result = apply(subtraction(), &[make_int(7), make_int(3)]);
    assert_eq!(read_int(result), 4);
    fai_drop(result);
    assert_eq!(live_count(), baseline);
}

#[test]
fn subtraction_closure_preserves_full_width_wrapping() {
    let _guard = lock();
    let baseline = live_count();
    let result = apply(subtraction(), &[make_int(i64::MAX), make_int(-1)]);
    assert_eq!(read_int(result), i64::MIN);
    fai_drop(result);
    assert_eq!(live_count(), baseline);
}

#[test]
fn subtraction_closure_can_be_partially_applied() {
    let _guard = lock();
    let baseline = live_count();
    let partial = apply(subtraction(), &[make_int(i64::MAX)]);
    let result = apply(partial, &[make_int(i64::MAX - 1)]);
    assert_eq!(read_int(result), 1);
    fai_drop(result);
    assert_eq!(live_count(), baseline);
}

#[test]
fn subtraction_closure_preserves_a_shared_operand() {
    let _guard = lock();
    let baseline = live_count();
    let input = make_int(i64::MAX);
    let result = apply(subtraction(), &[fai_dup(input), make_int(1)]);
    assert_eq!(read_int(input), i64::MAX);
    assert_eq!(read_int(result), i64::MAX - 1);
    fai_drop(input);
    fai_drop(result);
    assert_eq!(live_count(), baseline);
}
