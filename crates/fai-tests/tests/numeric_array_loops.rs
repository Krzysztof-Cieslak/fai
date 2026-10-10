//! Generated full-width updates preserve unique and shared numeric arrays.

mod proptests {
    use fai_db::{Db, FaiDatabase};
    use fai_runtime as rt;
    use fai_syntax::Symbol;
    use proptest::prelude::*;

    #[test]
    fn generated_updates_match_value_semantics() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), "module M\npublic bump : Int -> Array Int -> Array Int\nlet bump i xs = if i >= Array.length xs then xs else bump (i + 1) (Array.unsafeSet i (Array.unsafeGet i xs + 1) xs)\npublic main : Runtime -> Unit\nlet main _ = ()\n".into());
        let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        let bump = program.function(Symbol::intern("bump")).unwrap();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 96,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(prop::collection::vec(any::<i64>(), 0..64), any::<bool>()),
                |(values, shared)| {
                    let baseline = (rt::live_count(), rt::live_bytes());
                    let mut array = rt::fai_array_with_capacity(rt::make_int(values.len() as i64));
                    for &value in &values {
                        array = rt::fai_array_push(array, rt::make_int(value));
                    }
                    let retained = shared.then(|| rt::fai_dup(array));
                    rt::reset_allocations();
                    let result = rt::apply(rt::fai_dup(bump), &[rt::make_int(0), array]);
                    prop_assert_eq!(rt::array_copies(), i64::from(shared && !values.is_empty()));
                    for (index, &expected) in values.iter().enumerate() {
                        let actual = rt::fai_array_get_borrowed(result, rt::make_int(index as i64));
                        prop_assert_eq!(rt::read_int(actual), expected.wrapping_add(1));
                        rt::fai_drop(actual);
                        if let Some(retained) = retained {
                            let original =
                                rt::fai_array_get_borrowed(retained, rt::make_int(index as i64));
                            prop_assert_eq!(rt::read_int(original), expected);
                            rt::fai_drop(original);
                        }
                    }
                    rt::fai_drop(result);
                    if let Some(retained) = retained {
                        rt::fai_drop(retained);
                    }
                    prop_assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
                    Ok(())
                },
            )
            .unwrap();
    }
}
