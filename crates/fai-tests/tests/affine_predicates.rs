//! Affine comparison bounds agree with step-by-step wrapping subtraction.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    fn verify(name: &str, step: i64, fixed_step: i64) {
        let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id =
            db.add_source("Main.fai".into(), include_str!("fixtures/AffinePredicates.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(any::<i64>(), any::<i64>(), prop::collection::vec((0..5u8, any::<i64>()), 0..16)),
                |(fixed, counter, inputs)| {
                    let baseline = rt::live_count();
                    let (mut position, mut center) = (counter, fixed);
                    let values: Vec<_> = inputs
                        .into_iter()
                        .map(|(kind, raw)| {
                            let value = match kind {
                                0 => center.wrapping_add(position),
                                1 => center.wrapping_sub(position),
                                _ => raw,
                            };
                            position = position.wrapping_add(step);
                            center = center.wrapping_add(fixed_step);
                            value
                        })
                        .collect();
                    let mut array = rt::fai_array_with_capacity(rt::make_int(values.len() as i64));
                    for &value in &values {
                        array = rt::fai_array_push(array, rt::make_int(value));
                    }
                    let (mut position, mut center) = (counter, fixed);
                    let expected = values.iter().all(|value| {
                        let ok = value.wrapping_sub(center) != position
                            && center.wrapping_sub(*value) != position;
                        position = position.wrapping_add(step);
                        center = center.wrapping_add(fixed_step);
                        ok
                    });
                    let function = program.borrow_mut().function(Symbol::intern(name)).unwrap();
                    let result =
                        rt::apply(function, &[rt::make_int(fixed), rt::make_int(counter), array]);
                    prop_assert_eq!(rt::read_int(result) != 0, expected);
                    rt::fai_drop(result);
                    prop_assert_eq!(rt::live_count(), baseline);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn increasing_counters_preserve_full_width_predicates() {
        verify("run", 1, 0);
    }

    #[test]
    fn decreasing_counters_preserve_full_width_predicates() {
        verify("backward", -7, 0);
    }

    #[test]
    fn changing_invariants_keep_the_original_comparisons() {
        verify("dynamic", 1, 3);
    }
}
