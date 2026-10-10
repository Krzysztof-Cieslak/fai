//! Range-assisted division keeps signed semantics after guards and overflow.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn guarded_and_wrapping_values_match_signed_rust_arithmetic() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db
            .add_source("Main.fai".into(), include_str!("fixtures/NonnegativeDivision.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let call = |name: &str, value| {
            let function = program.borrow_mut().function(Symbol::intern(name)).unwrap();
            let result = rt::apply(function, &[rt::make_int(value)]);
            let value = rt::read_int(result);
            rt::fai_drop(result);
            value
        };
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                prop_assert_eq!(
                    call("guarded", value),
                    if value < 0 { value } else { value / 8 + value % 8 }
                );
                prop_assert_eq!(
                    call("wrapping", value),
                    if value < 0 { value } else { value.wrapping_add(2) / 2 }
                );
                prop_assert_eq!(call("signed", value), value / 2);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
