//! Signed remainder predicates retain full-width values and loop SSA provenance.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn signed_predicates_and_observed_remainders_match_rust() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(
            "Main.fai".into(),
            include_str!("fixtures/PowerTwoDivisibility.fai").into(),
        );
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
                let remainder = value % 8;
                prop_assert_eq!(call("divisible", value) != 0, remainder == 0);
                prop_assert_eq!(call("reversed", value) != 0, remainder == 0);
                prop_assert_eq!(
                    call("observed", value),
                    if remainder == 0 { 41 } else { remainder }
                );
                prop_assert_eq!(call("otherRemainder", value) != 0, remainder == 1);
                prop_assert_eq!(call("count", value), 4);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
