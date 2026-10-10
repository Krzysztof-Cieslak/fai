//! Ordered literal folds preserve full-width arithmetic and release all values.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn both_fold_orders_match_their_original_scalar_operations() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db
            .add_source("Main.fai".into(), include_str!("fixtures/OrderedLiteralFold.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let call = |name: &str, value| {
            let function = program.borrow_mut().function(Symbol::intern(name)).unwrap();
            let result = rt::apply(function, &[rt::make_int(value)]);
            let output = rt::read_int(result);
            rt::fai_drop(result);
            output
        };
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                let a = value | 1;
                let b = value.wrapping_add(2) | 1;
                prop_assert_eq!(
                    call("compute", value),
                    value.wrapping_add(100 / a).wrapping_add(100 / b)
                );
                prop_assert_eq!(
                    call("computeRight", value),
                    (100i64 / a).wrapping_sub((100i64 / b).wrapping_sub(value))
                );
                let function = program.borrow_mut().function(Symbol::intern("floatBits")).unwrap();
                let result = rt::apply(function, &[rt::fai_box_float(value)]);
                prop_assert_eq!(rt::read_int(result), value);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
