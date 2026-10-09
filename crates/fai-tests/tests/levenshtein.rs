//! Single-row edit distance matches a two-row oracle and preserves shared inputs.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

fn array(values: &[i64]) -> rt::Value {
    values.iter().fold(rt::fai_array_with_capacity(rt::make_int(values.len() as i64)), |a, &x| {
        rt::fai_array_push(a, rt::make_int(x))
    })
}

fn reference(a: &[i64], b: &[i64]) -> i64 {
    let mut old: Vec<_> = (0..=b.len() as i64).collect();
    for (i, &x) in a.iter().enumerate() {
        let mut row = vec![i as i64 + 1];
        for (j, &y) in b.iter().enumerate() {
            row.push((row[j] + 1).min(old[j + 1] + 1).min(old[j] + i64::from(x != y)));
        }
        old = row;
    }
    old[b.len()]
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = format!(
            "{}\npublic distance : Array Int -> Array Int -> Int\nlet distance a b =\n  let row = runRows 1 a b (Array.range 0 (Array.length b + 1))\n  Array.unsafeGet (Array.length b) row\npublic step : Int -> Int -> Array Int -> Array Int -> Array Int\nlet step a i b row = fillRow a i b row\n",
            include_str!("../../../samples/algorithms/Levenshtein.fai")
        );
        let id = db.add_source("Levenshtein.fai".into(), source);
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
            .unwrap_or_else(|d| panic!("{d:?}"));
        Self { program, _guard: guard }
    }

    fn check(&mut self, a: &[i64], b: &[i64]) -> i64 {
        let baseline = rt::live_count();
        let first = array(a);
        let second = array(b);
        let f = self.program.function(Symbol::intern("distance")).unwrap();
        rt::reset_allocations();
        let result = rt::apply(f, &[first, second]);
        let allocations = rt::allocations();
        assert_eq!(rt::read_int(result), reference(a, b));
        rt::fai_drop(result);
        assert_eq!(rt::live_count(), baseline);
        allocations
    }
}

#[test]
fn empty_sequences_have_zero_distance() {
    Harness::new().check(&[], &[]);
}
#[test]
fn empty_left_inserts_the_right_sequence() {
    Harness::new().check(&[], &[1, 2, 3]);
}
#[test]
fn empty_right_deletes_the_left_sequence() {
    Harness::new().check(&[1, 2, 3], &[]);
}
#[test]
fn one_element_substitution_uses_the_old_diagonal() {
    Harness::new().check(&[1], &[2]);
}
#[test]
fn equal_sequences_have_zero_distance() {
    Harness::new().check(&[1, 2, 3], &[1, 2, 3]);
}
#[test]
fn disjoint_unequal_lengths_match_the_oracle() {
    Harness::new().check(&[1, 2, 3], &[4, 5]);
}
#[test]
fn repeated_values_match_the_oracle() {
    Harness::new().check(&[1, 1, 2, 1], &[1, 2, 2, 1, 1]);
}

#[test]
fn row_allocations_are_constant_in_the_left_length() {
    let mut h = Harness::new();
    assert_eq!(h.check(&[1; 4], &[2; 12]), 1);
    assert_eq!(h.check(&[1; 80], &[2; 12]), 1);
}

#[test]
fn updating_a_shared_row_preserves_the_original() {
    let mut h = Harness::new();
    let f = h.program.function(Symbol::intern("step")).unwrap();
    let row = array(&[0, 1, 2]);
    let retained = rt::fai_dup(row);
    let b = array(&[4, 5]);
    rt::reset_allocations();
    let result = rt::apply(f, &[rt::make_int(4), rt::make_int(1), b, row]);
    assert_eq!(rt::array_copies(), 1);
    let read = |value| {
        (0..3)
            .map(|i| rt::read_int(rt::fai_array_get_borrowed(value, rt::make_int(i))))
            .collect::<Vec<_>>()
    };
    assert_eq!(read(retained), vec![0, 1, 2]);
    assert_eq!(read(result), vec![1, 0, 1]);
    rt::fai_drop(retained);
    rt::fai_drop(result);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn generated_sequences_match_two_row_reference() {
        let harness = RefCell::new(Harness::new());
        proptest::test_runner::TestRunner::default()
            .run(
                &(prop::collection::vec(-3i64..4, 0..16), prop::collection::vec(-3i64..4, 0..16)),
                |(a, b)| {
                    harness.borrow_mut().check(&a, &b);
                    Ok(())
                },
            )
            .unwrap();
    }
}
