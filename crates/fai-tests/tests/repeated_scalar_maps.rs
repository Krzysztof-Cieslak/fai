//! Interchanged numeric maps retain exact results, shared inputs and bounded space.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = include_str!("fixtures/RepeatedScalarMaps.fai");

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), SOURCE.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }

    fn floats(&mut self, input: &[u64], start: i64, limit: i64, shared: bool) {
        let baseline = (rt::live_count(), rt::live_bytes());
        let mut array = rt::fai_array_with_capacity(rt::make_int(input.len() as i64));
        for bits in input {
            array = rt::fai_array_push(array, rt::fai_box_float(*bits as i64));
        }
        let retained = shared.then(|| rt::fai_dup(array));
        let function = self.program.function(Symbol::intern("floats")).unwrap();
        let result = rt::apply(function, &[rt::make_int(start), rt::make_int(limit), array]);
        for (index, bits) in input.iter().enumerate() {
            let mut expected = f64::from_bits(*bits);
            for _ in start..limit {
                expected = expected * 0.5 + 0.25;
            }
            let value = rt::fai_array_get_borrowed(result, rt::make_int(index as i64));
            assert_eq!(rt::read_float(value).to_bits(), expected.to_bits());
            rt::fai_drop(value);
            if let Some(retained) = retained {
                let original = rt::fai_array_get_borrowed(retained, rt::make_int(index as i64));
                assert_eq!(rt::read_float(original).to_bits(), *bits);
                rt::fai_drop(original);
            }
        }
        rt::fai_drop(result);
        if let Some(retained) = retained {
            rt::fai_drop(retained);
        }
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }
}

#[test]
fn empty_inputs_remain_empty() {
    Harness::new().floats(&[], -2, 3, false);
}

#[test]
fn an_odd_final_element_keeps_its_own_iterations() {
    Harness::new().floats(&[1.0f64.to_bits(), 2.0f64.to_bits(), 3.0f64.to_bits()], 0, 7, false);
}

#[test]
fn shared_arrays_keep_their_original_elements() {
    Harness::new().floats(&[(-0.0f64).to_bits(), 8.0f64.to_bits()], -3, 4, true);
}

#[test]
fn completed_counters_preserve_exact_original_bits() {
    Harness::new().floats(&[0x7ff0_0000_0000_2345, 0x8000_0000_0000_0000], 4, -1, true);
}

#[test]
fn full_width_counters_finish_without_wrapping() {
    Harness::new().floats(&[1, f64::MAX.to_bits()], i64::MAX - 2, i64::MAX, false);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn paired_float_records_match_each_original_transition() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(
                    prop::collection::vec((any::<u64>(), any::<u64>()), 0..8),
                    0..8i64,
                    any::<bool>(),
                ),
                |(values, count, shared)| {
                    let baseline = rt::live_count();
                    let mut array = rt::fai_array_with_capacity(rt::make_int(values.len() as i64));
                    for (x, y) in &values {
                        let make = harness
                            .borrow_mut()
                            .program
                            .function(Symbol::intern("recordOne"))
                            .unwrap();
                        let record = rt::apply(
                            make,
                            &[rt::fai_box_float(*x as i64), rt::fai_box_float(*y as i64)],
                        );
                        array = rt::fai_array_push(array, record);
                    }
                    let retained = shared.then(|| rt::fai_dup(array));
                    let function =
                        harness.borrow_mut().program.function(Symbol::intern("records")).unwrap();
                    let result =
                        rt::apply(function, &[rt::make_int(0), rt::make_int(count), array]);
                    for (index, (x, y)) in values.iter().enumerate() {
                        let (mut x, mut y) = (f64::from_bits(*x), f64::from_bits(*y));
                        for _ in 0..count {
                            x = x * 0.5 + 0.25;
                            y += 1.0;
                        }
                        let record = rt::fai_array_get_borrowed(result, rt::make_int(index as i64));
                        let actual_x = rt::fai_data_field(record, 0);
                        let actual_y = rt::fai_data_field(record, 1);
                        prop_assert_eq!(rt::read_float(actual_x).to_bits(), x.to_bits());
                        prop_assert_eq!(rt::read_float(actual_y).to_bits(), y.to_bits());
                        rt::fai_drop(actual_x);
                        rt::fai_drop(actual_y);
                        rt::fai_drop(record);
                        if let Some(retained) = retained {
                            let record =
                                rt::fai_array_get_borrowed(retained, rt::make_int(index as i64));
                            let old_x = rt::fai_data_field(record, 0);
                            let old_y = rt::fai_data_field(record, 1);
                            prop_assert_eq!(rt::read_float(old_x).to_bits(), values[index].0);
                            prop_assert_eq!(rt::read_float(old_y).to_bits(), values[index].1);
                            rt::fai_drop(old_x);
                            rt::fai_drop(old_y);
                            rt::fai_drop(record);
                        }
                    }
                    rt::fai_drop(result);
                    if let Some(retained) = retained {
                        rt::fai_drop(retained);
                    }
                    prop_assert_eq!(rt::live_count(), baseline);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn arbitrary_float_bits_match_iteration_major_execution() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(prop::collection::vec(any::<u64>(), 0..8), -3i64..8, -3i64..8, any::<bool>()),
                |(bits, start, limit, shared)| {
                    harness.borrow_mut().floats(&bits, start, limit, shared);
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn full_width_integer_elements_keep_wrapping_arithmetic() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), 0..8i64), |(value, count)| {
                let baseline = rt::live_count();
                let array = rt::fai_array_repeat(rt::make_int(3), rt::make_int(value));
                let function =
                    harness.borrow_mut().program.function(Symbol::intern("ints")).unwrap();
                let result = rt::apply(function, &[rt::make_int(0), rt::make_int(count), array]);
                let expected =
                    (0..count).fold(value, |value, _| value.wrapping_mul(3).wrapping_add(1));
                let element = rt::fai_array_get_borrowed(result, rt::make_int(2));
                prop_assert_eq!(rt::read_int(element), expected);
                rt::fai_drop(element);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
