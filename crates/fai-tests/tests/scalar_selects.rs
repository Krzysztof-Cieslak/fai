//! Eager raw-integer alternatives preserve wrapping arithmetic and branch barriers.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn full_width_selects_match_wrapping_rust_and_skip_untaken_traps() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db
            .add_source("Main.fai".into(), include_str!("fixtures/SmallIntegerSelects.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                let function = program.borrow_mut().function(Symbol::intern("step")).unwrap();
                let result = rt::apply(function, &[rt::make_int(value)]);
                let expected =
                    if value % 2 == 0 { value / 2 } else { value.wrapping_mul(3).wrapping_add(1) };
                prop_assert_eq!(rt::read_int(result), expected);
                rt::fai_drop(result);
                let function = program.borrow_mut().function(Symbol::intern("guarded")).unwrap();
                let result = rt::apply(function, &[rt::make_int(1), rt::make_int(value)]);
                prop_assert_eq!(rt::read_int(result), value);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
