//! Byte-length proofs retain their own allocation ceiling and buffer identity.

use super::tests::{int_lit, local, local_expr, prim};
use super::*;

fn guarded() -> Bounds {
    let mut bounds = Bounds::new();
    bounds.transfer_let(local(2), &prim(Prim::BytesLength, vec![local_expr(0)]));
    bounds.transfer_let(local(10), &prim(Prim::IntGe, vec![local_expr(1), int_lit(0)]));
    bounds.refine(&local_expr(10), true);
    bounds.transfer_let(local(11), &prim(Prim::IntLt, vec![local_expr(1), local_expr(2)]));
    bounds.refine(&local_expr(11), true);
    bounds
}

#[test]
fn byte_guards_prove_the_same_buffer_only() {
    let bounds = guarded();
    assert!(bounds.bytes_index_in_bounds(local(0), &local_expr(1)));
    assert!(!bounds.bytes_index_in_bounds(local(3), &local_expr(1)));
    assert!(!bounds.index_in_bounds(local(0), &local_expr(1)));
}

#[test]
fn byte_guards_do_not_prove_the_exclusive_endpoint() {
    let bounds = guarded();
    assert!(!bounds.bytes_index_in_bounds(local(0), &local_expr(2)));
}

#[test]
fn byte_access_proves_only_its_successful_continuation() {
    let mut bounds = Bounds::new();
    assert!(!bounds.bytes_index_in_bounds(local(0), &local_expr(1)));
    bounds.transfer_let(local(2), &prim(Prim::BytesGet, vec![local_expr(0), local_expr(1)]));
    assert!(bounds.bytes_index_in_bounds(local(0), &local_expr(1)));
    assert!(!bounds.bytes_index_in_bounds(local(3), &local_expr(1)));
}

#[test]
fn byte_lengths_do_not_inherit_the_array_ceiling() {
    let mut bounds = Bounds::new();
    bounds.transfer_let(local(2), &prim(Prim::BytesLength, vec![local_expr(0)]));
    assert!(!bounds.integer_in_range(&local_expr(2), 0, i64::MAX / 8));
    assert!(!bounds.integer_in_range(&local_expr(2), 0, (1i64 << 62) - 1));
    bounds.transfer_let(local(3), &prim(Prim::IntAdd, vec![local_expr(2), int_lit(64)]));
    assert_eq!(bounds.bound(Term::Int(local(3)), Term::Int(local(2))), None);
}

#[test]
fn guarded_byte_index_increment_cannot_wrap() {
    let mut bounds = guarded();
    bounds.transfer_let(local(3), &prim(Prim::IntAdd, vec![local_expr(1), int_lit(1)]));
    assert_eq!(bounds.bound(Term::Int(local(3)), Term::Int(local(1))), Some(1));
}

#[test]
fn byte_alias_preserves_its_length() {
    let mut bounds = guarded();
    let source = CExpr::new(K::Local(local(0)), fai_types::Ty::Con(fai_types::Con::Bytes));
    bounds.transfer_let(local(3), &source);
    assert!(bounds.bytes_index_in_bounds(local(3), &local_expr(1)));
}

#[test]
fn byte_entry_facts_roundtrip_and_seed() {
    let sig = BoundSig {
        edges: vec![
            (PTerm::Zero, PTerm::Param(1), 0),
            (PTerm::Param(1), PTerm::BytesLenParam(0), -1),
        ],
    };
    let sig = serde_json::from_slice(&serde_json::to_vec(&sig).unwrap()).unwrap();
    let mut bounds = Bounds::new();
    bounds.seed_entry(&sig, &[local(0), local(1)]);
    assert!(bounds.bytes_index_in_bounds(local(0), &local_expr(1)));
    assert!(!bounds.index_in_bounds(local(0), &local_expr(1)));
}

#[test]
fn byte_result_length_transfers_to_split_components() {
    let mut bounds = guarded();
    let sig = ResultSig {
        edges: vec![
            (RTerm::ResultBytesLen(1), RTerm::BytesLenParam(0), 0),
            (RTerm::BytesLenParam(0), RTerm::ResultBytesLen(1), 0),
        ],
    };
    let sig = serde_json::from_slice(&serde_json::to_vec(&sig).unwrap()).unwrap();
    bounds.transfer_many(&[local(3), local(4)], &sig, &[local_expr(0)]);
    assert!(bounds.bytes_index_in_bounds(local(4), &local_expr(1)));
    assert!(!bounds.index_in_bounds(local(4), &local_expr(1)));
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn byte_length_offset_proofs_respect_full_width_wrapping(
            length in 0i64..=i64::MAX,
            offset in -128i64..=128,
        ) {
            let mut bounds = Bounds::new();
            bounds.transfer_let(local(2), &prim(Prim::BytesLength, vec![local_expr(0)]));
            bounds.transfer_let(local(3), &prim(Prim::IntAdd, vec![local_expr(2), int_lit(offset)]));
            if let Some(distance) = bounds.bound(Term::Int(local(3)), Term::Int(local(2))) {
                prop_assert!(i128::from(length.wrapping_add(offset)) <= i128::from(length) + i128::from(distance));
            }
        }
    }
}
