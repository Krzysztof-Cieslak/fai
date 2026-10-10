//! Scaled-add lowering agrees with full-width wrapping multiplication.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn both_operand_orders_preserve_all_low_word_products() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(
            "Main.fai".into(),
            include_str!("fixtures/ScaledIntegerMultiples.fai").into(),
        );
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                let function = program.borrow_mut().function(Symbol::intern("products")).unwrap();
                let result = rt::apply(function, &[rt::make_int(value)]);
                let actual: Vec<_> = (0..6)
                    .map(|index| {
                        let field = rt::fai_data_field(result, index);
                        let value = rt::read_int(field);
                        rt::fai_drop(field);
                        value
                    })
                    .collect();
                let expected = vec![
                    value.wrapping_mul(3),
                    value.wrapping_mul(3),
                    value.wrapping_mul(5),
                    value.wrapping_mul(5),
                    value.wrapping_mul(9),
                    value.wrapping_mul(9),
                ];
                prop_assert_eq!(actual, expected);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
