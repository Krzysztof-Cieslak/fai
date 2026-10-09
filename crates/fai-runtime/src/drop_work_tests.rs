//! Initialized-prefix and LIFO invariants of the iterative destruction worklist.

use super::{DROP_INLINE, DropWork};

#[test]
fn empty_worklist_reads_no_slot() {
    assert_eq!(DropWork::new().pop(), None);
}

#[test]
fn one_element_initializes_only_its_live_slot() {
    let mut work = DropWork::new();
    work.push(i64::MIN);
    assert_eq!(work.pop(), Some(i64::MIN));
    assert_eq!(work.pop(), None);
}

#[test]
fn full_inline_prefix_is_lifo() {
    let mut work = DropWork::new();
    for i in 0..DROP_INLINE {
        work.push(i as i64);
    }
    assert!(work.spill.is_empty());
    let values: Vec<_> = std::iter::from_fn(|| work.pop()).collect();
    assert_eq!(values, (0..DROP_INLINE as i64).rev().collect::<Vec<_>>());
}

#[test]
fn spilled_entries_pop_before_inline_entries() {
    let mut work = DropWork::new();
    for i in 0..DROP_INLINE + 3 {
        work.push(i as i64);
    }
    assert_eq!(work.spill.len(), 3);
    let values: Vec<_> = std::iter::from_fn(|| work.pop()).collect();
    assert_eq!(values, (0..(DROP_INLINE + 3) as i64).rev().collect::<Vec<_>>());
}

#[test]
fn popped_slots_are_reinitialized_before_reuse() {
    let mut work = DropWork::new();
    for value in [0, -1, i64::MAX, i64::MIN] {
        work.push(value);
        assert_eq!(work.pop(), Some(value));
        assert_eq!(work.pop(), None);
    }
}

#[test]
fn alternating_at_the_spill_boundary_preserves_order() {
    let mut work = DropWork::new();
    for i in 0..DROP_INLINE {
        work.push(i as i64);
    }
    work.push(100);
    assert_eq!(work.pop(), Some(100));
    assert_eq!(work.pop(), Some((DROP_INLINE - 1) as i64));
    work.push(101);
    work.push(102);
    assert_eq!(work.pop(), Some(102));
    assert_eq!(work.pop(), Some(101));
    assert_eq!(work.pop(), Some((DROP_INLINE - 2) as i64));
}

mod proptests {
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn arbitrary_push_pop_matches_vec(ops in prop::collection::vec(prop::option::of(any::<i64>()), 0..256)) {
            let mut work = super::DropWork::new();
            let mut model = Vec::new();
            for operation in ops {
                if let Some(value) = operation {
                    work.push(value);
                    model.push(value);
                } else {
                    prop_assert_eq!(work.pop(), model.pop());
                }
            }
            let rest: Vec<_> = std::iter::from_fn(|| work.pop()).collect();
            model.reverse();
            prop_assert_eq!(rest, model);
        }
    }
}
