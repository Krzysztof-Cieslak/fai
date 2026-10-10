//! Constructor reduction preserves full-width values and closure capture scopes.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn constructor_branches_keep_integer_and_float_bits() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id =
            db.add_source("Main.fai".into(), include_str!("fixtures/LocalConstructors.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), any::<bool>(), any::<u64>()), |(value, yes, bits)| {
                let baseline = rt::live_count();
                let function = program.borrow_mut().function(Symbol::intern("compute")).unwrap();
                let result =
                    rt::apply(function, &[rt::make_int(i64::from(yes)), rt::make_int(value)]);
                let expected =
                    if yes { value.wrapping_add(value.wrapping_add(1)).wrapping_add(7) } else { 7 };
                prop_assert_eq!(rt::read_int(result), expected);
                rt::fai_drop(result);
                let function = program.borrow_mut().function(Symbol::intern("floatBits")).unwrap();
                let result = rt::apply(function, &[rt::fai_box_float(bits as i64)]);
                prop_assert_eq!(rt::read_int(result) as u64, bits);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
